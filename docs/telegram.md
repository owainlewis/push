# Telegram Setup and Security

push supports Telegram private chats through Bot API long polling. It makes
outbound HTTPS requests only. You do not need to expose a webhook or public
server port. Telegram documents the polling contract in the official
[Bot API reference](https://core.telegram.org/bots/api#getupdates).

## Create and Configure the Bot

1. Open a private chat with Telegram's official `@BotFather` account.
2. Send `/newbot` and follow the prompts.
3. Store the bot token in a service environment variable. Do not paste it into
   source control, issue comments, commands that enter shell history, or logs.
4. Send one private message to the new bot.
5. Find your stable numeric user and chat ids from a trusted Bot API
   `getUpdates` response or a trusted id lookup bot. Do not allowlist a Telegram
   username because usernames can change.

If the bot previously used a webhook, remove it before starting push because
Telegram does not allow `getUpdates` while a webhook is active. The first push
start discards pending updates, so send a new message after the gateway reports
that it is running.

Export the token in the environment that starts push:

```sh
export TELEGRAM_BOT_TOKEN='replace-with-the-token-from-BotFather'
```

Use a Telegram-only config:

```toml
channel = "telegram"
agent = "codex"

[telegram]
allow_user_ids = [123456789]

[[routes]]
thread = "telegram:dm:123456789"
agent = "claude"
```

The token environment variable defaults to `TELEGRAM_BOT_TOKEN`. Set
`telegram.bot_token_env` only when you need a different variable name.
`telegram.bot_token` is supported for constrained deployments, but the
environment-variable form is safer because it keeps the credential out of the
config file. push never prints the token. Run:

```sh
push doctor --config /absolute/path/to/config.toml
push --config /absolute/path/to/config.toml
```

Doctor validates that a token is available without displaying its value. A
Telegram-only preflight does not open `chat.db` and does not require macOS or
`osascript`.

## Allowlisting and Routing

An incoming Telegram update reaches the agent only when all of these are true:

- it is a normal text message in a private chat
- its numeric sender id is in `telegram.allow_user_ids`, or its numeric chat id
  is in `telegram.allow_chat_ids`
- the text is not empty

Group chats, channels, forum topics, edited messages, and other update types are
out of scope and ignored. The private-chat thread key is
`telegram:dm:<chat_id>`. Replies are sent to that same chat id.

A route with `"channel": "telegram"` selects a backend for all accepted
Telegram messages. An exact `"thread": "telegram:dm:<chat_id>"` route takes
priority over a channel route. The default `agent` applies when no route
matches. iMessage keys include the `imessage:` prefix, so identical numeric or
text identifiers cannot share Telegram session state.

## Cursor and Restart Behavior

`state.json` stores independent `imessage` and `telegram` cursors. On the first
Telegram start, push asks Telegram for the newest pending update and records its
id without running it. This explicit backlog skip prevents old bot messages
from unexpectedly starting agent work. Later accepted and ignored updates
advance only the Telegram cursor. Restarts continue from the next update id.

As with iMessage, a crash after delivery but before cursor persistence can
repeat a reply. Keep `state.json` on durable storage.

## Linux and Service Mode

Telegram-only mode works on Linux or a VM because it does not depend on the
macOS Messages database. Use the systemd example in [services.md](services.md),
and provide the token through the service environment or a root-readable
credentials file supported by your service manager. Avoid placing the token
directly in a world-readable unit file.

Protect these files as credentials or private assistant data:

- the bot token and configuration
- `state.json` and the audit log
- the session workspace directory
- `assistant/User.md` and `assistant/Memory.md`

Never commit bot tokens, state files, audit logs, session workspaces, or
assistant memory. Rotate the bot token with BotFather immediately if it is
exposed. Keep allowlists narrow because an allowed sender can instruct the
configured agent to use its local tools and credentials.
