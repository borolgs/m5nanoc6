# CLAUDE.md

## Project

`no_std` firmware for the M5Stack NanoC6 Dev Kit (ESP32-C6, RISC-V).
Stack: `esp-hal` 1.1 + `esp-rtos`/`embassy` async runtime, `esp-radio` for Wi-Fi.

Target `riscv32imac-unknown-none-elf`, build-std, edition 2024 — all preconfigured in
`.cargo/config.toml` and `rust-toolchain.toml`, so plain `cargo` commands work.

## Commands

```bash
cargo check              # main verification loop — no hardware needed
cargo clippy
cargo run                # flash + monitor via espflash (requires a connected board)
```

Flashing needs hardware; don't run `cargo run` unless the user asks.

## Layout

- `src/lib.rs` — crate root (`no_std`), module list only.
- `src/events.rs` — global `EVENTS` pub/sub channel (`embassy_sync::PubSubChannel`), the
  `Event` enum, and every payload type that travels on it (`EnvData`, and the LED command
  vocabulary `Rgb`/`Pattern`/`LedCmd`). `CAP`/`SUBS`/`PUBS` are compile-time bounds: bump
  `SUBS`/`PUBS` when adding a subscriber/publisher, otherwise
  `EVENTS.subscriber()`/`.publisher()` returns `Err` at runtime.
- `src/button.rs` — `button_task`: debounced GPIO9 input, publishes `ButtonDown`/`ButtonUp`.
- `src/env_pro.rs` — `env_pro_task`: polls the Unit ENV-Pro (BME688) on the Grove port over I2C
  (`bosch-bme680`, address 0x77), publishes `Env(EnvData)` every 5s. Re-initializes on read
  error, so the unit can be hot-plugged.
- `src/led.rs` — on-board LEDs: blue status LED (GPIO7, plain `Output`) and the WS2812 RGB
  LED (`RgbLed`, data on GPIO20 via RMT, supply gated by GPIO19). WS2812 bit encoding is
  hand-rolled because `esp-hal-smartled` still pins `esp-hal ~1.0`. `led_task` owns both and
  plays `Event::Led(LedCmd)`; `Pattern::For`/`Blink { count: Some(_) }` restore the last
  `Pattern::Solid` value on their own, so publishers fire and forget.
- `src/wifi.rs` — `wifi_task`: sweeps `networks()` in order, associates and waits for a DHCPv4
  lease, publishes `Wifi(WifiState)`. Status LED blinks for the length of a sweep and goes
  solid once online; a sweep that reaches the end of the list leaves the RGB LED red and
  retries with an exponential backoff. `wait_for_disconnect_async` drives reconnects.
  `net_task` runs the `embassy-net` stack. `esp-radio` 0.18 has no `start()`/`stop()` —
  `connect_async` drives the radio.
- `.env` (gitignored, see `.env.example`) — `build.rs` forwards every line to the compiler as
  `cargo:rustc-env`, so `env!`/`option_env!` can read it. Wi-Fi credentials come in through
  `WIFI_CREDS=ssid1,pass1;ssid2,pass2`, split in `wifi.rs`. A missing `.env` is not an
  error — the build works on a fresh clone and just leaves Wi-Fi off.
- `src/bin/main.rs` — init (clocks, heap, `esp_rtos::start`, Wi-Fi + `embassy-net` stack),
  peripheral setup, task spawning, event loop.

## Conventions

- Peripherals are configured in `main.rs` and passed into tasks as `'static` values; tasks
  themselves stay hardware-agnostic beyond the type they receive.
- Cross-task communication goes through the `EVENTS` channel — not shared statics.
- `events.rs` owns every type that travels on the bus and imports nothing from the crate —
  the dependency only ever points at it, never out of it.
- LEDs are driven by publishing `Event::Led(..)`; nothing outside `led.rs` touches the pins.
  A finite `Blink`/`For` falls back to the last `Solid` value, so clear a steady color with a
  `Solid` command before playing a one-shot on top of it.
- Tasks are `#[embassy_executor::task]` fns living in their own module.
- Logging via the `log` crate (`esp-println` backend); level from `ESP_LOG` (`debug` by default).
- `main.rs` keeps `#![deny(clippy::mem_forget)]` and `#![deny(clippy::large_stack_frames)]` —
  don't remove them; `.clippy.toml` sets the stack-frame threshold to 1024 bytes.
