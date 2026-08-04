# Slack direct messages

Push supports one-to-one Slack app direct messages. It receives events through
[Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode/) and
sends replies with [`chat.postMessage`](https://docs.slack.dev/reference/methods/chat.postMessage/).
It ignores public and private channels, group DMs, slash commands, and bot
messages.

## Create the Slack app

1. Create a Slack app for one workspace and enable Socket Mode.
2. Create an app-level token with `connections:write`. App tokens start with
   `xapp-`.
3. Add the bot token scopes `im:history`, `chat:write`, and `files:read`.
4. Under Event Subscriptions, subscribe to the bot event `message.im`.
5. Install or reinstall the app and open its Messages tab.
6. Copy your stable Slack member ID, such as `U012ABCDEF`, from your profile.

These are the only Slack scopes needed. Do not add channel, group-DM,
`chat:write.public`, or user-token scopes.

## Configure Push

Prefer environment variables for both secrets:

```sh
export SLACK_APP_TOKEN='xapp-...'
export SLACK_BOT_TOKEN='xoxb-...'
```

Configure the channel and an explicit user allowlist:

```toml
channel = "slack"
agent = "codex"
assistant_root = "~/Code/assistant"

[slack]
allow_user_ids = ["U012ABCDEF"]
```

You can instead set `slack.app_token` and `slack.bot_token` in the private Push
config. Never put tokens in the Git-versioned assistant repository. Run:

```sh
chmod 600 "${PUSH_HOME:-$HOME/.push}/config.toml"
push doctor
push
```

At runtime Push checks the bot token with Slack's
[`auth.test`](https://docs.slack.dev/reference/methods/auth.test/) method and
binds the connection to that workspace and bot user. Usernames and display
names are not accepted in the allowlist.

## Delivery and recovery

Push validates the workspace, message shape, direct-message channel type,
sender ID, and bot origin before an event can reach an agent. Ordinary text
messages and `file_share` messages are supported.

Send up to four JPEG, PNG, or WebP images in one DM, with or without message
text. Their combined size must be at most 6 MiB. Images work with Claude Code,
Codex, and Pi. Push stores only Slack file IDs, sizes, and MIME type hints in
the durable inbox before acknowledging the event. It never stores private
download URLs or file bytes there. After the workspace, DM, and sender
allowlist checks pass, Push resolves current file metadata with `files.info`,
downloads the files using the bot token, validates the shared image limits and
file signatures, and removes the private temporary files after the turn.
Conversation history retains only the message text or an image placeholder.
Review the configured model provider's image and data controls before using
this feature.

Each conversation uses the stable key
`slack:dm:<workspace-id>:<dm-channel-id>`. Replies go to the Slack thread rooted
at the originating message. Push uses
[`assistant.threads.setStatus`](https://docs.slack.dev/reference/methods/assistant.threads.setStatus/)
as a best-effort progress signal. A status failure never blocks the final
reply.

Before acknowledging an accepted Socket Mode envelope, Push commits it to a
private SQLite inbox beside `state_path`. Ignored envelopes retain only redacted
rejection metadata, not message text or attachment metadata, before they are
acknowledged. Slack's globally unique Events API
`event_id` is the durable deduplication key, while a local monotonic row ID
drives ordered cursor recovery. A crash before acknowledgement causes Slack to
redeliver the same event; a restart resumes committed rows above the saved cursor. Slack does
not document an idempotency key for `chat.postMessage`, so a network failure
after Slack accepts a send can still produce an ambiguous delivery.

Web API rate limits are retried once using Slack's `Retry-After` header, then
the normal Push delivery retry path applies. Replies are split at 4,000 Unicode
characters. If image metadata or download fails, Push sends a safe retry message
without starting an agent. Sending generated images and Slack voice messages or
replies is not supported.

For scheduled delivery, use an allowlisted Slack user ID:

```toml
[primary_delivery]
channel = "slack"
target = "U012ABCDEF"
```

Slack accepts the user ID as the `chat.postMessage` destination and opens the
app's direct-message conversation.

## Troubleshooting

- If `Slack app token` fails in `push doctor`, set `SLACK_APP_TOKEN` or
  `slack.app_token`.
- If `Slack bot token` fails, set `SLACK_BOT_TOKEN` or `slack.bot_token`.
- If messages are ignored, confirm `message.im`, `im:history`, the exact member
  ID, and that the app was reinstalled after scope changes.
- If Slack returns `missing_scope`, confirm `connections:write` is on the app
  token and `im:history`, `chat:write`, plus `files:read` are on the bot token.
- Reconnect messages are expected because Slack refreshes Socket Mode
  connections periodically. Push's dedicated receiver reconnects automatically.
