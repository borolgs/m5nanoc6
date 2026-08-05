use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, pubsub};

#[derive(Debug, Clone)]
pub enum Event {
    ButtonUp,
    ButtonDown,
    Env(EnvData),
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

const CAP: usize = 4;
const SUBS: usize = 4;
const PUBS: usize = 5;

pub type Channel = pubsub::PubSubChannel<CriticalSectionRawMutex, Event, CAP, SUBS, PUBS>;
pub type Publisher = pubsub::Publisher<'static, CriticalSectionRawMutex, Event, CAP, SUBS, PUBS>;
pub type Subscriber = pubsub::Subscriber<'static, CriticalSectionRawMutex, Event, CAP, SUBS, PUBS>;

pub static EVENTS: Channel = Channel::new();
