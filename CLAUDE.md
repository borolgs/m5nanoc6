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
  vocabulary `Rgb`/`Pattern`/`LedCmd`). `CAP`/`SUBS`/`PUBS` (8/5/6) are compile-time bounds:
  bump `SUBS`/`PUBS` when adding a subscriber/publisher, otherwise
  `EVENTS.subscriber()`/`.publisher()` returns `Err` at runtime. The channel is lossy —
  a slow subscriber loses the oldest messages rather than blocking publishers, so tasks keep
  whatever they need locally instead of expecting to re-read it.
- `src/config.rs` — the only module that reads the environment. Tunables come from `[env]` in
  `.cargo/config.toml`, secrets from `.env`; both arrive as `option_env!` and both are baked
  into the image. One flat `Config` holds every setting; `config()` parses it on first use into
  an `OnceLock` and hands out `&'static Config`, so any task can read it without being passed
  anything. `Config::wifi_networks()` splits `WIFI_CREDS`; a bot with an empty token or chat id
  is what "notifier off" looks like. Like `events.rs` it imports nothing from the crate.
- `src/button.rs` — `button_task`: debounced GPIO9 input, publishes `ButtonDown`/`ButtonUp`.
- `src/env_pro.rs` — `env_pro_task`: polls the Unit ENV-Pro (BME688) on the Grove port over I2C
  (`bosch-bme680`, address 0x77), publishes `Env(EnvData)`. Re-initializes on read
  error, so the unit can be hot-plugged.
- `src/led.rs` — on-board LEDs: blue status LED (GPIO7, plain `Output`) and the WS2812 RGB
  LED (`RgbLed`, data on GPIO20 via RMT, supply gated by GPIO19). WS2812 bit encoding is
  hand-rolled because `esp-hal-smartled` still pins `esp-hal ~1.0`. `led_task` owns both and
  plays `Event::Led(LedCmd)`; `Pattern::For`/`Blink { count: Some(_) }` restore the last
  `Pattern::Solid` value on their own, so publishers fire and forget.
- `src/wifi.rs` — `wifi_task`: sweeps `config().wifi_networks()` in order, associates and waits for a DHCPv4
  lease, publishes `Wifi(WifiState)`. Status LED blinks for the length of a sweep and goes
  solid once online; a sweep that reaches the end of the list leaves the RGB LED red and
  retries with an exponential backoff. `wait_for_disconnect_async` drives reconnects.
  `net_task` runs the `embassy-net` stack. `esp-radio` 0.18 has no `start()`/`stop()` —
  `connect_async` drives the radio. `SOCKETS` (4) sizes `StackResources`: DHCP and DNS each
  cost a slot like any other socket, plus one for `telegram.rs` and one spare.
- `src/app.rs` — the policy layer: `app_task` drives an `App`, the device's state, and `App` is
  the one place a rule about what the device *does* belongs. Today that is the notifier: loud
  alarm below `TG_MIN_C`, loud recovery above `TG_MIN_C + TG_HYST_C`, loud repeats every
  `TG_REPEAT_MIN` while cold, silent heartbeat every `TG_HEARTBEAT_MIN`. Each `on_*` handler is
  a pure state transition returning an optional `telegram::Notification`; the loop hands it to
  `telegram::notify` and goes straight back to the bus. Nothing here awaits the network.
- `src/telegram.rs` — the client that gets a `Notification` delivered: wait for the link, send,
  retry with `BACKOFF`, blink blue on success and yellow twice on failure. Parks when
  unconfigured. `app.rs` reaches it through `notify()`, which drops into the one-slot `OUTBOX`
  and returns; `Bot` is how a message is actually said — `reqwless` + `embedded-tls` over one
  connection per message, its ~27 KiB of buffers `static` rather than on the heap. Link state
  comes from `stack.is_config_up()`/`wait_config_up()`, not the bus, which this task no longer
  reads at all. See `docs/telegram-bot.md`, including why certificate verification is off.
- `build.rs` — forwards every line of `.env` to the compiler as `cargo:rustc-env`. A missing
  `.env` is not an error: the build works on a fresh clone and just leaves Wi-Fi and the
  notifier off. Only `config.rs` reads what it emits.
- `src/bin/main.rs` — wiring only: init (clocks, heap, `esp_rtos::start`, Wi-Fi + `embassy-net`
  stack), peripheral setup, task spawning. No logic — it belongs in `app.rs`.
- `docs/telegram-bot.md` — the Telegram runbook (source of truth);
  `.claude/skills/telegram-bot/` is a thin wrapper over it that automates `chat_id` discovery
  and the test sends. Neither ever writes or echoes the bot token — that stays the user's job.

## Conventions

- Peripherals are configured in `main.rs` and passed into tasks as `'static` values; tasks
  themselves stay hardware-agnostic beyond the type they receive.
- Cross-task communication goes through the `EVENTS` channel — not shared statics. The one
  exception is `telegram::OUTBOX`, a `Signal`: its consumer parks on the network for tens of
  seconds, and a lossy bus would drop the alarm it was meant to carry. Newest-wins is also the
  semantics wanted there — an alarm should displace the heartbeat queued behind it.
- What the device decides lives in `app.rs`; a module below it does one thing and does not grow
  policy. `app.rs → telegram.rs`, never the reverse.
- Build-time settings are read in `config.rs` and nowhere else; the rest of the crate gets them
  from `config::config()`. Tunables belong in `.cargo/config.toml`; `.env` is for secrets only.
- `events.rs` owns every type that travels on the bus and imports nothing from the crate —
  the dependency only ever points at it, never out of it.
- LEDs are driven by publishing `Event::Led(..)`; nothing outside `led.rs` touches the pins.
  A finite `Blink`/`For` falls back to the last `Solid` value, so clear a steady color with a
  `Solid` command before playing a one-shot on top of it.
- Tasks are `#[embassy_executor::task]` fns living in their own module.
- Logging via the `log` crate (`esp-println` backend); level from `ESP_LOG` (`debug` by default).
- `main.rs` keeps `#![deny(clippy::mem_forget)]` and `#![deny(clippy::large_stack_frames)]` —
  don't remove them; `.clippy.toml` sets the stack-frame threshold to 1024 bytes.
- `reqwless` is pinned to `=0.14.0`: git `main` has moved to a different, `mbedtls-rs`-based
  `TlsConfig`. `der` is a direct dependency the crate never imports — `embedded-tls` 0.18 was
  written against `der 0.8.0-rc.2`, where `SequenceOf`/`SetOf` were unconditional, and only a
  direct dependency can turn on the `heapless` feature that gates them in released `der`.

## Style

- Comments: only where the code can't speak for itself — a non-obvious *why*, a hardware quirk,
  a workaround. One sentence is the norm; if a comment runs longer, that's a signal the code
  needs fixing or the note belongs in `CLAUDE.md`/`docs/`. Don't restate what the line does.
- Function order: the entry point (task or public fn) first, helpers below it in call order,
  so a module reads top-down.
