# iMessage

Push supports one-to-one iMessage conversations on macOS. It reads the local
Messages database and sends replies with `osascript`. It does not use a cloud
iMessage API or expose a network service.

## Requirements

- macOS with Messages signed in
- Full Disk Access for the terminal or service process running Push
- access to `~/Library/Messages/chat.db`
- `osascript` on `PATH`

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
can ask the agent to use any capability allowed by the selected permission
profile.

## What Push ignores

- group chats
- tapbacks and Messages system rows
- blank messages
- messages from handles outside the allowlist
- Push's own replies containing the configured marker

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
permission_profile = "workspace"
```

See [configuration](../configuration.md#routing) for precedence and permission
selection.

## Restart behavior

Push stores the last completed Messages row in `state.json` and accepted
conversation turns in `push.db`. It advances the cursor only after a row is
ignored or completed. An earlier in-flight row prevents later completed rows
from pushing the cursor past it.

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

### Messages are ignored

Confirm the conversation is one-to-one and that its sender or chat identifier
matches `self_handles` or `allow_from`. Check the audit log for the rejection
reason without enabling message content logging.

### Interactive use works but `launchd` does not

Use absolute paths in the service definition, ensure the backend is on the
service `PATH`, and grant Full Disk Access to the process that actually opens
Messages. See [the service guide](../services.md#macos-launchd).
