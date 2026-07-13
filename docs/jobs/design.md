# Jobs as Runbooks with Separate Triggers

**Status:** Draft

**Author:** Owain Lewis  **Date:** 2026-07-12

## Summary

Push jobs are user-owned Markdown runbooks stored under
`<assistant_root>/jobs/`. A job
contains one instruction body plus execution policy such as its permission
profile, timeout, working directory, and backend override.
Manual and cron starts are triggers attached to the same job rather than
different job types. Every run uses a fresh backend session. Once the scheduler
ships, it uses the same SQLite claim and execution path as manual runs,
allowing scheduling and delivery to remain durable without turning Push into
an agent runtime.

## Goals

- Keep jobs readable, reviewable, and editable without a database UI.
- Use one execution path for manual and scheduled runs.
- Require permissions, timeouts, and working directories to be explicit and
  validated before execution.
- Prevent jobs from exceeding the operator-configured permission ceiling.
- Make scheduling safe across restarts, overlap, timezones, and delivery
  failures.
- Preserve Push's existing polling-only architecture and backend boundary.

## Non-goals

- Event, webhook, calendar, or email triggers.
- Dependencies or workflows between jobs.
- Automatic retries of agent execution.
- Reusing conversation or backend sessions.
- Allowing jobs to define tools or raw backend permission flags.
- Agent-authored drafts or approval flows in the first jobs runtime.
- A distributed queue shared across multiple gateway processes or machines.

## Constraints

- Push has one long-running gateway process with no inbound server port.
  Commands may run as short-lived local processes against the same SQLite
  store.
- Claude Code, Codex, and Pi have different permission controls, but a job selects
  only a named Push permission profile.
- Scheduled work can have external side effects, so duplicate execution is more
  dangerous than skipping a missed run.
- Job files, `SOUL.md`, and `context/` belong to the Git-versioned assistant
  repository. Runtime and ledger state remain Push-owned outside it.
- Existing installations without a jobs directory continue to start normally.

## Proposed design

### Job format and identity

Each job is one UTF-8 file at
`<assistant_root>/jobs/<job-name>.md`. The file stem is the
stable job name and must be a lowercase ASCII slug containing letters, digits,
and hyphens. Subdirectories, symlinks, duplicate canonical paths, and names
that differ only by case are rejected. Renaming a file creates a new job
identity while preserving old ledger rows under the old name.

The file uses TOML frontmatter between `+++` delimiters followed by a non-empty
Markdown instruction body. Version 1 supports these fields:

- `version`: required and equal to `1`.
- `permission_profile`: required named profile from Push configuration.
- `timeout`: required positive duration, capped by the configured job maximum.
- `workdir`: required fixed directory, expanded and canonicalised at validation.
- `backend`: optional `claude`, `codex`, or `pi` override; otherwise use the configured
  jobs default.
- `triggers`: optional list of separately identified trigger definitions.

Unknown fields are errors. The Markdown body is sent as the current request
without template expansion, shell interpolation, or implicit user input. Every
valid job can be started manually; trigger entries add other ways to start it.

Manual-only example:

```markdown
+++
version = 1
permission_profile = "research-readonly"
timeout = "10m"
workdir = "~/Code"
backend = "codex"
+++

Review the repositories in this directory. Summarise branches with uncommitted
work and identify pull requests that appear ready for review. Do not change any
files or remote state.
```

Scheduled example:

```markdown
+++
version = 1
permission_profile = "calendar-readonly"
timeout = "5m"
workdir = "~/.push/workspaces/morning-agenda"

[[triggers]]
id = "weekday-morning"
kind = "cron"
schedule = "0 8 * * 1-5"
timezone = "Europe/London"
enabled = true
+++

Prepare today's agenda. List fixed appointments, useful preparation, and the
three most important open loops. Keep the reply below 500 words.
```

Cron expressions use five fields with minute granularity. Six-field expressions
and aliases such as `@daily` are rejected. Each cron trigger has a job-local
slug id, an IANA timezone, and an explicit enabled flag. IANA timezone rules
determine daylight-saving transitions: a nonexistent local time does not run,
while an ambiguous repeated local time runs once at the first matching instant.
Trigger ids make schedule edits inspectable without changing the job's
identity.

### Validation and capability ceiling

Push validates all installed jobs during startup preflight and through a
read-only `push job validate` command. Invalid jobs are disabled, reported
together, and make the validation command return non-zero, but they do not
prevent channel polling or valid jobs from starting. A missing jobs directory
and an empty directory are valid. Draft files outside the jobs directory are
not installed jobs and do not affect startup.

