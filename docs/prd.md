# push v1 PRD

## Summary

push is a small Rust binary that turns coding-agent runtimes into a personal
assistant you can text.

It polls iMessage, filters allowed senders, loads user-owned assistant context,
runs a configured backend, and sends the backend's final reply back over
iMessage.

The first supported backends are Claude Code and Codex.

## Product Goal

Build the smallest useful personal assistant gateway:

- Message it naturally.
- Let it use a real coding-agent runtime.
- Keep your assistant memory and preferences outside any one vendor.
- Keep backend agents replaceable.

## Goals

- Receive iMessages and reply through Messages.
- Support Claude Code as a backend.
- Support Codex as a backend.
- Persist conversation to backend-session mappings.
- Inject user-owned assistant context into each run.
- Keep the binary local and self-contained.

## Non-Goals

- Build a custom agent runtime.
- Build a plugin system in the gateway.
- Reimplement MCP, tools, skills, or permission prompts.
- Auto-write memory.
- Support group chats.
- Support proactive messages.
- Support multiple active backends in one conversation.

## Target User

The first user is a technical operator who already uses coding agents and wants
a personal assistant reachable through messages.

The assistant should know the user, their business, their projects, and their
preferences. The selected backend should do the actual work.

## Positioning

Hermes and similar projects build more of the runtime: memory databases,
summarizers, skills, subagents, schedulers, provider abstractions, and custom
agent behavior.

push takes a narrower bet:

| Area | Hermes-style product | push |
|---|---|---|
| Runtime | Built into the product | External backend |
| Tools | Product-owned | Backend-owned |
| Memory | Opaque or generated store | Plain markdown first |
| Gateway | One part of the product | The product |
| Backend choice | Abstracted provider/runtime | Adapter to real CLIs |
| First backends | Product-specific | Claude Code and Codex |

## Core User Flow

1. User sends a text.
2. push reads the new row from `chat.db`.
3. push filters by allowlist and reply marker.
4. push loads assistant context.
5. push resolves the thread's backend session.
6. push runs Claude Code or Codex.
7. push sends the final reply back over iMessage.
8. push stores the latest message row and backend session state.

## Components

- `src/imessage/poller.rs`: reads Messages rows.
- `src/imessage/sender.rs`: sends replies through AppleScript.
- `src/gateway.rs`: poll loop, filtering, commands, queues, worker dispatch.
- `src/agent.rs`: backend boundary.
- `src/claude.rs`: Claude Code adapter.
- `src/codex.rs`: Codex adapter.
- `src/store.rs`: last row and backend session state.
- `src/memory.rs`: markdown assistant context loading.
- `src/config.rs`: JSON configuration.

## Backend Behavior

### Claude Code

Claude Code is selected with:

```json
{ "agent": "claude" }
```

It uses:

- `claude -p`
- `--session-id` for new conversations
- `--resume` for existing conversations
- `--append-system-prompt` for assistant context

### Codex

Codex is selected with:

```json
{ "agent": "codex" }
```

It uses:

- `codex exec --json`
- `codex exec resume <thread_id>`
- `--output-last-message` internally for final reply capture
- JSONL event parsing to store the Codex thread id

Codex assistant context is passed as part of the prompt wrapper.

## Configuration

| Field | Meaning |
|---|---|
| `agent` | `claude` or `codex`. |
| `db_path` | Path to Messages `chat.db`. |
| `poll_interval` | How often to poll. |
| `run_timeout` | Max backend run time. |
| `self_handles` | User's own iMessage handles. |
| `allow_from` | Other allowed senders. |
| `claude_bin` | Claude Code binary. |
| `claude_permission_mode` | Claude Code permission mode. |
| `codex_bin` | Codex binary. |
| `codex_sandbox` | Codex sandbox mode. |
| `codex_approval_policy` | Codex approval policy. |
| `codex_model` | Optional Codex model override. |
| `sessions_dir` | Per-thread working dirs. |
| `state_path` | JSON state path. |
| `assistant_dir` | Directory with `User.md` and `Memory.md`. |
| `reply_marker` | Footer used to skip push's own replies. |

## Control Commands

- `/clear`, `/new`, `/reset`: rotate the current backend session.
- `/help`: show available commands.

## Acceptance Criteria

- A configured self-chat message gets a reply.
- Non-allowlisted senders are ignored.
- push does not answer messages containing the reply marker.
- `/clear` starts a fresh backend session.
- Claude backend can create and resume a session.
- Codex backend can create a session, store the Codex thread id, and resume it.
- Assistant memory is included in backend runs.
- Tests cover filtering and backend output parsing.

## Risks

- Messages are currently marked processed after enqueue, not after delivery.
  This can lose a message if push crashes before the worker replies.
- Codex resume behavior depends on the Codex CLI session store.
- Claude `bypassPermissions` and Codex non-interactive automation are powerful
  local execution modes.
- iMessage database shape can change across macOS versions.

## Next Scope

1. Reliable delivery state.
2. Explicit assistant profile fields.
3. Runtime routing rules.
4. A second message channel.
5. Audited memory write-back.
