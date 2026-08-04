use embassy_time::{Duration, Timer};
use esp_hal::gpio::Input;

use crate::events::Publisher;

#[embassy_executor::task]
pub async fn button_task(mut button: Input<'static>, publisher: Publisher) {
    loop {
        button.wait_for_low().await;
        log::debug!("Button press detected!");
        publisher.publish_immediate(crate::events::Event::ButtonDown);

        Timer::after(Duration::from_millis(50)).await;

        button.wait_for_high().await;
        log::debug!("Button released!");
        publisher.publish_immediate(crate::events::Event::ButtonUp);

        Timer::after(Duration::from_millis(50)).await;
    }
}
