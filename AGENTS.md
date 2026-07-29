# AGENTS.md

This guide is for coding agents working on Push. For general contributor setup,
pull request expectations, and the documentation workflow, read
[CONTRIBUTING.md](CONTRIBUTING.md) and
[docs/contributing.md](docs/contributing.md).

## Product boundary

Push is one small local Rust process. It connects private iMessage, Telegram,
and Slack conversations to Claude Code, Codex, or Pi, and runs scheduled
Markdown jobs.

Keep ownership clear:

- Push owns channels, allowlists, routing, scheduling, durable history,
  session state, crash recovery, and delivery.
- The assistant repository owns `SOUL.md`, context, jobs, and optional project
  skills.
- The selected agent owns reasoning, tools, skills, MCP, authentication,
  models, and interactive permissions.

Do not add an agent loop, plugin system, MCP layer, or tool runner to Push.
Extend the existing channel or backend boundaries instead.

Read [docs/architecture.md](docs/architecture.md) before changing state,
sessions, queues, cursors, crash recovery, scheduling, shutdown, or delivery.

## Code map

| Area | Location |
| --- | --- |
| CLI and startup | `src/main.rs` |
| Config loading, migration, and validation | `src/config.rs` |
| Channel-neutral contract | `src/channel.rs` |
| iMessage adapter | `src/imessage/` |
| Telegram adapter | `src/telegram.rs` |
| Slack adapter and durable inbox | `src/slack.rs` |
| Gateway loops and per-thread queues | `src/gateway/` |
| Agent boundary | `src/agent.rs` |
| Claude Code, Codex, and Pi adapters | `src/claude.rs`, `src/codex.rs`, `src/pi.rs` |
| Canonical SQLite history | `src/history.rs` |
| JSON cursor and session state | `src/store.rs` |
| Jobs, scheduler, locks, and run ledger | `src/jobs.rs` |
| Assistant repository setup | `src/assistant.rs`, `src/soul.rs` |
| Diagnostics and audit | `src/doctor.rs`, `src/audit.rs` |
| Voice processing | `src/voice.rs` |
| Gateway integration tests | `src/gateway/tests.rs` |
| CLI, docs, installer, and crash tests | `tests/` |

## Development

Use the stable Rust toolchain. Build and test with the lockfile:

```sh
cargo build --locked
cargo test --locked
```

Before every pull request, run the same core checks as CI:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
cargo test --locked
```

Run focused tests while iterating, then run the full gate. CI runs the Rust
gate on Linux and macOS. It also runs `tests/install.sh`,
`tests/release-version.sh`, and `mkdocs build --strict`.

For documentation-only changes, at minimum run:

```sh
cargo test --locked --test docs
mkdocs build --strict
```

The docs command needs the packages in `requirements-docs.txt`.

## Architecture rules

### Channels

- `ChannelContract` is the provider boundary. Add a concrete implementation
  and a `Channel` variant for a new built-in channel.
- Keep provider details such as addressing, allowlists, message splitting,
  rich formatting, retry timing, typing, and voice transport in the adapter.
- Do not add channel-name branches to shared polling, routing, worker,
  delivery, or shutdown code.
- Accepted messages must produce a stable channel-qualified thread key and an
  exact reply target. Never let replies or sessions cross channels.
- Poll and send futures must be safe to drop during shutdown. Typing updates
  are best effort and must not fail an assistant turn.

### Durable processing

- Record accepted inbound content before backend dispatch.
- Persist generated outbound content before delivery. Retries must resend the
  stored result without rerunning the backend.
- A chat backend run may repeat if the process crashes after execution but
  before recording its outbound result. Only an existing outbound row prevents
  rerunning that turn.
- Do not advance a channel cursor past an earlier in-flight row.
- Checkpoint chunk delivery monotonically and resume at the first unsent chunk.
- Preserve existing `state.json` compatibility fields and SQLite data. When
  changing the history schema, add a migration, advance the schema version,
  and test upgrade behavior.
- Keep work for each thread ordered while allowing independent threads and
  channels to make progress.

### Agent backends

- Keep the shared request and result shape in `src/agent.rs`. Backend-specific
  CLI flags and JSON parsing belong in the matching adapter.
- Chat runs preserve the selected agent's permission configuration. Scheduled
  jobs are unattended and cannot depend on interactive approval.
- Keep system instructions separate from user prompt content.
- A missing stored backend session may rotate once and rehydrate from bounded
  canonical history. Do not create unbounded prompts or retry loops.
- Use the fake runner and contract tests. Unit tests must not invoke installed
  agent binaries.

### Jobs and recovery

- Validate job Markdown before execution and keep the run ledger authoritative.
- Scheduled work must remain bounded by worker and timeout limits.
- Preserve the rule that a result is committed before notification. Restart
  recovery may resume queued work or delivery, but must not rerun backend work
  already recorded as started.
- Keep scheduled delivery separate from ordinary reply delivery when their
  ledgers or retry rules differ.

### Security

- Push uses outbound connections only. Do not introduce a listening server or
  webhook without an explicit architecture decision.
- Apply channel type checks and allowlists before backend dispatch or voice
  download.
- Never commit or expose tokens, personal config, message content, assistant
  identity, audit logs, databases, cursor state, or backend session IDs.
  Normal logs must redact content. Audit content is allowed only through the
  explicit `audit_log_content` opt-in and remains sensitive.
- Runtime state and secrets must stay outside the Git-versioned assistant
  repository. Preserve path canonicalization, symlink checks, and owner-only
  permissions.
- Treat every allowlisted sender as an operator of the configured agent.

## Testing rules

- For a bug, add a regression test that fails for the original cause when
  practical.
- Cover failure and restart paths for changes to persistence, cursors,
  sessions, queues, or delivery.
- Use fakes and local test servers for agent and provider behavior. Tests must
  not need real chat databases, API credentials, network services, or an
  installed agent.
- Keep platform behavior explicit. iMessage production access is macOS-only;
  Telegram and Slack must remain usable on Linux.
- If public behavior, configuration, or CLI output changes, update the
  canonical page under `docs/` and any focused CLI or config tests.

## Documentation and releases

- `docs/` is the source for the website. Do not edit generated `site/` files.
- Keep each fact on one canonical docs page and link to it elsewhere. The
  README is a product overview, not a second configuration reference.
- `Cargo.toml` is the source of the binary version. Release changes must refresh
  `Cargo.lock` and keep the release tag check in `tests/release-version.sh`
  consistent.

## Common gotchas

1. First startup skips existing channel backlog. Tests for startup and cursor
   changes must cover both the initial and resumed cases.
2. Slack acknowledges accepted Socket Mode events only after its local inbox
   persists them. Keep provider event IDs as the deduplication key.
3. A delivery error after a provider accepts a send can be ambiguous. Do not
   claim exactly-once external delivery.
4. Changing a backend for a thread starts a fresh backend session with bounded
   history. Never resume a session created by another backend.
5. Live iMessage checks require Full Disk Access for the exact terminal or
   service process. Automated tests should use the existing fakes instead.
6. Configuration supports documented legacy migrations. Do not silently
   reinterpret removed keys or weaken actionable validation errors.
