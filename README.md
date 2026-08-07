# M5NanoC6

A room watchdog on the [M5Stack NanoC6 Dev Kit](https://docs.m5stack.com/en/core/M5NanoC6)
(ESP32-C6). It polls temperature, humidity and pressure from an ENV-Pro (BME688) unit and
pushes a Telegram alarm the moment the room goes cold.

`no_std` Rust on `esp-hal` + `embassy`, with `esp-radio` for Wi-Fi.

# Quickstart

```bash
# Secrets go in .env: the Wi-Fi networks to try, in order, and the Telegram bot token.
cp .env.example .env

cargo check
cargo run
```

`cargo run` flashes the connected board via `espflash` and opens the serial monitor.

Bot setup is a one-time walkthrough in [`docs/telegram-bot.md`](docs/telegram-bot.md). Leave
`TELEGRAM_TOKEN` empty and the notifier stays off.

Thresholds (`TG_MIN_C` and friends) are in `.cargo/config.toml`, next to the other build
settings. `.env` holds nothing but secrets.

# License

[MIT](LICENSE)
