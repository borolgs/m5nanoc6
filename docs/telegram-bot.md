# Telegram bot setup

The firmware pushes two kinds of message to one Telegram chat:

- a **loud** alarm when the temperature falls below `TG_MIN_C`, repeated every `TG_REPEAT_MIN`
  while it stays there, and a loud "recovered" line once it climbs back above
  `TG_MIN_C + TG_HYST_C`;
- a **silent** heartbeat every `TG_HEARTBEAT_MIN` carrying the current reading — sent **loud**,
  as `no sensor reading for over 5 minutes`, when the ENV-Pro has stopped answering, because a
  device with no reading cannot raise an alarm at all.

An alarm is never held back, but a recovery has to wait out `TG_DWELL_MIN`: `TG_HYST_C` keeps a
steady temperature from chattering, and the dwell caps a swinging one at one alarm per interval
instead of one per 5-second sample. Delaying the alarm instead would lose a short excursion
outright, which is the one thing this device exists to catch.

Neither the loud alarm nor the loud sensor-silent line carries an elapsed span, on purpose: a
loud message waits for the link, so "silent for 5m" would arrive reading as current.

The heartbeat exists because an alarm-only device is indistinguishable from a dead one — no
power, no Wi-Fi, a crashed task and "everything is fine" all look the same from the chat. Be
clear-eyed about its limit: a heartbeat proves liveness only *when it arrives*. Nothing on the
device can detect its own death, so noticing missing heartbeats stays a manual job. If that
ever stops being good enough, the fix is off-device — a cron or uptime monitor that alerts on a
missed ping — not more firmware.

Follow this once, top to bottom.

## 1. Create the bot

