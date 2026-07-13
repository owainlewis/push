# Push v1 PRD

## Summary

Push is a small Rust binary that turns coding-agent runtimes into a personal
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
- Support simple per-thread backend routing.
- Load one user-owned `SOUL.md` identity file.
- Create and configure one user-selected assistant repository with `push init`.
- Keep the binary local and self-contained.

## Non-Goals

- Build a custom agent runtime.
- Build a plugin system in the gateway.
- Reimplement MCP, tools, skills, or permission prompts.
- Auto-write memory.
- Support group chats.
- Support proactive messages.
- Support multiple active backends in the same conversation at the same time.
- Support assistant IDs, registries, selection, or multiple assistants.

## Target User

The first user is a technical operator who already uses coding agents and wants
a personal assistant reachable through messages.

The assistant should know the user, their business, their projects, and their
preferences. The selected backend should do the actual work.

## Positioning

Hermes and similar projects build more of the runtime: memory databases,
summarizers, skills, subagents, schedulers, provider abstractions, and custom
agent behavior.

Push takes a narrower bet:

| Area | Hermes-style product | Push |
|---|---|---|
| Runtime | Built into the product | External backend |
| Tools | Product-owned | Backend-owned |
| Memory | Opaque or generated store | Plain markdown first |
| Gateway | One part of the product | The product |
| Backend choice | Abstracted provider/runtime | Adapter to real CLIs |
| First backends | Product-specific | Claude Code and Codex |

## Core User Flow

1. User sends a text.
2. Push reads the new row from `chat.db`.
3. Push filters by allowlist and reply marker.
4. Push loads assistant context.
5. Push resolves the thread's backend session.
6. Push runs Claude Code or Codex.
7. Push sends the final reply back over iMessage.
8. Push stores the latest message row and backend session state.

## Components

- `src/imessage/poller.rs`: reads Messages rows.
- `src/imessage/sender.rs`: sends replies through AppleScript.
- `src/gateway.rs`: poll loop, filtering, commands, queues, worker dispatch.
- `src/history.rs`: canonical SQLite conversations and messages.
- `src/jobs.rs`: validated runbooks, advisory locking, manual execution, and run ledger.
- `src/assistant.rs`: safe assistant repository scaffolding and root persistence.
- `src/approval.rs`: durable bounded questions and normalized answers.
- `src/agent.rs`: backend boundary.
- `src/claude.rs`: Claude Code adapter.
- `src/codex.rs`: Codex adapter.
- `src/store.rs`: last row and backend session state.
- `src/soul.rs`: Markdown assistant identity loading.
- `src/config.rs`: TOML configuration.

## Backend Behavior

### Claude Code

Claude Code is selected with:

```toml
agent = "claude"
```

It uses:

- `claude -p`
- `--session-id` for new conversations
- `--resume` for existing conversations
- `--append-system-prompt` for assistant identity

### Codex

Codex is selected with:

```toml
agent = "codex"
```

It uses:

- `codex exec --json`
- `codex exec resume <thread_id>`
- `--output-last-message` internally for final reply capture
- JSONL event parsing to store the Codex thread id

Codex assistant identity is passed as developer instructions, separate from the
user prompt.

## Configuration

| Field | Meaning |
|---|---|
| `agent` | `claude` or `codex`. |
| `routes` | Exact thread to backend overrides. |
| `permission_profile` | Default named profile; defaults to `restricted`. |
| `channels` | Optional advanced list of reply channels to poll concurrently; otherwise `channel` is used unchanged. |
| `primary_delivery` | Optional enabled channel and allowlisted target for proactive output. |
| `permission_profiles` | Custom names mapped to a common capability. |
| `assistant_root` | Canonical root of the one assistant repository. `SOUL.md`, `context/`, and `jobs/` are derived. |
| `drafts_dir` | Agent-written inactive job proposals; defaults to `~/.push/drafts`. |
| `jobs_agent` | Optional default job backend; otherwise uses `agent`. |
| `jobs_max_timeout` | Maximum validated job timeout; defaults to `30m`. |
| `jobs_run_dir` | Local advisory-lock state; defaults to `~/.push/run`. |
| `jobs_max_workers` | Maximum concurrent scheduled jobs; defaults to `2`. |
| `imessage.db_path` | Path to Messages `chat.db`. |
| `poll_interval` | How often to poll. |
| `run_timeout` | Max backend run time. |
| `imessage.self_handles` | User's own iMessage handles. |
| `imessage.allow_from` | Other allowed senders. |
| `telegram.bot_token_env` | Environment variable containing the bot token. |
| `telegram.allow_user_ids` | Allowed private-chat sender IDs. |
| `telegram.allow_chat_ids` | Allowed private chat IDs. |
| `claude_bin` | Claude Code binary. |
| `codex_bin` | Codex binary. |
| `codex_model` | Optional Codex model override. |
| `sessions_dir` | Per-thread working dirs. |
| `state_path` | JSON state path. |
| `audit_log_path` | Local JSONL audit log path. |
| `audit_log_content` | Whether audit events include message and reply text. |
| `database_path` | Canonical SQLite history path; defaults to `~/.push/push.db`. |
| `reply_marker` | Footer used to skip Push's own replies. |

## Control Commands

- `/clear`, `/new`, `/reset`: rotate the current backend session.
- `/help`: show available commands.

## Acceptance Criteria

- A configured self-chat message gets a reply.
- Non-allowlisted senders are ignored.
- Push does not answer messages containing the reply marker.
- Push only advances `last_row_id` after a message is ignored or completed.
- `/clear` starts a fresh backend session.
- Claude backend can create and resume a session.
- Codex backend can create a session, store the Codex thread id, and resume it.
- Workspace routes can propose a validated job draft, but only an exact-revision
  approval from the bound allowlisted identity can install it.
- Fresh or lost backend sessions receive bounded recent canonical history;
  resumed sessions receive only the new request.
- Assistant identity is included in backend runs at instruction priority.
- `push init [path]` safely creates and Git-initializes the conventional
  assistant structure, defaults to `./assistant`, and persists one canonical
  root without overwriting user files.
- Every backend run receives resolved assistant, context, and jobs locations in
  gateway-owned instructions.
- Exact thread routes can choose a non-default backend.
- Tests cover filtering and backend output parsing.

## Risks

- A crash during an in-flight run retries that message on restart, which can
  duplicate backend work but avoids silently losing the message.
- Codex resume behavior depends on the Codex CLI session store.
- Full-access profiles are powerful local execution modes.
- iMessage database shape can change across macOS versions.

## Next Scope

1. A second message channel.
2. Audited memory write-back.
3. Richer routing rules, such as task-type routing.
4. Homebrew formula after the release flow is proven.
