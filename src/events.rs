//! The channels tasks talk over — the [`EVENTS`] bus and the [`notify`] outbox — and every
//! type that travels on either.
//!
//! This module imports nothing from the rest of the crate — the dependency only ever points
//! at it, so tasks can talk to each other without knowing about each other's hardware.

use alloc::string::String;
use core::net::Ipv4Addr;

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, pubsub, signal::Signal};
use embassy_time::Duration;

#[derive(Debug, Clone)]
pub enum Event {
    ButtonUp,
    ButtonDown,
    Env(EnvData),
    Led(LedCmd),
    Wifi(WifiState),
}

/// One ENV-Pro reading.
#[derive(Debug, Clone, Copy)]
pub struct EnvData {
    /// °C
    pub temperature: f32,
    /// %RH
    pub humidity: f32,
    /// hPa
    pub pressure: f32,
    /// Ohm
    pub gas_resistance: Option<f32>,
}

/// Where the Wi-Fi station is in its connect cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiState {
    /// Sweeping the configured networks.
    Connecting,
    Connected {
        ssid: &'static str,
        ip: Ipv4Addr,
    },
    /// Was associated, lost the access point — a new sweep follows.
    Disconnected,
    /// The whole list was exhausted without a lease.
    Failed,
}

impl From<WifiState> for Event {
    fn from(state: WifiState) -> Self {
        Self::Wifi(state)
    }
}

/// What an LED should show, and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedCmd {
    /// The blue status LED on GPIO7.
    Status(bool, Pattern),
    /// The WS2812 RGB LED on GPIO20.
    Rgb(Rgb, Pattern),
}

impl LedCmd {
    pub const fn status(on: bool) -> Self {
        Self::Status(on, Pattern::Solid)
    }

    pub const fn status_for(on: bool, duration: Duration) -> Self {
        Self::Status(on, Pattern::For(duration))
    }

    pub const fn status_blink(count: u16) -> Self {
        Self::Status(true, Pattern::blink(count))
    }

    /// Blink the status LED until the next command for it.
    pub const fn status_blink_forever() -> Self {
        Self::Status(true, Pattern::blink_forever())
    }

    pub const fn rgb(color: Rgb) -> Self {
        Self::Rgb(color, Pattern::Solid)
    }

    pub const fn rgb_for(color: Rgb, duration: Duration) -> Self {
        Self::Rgb(color, Pattern::For(duration))
    }

    pub const fn blink(color: Rgb, count: u16) -> Self {
        Self::Rgb(color, Pattern::blink(count))
    }

    pub const fn blink_forever(color: Rgb) -> Self {
        Self::Rgb(color, Pattern::blink_forever())
    }
}

impl From<LedCmd> for Event {
    fn from(cmd: LedCmd) -> Self {
        Self::Led(cmd)
    }
}

/// A 24-bit color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// The palette below is deliberately dim — the WS2812 at full scale is blinding.
    pub const LEVEL: u8 = 32;

    pub const OFF: Self = Self::new(0, 0, 0);
    pub const RED: Self = Self::new(Self::LEVEL, 0, 0);
    pub const GREEN: Self = Self::new(0, Self::LEVEL, 0);
    pub const BLUE: Self = Self::new(0, 0, Self::LEVEL);
    pub const YELLOW: Self = Self::new(Self::LEVEL, Self::LEVEL, 0);
    pub const WHITE: Self = Self::new(Self::LEVEL, Self::LEVEL, Self::LEVEL);

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// The same hue re-scaled so that a channel at [`Rgb::LEVEL`] ends up at `level`.
    ///
    /// Channels brighter than [`Rgb::LEVEL`] saturate at full scale rather than wrapping,
    /// so the hue can only wash out, never flip.
    pub const fn scaled(self, level: u8) -> Self {
        const fn scale(c: u8, level: u8) -> u8 {
            let scaled = (c as u32 * level as u32) / Rgb::LEVEL as u32;
            if scaled > u8::MAX as u32 {
                u8::MAX
            } else {
                scaled as u8
            }
        }

        Self::new(
            scale(self.r, level),
            scale(self.g, level),
            scale(self.b, level),
        )
    }
}

