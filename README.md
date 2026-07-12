# Push

Push is a tiny personal assistant gateway. You text it, it sends the message to
a configured coding-agent runtime, then it sends the answer back.

The product is the gateway: messaging, allowlists, routing, assistant identity,
and conversation state. The agent runtime is deliberately disposable.
Claude Code and Codex are the first two backends.

## How it works

```text
iMessage or Telegram -> Push gateway -> Claude Code or Codex -> same-channel reply
```

1. Poll `~/Library/Messages/chat.db` or the Telegram Bot API for new messages.
2. Keep only messages from yourself or configured allowed senders.
3. Store the accepted message in `~/.push/push.db`.
4. Map each conversation to the active backend session.
5. Load your assistant identity from `~/.push/SOUL.md`.
6. Run the configured backend headlessly.
7. Store the generated reply, then deliver it to the originating conversation.

Identity is plain Markdown you own. The gateway injects it into each run, so
you can read it, edit it, and version it without learning a custom format.

## The Bet

Coding agents are becoming commodity runtimes. Claude Code, Codex, Cursor, AMP,
Pi-style agents, and independent agents all compete on the same layer: tool use,
repo edits, command execution, MCP, plugins, model choice, and coding workflow.

Push does not try to win that layer. It treats those agents as workers behind a
small contract: given this user message and assistant context, produce the reply
or task result that should be sent back.

Push owns the personal assistant layer:

- Message ingress and egress.
- Sender allowlists and reply loop prevention.
- User-owned assistant identity.
- Conversation to backend-session mapping.
- Routing between channels and runtimes.

The framing is personal assistant first, coding agent second. The backend may be
Claude Code today and Codex tomorrow, but the assistant identity, memory, and
messaging relationship stay with Push.

See [docs/strategy.md](docs/strategy.md) for the full direction.

## Backends

### Claude Code

Claude Code uses `claude -p` with `--session-id` for new conversations and
`--resume` for existing conversations. `SOUL.md` is passed with
`--append-system-prompt`, so Claude Code keeps its normal tools, MCP servers,
permissions, login, and `CLAUDE.md` behavior.

### Codex

Codex uses `codex exec` in non-interactive mode. The first run captures the
Codex thread id from JSONL output; later turns resume that session with
`codex exec resume`. `SOUL.md` is passed as Codex developer instructions, kept
separate from the user's message on new and resumed sessions.

## v1 Scope

- iMessage and Telegram private-chat channels.
- Claude Code backend.
- Codex backend.
- Read-only assistant identity.
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
mkdir -p ~/.push
cp assistant/SOUL.example.md ~/.push/SOUL.md
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

To run Push continuously, see [Running Push as a Service](docs/services.md) for
macOS `launchd`, Linux `systemd` where supported, logs, restart behavior, and
headless security notes.

## iMessage Support

Push supports one-to-one iMessage conversations: self-chat and allowlisted
direct messages. Group chats are not supported in v1 and are ignored.

The iMessage channel reads `~/Library/Messages/chat.db` directly, so the process
needs Full Disk Access on macOS. It assumes the recent macOS Messages schema with
`message`, `handle`, `chat`, `chat_message_join`, and `chat_handle_join` tables;
`push doctor` and the runtime report database access or query failures. Tapbacks,
system rows, blank messages, messages from non-allowlisted senders, and Push's
own marked replies are ignored. Phone numbers are matched after removing
formatting, and email handles are matched case-insensitively.

`state.json` stores the last completed Messages row and backend sessions. On
restart, Push resumes after the last completed row and keeps existing backend
sessions when the selected backend has not changed.

`push.db` is the canonical conversation journal. It stores accepted inbound
messages before command or backend work, and stores generated outbound messages
before delivery. Channel event IDs make inbound retries idempotent, while one
outbound row per inbound turn prevents a restart from generating a second
assistant response. Cursors and backend session IDs remain in `state.json`.
If the process stops after a channel accepts a reply but before SQLite records
delivery, restart may resend the same stored reply; it still does not generate
a different second response.

Backend sessions are disposable caches. A normal resumed turn sends only the
new message. A new session, backend switch, `/clear`, or recognized missing
backend session is seeded with up to 20 recent messages from the exact
channel-qualified conversation. Historical content is JSON-delimited, each
message is capped at 4 KiB, and the complete history block is capped at 16 KiB.
`SOUL.md` remains separate backend instruction context. Audit events record
whether a run was new and how many messages were used for rehydration.

