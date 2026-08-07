//! Telegram client: takes a [`Notification`] off the outbox and gets it delivered.
//!
//! What to say and when was decided upstream; this module only knows how to say it — wait for
//! the link, send, retry, report the outcome on the LED. See `docs/telegram-bot.md`, including
//! why certificate verification is off.

use alloc::{format, string::String};
use core::{
    fmt::Write as _,
    future::{Future, pending},
    pin::pin,
};

use embassy_futures::select::{Either, select};
use embassy_net::{
    Stack,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
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
    events::{Notification, Urgency, next_notification},
    led::{self, LedCmd, Rgb},
};

#[embassy_executor::task]
pub async fn telegram_task(stack: Stack<'static>) {
    let config = config::config();

    if config.telegram_token.is_empty() || config.telegram_chat_id.is_empty() {
        log::warn!("Telegram not configured — see docs/telegram-bot.md");
        // Park instead of returning, the way `wifi_task` does without credentials.
        return pending().await;
    }

    let mut bot = Bot::new(stack, config.telegram_token, config.telegram_chat_id);
    let mut outgoing = Pending::first().await;

    loop {
        // Link state comes from the stack, not the bus: the bus is lossy and a send can park
        // this task long enough to miss a `Wifi` event outright.
        if !stack.is_config_up() {
            // The link can stay down for hours; this would arrive reading as current.
            if outgoing.message.urgency == Urgency::Perishable {
                log::warn!("Telegram: link down, dropping: {}", outgoing.message.text);
                outgoing = outgoing.done().await;
            } else {
                outgoing.wait_or_replace(stack.wait_config_up()).await;
            }
            continue;
        }

        let sent = bot.send(&outgoing.message).await;

        match sent {
            Ok(()) => {
                log::info!("Telegram: sent {}", outgoing.message.text);
                led::send(LedCmd::blink(Rgb::BLUE, 1));
                outgoing = outgoing.done().await;
            }
            Err(e) => {
                led::send(LedCmd::blink(Rgb::YELLOW, 2));
                let again = outgoing.backoff();

                match again {
                    Some(delay) => {
                        log::warn!("Telegram: {e}, retrying in {}s", delay.as_secs());
                        outgoing.wait_or_replace(Timer::after(delay)).await;
                    }
                    None => {
                        log::warn!("Telegram: {e}, giving up on: {}", outgoing.message.text);
                        outgoing = outgoing.done().await;
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

/// The message being delivered, and what it has cost so far.
struct Pending {
    message: Notification,
    attempts: u8,
    /// Off the outbox but not superseding, so it waits its turn rather than being lost.
    held: Option<Notification>,
}

impl Pending {
    async fn first() -> Self {
        Self {
            message: next_notification().await,
            attempts: 0,
            held: None,
        }
    }

    /// Done with this message, delivered or not: on to the one held back, or to the wait.
    async fn done(self) -> Self {
        match self.held {
            Some(message) => Self {
                message,
                attempts: 0,
                held: None,
            },
            None => Self::first().await,
        }
    }

    /// Wait for `until`, unless a message that supersedes this one arrives first.
    async fn wait_or_replace(&mut self, until: impl Future<Output = ()>) {
        let mut until = pin!(until);

        loop {
            match select(until.as_mut(), next_notification()).await {
                Either::First(()) => return,
                Either::Second(newer) if newer.supersedes(&self.message) => {
                    log::warn!("Telegram: giving up on: {}", self.message.text);
                    self.message = newer;
                    self.attempts = 0;
                    return;
                }
                Either::Second(other) => {
                    self.held = Some(Notification::keep(self.held.take(), other))
                }
            }
        }
    }

    /// How long to wait before trying again, or `None` once the budget is spent.
    fn backoff(&mut self) -> Option<Duration> {
        (self.attempts < retries(self.message.urgency)).then(|| {
            let delay = BACKOFF[usize::from(self.attempts).min(BACKOFF.len() - 1)];
            self.attempts = self.attempts.saturating_add(1);
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
