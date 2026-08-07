//! The `EVENTS` bus and every type that travels on it.

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, pubsub};

#[derive(Debug, Clone)]
pub enum Event {
    ButtonUp,
    ButtonDown,
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
