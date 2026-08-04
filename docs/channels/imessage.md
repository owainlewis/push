# iMessage

Push supports one-to-one iMessage conversations on macOS. It reads the local
Messages database and sends replies with `osascript`. It does not use a cloud
iMessage API or expose a network service.

## Requirements

- macOS with Messages signed in
- Full Disk Access for the terminal or service process running Push
- access to `~/Library/Messages/chat.db` and `~/Library/Messages/Attachments`
- `osascript` on `PATH`
- `/usr/bin/sips` for HEIC and HEIF image conversion

Run `push doctor` from the same user and environment as the long-running
service. A successful interactive check does not prove that a separate service
account has Full Disk Access.

## Self-chat configuration

Use a Messages conversation with your own iMessage handle:

```toml
channel = "imessage"
agent = "codex"

[imessage]
self_handles = ["you@icloud.com"]
```

Push identifies self-chat from the chat identifier and accepts your own
messages in that conversation. It adds a reply marker to outbound messages so
they are not fed back into the agent.

## Allow another sender

```toml
[imessage]
self_handles = ["you@icloud.com"]
allow_from = ["+15551234567", "trusted@example.com"]
```

Only direct messages from these handles are accepted. Phone numbers are
matched after formatting is removed. Email addresses are matched without case
sensitivity.

Treat every allowed handle as an operator of the configured backend. A sender
can ask the agent to use any capability allowed by that agent's configuration.

## Image messages

Send up to four JPEG, PNG, WebP, HEIC, or HEIF images in one accepted
conversation, with or without message text. Their combined prepared size must
be at most 6 MiB. Each HEIC or HEIF source must be at most 32 MiB before local
conversion. Images work with Claude Code, Codex, and Pi.

Polling reads attachment paths, byte-size hints, and MIME type hints from
`chat.db`; it does not open the attachment files. If Messages has not populated
an attachment filename yet, Push gives it a three-poll grace period and defers
that message and later rows so the cursor cannot skip the image. If the filename
is still blank after that grace period, the worker treats it as missing, sends
the safe fallback, and lets later messages continue. After the direct-message,
sender, reply-marker, and message checks pass, the worker canonicalizes each
path and requires it to remain under `~/Library/Messages/Attachments` (or the
`Attachments` directory beside a custom `imessage.db_path`). Missing files,
directories, escaping symlinks, and unsupported documents are rejected with a
safe retry reply before an agent starts.

JPEG, PNG, and WebP files go through the shared byte limit and signature
validation directly. Push converts HEIC and HEIF locally with macOS `sips` in
an owner-only temporary directory, validates the resulting JPEG against the
same shared limit, and removes the conversion file immediately. The prepared
agent handoff files are also owner-only and are removed after the turn.
Conversation history retains only the message text or an image placeholder,
not image bytes or local attachment paths. Review the configured backend's
image and data controls before using this feature.

Sending generated images back through iMessage is not supported.

## What Push ignores

- group chats
- tapbacks and Messages system rows
- blank messages without an image attachment
- stickers, videos, and Live Photo video components
- messages from handles outside the allowlist
- Push's own replies containing the built-in Push reply marker

The channel expects a recent macOS Messages schema. `push doctor` and runtime
logs report database access or query failures rather than silently accepting
no messages.

## Thread keys and routing

Self-chat keys use `imessage:self:<handle>`. Allowed direct-message keys use
`imessage:dm:<handle>`.

```toml
[[routes]]
thread = "imessage:self:you@icloud.com"
agent = "claude"
```

See [configuration](../configuration.md#routing) for route precedence.

## Restart behavior

On the first iMessage start, Push records the newest existing Messages row
without running it. Send a new message after the gateway starts. Later starts
continue after the last completed row stored in `push.db`.

Push stores both the last completed Messages row and accepted conversation
turns in `push.db`. It advances the cursor only after a row is ignored or
completed. An earlier in-flight row prevents later completed rows from pushing
the cursor past it. Existing `state.json` data is imported once on upgrade and
the original file remains as a recovery copy.

If a generated outbound reply was stored before a crash, restart delivers the
stored reply without generating a different second answer. A crash before the
backend result is persisted may repeat backend work.

## Troubleshooting

### `chat.db` cannot be opened

Grant Full Disk Access to the exact terminal or service host, restart that
process, and rerun:

```sh
push doctor
```

Image messages also require that the same process can read
`~/Library/Messages/Attachments`. Recheck Full Disk Access if text works but
images fail.

### Messages are ignored

Confirm the conversation is one-to-one and that its sender or chat identifier
matches `self_handles` or `allow_from`. Check the audit log for the rejection
reason without enabling message content logging.

### Interactive use works but `launchd` does not

Use absolute paths in the service definition, ensure the backend is on the
service `PATH`, and grant Full Disk Access to the process that actually opens
Messages. See [the service guide](../services.md#macos-launchd).
