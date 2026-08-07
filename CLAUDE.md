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

Every module opens with a `//!` header holding its own design rationale — read that before
editing it, and put new rationale there rather than here.

- `src/lib.rs` — crate root (`no_std`), module list only.
- `src/events.rs` — the `EVENTS` bus, the `OUTBOX`, and every type that travels on either.
  `CAP`/`SUBS`/`PUBS` (8/5/6) are compile-time bounds: bump `SUBS`/`PUBS` when adding a
  subscriber/publisher, or `EVENTS.subscriber()`/`.publisher()` returns `Err` at runtime.
- `src/config.rs` — the only module that reads the environment; hands out `&'static Config`.
- `src/button.rs` — `button_task`: debounced GPIO9, publishes `ButtonDown`/`ButtonUp`.
- `src/env_pro.rs` — `env_pro_task`: Unit ENV-Pro (BME688) on the Grove port over I2C, publishes
  `Env(EnvData)`. Re-initializes on read error, so the unit can be hot-plugged.
- `src/led.rs` — `led_task`: blue status LED (GPIO7) and the WS2812 RGB LED (GPIO20 via RMT,
  supply gated by GPIO19).
- `src/wifi.rs` — `wifi_task`: sweeps the configured networks, waits for a DHCPv4 lease,
  publishes `Wifi(WifiState)`, reconnects on its own. `net_task` runs the `embassy-net` stack.
- `src/app.rs` — the policy layer: thresholds, the heartbeat schedule, the wording of every
  message. The one place a rule about what the device *does* belongs.
- `src/telegram.rs` — the notifier currently fitted: outbox, link, send, retry.
- `build.rs` — forwards every line of `.env` to the compiler as `cargo:rustc-env`. A missing
  `.env` is not an error: a fresh clone builds with Wi-Fi and the notifier off.
- `src/bin/main.rs` — wiring only: init, peripheral setup, task spawning. No logic.
- `docs/telegram-bot.md` — the Telegram runbook (source of truth);
  `.claude/skills/telegram-bot/` is a thin wrapper over it. Neither ever writes or echoes the
  bot token — that stays the user's job.

## Conventions

- Peripherals are configured in `main.rs` and passed into tasks as `'static` values; tasks
  themselves stay hardware-agnostic beyond the type they receive.
- Cross-task communication goes through the `EVENTS` channel — not shared statics. It is lossy,
  so a task keeps whatever it needs locally instead of expecting to re-read it. The one
  exception is `events::OUTBOX`, a `Signal`: replacement there goes by
  `Notification::supersedes`, not arrival order.
- What the device decides lives in `app.rs`; a module below it does one thing and does not grow
  policy. They meet at `events.rs` and nowhere else: `app.rs` names no transport, `telegram.rs`
  no policy. A second notifier should drop in beside it without `app.rs` changing.
- `events.rs` owns every type that travels on the bus and imports nothing from the crate —
  the dependency only ever points at it, never out of it.
- Build-time settings are read in `config.rs` and nowhere else; the rest of the crate gets them
  from `config::config()`. Tunables belong in `.cargo/config.toml`; `.env` is for secrets only.
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
  a workaround. **One line, hard limit**, `//` and `///` alike. A comment that needs two lines
  is not a comment: either the code needs fixing, or the note belongs in the module's `//!`
  header, `CLAUDE.md`, or `docs/`. Don't restate what the line does, don't argue for the design,
  don't spell out the failure it prevents — one line naming the reason is the whole budget.
  When in doubt, no comment.
- Function order: the entry point (task or public fn) first, helpers below it in call order,
  so a module reads top-down.