DM [@BotFather](https://t.me/BotFather), send `/newbot`, pick a display name and a username
ending in `bot`. It replies with a token that looks like `123456789:AAE…`.

## 2. Put the token in `.env` yourself, by hand, now

```bash
cp .env.example .env   # if you have not already
```

Open `.env` and paste the token into `TELEGRAM_TOKEN=`. Nothing automated writes it — not the
`telegram-bot` skill, not any command in this document. From here on every example reads it
from the environment, so the token never lands in your shell history or in a transcript:

```bash
export $(grep -v '^#' .env | xargs)
```

`grep -v '^#'` is needed because `.env` has comment lines and `xargs` would hand them to
`export` as words. If a value ever contains a space, use `set -a; . ./.env; set +a` instead.

## 3. The "whitelist" that actually matters

A Telegram bot **cannot start a conversation**. It can only message a chat that has already
contacted it, so you must send `/start` to your own bot before the device can reach you. That
is Telegram's built-in allowlist, and it is why a leaked token still cannot spam strangers.

The inverse is also true: *anyone* who knows the bot's `@username` can message it. This
firmware only ever **sends**, to one hardcoded `chat_id`, so nothing inbound can make it act —
but if receiving is ever added, filter on `chat_id` first.

## 4. Lock it down in BotFather

- `/setjoingroups` → Disable — the bot cannot be added to groups.
- `/setprivacy` → Enable.
- `/setdescription` and `/setuserpic` if you like.

## 5. Find your `chat_id`

Send `/start` to your bot first, then:

```bash
export $(grep -v '^#' .env | xargs)
curl -s "https://api.telegram.org/bot$TELEGRAM_TOKEN/getUpdates" | jq '.result[].message.chat'
```

A positive id is a private chat; `-100…` is a supergroup. (Alternative: ask
[@userinfobot](https://t.me/userinfobot).)

Put it in `.env` as `TELEGRAM_CHAT_ID=`, then re-run the `export` line so your shell picks it up.

## 6. Test both notification modes

This is the exact call the firmware makes:

```bash
curl -sG "https://api.telegram.org/bot$TELEGRAM_TOKEN/sendMessage" \
  --data-urlencode "chat_id=$TELEGRAM_CHAT_ID" \
  --data-urlencode "text=silent test" \
  --data-urlencode "disable_notification=true"
```

`disable_notification=true` delivers silently — badge only, no sound or vibration. Drop the
parameter to hear it ring. There is no way to make a message *louder* than default from the
API; that is a client-side per-chat setting (step 7).

The whole Bot API accepts plain `GET` with query parameters, which is why the device needs no
JSON encoder. Useful extras: `parse_mode=HTML` for bold, `protect_content=true`,
`link_preview_options`.

## 7. Make alarms actually wake you

In Telegram, open the bot chat → Notifications → set a custom sound and an exception so Do Not
Disturb does not swallow it. This is client-side only; the API cannot force it.

## 8. Set the thresholds

Secrets live in `.env`; everything else is a build setting in `.cargo/config.toml`, under
`[env]`. Both are read in `src/config.rs` and nowhere else.

| Key | Where | Default | Meaning |
|---|---|---|---|
| `TELEGRAM_TOKEN` | `.env` | — | `123456789:AAE…` from @BotFather |
| `TELEGRAM_CHAT_ID` | `.env` | — | your chat id (negative for groups) |
| `TG_MIN_C` | `.cargo/config.toml` | `10.0` | alarm below this, in °C |
| `TG_HYST_C` | `.cargo/config.toml` | `1.0` | recovery at `TG_MIN_C + TG_HYST_C` |
| `TG_DWELL_MIN` | `.cargo/config.toml` | `5` | minutes an alarm stands before a recovery can clear it; `0` clears at once |
| `TG_REPEAT_MIN` | `.cargo/config.toml` | `60` | minutes between repeats of a standing alarm; `0` sends it once |
| `TG_HEARTBEAT_MIN` | `.cargo/config.toml` | `360` | minutes between silent heartbeats; `0` turns them off |

With `TELEGRAM_TOKEN` or `TELEGRAM_CHAT_ID` missing or empty, the Telegram client logs
`Telegram not configured` and parks. The rest of the firmware is unaffected: `app.rs` keeps
deciding what it would have said, and those messages go nowhere.

`.env` is gitignored, but the token is **compiled into the firmware image**: anyone who can
dump the flash has it. What that is worth to an attacker is exactly one spam channel into one
chat — see step 10 for how to burn it.

## 9. Rate limits

Roughly 1 message/second to a single chat and ~30/s overall; exceeding that returns `429` with
a `retry_after`. This design sends a handful of messages a day, nowhere near the limit.

A loud message waits for the link and is then retried for about a quarter of an hour (5 s, 15 s,
then 45 s between attempts), so a long outage delays an alarm rather than losing it. It does
give up in the end: when the *chat* is what refuses — a stale `chat_id`, a blocked bot — every
message behind it would otherwise be starved, and a standing alarm is repeated anyway.

A silent one gets three attempts, and the heartbeat none at all — it is dropped outright while
the link is down, since an "alive" line that arrives hours late reads as current and is worse
than no line. The interval is not spent on it, though: a heartbeat that comes due with the link
down is not composed at all, and goes out within a minute of Wi-Fi coming back.

## 10. Rotate or revoke

BotFather `/token` regenerates the token — the old one dies immediately. `/deletebot` removes
the bot entirely. Either way, update `.env` by hand and re-flash.

## TLS: certificate verification is off

The device talks to `api.telegram.org` over TLS 1.3, but passes `TlsVerify::None`, so
`embedded-tls` encrypts the connection without checking who is on the other end. Anyone able to
MITM the device's Wi-Fi can therefore read the bot token out of the request URL.

Turning verification on is possible — this is a choice, not a limitation:

- Telegram's chain is RSA-2048 / SHA-256 the whole way up (leaf ← *Go Daddy Secure Certificate
  Authority - G2* ← *Go Daddy Root Certificate Authority - G2*), so it needs `embedded-tls`'s
  `rsa` feature, reached through `reqwless`'s. That pulls in the `rsa` crate, which wants
  `alloc` — and this firmware has `alloc`, so that part is free.
- The Go Daddy G2 root then has to be pinned in flash as DER and handed over as
  `TlsVerify::Certificate { ca, .. }`.

Two things it still would not buy you:

- **No expiry checking.** The board has no RTC and does not run NTP, so `embedded-tls` gets
  `NoClock` and skips `notBefore`/`notAfter` entirely. An expired certificate would still be
  accepted.
- **A new way for the alarm to fail silently.** A pinned root is a hard dependency on Telegram
  keeping that CA. If they rotate, every send fails until the firmware is rebuilt — and the
  whole point of this device is to be the thing that still works when something else broke.

For a device on a home LAN talking to exactly one host, where the worst case is a leaked
single-chat bot token, that trade was not worth making. On a network you do not trust it is.

## Verifying on real hardware

Temporarily set `TG_HEARTBEAT_MIN=1` and `TG_MIN_C=30` in `.cargo/config.toml` (so room
temperature counts as "below threshold"), then `cargo run` and watch for:

- Wi-Fi `Connected` in the serial log, then the silent `m5nanoc6 started` line in the chat;
- a loud `ALARM:` message within ~5 s of the first sensor reading;
- a silent heartbeat about a minute later;
- one blue blink on the RGB LED per successful send, two yellow blinks per failure.

Cup a warm hand over the sensor (or set `TG_MIN_C=5`) and confirm the loud recovery fires once —
five minutes after the alarm at the earliest, which is `TG_DWELL_MIN` doing its job. Unplug the
ENV-Pro and wait: the next heartbeat must arrive loud, saying `no sensor reading for over 5
minutes`. Pull the router mid-run: sends must fail with a yellow double-blink and a `warn!`, the
task must not panic, and a deferred *alarm* must go out once Wi-Fi returns — a heartbeat already
in flight is meant to be dropped, and logs `link down, dropping`, while one that comes due with
the link down is skipped and sent on reconnect instead.

Restore the real `TG_MIN_C` / `TG_HEARTBEAT_MIN` values before the final flash.
