//! Telegram client: the outbox anything upstream drops a [`Notification`] into, and the task
//! that gets it delivered.
//!
//! What to say and when was decided upstream; this module only knows how to say it — wait for
//! the link, send, retry, report the outcome on the LED. See `docs/telegram-bot.md`, including
//! why certificate verification is off.
//!
//! The outbox is a max-heap keyed on [`Urgency`] and then on arrival, so an alarm queued behind
//! an info goes out first and two equally urgent messages keep their order. When it is full the
//! quietest, oldest message it holds goes — which may be the one being queued.
//!
//! One message is in flight at a time and keeps its retry budget. A louder one arriving while
//! that message waits — for the link, or for its next attempt — takes the link instead: the
//! waiting message goes back in the outbox, keeping its place in the order but not its budget.

use alloc::{format, string::String, vec::Vec};
use core::{
    cmp::{Ordering, Reverse},
    fmt::Write as _,
    future::{Future, pending, poll_fn},
    sync::atomic::{AtomicU32, Ordering::Relaxed},
    task::Poll,
};

use embassy_futures::select::{Either, select};
use embassy_net::{
    Stack,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    channel::TrySendError,
    priority_channel::{Max, PriorityChannel},
};
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::rng::Rng;
use reqwless::{
    client::{HttpClient, TlsConfig, TlsVerify},
    request::Method,
};
use static_cell::StaticCell;

use crate::{
    config,
    led::{self, LedCmd, Rgb},
};

/// One message in flight and a few waiting is already more than a link this slow can spend.
const DEPTH: usize = 4;

static OUTBOX: PriorityChannel<CriticalSectionRawMutex, Notification, Max, DEPTH> =
    PriorityChannel::new();

/// Stamped on every message the outbox takes, so equal urgencies keep the order spoken in.
static SEQ: AtomicU32 = AtomicU32::new(0);

/// Hand a message to the notifier. Never blocks.
pub fn notify(mut message: Notification) {
    if !configured() {
        // Nothing drains the outbox with the notifier off, so it must not fill either.
        log::debug!("Telegram off, ignoring: {}", message.text);
        return;
    }

    message.seq = SEQ.fetch_add(1, Relaxed);
    enqueue(message);
}

/// Queues `message`, losing the least worthwhile in hand — possibly `message` itself.
fn enqueue(message: Notification) {
    let Err(TrySendError::Full(message)) = OUTBOX.try_send(message) else {
        return;
    };

    // `remove_if` is the only way to look inside the heap and it clones; draining moves.
    let mut held = Vec::with_capacity(DEPTH + 1);
    while let Ok(queued) = OUTBOX.try_receive() {
        held.push(queued);
    }
    held.push(message);

    // Worth most first, so the quietest and oldest of them ends up last.
    held.sort_unstable_by_key(|queued| Reverse((queued.urgency, queued.seq)));

    if let Some(dropped) = held.pop() {
        log::warn!("Outbox full, dropping: {}", dropped.text);
    }

    for queued in held {
        let _ = OUTBOX.try_send(queued);
    }
}

/// A fresh clone builds with the notifier off, and `telegram_task` then never runs.
fn configured() -> bool {
    let config = config::config();
    !config.telegram_token.is_empty() && !config.telegram_chat_id.is_empty()
}

/// Something for a person to read.
#[derive(Debug, Clone)]
pub struct Notification {
    pub text: String,
    pub urgency: Urgency,
    seq: u32,
}

/// Declared quietest first: this order *is* the outbox's priority rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    /// True only right now — better dropped than delivered late.
    Perishable,
    /// Worth having in the log, not worth waking anyone.
    Info,
    /// Must reach a person, and must not be dropped on the way there.
    Alarm,
}

impl Notification {
    pub fn alarm(text: String) -> Self {
        Self::new(text, Urgency::Alarm)
    }

    pub fn info(text: String) -> Self {
        Self::new(text, Urgency::Info)
    }