/// How long an LED holds the value it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    /// Hold until the next command. This becomes the LED's steady state.
    Solid,
    /// Hold for this long, then restore the steady state.
    For(Duration),
    /// Alternate between the value and off. `count: None` blinks until the next command;
    /// `Some(n)` plays `n` on-phases and then restores the steady state.
    Blink {
        on: Duration,
        off: Duration,
        count: Option<u16>,
    },
}

impl Pattern {
    pub const DEFAULT_ON: Duration = Duration::from_millis(150);
    pub const DEFAULT_OFF: Duration = Duration::from_millis(150);

    /// Blink `count` times at the default rate, then restore the steady state.
    /// `count: 0` is a no-op: the steady state is kept as-is.
    pub const fn blink(count: u16) -> Self {
        Self::Blink {
            on: Self::DEFAULT_ON,
            off: Self::DEFAULT_OFF,
            count: Some(count),
        }
    }

    /// Blink at the default rate until the next command for that LED.
    pub const fn blink_forever() -> Self {
        Self::Blink {
            on: Self::DEFAULT_ON,
            off: Self::DEFAULT_OFF,
            count: None,
        }
    }
}

const CAP: usize = 8;
const SUBS: usize = 5;
const PUBS: usize = 6;

pub type Channel = pubsub::PubSubChannel<CriticalSectionRawMutex, Event, CAP, SUBS, PUBS>;
pub type Publisher = pubsub::Publisher<'static, CriticalSectionRawMutex, Event, CAP, SUBS, PUBS>;
pub type Subscriber = pubsub::Subscriber<'static, CriticalSectionRawMutex, Event, CAP, SUBS, PUBS>;

static EVENTS: Channel = Channel::new();

/// Panics past `PUBS` — a boot-time failure in the module that added the publisher.
pub fn publisher() -> Publisher {
    EVENTS.publisher().expect("too many publishers: raise PUBS")
}

/// Panics past `SUBS` — a boot-time failure in the module that added the subscriber.
pub fn subscriber() -> Subscriber {
    EVENTS
        .subscriber()
        .expect("too many subscribers: raise SUBS")
}

/// Something for a person to read, for whichever notifier is fitted to deliver it.
///
/// Not an [`Event`]: the bus is lossy, and the one message worth sending is the one a slow
/// subscriber would lose.
pub struct Notification {
    pub text: String,
    pub urgency: Urgency,
}

/// How badly the device wants a message delivered. A transport reads its own settings off
/// this — how loudly to ring, how long to keep trying — and nothing above it needs to know
/// what those are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// Must reach a person, and must not be dropped on the way there.
    Alarm,
    /// Worth having in the log, not worth waking anyone.
    Info,
    /// True only right now — better dropped than delivered late.
    Perishable,
}

impl Urgency {
    /// How hard the device tries; the higher of two wins the one slot they have to share.
    const fn priority(self) -> u8 {
        match self {
            Self::Alarm => 2,
            Self::Info => 1,
            Self::Perishable => 0,
        }
    }
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

    /// Newest wins between equals, but a quieter message never displaces a louder one.
    pub fn supersedes(&self, other: &Self) -> bool {
        self.urgency.priority() >= other.urgency.priority()
    }

    /// Fit two messages into one slot: the one that supersedes stays, the other is dropped.
    pub fn keep(waiting: Option<Self>, message: Self) -> Self {
        match waiting {
            Some(waiting) if !message.supersedes(&waiting) => {
                log::warn!("Outbox: dropping: {}", message.text);
                waiting
            }
            Some(dropped) => {
                log::warn!("Outbox: dropping an undelivered message: {}", dropped.text);
                message
            }
            None => message,
        }
    }
}

/// The one message waiting to go out; [`Notification::supersedes`] decides replacements.
static OUTBOX: Signal<CriticalSectionRawMutex, Notification> = Signal::new();

/// Hand a message to the notifier. Never blocks, so the caller keeps serving the bus while
/// a send is parked on the network.
pub fn notify(message: Notification) {
    OUTBOX.signal(Notification::keep(OUTBOX.try_take(), message));
}

/// The notifier's end of [`notify`]: the next message to deliver.
pub async fn next_notification() -> Notification {
    OUTBOX.wait().await
}
