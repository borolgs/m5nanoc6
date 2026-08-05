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
- `src/events.rs` — global `EVENTS` pub/sub channel (`embassy_sync::PubSubChannel`) and the
  `Event` enum. `CAP`/`SUBS`/`PUBS` are compile-time bounds: bump `SUBS`/`PUBS` when adding
  a subscriber/publisher, otherwise `EVENTS.subscriber()`/`.publisher()` returns `Err` at runtime.
- `src/button.rs` — `button_task`: debounced GPIO9 input, publishes `ButtonDown`/`ButtonUp`.
- `src/env_pro.rs` — `env_pro_task`: polls the Unit ENV-Pro (BME688) on the Grove port over I2C
  (`bosch-bme680`, address 0x77), publishes `Env(EnvData)` every 5s. Re-initializes on read
  error, so the unit can be hot-plugged.
- `src/bin/main.rs` — init (clocks, heap, `esp_rtos::start`, Wi-Fi), peripheral setup,
  task spawning, event loop.

## Conventions

- Peripherals are configured in `main.rs` and passed into tasks as `'static` values; tasks
  themselves stay hardware-agnostic beyond the type they receive.
- Cross-task communication goes through the `EVENTS` channel — not shared statics.
- Tasks are `#[embassy_executor::task]` fns living in their own module.
- Logging via the `log` crate (`esp-println` backend); level from `ESP_LOG` (`debug` by default).
- `main.rs` keeps `#![deny(clippy::mem_forget)]` and `#![deny(clippy::large_stack_frames)]` —
  don't remove them; `.clippy.toml` sets the stack-frame threshold to 1024 bytes.