    pub fn perishable(text: String) -> Self {
        Self::new(text, Urgency::Perishable)
    }

    /// `seq` means nothing until [`notify`] stamps it, which is where the order begins.
    fn new(text: String, urgency: Urgency) -> Self {
        Self {
            text,
            urgency,
            seq: 0,
        }
    }
}

/// Loudest first, then earliest — a max-heap orders equal keys however it likes.
impl Ord for Notification {
    fn cmp(&self, other: &Self) -> Ordering {
        self.urgency
            .cmp(&other.urgency)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for Notification {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Notification {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Notification {}

#[embassy_executor::task]
pub async fn telegram_task(stack: Stack<'static>) {
    let config = config::config();

    if !configured() {
        log::warn!("Telegram not configured — see docs/telegram-bot.md");
        // Park instead of returning, the way `wifi_task` does without credentials.
        return pending().await;
    }

    let mut bot = Bot::new(stack, config.telegram_token, config.telegram_chat_id);

    loop {
        let message = OUTBOX.receive().await;
        let mut attempts = 0;

        loop {
            // Link state comes from the stack: a send can park this task for half a minute.
            if !stack.is_config_up() {
                // The link can stay down for hours; this would arrive reading as current.
                if message.urgency == Urgency::Perishable {
                    log::warn!("Telegram: link down, dropping: {}", message.text);
                    break;
                }
                if preempted(message.urgency, stack.wait_config_up()).await {
                    enqueue(message);
                    break;
                }
                continue;
            }

            let Err(e) = bot.send(&message).await else {
                log::info!("Telegram: sent {}", message.text);
                led::send(LedCmd::blink(Rgb::BLUE, 1));
                break;
            };

            led::send(LedCmd::blink(Rgb::YELLOW, 2));

            let Some(delay) = backoff(&mut attempts, message.urgency) else {
                log::warn!("Telegram: {e}, giving up on: {}", message.text);
                break;
            };

            log::warn!("Telegram: {e}, retrying in {}s", delay.as_secs());

            if preempted(message.urgency, Timer::after(delay)).await {
                enqueue(message);
                break;
            }
        }
    }
}

/// Waits for `wait`, unless a louder message turns up first and takes the link instead.
async fn preempted(urgency: Urgency, wait: impl Future<Output = ()>) -> bool {
    matches!(select(wait, louder_than(urgency)).await, Either::Second(()))
}

/// Resolves once the outbox holds something more urgent than `urgency`.
async fn louder_than(urgency: Urgency) {
    poll_fn(|cx| {
        // Registers the receiver waker, so the next `enqueue` polls this again.
        let _ = OUTBOX.poll_ready_to_receive(cx);

        match OUTBOX.try_peek() {
            Ok(next) if next.urgency > urgency => Poll::Ready(()),
            _ => Poll::Pending,
        }
    })
    .await
}

/// Waits between the attempts at one message; the last one repeats.
const BACKOFF: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(45),
];

/// Attempts past the first; an alarm gives up after ~15 min so it cannot starve the rest.
const fn retries(urgency: Urgency) -> u8 {
    match urgency {
        Urgency::Alarm => 20,
        Urgency::Info => BACKOFF.len() as u8,
        Urgency::Perishable => 0,
    }
}

/// `disable_notification`: delivered as a badge, with no sound or vibration.
const fn disable_notification(urgency: Urgency) -> bool {
    !matches!(urgency, Urgency::Alarm)
}

/// How long to wait before trying again, or `None` once the budget is spent.
fn backoff(attempts: &mut u8, urgency: Urgency) -> Option<Duration> {
    (*attempts < retries(urgency)).then(|| {
        let delay = BACKOFF[usize::from(*attempts).min(BACKOFF.len() - 1)];
        *attempts = attempts.saturating_add(1);
        delay
    })
}

const HOST: &str = "api.telegram.org";

/// The reply is a short JSON object; RX is sized for the TLS certificate flight instead.
const TCP_TX: usize = 1536;
const TCP_RX: usize = 4096;

/// Whole-request budget: DNS, TCP, the TLS handshake and the exchange itself.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Give up on a socket that goes quiet well before [`SEND_TIMEOUT`] would.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

/// The Telegram Bot API over HTTPS: plain `GET` with query parameters, so no JSON to encode.
struct Bot {
    token: &'static str,
    chat_id: &'static str,
    tcp: TcpClient<'static, 1, TCP_TX, TCP_RX>,
    dns: DnsSocket<'static>,
    tls_rx: &'static mut [u8],
    tls_tx: &'static mut [u8],
    headers: &'static mut [u8],
}

impl Bot {
    fn new(stack: Stack<'static>, token: &'static str, chat_id: &'static str) -> Self {
        // `.bss`, not the heap: ~27 KiB is most of the 64 KiB `heap_allocator!` gives out.
        static TLS_RX: StaticCell<[u8; 16640]> = StaticCell::new();
        static TLS_TX: StaticCell<[u8; 4096]> = StaticCell::new();
        static HEADERS: StaticCell<[u8; 1024]> = StaticCell::new();
        static SOCKETS: StaticCell<TcpClientState<1, TCP_TX, TCP_RX>> = StaticCell::new();

        let mut tcp = TcpClient::new(stack, SOCKETS.init(TcpClientState::new()));
        tcp.set_timeout(Some(SOCKET_TIMEOUT));

        Self {
            token,
            chat_id,
            tcp,
            dns: DnsSocket::new(stack),
            tls_rx: TLS_RX.init([0; 16640]),
            tls_tx: TLS_TX.init([0; 4096]),
            headers: HEADERS.init([0; 1024]),
        }
    }

    async fn send(&mut self, message: &Notification) -> Result<(), SendError> {
        let url = self.url(message);

        with_timeout(SEND_TIMEOUT, self.request(&url))
            .await
            .unwrap_or(Err(SendError::Timeout))
    }

    /// Carries the bot token — never log it.
    fn url(&self, message: &Notification) -> String {
        let mut url = format!("https://{HOST}/bot{}/sendMessage?chat_id=", self.token);
        encode(&mut url, self.chat_id);
        if disable_notification(message.urgency) {
            url.push_str("&disable_notification=true");
        }
        url.push_str("&text=");
        encode(&mut url, &message.text);
        url
    }

    async fn request(&mut self, url: &str) -> Result<(), SendError> {
        let Self {
            tcp,
            dns,
            tls_rx,
            tls_tx,
            headers,
            ..
        } = self;

        // `reqwless` derives the session's randomness from this seed; the RNG is true with RF on.
        let rng = Rng::new();
        let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());

        // Certificate verification is off — see "TLS" in `docs/telegram-bot.md`.
        let tls = TlsConfig::new(seed, &mut tls_rx[..], &mut tls_tx[..], TlsVerify::None);
        let mut client = HttpClient::new_with_tls(tcp, dns, tls);

        let mut request = client
            .request(Method::GET, url)
            .await
            .map_err(SendError::Http)?;
        let response = request
            .send(&mut headers[..])
            .await
            .map_err(SendError::Http)?;

        if response.status.is_successful() {
            Ok(())
        } else {
            Err(SendError::Status(response.status.0))
        }
    }
}

enum SendError {
    Timeout,
    Http(reqwless::Error),
    /// Telegram answered, and said no.
    Status(u16),
}

impl core::fmt::Display for SendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Timeout => write!(f, "no answer within {}s", SEND_TIMEOUT.as_secs()),
            Self::Http(e) => write!(f, "{e}"),
            Self::Status(status) => write!(f, "HTTP {status}"),
        }
    }
}

/// Percent-encode byte-wise, not char-wise: multi-byte UTF-8 needs an escape per byte.
fn encode(out: &mut String, value: &str) {
    for &byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
}
