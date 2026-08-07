//! The channels tasks talk over — the `EVENTS` bus and the [`notify`] outbox — and every
//! type that travels on either.
//!
//! This module imports nothing from the rest of the crate — the dependency only ever points
//! at it, so tasks can talk to each other without knowing about each other's hardware.

use alloc::string::String;
use core::net::Ipv4Addr;

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, pubsub, signal::Signal};

#[derive(Debug, Clone)]
pub enum Event {
    ButtonUp,
    ButtonDown,
    Env(EnvData),
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
