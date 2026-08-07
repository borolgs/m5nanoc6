//! Telegram client: the outbox anything upstream drops a [`Notification`] into, and the task
//! that gets it delivered.
//!
//! What to say and when was decided upstream; this module only knows how to say it — wait for
//! the link, send, retry, report the outcome on the LED. See `docs/telegram-bot.md`, including
//! why certificate verification is off.
//!
//! The outbox is a max-heap keyed on [`Urgency`], so an alarm queued behind an info goes out
//! first without either being lost. One message is in flight at a time and keeps its retry
//! budget: a louder one that arrives mid-flight is served next, not instead.

use alloc::{format, string::String};
use core::{cmp::Ordering, fmt::Write as _, future::pending};

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

/// Hand a message to the notifier. Never blocks, so the caller keeps serving its own channels
/// while a send is parked on the network.
pub fn notify(message: Notification) {
    let Err(TrySendError::Full(message)) = OUTBOX.try_send(message) else {
        return;
    };

    // Room for a louder message is worth more than every quieter one already waiting.
    OUTBOX.remove_if(|queued| queued.urgency < message.urgency);

    if let Err(TrySendError::Full(message)) = OUTBOX.try_send(message) {
        log::warn!("Outbox full, dropping: {}", message.text);
    }
}

/// Something for a person to read.
#[derive(Debug, Clone)]
pub struct Notification {
    pub text: String,
    pub urgency: Urgency,
}

/// How badly the device wants a message delivered. A transport reads its own settings off
/// this — how loudly to ring, how long to keep trying — and nothing above it needs to know
/// what those are.
///
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
        Self {
            text,
            urgency: Urgency::Alarm,
        }
    }

    pub fn info(text: String) -> Self {
        Self {
            text,
            urgency: Urgency::Info,
        }
    }

    pub fn perishable(text: String) -> Self {
        Self {
            text,
            urgency: Urgency::Perishable,
        }
    }
}

/// Urgency alone, because that is what the outbox sorts on. "Equal" therefore means equally
/// urgent, not the same message — the heap doesn't care, but a reader might.
impl Ord for Notification {
    fn cmp(&self, other: &Self) -> Ordering {
        self.urgency.cmp(&other.urgency)
    }
}

impl PartialOrd for Notification {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Notification {
    fn eq(&self, other: &Self) -> bool {
        self.urgency == other.urgency
    }
}

impl Eq for Notification {}

#[embassy_executor::task]
pub async fn telegram_task(stack: Stack<'static>) {
    let config = config::config();

    if config.telegram_token.is_empty() || config.telegram_chat_id.is_empty() {
        log::warn!("Telegram not configured — see docs/telegram-bot.md");
        // Park instead of returning, the way `wifi_task` does without credentials.
        return pending().await;
    }

    let mut bot = Bot::new(stack, config.telegram_token, config.telegram_chat_id);

    loop {
        let message = OUTBOX.receive().await;
        let mut attempts = 0;

        loop {
            // Link state comes from the stack, not `wifi.rs`: a send parks this task for up to
            // half a minute, and what matters is whether there is a link now.
            if !stack.is_config_up() {
                // The link can stay down for hours; this would arrive reading as current.
                if message.urgency == Urgency::Perishable {
                    log::warn!("Telegram: link down, dropping: {}", message.text);
                    break;
                }
                stack.wait_config_up().await;
                continue;
            }

            match bot.send(&message).await {
                Ok(()) => {
                    log::info!("Telegram: sent {}", message.text);
                    led::send(LedCmd::blink(Rgb::BLUE, 1));
                    break;
                }
                Err(e) => {
                    led::send(LedCmd::blink(Rgb::YELLOW, 2));

                    match backoff(&mut attempts, message.urgency) {
                        Some(delay) => {
                            log::warn!("Telegram: {e}, retrying in {}s", delay.as_secs());
                            Timer::after(delay).await;
                        }
                        None => {
                            log::warn!("Telegram: {e}, giving up on: {}", message.text);
                            break;
                        }
                    }
                }
            }
        }
    }
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

/// TCP buffers behind the one connection at a time. The reply is a short JSON object; the
/// receive side is sized for the TLS certificate flight instead.
const TCP_TX: usize = 1536;
const TCP_RX: usize = 4096;

/// Whole-request budget: DNS, TCP, the TLS handshake and the exchange itself.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Give up on a socket that goes quiet well before [`SEND_TIMEOUT`] would.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

/// The Telegram Bot API over HTTPS, one connection per message.
///
/// The API takes plain `GET` with query parameters, so there is no JSON to encode.
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
        // `.bss`, not the heap: ~27 KiB for the lifetime of the task would be most of the
        // 64 KiB `heap_allocator!` gives out.
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

        // A fresh seed per handshake: `reqwless` derives the whole session's randomness from
        // it. The hardware RNG is a true one while the RF subsystem is on, which it is — we
        // only get here with a link.
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

/// Percent-encode into `out`, escaping everything outside the unreserved set.
///
/// Byte-wise, not char-wise: `°` and Cyrillic are multi-byte UTF-8 and each byte needs its
/// own escape.
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
