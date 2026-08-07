//! Unit ENV-Pro (BME688) on the Grove port, over I2C.
//!
//! A reading is state: only the latest one means anything, so it goes out on a [`Watch`] and a
//! late reader gets the current value rather than a queue of old ones.
//!
//! A failed read re-initializes the sensor, so the unit can be unplugged and plugged back in.

use bosch_bme680::{AsyncBme680, Configuration, DeviceAddress, MeasurmentData};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    watch::{Receiver, Watch},
};
use embassy_time::{Delay, Timer};
use esp_hal::{Async, i2c::master::I2c};

use crate::{
    config,
    led::{self, LedCmd, Rgb},
};

const RECEIVERS: usize = 2;

static READINGS: Watch<CriticalSectionRawMutex, EnvData, RECEIVERS> = Watch::new();

pub fn subscribe() -> Receiver<'static, CriticalSectionRawMutex, EnvData, RECEIVERS> {
    READINGS
        .receiver()
        .expect("too many ENV-Pro receivers: raise RECEIVERS")
}

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

#[embassy_executor::task]
pub async fn env_pro_task(i2c: I2c<'static, Async>) {
    let cfg = config::config();
    let readings = READINGS.sender();
    let mut sensor = AsyncBme680::new(i2c, DeviceAddress::Secondary, Delay, cfg.env_ambient_c);
    let sensor_config = Configuration::default();

    loop {
        if let Err(e) = sensor.initialize(&sensor_config).await {
            log::warn!("ENV-Pro init failed: {e:?}");
            led::send(LedCmd::blink(Rgb::RED, 3));
            Timer::after(cfg.env_retry).await;
            continue;
        }
        log::info!("ENV-Pro initialized");

        loop {
            match sensor.measure().await {
                Ok(data) => {
                    readings.send(data.into());
                    led::send(LedCmd::blink(Rgb::GREEN, 1));
                }
                Err(e) => {
                    log::warn!("ENV-Pro measure failed, re-initializing: {e:?}");
                    led::send(LedCmd::blink(Rgb::RED, 3));
                    Timer::after(cfg.env_retry).await;
                    break;
                }
            }
            Timer::after(cfg.env_sample).await;
        }
    }
}

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
