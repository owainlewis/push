# Jobs and schedules

Jobs make Push useful while you are not in a conversation. Each job is a
user-owned Markdown runbook in the configured assistant repository's `jobs/`
directory. TOML frontmatter defines execution policy; the Markdown body is
sent verbatim to a fresh backend session.

## Create a job

Create `<assistant_root>/jobs/repo-review.md`:

```markdown
+++
version = 1
timeout = "5m"
workdir = "~/Code"
backend = "codex"
+++

Review repositories with uncommitted work. Summarize the risk and the next
useful action. Do not change files or remote state.
```

Job names are lowercase ASCII slugs made from letters, digits, and hyphens.
Files must be regular UTF-8 Markdown files directly inside the derived
`<assistant_root>/jobs` directory.
Subdirectories and symlinks are rejected.

Frontmatter fields:

| Field | Required | Meaning |
| --- | --- | --- |
| `version` | yes | Format version, currently `1` |
| `timeout` | yes | Positive duration no greater than `jobs_max_timeout` |
| `workdir` | yes | Existing working directory for the backend |
| `backend` | no | `claude`, `codex`, or `pi`; defaults to `jobs_agent`, then root `agent` |
| `triggers` | no | One or more cron trigger tables |

Unknown fields are errors. A job work directory may not overlap the assistant
root, Push config, sessions, state, database, audit, drafts, or lock paths.

## Validate and inspect jobs

```sh
push job validate --config ~/.config/push/config.toml
push job list --config ~/.config/push/config.toml
push job show repo-review --config ~/.config/push/config.toml
```

Validation reports every valid and invalid file. An invalid job is disabled
individually and does not stop messaging or other valid jobs.

## Run a job manually

```sh
push job run repo-review --config ~/.config/push/config.toml
push job runs repo-review --config ~/.config/push/config.toml
```

A manual run executes in the invoking CLI process and prints its result there.
It does not proactively message a channel. Push records and claims the run in
SQLite before starting the backend and holds a non-blocking per-job advisory
lock for the run's lifetime.

If the same job is already active, the new attempt is recorded as
`skipped_overlap`. A fresh claim can recover a stale claim only after acquiring
the released OS lock, so a live process is not reclaimed from database state
alone.

## Schedule a job

Add one or more five-field cron triggers:

```toml
[[triggers]]
id = "weekday-morning"
kind = "cron"
schedule = "0 8 * * 1-5"
timezone = "Europe/London"
enabled = true
```

Then configure a delivery destination:

```toml
[primary_delivery]
channel = "telegram"
target = "123456789"
```

Scheduling starts only when the primary destination is enabled and
allowlisted. A missing or invalid destination disables new scheduled starts
without affecting conversations or manual jobs.

Push runs at most `jobs_max_workers` scheduled jobs concurrently. It does not
catch up cron occurrences missed while offline. Daylight-saving gaps are
skipped; repeated local times run once at their first instant.

## Execution and delivery guarantees

- Every run uses a fresh backend session, without chat history.
- Jobs always inherit the backend's own permission configuration.
- Push does not retry failed or timed-out backend execution because the agent
  may have completed external side effects before failing.
- Success, failure, timeout, overlap, and delivery state are stored separately.
- Scheduled output is persisted before delivery.
- Delivery retries use the stored result and never rerun the backend.
- Queued runs and pending delivery survive restart. Interrupted execution is
  not automatically replayed.

Use `push job runs [<name>]` to inspect execution state, delivery attempts,
destination, bounded result, and error details.

## Agent-drafted jobs

A `workspace` or `inherit` chat route can propose a new job by writing one
complete runbook to its origin-specific drafts inbox. Push provides that
opaque path as an additional writable boundary, so different senders, chats,
and topics cannot claim each other's drafts.

Push validates the filename, complete contents, work directory, timeout,
backend, trigger, symlink status, and protected paths before presenting the
proposal. It sends the exact draft to the originating chat with Approve and
Reject choices.

Approval is bound to the channel, sender, chat, thread or topic, and the exact
SHA-256 revision shown. Any edit after presentation invalidates approval. A
valid approved revision is installed atomically without replacing an existing
job. Rejection leaves the proposal inactive.

!!! warning

    Jobs use backend-inherited permissions, not chat permission profiles. A
    headless backend cannot ask for approval interactively, so configure its
    unattended permissions carefully and keep job work directories narrow.
