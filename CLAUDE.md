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

There is no central bus. Each module owns the channel its own traffic goes out on, the
payload types that travel on it, and the accessor others reach it through; the `static` itself
stays private, so only the producing module can publish.

- `src/lib.rs` — crate root (`no_std`), module list only.
- `src/config.rs` — the only module that reads the environment; hands out `&'static Config`.
- `src/button.rs` — `button_task`: debounced GPIO9. Owns a `PubSubChannel<ButtonEvent>` and
  `button::subscribe()`. Nothing subscribes yet.
- `src/env_pro.rs` — `env_pro_task`: Unit ENV-Pro (BME688) on the Grove port over I2C. Owns a
  `Watch<EnvData>` and `env_pro::subscribe()`. Re-initializes on read error, so the unit can be
  hot-plugged.
- `src/led.rs` — `led_task`: blue status LED (GPIO7) and the WS2812 RGB LED (GPIO20 via RMT,
  supply gated by GPIO19). Owns a `Channel<LedCmd>` and `led::send()`.
- `src/wifi.rs` — `wifi_task`: sweeps the configured networks, waits for a DHCPv4 lease,
  reconnects on its own. Owns a `Watch<WifiState>` and `wifi::subscribe()`. `net_task` runs the
  `embassy-net` stack.
- `src/app.rs` — the policy layer: thresholds, the heartbeat schedule, the wording of every
  message. The one place a rule about what the device *does* belongs.
- `src/telegram.rs` — the notifier currently fitted: link, send, retry. Owns the outbox — a
  `PriorityChannel<Notification, Max, 4>` — and `telegram::notify()`.
- `build.rs` — forwards every line of `.env` to the compiler as `cargo:rustc-env`. A missing
  `.env` is not an error: a fresh clone builds with Wi-Fi and the notifier off.
- `src/bin/main.rs` — wiring only: init, peripheral setup, task spawning. No logic.
- `docs/telegram-bot.md` — the Telegram runbook (source of truth);
  `.claude/skills/telegram-bot/` is a thin wrapper over it. Neither ever writes or echoes the
  bot token — that stays the user's job.

## Conventions

- Peripherals are configured in `main.rs` and passed into tasks as `'static` values; tasks
  themselves stay hardware-agnostic beyond the type they receive.
- Cross-task communication goes through channels, not shared statics. Pick the primitive to
  match the cargo: `Watch` for state (latest value, never lost, but the *sequence* is not
  guaranteed — never count transitions off one), `PubSubChannel` for edges, `Channel` +
  `try_send` for commands whose loss is cosmetic, `PriorityChannel` for work where the loudest
  goes first. Every bounded channel is lossy once it fills; what a module owes is a stated rule
  for *which* item goes and a log line when one does. Separate channels means no topic can evict
  another's traffic.
- Never block a producer on a consumer. `try_send`, `publish_immediate` and `Watch::send` all
  return immediately; `send().await` waits on the *slowest* consumer, and a task that both
  publishes and consumes can deadlock the firmware that way during a Wi-Fi flap.
- A task acquires its handles at task start, not as spawn arguments — the same way it reaches
  `config::config()`. Arguments are what a task *owns* (peripherals, `Stack`); channels are what
  everyone *shares*. The `.expect()` on a receiver count is a boot-time panic in the module that
  caused it.
- There is no total order between topics. Anything time-sensitive stamps `Instant::now()` where
  it is received, and nothing leans on one topic arriving before another.
- What the device decides lives in `app.rs`; a module below it does one thing and does not grow
  policy. `app.rs` names `telegram::notify` only because the outbox lives with its consumer —
  if a second notifier ever appears, move the outbox to a neutral module rather than teaching
  `app.rs` about both.
- Build-time settings are read in `config.rs` and nowhere else; the rest of the crate gets them
  from `config::config()`. Tunables belong in `.cargo/config.toml`; `.env` is for secrets only.
- LEDs are driven by `led::send(..)`; nothing outside `led.rs` touches the pins.
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
