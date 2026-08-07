//! The front button on GPIO9: pulled up, so a press reads low.
//!
//! Presses are edges, not state — a `Watch<bool>` would swallow a double-tap — so they go out
//! on a small [`PubSubChannel`]. Nothing subscribes yet.

use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    pubsub::{PubSubChannel, Subscriber},
};
use embassy_time::{Duration, Timer};
use esp_hal::gpio::Input;

const CAP: usize = 4;
const SUBS: usize = 2;
const PUBS: usize = 1;

static PRESSES: PubSubChannel<CriticalSectionRawMutex, ButtonEvent, CAP, SUBS, PUBS> =
    PubSubChannel::new();

pub fn subscribe() -> Subscriber<'static, CriticalSectionRawMutex, ButtonEvent, CAP, SUBS, PUBS> {
    PRESSES
        .subscriber()
        .expect("too many button subscribers: raise SUBS")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonEvent {
    Down,
    Up,
}

/// Long enough for the contact to settle, short enough not to eat a real second press.
const DEBOUNCE: Duration = Duration::from_millis(50);

#[embassy_executor::task]
pub async fn button_task(mut button: Input<'static>) {
    let presses = PRESSES
        .publisher()
        .expect("too many button publishers: raise PUBS");

    loop {
        button.wait_for_low().await;
        log::debug!("Button pressed");
        presses.publish_immediate(ButtonEvent::Down);

        Timer::after(DEBOUNCE).await;

        button.wait_for_high().await;
        log::debug!("Button released");
        presses.publish_immediate(ButtonEvent::Up);

        Timer::after(DEBOUNCE).await;
    }
}