`ask_user` is the durable approval boundary for later workflows. It stores a
bounded question in `push.db` before sending the same plain numbered list on
iMessage or Telegram. A reply may be a number when exactly one question is
pending for that origin, or `<correlation-id> <number>`. Answers are bound to
the allowlisted sender, chat, channel, and exact thread or Telegram topic. The
workflow can consume the normalized selected value once. Pending questions
survive restart; expired, cancelled, duplicate, ambiguous, and mismatched
replies never reach an agent and are recorded in the audit log. Failed delivery
leaves the question persisted with failed delivery status for diagnosis or
cancellation. Approval replies are control inputs, so they update approval and
audit state without entering the conversation transcript used for rehydration.

## Telegram Support

Telegram uses Bot API long polling, so Push opens no public port and needs no
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

# Advanced shared settings.
claude_bin = "claude"
codex_bin = "codex"
audit_log_path = "~/.push/audit.jsonl"
audit_log_content = false
database_path = "~/.push/push.db"
assistant_dir = "~/.push"
permission_profile = "restricted"
job_permission_profiles = ["restricted", "research"]
jobs_dir = "~/.push/jobs"
drafts_dir = "~/.push/drafts"
jobs_agent = "codex"
jobs_max_timeout = "30m"
jobs_run_dir = "~/.push/run"
jobs_max_workers = 2

[permission_profiles.research]
capability = "read-only"

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
permission_profile = "workspace"
```

`channel` can be `imessage` or `telegram`. `agent` can be `claude` or `codex`.
Channel settings belong under `[imessage]` and `[telegram]`. Existing flat
channel keys remain accepted for compatibility, but do not set the same option
in both places.

The single `channel` setting remains the default quick start. To poll both
configured providers concurrently, replace it with the advanced `channels`
list:

```toml
channels = ["imessage", "telegram"]

[imessage]
self_handles = ["you@icloud.com"]
allow_from = ["+15551234567"]

[telegram]
bot_token_env = "TELEGRAM_BOT_TOKEN"
allow_user_ids = [123456789]

[primary_delivery]
channel = "telegram"
target = "123456789"
```

Each enabled channel polls independently, keeps its own cursor and ordered
thread queues, and replies through the provider and exact topic that originated
the message. A poll failure on one provider is logged without stopping the
other. Startup preflight checks only enabled providers.

`primary_delivery` is optional and is reserved for proactive job results,
failures, and future approvals. Its channel must be enabled and its target must
already appear in that provider's allowlist. Telegram topics use
`"<chat_id>:<topic_id>"`; iMessage uses an allowed handle. Missing or invalid
primary delivery produces a scoped error only when proactive delivery is
requested and never disables ordinary replies.

Routes can override the backend by channel or exact channel-qualified thread:

```toml
[[routes]]
channel = "telegram"
agent = "codex"
permission_profile = "restricted"

[[routes]]
thread = "telegram:dm:123456789"
agent = "claude"
permission_profile = "workspace"
```

iMessage thread keys are `imessage:self:<handle>` and
`imessage:dm:<handle>`. Telegram private-chat keys are
`telegram:dm:<chat_id>`. Private-chat topic keys append
`:topic:<topic_id>`; topic routes inherit the parent private-chat route unless
an exact topic route is configured. Legacy unqualified iMessage route keys
remain accepted.

## Permission Profiles

Every route selects a named Push permission profile. The default is the built-in
`restricted` profile. Built-ins are `restricted` (`read-only` capability),
`workspace`, and the deliberately explicit `full-access`. Custom profiles can
only select one of those capabilities; they cannot inject raw backend flags.

Backend translation is intentionally conservative:

- `restricted`: Claude gets only Read, Grep, and Glob with Bash and write tools
  denied; Codex uses the read-only sandbox with approvals disabled.
- `workspace`: Claude gets read and file-edit tools but no Bash because Claude
  Code has no equivalent to Codex filesystem sandboxing; Codex uses
  `workspace-write` with approvals disabled.
- `full-access`: maps to backend bypass modes, but Push rejects it for
  unattended routes and jobs because it cannot protect installed jobs,
  configuration, or state from direct writes.

Future jobs request only a profile name explicitly listed in
`job_permission_profiles`. The allow-list defaults to only `restricted`; adding
`workspace` or a contained custom profile is an explicit operator choice.
Unknown route profiles fail startup. Unknown or unapproved job references return
a job-scoped validation error without invalidating messaging routes.

Legacy raw settings such as `claude_permission_mode`, `claude_tools`,
`claude_allowed_tools`, `claude_disallowed_tools`, `codex_sandbox`, and
`codex_approval_policy` now produce a migration error; replace them with a named
profile.

## Manual Jobs

Jobs are user-owned Markdown runbooks in `~/.push/jobs/`. The filename is a
lowercase slug and the body is sent verbatim to a fresh backend session:

```markdown
+++
version = 1
permission_profile = "restricted"
timeout = "5m"
workdir = "~/Code"
backend = "codex"
+++