The running gateway checks job files before each schedule evaluation. A changed
file replaces its prior definition only after full validation; an invalid
change disables new runs of that job and produces an actionable local error
without stopping unrelated messaging or jobs. Every manual or scheduled claim
rereads and validates the exact file bytes, then records their snapshot hash.
A scheduled occurrence is cancelled if its trigger no longer exists in that
validated snapshot. Immediately before spawning a backend, Push resolves the
work directory again and rechecks it against the permission profile so a path
replacement is likely to be detected. This path-based check does not eliminate
a replacement race between validation and child startup; backend sandboxing
and OS permissions remain the enforcement boundary.

The selected permission profile must exist and be included in the configured
set of profiles allowed for jobs. This allow-set is the capability ceiling: a
job may select one approved name but cannot define permissions, backend flags,
or a new profile. Push resolves the profile to backend controls only after the
job passes validation. The timeout must not exceed the configured jobs maximum,
and the canonical work directory must meet the restrictions of the permission
profile.

Notification behavior follows the trigger rather than job metadata. A manual
run prints its result to the invoking terminal and does not send a message. A
scheduled run sends its success, failure, or timeout result to the configured
primary destination. If no primary destination is valid, scheduled triggers
remain disabled with an actionable error while the job remains manually
runnable. Secrets are referenced through the selected profile or process
environment policy, never embedded as special job fields.

### Execution

Manual and cron triggers create the same immutable run request containing a
snapshot hash of the validated job, trigger information, scheduled time,
backend, permission profile, timeout, and canonical work directory. Editing the
job affects later runs but not an already claimed run.

Each run starts a fresh Claude Code, Codex, or Pi session. Push supplies the composed
`SOUL.md`, resolved assistant paths, and gateway policy at instruction priority
and the job body as the current request. Jobs receive neither backend
conversation history nor canonical chat history. Their backend session ids are
not saved for reuse. The working
directory is stable across runs, so filesystem state may persist even though
conversation state does not.

Before backend execution, both manual and scheduled starts attempt a
non-blocking OS advisory lock for the job under `~/.push/run/locks/`. The
winning process holds that lock until its run finishes. It then uses one SQLite
transaction to check queued and running state, record the run, and claim the
job. A start that loses the file lock or database claim is recorded as
`skipped_overlap`; it does not wait in an unbounded queue. Advisory locks are
the local liveness signal and SQLite is the durable history and uniqueness
boundary.

`push job run` executes directly in its CLI process after winning the claim.
The long-running gateway executes scheduled claims and applies its configured
worker limit to scheduled work. Explicit operator-run CLI jobs do not consume
that scheduler limit, but they obey the same per-job overlap rule. This avoids
a local request queue, daemon handoff, global runtime lock, or network listener
while keeping cross-process claims durable. The jobs directory and lock
directory must be on a local filesystem that provides process-scoped advisory
locks.

Push does not catch up cron occurrences missed while it was stopped. On
startup it schedules the next future occurrence. A unique ledger key over job,
trigger id, and scheduled instant prevents the same occurrence from being
claimed twice after a restart. Push also does not retry failed or timed-out
agent execution automatically because a backend may have completed side
effects before failing. The operator can start a new manual run instead.

### Run ledger and delivery

The SQLite ledger records each run before backend execution. Its minimum
logical fields are:

- run id, job name, job snapshot hash, trigger kind/id, and claim owner kind;
- scheduled, queued, started, and finished timestamps;
- backend, permission profile, timeout, canonical work directory, and resolved
  notification destination when applicable;
- lifecycle state and a bounded result or error reference;
- delivery state, attempt count, last attempt time, and delivery error.

Run lifecycle states are `queued`, `running`, `succeeded`, `failed`,
`timed_out`, `skipped_overlap`, and `cancelled`. Delivery is tracked separately
as `not_requested`, `pending`, `delivered`, or `failed`. A run can therefore
succeed while its delivery fails.

Successful output is stored before delivery. Failures and timeouts produce a
bounded diagnostic result and follow the same delivery policy. Delivery may be
retried up to three times with backoff using the already stored result; it
never reruns the agent. Exhausted delivery remains `failed` and is visible in
the read-only run log. Manual runs move directly to `running` in their claim
transaction and are never handed to the gateway. On restart, valid scheduled
`queued` rows remain eligible for the gateway because backend execution has not
begun. Push only marks a `running` row interrupted after it can acquire that
job's advisory lock, proving no local executor still holds it. If the lock is
held, recovery leaves the live run unchanged. The same stale-claim check runs
before each new claim, so a crashed CLI cannot block a job indefinitely even
while the gateway remains up. Recovery resumes pending delivery attempts, but
never backend execution. A queued row whose job snapshot is no longer
installed remains cancelled with a diagnostic rather than running different
content.

