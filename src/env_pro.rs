use bosch_bme680::{AsyncBme680, Configuration, DeviceAddress, MeasurmentData};
use embassy_time::{Delay, Duration, Timer};
use esp_hal::{Async, i2c::master::I2c};

use crate::events::{EnvData, Event, LedCmd, Publisher, Rgb};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Seed for the heater-resistance calculation; the driver self-corrects after the first read.
const INITIAL_AMBIENT_C: i32 = 20;

impl From<MeasurmentData> for EnvData {
    fn from(data: MeasurmentData) -> Self {
        Self {
            temperature: data.temperature,
            humidity: data.humidity,
            // `bosch-bme680` documents this as hPa, but the compensation formula returns Pa.
            pressure: data.pressure / 100.0,
            gas_resistance: data.gas_resistance,
        }
    }
}

#[embassy_executor::task]
pub async fn env_pro_task(i2c: I2c<'static, Async>, publisher: Publisher) {
    let mut sensor = AsyncBme680::new(i2c, DeviceAddress::Secondary, Delay, INITIAL_AMBIENT_C);
    let config = Configuration::default();

    loop {
        if let Err(e) = sensor.initialize(&config).await {
            log::warn!("ENV-Pro init failed: {e:?}");
            publisher.publish_immediate(LedCmd::blink(Rgb::RED, 3).into());
            Timer::after(RETRY_INTERVAL).await;
            continue;
        }
        log::info!("ENV-Pro initialized");

        loop {
            match sensor.measure().await {
                Ok(data) => {
                    publisher.publish_immediate(Event::Env(data.into()));
                    publisher.publish_immediate(LedCmd::blink(Rgb::GREEN, 1).into());
                }
                Err(e) => {
                    log::warn!("ENV-Pro measure failed, re-initializing: {e:?}");
                    publisher.publish_immediate(LedCmd::blink(Rgb::RED, 3).into());
                    Timer::after(RETRY_INTERVAL).await;
                    break;
                }
            }
            Timer::after(SAMPLE_INTERVAL).await;
        }
    }
}
