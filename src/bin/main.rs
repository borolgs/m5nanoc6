#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, StackResources};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::rmt::{Rmt, TxChannelConfig, TxChannelCreator};
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use m5nanoc6::events::{EVENTS, Event};
use m5nanoc6::led::{RMT_FREQUENCY, RgbLed};
use static_cell::StaticCell;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    let (wifi_controller, interfaces) = esp_radio::wifi::new(peripherals.WIFI, Default::default())
        .expect("Failed to initialize Wi-Fi controller");

    {
        static RESOURCES: StaticCell<StackResources<{ m5nanoc6::wifi::SOCKETS }>> =
            StaticCell::new();
        let rng = Rng::new();
        let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
        let (stack, runner) = embassy_net::new(
            interfaces.station,
            NetConfig::dhcpv4(Default::default()),
            RESOURCES.init(StackResources::new()),
            seed,
        );

        spawner.spawn(m5nanoc6::wifi::net_task(runner).unwrap());
        spawner.spawn(
            m5nanoc6::wifi::wifi_task(wifi_controller, stack, EVENTS.publisher().unwrap()).unwrap(),
        );
    }

    let button = Input::new(
        peripherals.GPIO9,
        InputConfig::default().with_pull(Pull::Up),
    );

    spawner.spawn(m5nanoc6::button::button_task(button, EVENTS.publisher().unwrap()).unwrap());

    let (status_led, rgb_led) = {
        // Blue status LED. `led_task` drives it from here on.
        let status_led = Output::new(peripherals.GPIO7, Level::Low, OutputConfig::default());

        // WS2812 RGB LED: data on GPIO20, supply gated by GPIO19.
        let rgb_power = Output::new(peripherals.GPIO19, Level::High, OutputConfig::default());
        let rmt = Rmt::new(peripherals.RMT, RMT_FREQUENCY)
            .expect("Failed to initialize RMT")
            .into_async();
        let rgb_channel = rmt
            .channel0
            .configure_tx(
                &TxChannelConfig::default()
                    .with_clk_divider(1)
                    .with_idle_output(true)
                    .with_idle_output_level(Level::Low),
            )
            .expect("Failed to configure the RMT TX channel")
            .with_pin(peripherals.GPIO20);
        let rgb_led = RgbLed::new(rgb_channel, rgb_power);
        (status_led, rgb_led)
    };

    spawner
        .spawn(m5nanoc6::led::led_task(status_led, rgb_led, EVENTS.subscriber().unwrap()).unwrap());

    let i2c = I2c::new(peripherals.I2C0, I2cConfig::default())
        .expect("Failed to initialize I2C")
        .with_sda(peripherals.GPIO2)
        .with_scl(peripherals.GPIO1)
        .into_async();

    spawner.spawn(m5nanoc6::env_pro::env_pro_task(i2c, EVENTS.publisher().unwrap()).unwrap());

    let mut subscriber = EVENTS.subscriber().unwrap();

    loop {
        let msg = subscriber.next_message_pure().await;
        match msg {
            Event::Env(env) => {
                log::info!("[main] receive env {env:?}")
            }
            Event::Wifi(state) => {
                log::info!("[main] receive wifi {state:?}")
            }
            other => log::debug!("[main] receive {other:?}"),
        }
    }
}
