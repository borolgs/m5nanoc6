---
name: telegram-bot
description: Set up or check this firmware's Telegram notifier — discover the chat_id and send silent and loud test messages. Use when the user says "set up the Telegram bot", "get my chat_id", "test the alarm", or types /telegram-bot.
---

# Telegram bot setup

`docs/telegram-bot.md` is the source of truth — read it if anything here is ambiguous. This
skill automates only the fiddly parts of it: finding the `chat_id` and confirming both
notification modes work end to end.

## The token is the user's job, and only the user's job

Never ask for the token, never accept it in chat, never write it to a file, never echo it.
Every command below reads it from the environment. If a token ever appears in command output,
redact it before showing the output. Never `git add .env`. Never suggest passing the token as a
literal argument on a command line.

## Step 0 — precondition

```bash
grep -q '^TELEGRAM_TOKEN=.\+' .env
```

If this fails (no `.env`, or the key is empty), **stop**. Tell the user to follow steps 1–2 of
`docs/telegram-bot.md` — create the bot in @BotFather and paste the token into `.env` by hand —
then re-run this skill. Do not offer to do it for them.

## Step 1 — load the environment

Every subsequent command runs in a shell that has done:

```bash
export $(grep -v '^#' .env | xargs)
```

## Step 2 — resolve the chat id

```bash
curl -s "https://api.telegram.org/bot$TELEGRAM_TOKEN/getUpdates"
```

- If `result` is empty, say why: the user has not sent `/start` to the bot yet. A bot cannot
  start a conversation, so there is nothing for `getUpdates` to return until they do. Ask them
  to message the bot, then retry.
- List the candidate chats — `id`, plus `title` / `username` / `first_name`, and whether each
  is private (positive id) or a group (`-100…`). If there is exactly one, use it. If there is
  more than one, ask the user which to use.

## Step 3 — test both modes

Send a silent message and a loud one, so the user can confirm the difference on their phone:

```bash
curl -sG "https://api.telegram.org/bot$TELEGRAM_TOKEN/sendMessage" \
  --data-urlencode "chat_id=<id>" \
  --data-urlencode "text=silent test from the telegram-bot skill" \
  --data-urlencode "disable_notification=true"

curl -sG "https://api.telegram.org/bot$TELEGRAM_TOKEN/sendMessage" \
  --data-urlencode "chat_id=<id>" \
  --data-urlencode "text=loud test from the telegram-bot skill"
```

Report `ok: true` / the error description for each. Ask the user to confirm the second one made
a sound — if it did not, point them at step 7 of `docs/telegram-bot.md` (custom sound and a Do
Not Disturb exception; the API cannot force this).

## Step 4 — write the chat id

`.env` holds secrets only. Set `TELEGRAM_CHAT_ID=<id>` there — if it already has a value, show
it and ask before overwriting. `TELEGRAM_TOKEN` is never touched.

The thresholds (`TG_MIN_C`, `TG_HYST_C`, `TG_REPEAT_MIN`, `TG_HEARTBEAT_MIN`) are already in
`.cargo/config.toml` under `[env]`. Show the user their current values and offer to change
them; do not duplicate them into `.env`.

Then say that both files are read at build time, so the change needs a rebuild: `cargo run` to
flash and watch, or `cargo check` if there is no board connected.