Review repositories with uncommitted work. Do not change files or remote state.
```

Use:

```bash
push job validate --config config.toml
push job list --config config.toml
push job show repo-review --config config.toml
push job run repo-review --config config.toml
push job runs repo-review --config config.toml
```

Validation rejects unknown frontmatter, unsafe filenames, symlinks, missing
work directories, unknown backends, excessive timeouts, and permission profiles
not explicitly allowed by `job_permission_profiles`. Invalid jobs are disabled
individually and do not stop messaging or other valid jobs.

Manual runs execute in the invoking CLI process. Push records and claims the
run in SQLite before execution and holds a non-blocking per-job OS advisory lock
for its full lifetime. An overlap is recorded as `skipped_overlap`. Once the
lock is released, the next start safely marks a stale `running` manual claim as
failed before claiming a new run. Results and diagnostics are bounded and
remain visible through `push job runs`; manual results are printed to the
terminal and are never proactively sent to a chat.

Add one or more cron triggers to schedule a job. Cron uses five fields and an
explicit IANA timezone:

```toml
[[triggers]]
id = "weekday-morning"
kind = "cron"
schedule = "0 8 * * 1-5"
timezone = "Europe/London"
enabled = true
```

Scheduling starts only when `primary_delivery` resolves to an enabled,
allowlisted destination. Missing or invalid primary delivery disables cron
without affecting replies or manual jobs. The gateway runs at most
`jobs_max_workers` scheduled jobs concurrently. It does not catch up occurrences
missed while offline, and daylight-saving gaps are skipped while repeated local
times run once at their first instant.

Scheduled state and bounded output are persisted before delivery. Success,
failure, timeout, overlap, and delivery state remain separate in
`push job runs`. Delivery retries up to three times with backoff from the stored
result, including after restart, and never reruns the backend.

## Agent-Drafted Jobs

A route using the `workspace` profile can propose a job by writing one complete
runbook to the identity-specific inbox Push provides beneath `drafts_dir`, which
defaults to `~/.push/drafts`. Push gives the backend only that opaque inbox as
its extra writable root, so concurrent senders and topics cannot claim each
other's files. `restricted` routes
remain read-only, and `full-access` routes and jobs are rejected because their
backend bypass modes cannot enforce this boundary.

After a route run finishes, fails, times out, or resumes from a persisted
outbound reply, Push reconciles every unrecorded revision in that route's inbox.
It validates each filename,
complete contents, work directory, timeout, backend, triggers, and named
permission profile. Invalid files, symlinks, path escapes, protected Push paths,
and profiles above the configured job ceiling never reach approval. A valid
draft is sent in full to the originating allowlisted channel followed by an
Approve or Reject `ask_user` question.

Approval is bound to that channel, sender, chat, thread or topic, and the exact
SHA-256 revision shown in the question. Push stores the proposal bytes and both
identities in `push.db`. It revalidates the current draft and configured ceiling
before installing from the stored bytes with an atomic no-clobber operation.
Any edit after presentation invalidates the approval. Rejection leaves the file
inactive in `drafts_dir`; duplicate answers cannot install twice.

Push reads TOML only. Convert any earlier `config.json` file to `config.toml`
before upgrading. The old JSON filename remains gitignored to reduce the risk
of committing a config that contains credentials.

## Assistant Identity and Migration

`SOUL.md` is the only assistant identity source. By default Push reads
`~/.push/SOUL.md`; set `assistant_dir` to keep the file elsewhere. Push reads
the file for every run, appends its own invariant instructions in memory, and
never creates or rewrites `SOUL.md`.

If `SOUL.md` is missing, backend runs continue with only Push's invariants and
no custom identity. Copy any identity, preferences, or stable context that you
want to preserve from the old `[assistant]`, `User.md`, and `Memory.md` inputs
into `SOUL.md`. Old context files are not loaded, and a remaining `[assistant]`
table produces an actionable configuration error.

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

The default `restricted` profile does not grant write or shell tools. Broader
profiles should still be treated as powerful automation because backend
enforcement differs and an allowed sender controls the request.

## Audit Log

Push writes a structured JSONL audit log to `audit_log_path`, which defaults to
`~/.push/audit.jsonl`. Each line is one event, such as `message_accepted`,
`message_ignored`, `backend_run_started`, `backend_run_failed`, `reply_sent`, or
`message_completed`.

By default, audit events include metadata only: row id, channel, thread,
backend, routing or error reason, target handle, and message or reply character
count. Message and reply text are not stored unless `audit_log_content` is set
to `true`. Keep the audit log local and protect it like service logs because it
can still contain handles, thread ids, file paths, backend errors, and optional
message content.
