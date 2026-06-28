# push

push is a tiny personal assistant gateway. You text it, it sends the message to
a configured coding-agent runtime, then it sends the answer back.

The product is the gateway: messaging, allowlists, routing, assistant profile,
memory, and conversation state. The agent runtime is deliberately disposable.
Claude Code and Codex are the first two backends.

## How it works

```text
iMessage -> push gateway -> Claude Code or Codex -> iMessage reply
```

1. Poll `~/Library/Messages/chat.db` for new messages.
2. Keep only messages from yourself or configured allowed senders.
3. Map each conversation to the active backend session.
4. Load your assistant context from `assistant/User.md` and
   `assistant/Memory.md`.
5. Run the configured backend headlessly.
6. Send the final answer back through Messages.

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

- iMessage channel.
- Claude Code backend.
- Codex backend.
- Read-only memory files.
- One configured backend at a time.

## Requirements

- macOS with iMessage signed in.
- Full Disk Access for your terminal so push can read `chat.db`.
- `osascript`, which ships with macOS.
- At least one backend on your `PATH`:
  - `claude` for Claude Code.
  - `codex` for Codex.
- A recent Rust toolchain.

## Quick Start

```sh
git clone https://github.com/owainlewis/push.git
cd push
cp config.example.json config.json
# edit config.json: set self_handles and choose "agent": "claude" or "codex"
cargo build --release
./target/release/push
```

Then text yourself in Messages. The reply comes back in the same thread.

### Commands You Can Text

- `/clear`, `/new`, `/reset`: start a fresh backend session.
- `/help`: list commands.

## Configuration

```json
{
  "agent": "codex",
  "self_handles": ["you@icloud.com", "+15551234567"],
  "allow_from": [],
  "claude_bin": "claude",
  "claude_permission_mode": "bypassPermissions",
  "codex_bin": "codex",
  "codex_sandbox": "workspace-write",
  "codex_approval_policy": "never"
}
```

`agent` can be `claude` or `codex`.

## Safety

An inbound text is an instruction to an agent with tool access. Keep
`allow_from` tight.

Claude Code defaults to `bypassPermissions` for headless use. Codex defaults to
`workspace-write` plus `never` approval for non-interactive use. Both settings
should be treated as powerful automation. Use the least access that still makes
the assistant useful, and run broad-access modes only in an environment you
control.
