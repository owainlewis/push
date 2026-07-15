# Configuration

Push reads TOML from `~/.push/config.toml` by default. Pass `--config <path>`
to use a different file for a gateway, doctor, init, or job command.

```sh
push doctor
push
```

Paths beginning with `~` are expanded. Invalid values, unknown fields inside
provider sections, unsafe path overlap, and removed gateway permission settings
fail configuration load with an actionable error.

Create the one assistant repository and persist its root before editing the
rest of the config:

```sh
push init ~/Code/assistant
```

For a new file, init writes a private, owner-only Telegram and Codex starting
point with empty `telegram.bot_token` and `telegram.allow_user_ids` values.
Fill both in before running Push. Push derives `SOUL.md`, `context/`, and `jobs/` from
`assistant_root`. At run time it appends their resolved absolute locations to
the user-owned `SOUL.md` instructions in memory. It does not write machine
paths into the repository.

Root configuration, route, and primary-delivery tables do not
yet reject every unknown key. Use the documented names, then run `push doctor`;
do not assume a silent key changed runtime behavior.

## Minimal configuration

```toml
channel = "telegram"
agent = "codex"
assistant_root = "~/Code/assistant"

[telegram]
bot_token = "token-from-BotFather"
allow_user_ids = [123456789]
```

`channel` is the easiest single-provider setup. `agent` is `claude`, `codex`,
or `pi`. Push does not set sandbox, approval, permission-mode, or tool flags.
Configure those in the selected agent.

### Pi setup

Install Pi from [pi.dev](https://pi.dev/) and configure a model provider or
complete its authentication as the same user that runs Push. Confirm `pi
--version` works in the service environment, then select it:

```toml
agent = "pi"
pi_bin = "pi"
```

Push runs `pi --print --mode json`, stores the session ID from Pi's JSON event
stream, and resumes it with `--session`. Clearing a conversation discards that
mapping, so the next turn creates a fresh Pi session. Push appends `SOUL.md` as
system instructions, separate from the user message. Pi is not required unless
the default backend, an enabled route, or `jobs_agent` selects it.

## Channels

### iMessage

```toml
[imessage]
db_path = "~/Library/Messages/chat.db"
self_handles = ["you@icloud.com"]
allow_from = ["+15551234567"]
```

At least one `self_handles` or `allow_from` value is required when iMessage is
enabled. See the [iMessage guide](channels/imessage.md).

### Telegram

```toml
[telegram]
bot_token = "token-from-BotFather"
allow_user_ids = [123456789]
allow_chat_ids = []
```

At least one stable numeric user or chat ID is required. Keep the config file
private. `push init` creates new config files with mode `0600` on Unix. The
`bot_token_env` setting remains available when an environment variable is a
better fit. See the [Telegram guide](telegram.md).

Telegram voice notes are optional and need no TOML settings. Set
`OPENAI_API_KEY` in the gateway process environment to enable both
transcription and speech replies. Without it, text remains fully available and
voice notes get a helpful fallback. See [Voice Messages](telegram.md#voice-messages).

### Run both providers

Use `channels` instead of `channel`:

```toml
channels = ["imessage", "telegram"]
agent = "codex"

[imessage]
self_handles = ["you@icloud.com"]

[telegram]
bot_token = "token-from-BotFather"
allow_user_ids = [123456789]

[primary_delivery]
channel = "telegram"
target = "123456789"
```

Each provider polls independently, keeps its own cursor, and replies through
the channel and exact conversation that originated the message. Failure in one
provider does not stop the other.

`primary_delivery` is the destination for scheduled job results. The channel
must be enabled and the target must appear in that channel's allowlist.
Telegram topic targets use `"<chat-id>:<topic-id>"`.

## Routing

Routes can override the backend for a channel or exact thread:

```toml
[[routes]]
channel = "telegram"
agent = "codex"

[[routes]]
thread = "telegram:dm:123456789"
agent = "claude"
```

Precedence is:

1. exact thread or topic route
2. parent Telegram private-chat route for a topic
3. channel route
4. root `agent`

Thread keys are:

- `imessage:self:<handle>`
- `imessage:dm:<handle>`
- `telegram:dm:<chat-id>`
- `telegram:dm:<chat-id>:topic:<topic-id>`

## Agent permissions

Permissions belong to the agent, not the gateway. Push invokes Claude Code,
Codex, and Pi without overriding their sandbox, approval mode, or tool lists.
This keeps interactive and gateway behavior aligned. Configure permissions in
the selected agent and review [permissions and security](security.md).

## Settings reference

### Core

| Setting | Default | Purpose |
| --- | --- | --- |
| `channel` | `"imessage"` | Single enabled provider when `channels` is empty |
| `channels` | `[]` | Concurrent enabled providers |
| `agent` | `"claude"` | Default backend |
| `poll_interval` | `"3s"` | Delay between channel polls |
| `run_timeout` | `"120s"` | Maximum chat backend run time |
| `claude_bin` | `"claude"` | Claude Code executable |
| `codex_bin` | `"codex"` | Codex executable |
| `codex_model` | unset | Optional Codex model override |
| `pi_bin` | `"pi"` | Pi coding agent executable |
| `reply_marker` | `"\n\n-- sent by push"` | iMessage loop-prevention marker |

### Local state

| Setting | Default | Purpose |
| --- | --- | --- |
| `assistant_root` | required for new setups | Canonical root of the one assistant repository; `SOUL.md`, `context/`, and `jobs/` are derived |
| `sessions_dir` | `~/.push/sessions` | Legacy compatibility setting; chat work directories use `assistant_root` |
| `state_path` | `~/.push/state.json` | Channel cursors and backend session IDs |
| `database_path` | `~/.push/push.db` | Canonical conversation, approval, and job history |
| `audit_log_path` | `~/.push/audit.jsonl` | Structured local audit log |
| `audit_log_content` | `false` | Include message and reply content in audit events |

### Jobs

| Setting | Default | Purpose |
| --- | --- | --- |
| `drafts_dir` | `~/.push/drafts` | Inactive agent-authored proposals |
| `jobs_agent` | root `agent` | Default jobs backend |
| `jobs_max_timeout` | `"30m"` | Maximum accepted job timeout |
| `jobs_run_dir` | `~/.push/run` | Local advisory locks |
| `jobs_max_workers` | `2` | Concurrent scheduled job workers |

Push validates that state, sessions, the assistant root, drafts, locks, the
loaded config file, and job work directories do not overlap in unsafe ways.
Runtime state and secrets must stay outside the Git-versioned assistant
repository.

## Complete example

```toml
channels = ["imessage", "telegram"]
agent = "codex"
assistant_root = "~/Code/assistant"
poll_interval = "3s"
run_timeout = "120s"

[imessage]
self_handles = ["you@icloud.com"]
allow_from = []

[telegram]
bot_token = "token-from-BotFather"
allow_user_ids = [123456789]

[primary_delivery]
channel = "telegram"
target = "123456789"

[[routes]]
thread = "telegram:dm:123456789"
agent = "claude"
```

Legacy flat channel fields remain accepted for migration, but new
configurations should use `[imessage]` and `[telegram]`. JSON configuration and
gateway permission fields are no longer supported. Configure permissions in
the selected agent instead.

Legacy `assistant_dir` and `jobs_dir` settings remain compatible only when the
jobs path is exactly `<assistant_dir>/jobs`. For separate legacy paths, move
`SOUL.md`, context, and jobs under one directory and replace both settings with
`assistant_root`.
