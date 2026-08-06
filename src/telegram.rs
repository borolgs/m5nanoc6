//! Telegram client: takes a [`Notification`] and gets it delivered.
//!
//! What to say and when is [`crate::app`]'s business; this module only knows how to say it —
//! wait for the link, send, retry, report the outcome on the LED. See `docs/telegram-bot.md`,
//! including why certificate verification is off.

use alloc::{format, string::String};
use core::{
    fmt::Write as _,
    future::{Future, pending},
};

use embassy_futures::select::{Either, select};
use embassy_net::{
    Stack,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::rng::Rng;
use reqwless::{
    client::{HttpClient, TlsConfig, TlsVerify},
    request::Method,
};
use static_cell::StaticCell;

use crate::{
    config,
    events::{LedCmd, Publisher, Rgb},
};

#[embassy_executor::task]
pub async fn telegram_task(stack: Stack<'static>, publisher: Publisher) {
    let config = config::config();

    if config.telegram_token.is_empty() || config.telegram_chat_id.is_empty() {
        log::warn!("Telegram not configured — see docs/telegram-bot.md");
        // Park instead of returning, the way `wifi_task` does without credentials.
        return pending().await;
    }

    let mut bot = Bot::new(stack, config.telegram_token, config.telegram_chat_id);
    let mut outgoing = Pending::next().await;

    loop {
        // Link state comes from the stack, not the bus: the bus is lossy and a send can park
        // this task long enough to miss a `Wifi` event outright.
        if !stack.is_config_up() {
            outgoing.wait_or_replace(stack.wait_config_up()).await;
            continue;
        }

        match bot.send(&outgoing.message).await {
            Ok(()) => {
                log::info!("Telegram: sent {}", outgoing.message.text);
                publisher.publish_immediate(LedCmd::blink(Rgb::BLUE, 1).into());
                outgoing = Pending::next().await;
            }
            Err(e) => {
                publisher.publish_immediate(LedCmd::blink(Rgb::YELLOW, 2).into());
                match outgoing.backoff() {
                    Some(delay) => {
                        log::warn!("Telegram: {e}, retrying in {}s", delay.as_secs());
                        outgoing.wait_or_replace(Timer::after(delay)).await;
                    }
                    None => {
                        log::warn!("Telegram: {e}, giving up on: {}", outgoing.message.text);
                        outgoing = Pending::next().await;
                    }
                }
            }
        }
    }
}

/// The one message waiting to go out. A newer one replaces it instead of queueing behind it:
/// an alarm matters more than the heartbeat it displaces, and a recovery more than the alarm
/// before it.
static OUTBOX: Signal<CriticalSectionRawMutex, Notification> = Signal::new();

/// Hand a message to [`telegram_task`]. Never blocks, so the caller keeps serving the bus
/// while a send is parked on the network.
pub fn notify(message: Notification) {
    if let Some(dropped) = OUTBOX.try_take() {
        log::warn!(
            "Telegram: dropping an undelivered message: {}",
            dropped.text
        );
    }
    OUTBOX.signal(message);
}

pub struct Notification {
    text: String,
    /// `disable_notification`: delivered as a badge, with no sound or vibration.
    silent: bool,
    /// Attempts past the first before the message is given up on.
    retries: u8,
}

impl Notification {
    pub fn loud(text: String) -> Self {
        Self {
            text,
            silent: false,
            retries: BACKOFF.len() as u8,
        }
    }

    pub fn silent(text: String) -> Self {
        Self {
            silent: true,
            ..Self::loud(text)
        }
    }

    /// Drop the message rather than retry it, for one that goes stale — a heartbeat's
    /// replacement is due soon enough, and a stale "alive" is worse than none at all.
    pub fn no_retry(self) -> Self {
        Self { retries: 0, ..self }
    }
}

/// Waits between the attempts at one message.
const BACKOFF: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(45),
];

/// The message being delivered, and what it has cost so far.
struct Pending {
    message: Notification,
    attempts: u8,
}

impl Pending {
    async fn next() -> Self {
        Self {
            message: OUTBOX.wait().await,
            attempts: 0,
        }
    }

    /// Wait for `until`, unless a newer message arrives first and takes this one's place.
    async fn wait_or_replace(&mut self, until: impl Future<Output = ()>) {
        if let Either::Second(newer) = select(until, OUTBOX.wait()).await {
            log::warn!("Telegram: giving up on: {}", self.message.text);
            self.message = newer;
            self.attempts = 0;
        }
    }

    /// How long to wait before trying again, or `None` once the budget is spent.
    fn backoff(&mut self) -> Option<Duration> {
        (self.attempts < self.message.retries).then(|| {
            let delay = BACKOFF[usize::from(self.attempts).min(BACKOFF.len() - 1)];
            self.attempts += 1;
            delay
        })
    }
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
        // `.bss`, not the heap: ~27 KiB held for the lifetime of the task would be most of
        // the 64 KiB `heap_allocator!` gives out. `TLS_RX` is the knob if RAM runs short —
        // 16640 is the largest TLS 1.3 record, but the handshake fits in 8192.
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
        if message.silent {
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
