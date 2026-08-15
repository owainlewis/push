# AGENTS.md

This guide contains project rules that are easy to miss from the code alone.
Use the contributor and architecture documentation for setup and deeper
context.

## Product boundary

Push is a small messaging gateway, not an agent runtime.

- Push owns channels, allowlists, routing, scheduling, durable history,
  sessions, recovery, and delivery.
- The assistant repository owns identity, context, jobs, and project skills.
- The selected backend owns reasoning, tools, skills, models, authentication,
  and interactive permissions.

Do not add an agent loop, plugin system, MCP layer, or tool runner. Extend the
existing channel or backend abstractions.

Read the architecture documentation before changing state, sessions, queues,
cursors, recovery, scheduling, shutdown, or delivery.

## Required checks

Use the stable Rust toolchain and the lockfile. Run focused tests while
iterating, then run the full gate before every pull request:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
cargo test --locked
```

Run the strict documentation build when documentation changes. Match any
additional platform or script checks selected by CI.

## Architecture rules

### Channels

- Keep provider-specific addressing, allowlists, formatting, retries, typing,
  and voice transport inside the channel adapter.
- Do not add provider-name branches to shared polling, routing, worker,
  delivery, or shutdown logic.
- Accepted messages must produce a stable channel-qualified thread key and the
  exact reply target. Replies and sessions must never cross channels.
- Poll and send operations must be safe to cancel during shutdown. Typing is
  best effort and must not fail a turn.

### Durable processing

- Record accepted input before backend dispatch.
- Persist generated output before delivery. Retry stored output without
  rerunning the backend.
- A chat run may repeat after a crash if output was not yet recorded. Only a
  recorded outbound result prevents rerunning that turn.
- Never advance a cursor past earlier in-flight work.
- Checkpoint chunk delivery monotonically and resume at the first unsent chunk.
- Preserve persisted state and database compatibility. Add and test migrations
  for schema changes.
- Keep each thread ordered while allowing independent threads and channels to
  make progress.
- Do not claim exactly-once external delivery. A provider may accept a send
  before returning an error.

### Agent backends and jobs

- Keep backend-specific flags and output parsing inside the backend adapter.
- Keep system instructions separate from user prompt content.
- Chat runs preserve the backend's permission configuration. Unattended jobs
  cannot depend on interactive approval.
- A missing backend session may rotate once and rehydrate from bounded history.
  Do not introduce unbounded prompts or retry loops.
- Validate jobs before execution and keep the durable run ledger authoritative.
- Commit a job result before notification. Recovery may resume queued work or
  delivery, but must not rerun backend work already recorded as started.

### Security

- Push uses outbound connections only. Adding a listener or webhook requires an
  explicit architecture decision.
- Apply message-type checks and allowlists before backend dispatch or attachment
  download.
- Never commit or expose credentials, personal config, message content,
  assistant identity, audit logs, databases, cursor state, or session IDs.
- Normal logs must redact content. Content auditing is explicit opt-in and
  remains sensitive.
- Runtime state and secrets stay outside the versioned assistant repository.
  Preserve path validation, symlink checks, and owner-only permissions.
- Treat every allowlisted sender as an operator of the configured backend.

## Testing and documentation

- For bugs, add a regression test for the original cause when practical.
- Cover failure and restart paths when changing persistence, cursors, sessions,
  queues, jobs, or delivery.
- Use fakes and local test servers. Automated tests must not require real
  credentials, chat data, network services, or installed agent binaries.
- Keep platform behavior explicit and preserve supported non-macOS operation.
- Update canonical documentation and focused tests when public behavior,
  configuration, or CLI output changes.
- Edit documentation sources only. Do not edit generated site output or repeat
  the same facts across several documents.