### Commands and ownership

The first runtime exposes read-only listing and inspection plus explicit
execution:

- `push job validate`
- `push job list`
- `push job show <name>`
- `push job run <name>`
- `push job runs [<name>]`

Push is the only writer to the run ledger. The CLI owns the manual run it
claims, and the gateway owns scheduled runs. Installed job files remain
operator-owned.

### Agent-authored draft extension

Route agents may write proposals only in an opaque, exact-origin inbox under
`~/.push/drafts/`, which Push adds as a Push-owned writable root for contained
workspace profiles. Concurrent channels, senders, chats, and topics therefore
cannot claim each other's files. Full-access
routes and jobs are rejected because backend bypass modes cannot prevent direct
writes to Push-owned files. Job work directories may not overlap the assistant
root, configuration, session, draft, installed-job, lock, audit, or database
paths.

After a route run completes, fails, times out, or resumes from a persisted
outbound reply, Push reconciles unrecorded revisions in that origin's inbox.
It validates each full runbook and sends its complete contents to the
originating allowlisted channel. The
following `ask_user` question binds Approve and Reject to that channel, sender,
chat, thread or topic, and exact SHA-256 revision. SQLite stores the exact bytes
and proposer identity with the question. Approval rereads and revalidates the
draft and current permission ceiling. A changed revision is invalidated; a
valid stored revision is staged inside the derived assistant `jobs/` directory
and installed with an atomic
no-clobber link. Rejection leaves the draft inactive. Proposal and approver
identities, terminal status, and errors remain durable across restart, while
duplicate answers cannot repeat installation.

## Alternatives and tradeoffs

### Separate manual and scheduled job types

This duplicates parsing, execution, permissions, and delivery policy. Treating
manual and cron as triggers keeps one testable execution path and leaves room
for later event triggers.

### Store jobs in SQLite

Database storage would simplify atomic updates but make jobs harder to review,
version, and edit. Markdown keeps intent legible while SQLite stores mutable
run state.

### Put triggers in separate files

Separate trigger files allow independent ownership but introduce joins,
orphaned references, and more filesystem races. Embedded trigger records are
still logically separate from the runbook body and are sufficient for the
single-user first version.

### Reuse backend sessions or chat history

Reuse could make recurring jobs more conversational, but it makes results
depend on hidden state and complicates recovery. Fresh sessions plus a stable
working directory provide repeatability without discarding useful filesystem
artifacts.

### Catch up missed schedules and retry failed runs

Catch-up and automatic execution retries improve eventual completion but can
duplicate external side effects after downtime or ambiguous backend failure.
The first version prefers an inspectable skipped or failed run and an explicit
manual retry.

### Send manual runs through the gateway

A daemon-owned queue would provide one global worker limit, but it requires IPC
or polling, waiting semantics, daemon availability handling, and a second
execution mode when the gateway is stopped. Direct CLI execution with a shared
SQLite claim and process-scoped advisory lock preserves per-job safety with
fewer moving parts.

## Risks

- A permissive profile can still grant broad machine access. Named profiles,
  the job allow-set, fixed work directories, and startup validation limit this
  risk but do not make arbitrary agent execution safe.
- A job body or file in its work directory may contain untrusted instructions.
  The permission profile remains the enforcement boundary; `SOUL.md` and the
  job body stay at distinct instruction levels.
- A local process with permission to replace the work directory can race the
  final path check. The first version treats this as a residual local-user risk
  and does not claim filesystem-identity pinning.
- System clock or timezone database changes can alter future occurrences.
  Persisting scheduled instants and trigger identities keeps past runs
  auditable.
- A process crash can leave external side effects without a successful result.
  Interrupted runs are never automatically replayed.
- Advisory locking is a local-filesystem assumption. Push rejects a lock setup
  it cannot verify rather than relying on SQLite state alone for executor
  liveness.
- Stored outputs may contain sensitive data. Results and errors must be
  bounded, locally permissioned, and excluded from normal logs.

## Rollout

1. Ship parsing, validation, listing, inspection, direct manual execution, the
   SQLite run ledger, and `push job runs` without enabling a scheduler. Every
   manual run is claimed and recorded before execution.
2. Add cron evaluation and primary-channel delivery behind the same
   transactional claim and execution path.
3. Enable agent-authored drafts only after approval and filesystem isolation
   rules are implemented.

Backout disables scheduling and manual execution while preserving job files
and ledger history. Existing messaging, cursor, and backend-session behavior
does not depend on jobs.

## Open questions

None required before implementing the first manual jobs runtime. Review may
revise the chosen file format, delivery rule, or scheduling semantics.

## Decision
