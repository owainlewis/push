# push

push is a tiny personal assistant gateway. You text it, it sends the message to
a configured coding-agent runtime, then it sends the answer back.

The product is the gateway: messaging, allowlists, routing, assistant profile,
memory, and conversation state. The agent runtime is deliberately disposable.
Claude Code and Codex are the first two backends.

## How it works

```text
iMessage or Telegram -> push gateway -> Claude Code or Codex -> same-channel reply
```

1. Poll `~/Library/Messages/chat.db` or the Telegram Bot API for new messages.
2. Keep only messages from yourself or configured allowed senders.
3. Map each conversation to the active backend session.
4. Load your assistant context from `assistant/User.md` and
   `assistant/Memory.md`.
5. Run the configured backend headlessly.
6. Send the final answer back to the originating channel and conversation.

Memory is plain markdown you own. The gateway injects it into each run, so you
can read it, edit it, and version it without learning a custom memory database.

## The Bet

Coding agents are becoming commodity runtimes. Claude Code, Codex, Cursor, AMP,
Pi-style agents, and independent agents all compete on the same layer: tool use,
repo edits, command execution, MCP, plugins, model choice, and coding workflow.

push does not try to win that layer. It treats those agents as workers behind a
small contract: given this user message and assistant context, produce the reply
or task result that should be sent back.

push owns the personal assistant layer:

- Message ingress and egress.
- Sender allowlists and reply loop prevention.
- User-owned assistant config.
- Durable memory files.
- Conversation to backend-session mapping.
- Routing between channels and runtimes.

The framing is personal assistant first, coding agent second. The backend may be
Claude Code today and Codex tomorrow, but the assistant identity, memory, and
messaging relationship stay with push.

See [docs/strategy.md](docs/strategy.md) for the full direction.

## Backends

### Claude Code

Claude Code uses `claude -p` with `--session-id` for new conversations and
`--resume` for existing conversations. Assistant context is passed with
`--append-system-prompt`, so Claude Code keeps its normal tools, MCP servers,
permissions, login, and `CLAUDE.md` behavior.

### Codex

Codex uses `codex exec` in non-interactive mode. The first run captures the
Codex thread id from JSONL output; later turns resume that session with
`codex exec resume`. Assistant context is included in the prompt because Codex
does not expose the same `--append-system-prompt` flag as Claude Code.

## v1 Scope

- iMessage and Telegram private-chat channels.
- Claude Code backend.
- Codex backend.
- Read-only memory files.
- One configured backend at a time.

## Requirements

- For iMessage: macOS with iMessage signed in, Full Disk Access for your
  terminal, and `osascript`.
- For Telegram: a bot token and an allowlisted private-chat user or chat id.
- At least one backend on your `PATH`:
  - `claude` for Claude Code.
  - `codex` for Codex.
- A recent Rust toolchain.

## Quick Start

Install the latest release:

```sh
curl -fsSL https://raw.githubusercontent.com/owainlewis/push/main/install.sh | sh
```

Or build from source:

```sh
git clone https://github.com/owainlewis/push.git
cd push
cp config.toml.example config.toml
# edit config.toml: replace the Telegram user ID
export TELEGRAM_BOT_TOKEN='your-bot-token'
cargo build --release
./target/release/push doctor --config config.toml
./target/release/push
```

Then message the configured iMessage account or Telegram bot. The reply comes
back through the same channel and conversation.

`push doctor` checks shared paths and backend binaries, then only the selected
channel's requirements. Telegram-only use does not need Messages, `chat.db`, or
`osascript`.

To run push continuously, see [Running push as a Service](docs/services.md) for
macOS `launchd`, Linux `systemd` where supported, logs, restart behavior, and
headless security notes.

## iMessage Support

push supports one-to-one iMessage conversations: self-chat and allowlisted
direct messages. Group chats are not supported in v1 and are ignored.

The iMessage channel reads `~/Library/Messages/chat.db` directly, so the process
needs Full Disk Access on macOS. It assumes the recent macOS Messages schema with
`message`, `handle`, `chat`, `chat_message_join`, and `chat_handle_join` tables;
`push doctor` and the runtime report database access or query failures. Tapbacks,
system rows, blank messages, messages from non-allowlisted senders, and push's
own marked replies are ignored. Phone numbers are matched after removing
formatting, and email handles are matched case-insensitively.

`state.json` stores the last completed Messages row and backend sessions. On
restart, push resumes after the last completed row and keeps existing backend
sessions when the selected backend has not changed.

## Telegram Support

Telegram uses Bot API long polling, so push opens no public port and needs no
webhook server. Private chats are supported first. Group chats, forum topics,
and non-text updates are ignored before they can reach the agent. Replies go to
the chat that originated the accepted message. Replies up to Telegram's 32,768
character rich-message limit use native Rich Markdown, so headings, lists,
links, quotes, tables, and code render instead of appearing as raw Markdown.
Larger replies fall back to ordered, Unicode-safe 4,096-character plain-text
chunks.

Use stable numeric Telegram user or chat ids in the allowlist. Usernames are
mutable and are not accepted as security identities. Keep the bot token in the
`TELEGRAM_BOT_TOKEN` environment variable rather than committing it to
`config.toml`:

```toml
channel = "telegram"
agent = "codex"

[telegram]
allow_user_ids = [123456789]
```

See [Telegram Setup and Security](docs/telegram.md) for BotFather setup, finding
numeric ids, routes, first-run cursor behavior, Linux service configuration,
and credential handling.

## Releases

Tagged releases publish binary archives for Linux and macOS on
[GitHub Releases](https://github.com/owainlewis/push/releases). Release notes
are generated from the merged pull requests and commits for the tag.

To create a release:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The release workflow builds with `cargo build --locked --release`, packages the
binary with `README.md`, `LICENSE`, and `config.toml.example`, uploads checksum
files, and publishes generated notes.

## Website

The project site is published with GitHub Pages from the static files in
[`site/`](site/). Once Pages is enabled for GitHub Actions in the repository
settings, pushes to `main` deploy the site automatically.

### Commands You Can Text

- `/clear`, `/new`, `/reset`: start a fresh backend session.
- `/help`: list commands.

## Configuration

```toml
channel = "imessage"
agent = "codex"

# Advanced shared and backend settings use their existing top-level names.
claude_bin = "claude"
claude_permission_mode = "bypassPermissions"
claude_allowed_tools = []
claude_disallowed_tools = []
codex_bin = "codex"
codex_sandbox = "workspace-write"
codex_approval_policy = "never"
audit_log_path = "~/.push/audit.jsonl"
audit_log_content = false

[imessage]
self_handles = ["you@icloud.com"]
allow_from = ["+15551234567"]

[telegram]
bot_token_env = "TELEGRAM_BOT_TOKEN"
allow_user_ids = [123456789]
allow_chat_ids = []

[[routes]]
thread = "imessage:self:you@icloud.com"
agent = "codex"

[assistant]
name = "push"
tone = "short, direct, and useful"
business = "Describe your business or work context here."
projects = ["push"]
preferences = ["Prefer concise replies."]
```

`channel` can be `imessage` or `telegram`. `agent` can be `claude` or `codex`.
Channel settings belong under `[imessage]` and `[telegram]`. Existing flat
channel keys remain accepted for compatibility, but do not set the same option
in both places.
Routes can override the backend by channel or exact channel-qualified thread:

```toml
[[routes]]
channel = "telegram"
agent = "codex"

[[routes]]
thread = "telegram:dm:123456789"
agent = "claude"
```

iMessage thread keys are `imessage:self:<handle>` and
`imessage:dm:<handle>`. Telegram private-chat keys are
`telegram:dm:<chat_id>`. Private-chat topic keys append
`:topic:<topic_id>`; topic routes inherit the parent private-chat route unless
an exact topic route is configured. Legacy unqualified iMessage route keys
remain accepted.

push reads TOML only. Convert any earlier `config.json` file to `config.toml`
before upgrading. The old JSON filename remains gitignored to reduce the risk
of committing a config that contains credentials.

## Safety

An inbound text is an instruction to an agent with tool access. The trust
boundary is the sender filter: iMessage uses `imessage.self_handles` and
`imessage.allow_from`;
Telegram uses stable numeric `telegram.allow_user_ids` and
`telegram.allow_chat_ids`. These fields decide who can
ask the configured backend to read files, edit files, run shell commands, call
MCP servers, or use any other backend tool. Keep `imessage.allow_from` tight, and treat a
lost or shared phone, forwarded iMessage account, or compromised allowed sender
as able to instruct the agent.

Claude Code defaults to `bypassPermissions` for headless use. Codex defaults to
`workspace-write` plus `never` approval for non-interactive use. Both settings
should be treated as powerful automation. Use the least access that still makes
the assistant useful, and run broad-access modes only in an environment you
control.

For Claude Code, `claude_tools` maps to `--tools`, which controls which tools
are available to the model. Set entries such as `"Read"` and `"Grep"` for a
read-only assistant, or `[""]` to disable tools for pure text replies.
`claude_allowed_tools` maps to `--allowed-tools`, which lets matching tools run
without a prompt, and `claude_disallowed_tools` maps to `--disallowed-tools`,
which denies matching tools. The shorter `tools`, `allowed_tools`, and
`disallowed_tools` names are also accepted.

## Audit Log

push writes a structured JSONL audit log to `audit_log_path`, which defaults to
`~/.push/audit.jsonl`. Each line is one event, such as `message_accepted`,
`message_ignored`, `backend_run_started`, `backend_run_failed`, `reply_sent`, or
`message_completed`.

By default, audit events include metadata only: row id, channel, thread,
backend, routing or error reason, target handle, and message or reply character
count. Message and reply text are not stored unless `audit_log_content` is set
to `true`. Keep the audit log local and protect it like service logs because it
can still contain handles, thread ids, file paths, backend errors, and optional
message content.
