# Jobs as Runbooks with Separate Triggers

**Status:** Draft

**Author:** Owain Lewis  **Date:** 2026-07-12

## Summary

Push jobs are user-owned Markdown runbooks stored under `~/.push/jobs/`. A job
contains one instruction body plus execution policy such as its permission
profile, timeout, working directory, backend override, and delivery target.
Manual and cron starts are triggers attached to the same job rather than
different job types. Every run uses a fresh backend session. Once the scheduler
and ledger ship, Push records each run in SQLite before execution, allowing
scheduling and delivery to remain durable without turning Push into an agent
runtime.

## Goals

- Keep jobs readable, reviewable, and editable without a database UI.
- Use one execution path for manual and scheduled runs.
- Require permissions, timeouts, working directories, and delivery behavior to
  be explicit and validated before execution.
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

- Push remains one local process with no inbound server port.
- Claude Code and Codex have different permission controls, but a job selects
  only a named Push permission profile.
- Scheduled work can have external side effects, so duplicate execution is more
  dangerous than skipping a missed run.
- Job files and `SOUL.md` are user-owned inputs. Runtime and ledger state remain
  gateway-owned.
- Existing installations without a jobs directory continue to start normally.

## Proposed design

### Job format and identity

Each job is one UTF-8 file at `~/.push/jobs/<job-name>.md`. The file stem is the
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
- `backend`: optional `claude` or `codex` override; otherwise use the configured
  jobs default.
- `delivery`: required `primary` or `none`.
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
delivery = "none"
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
delivery = "primary"

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

Cron expressions use five fields with minute granularity. Each cron trigger
has a job-local slug id, an IANA timezone, and an explicit enabled flag. IANA
timezone rules determine daylight-saving transitions: a nonexistent local
time does not run, while an ambiguous repeated local time runs once at the
first matching instant. Trigger ids make schedule edits inspectable without
changing the job's identity.

### Validation and capability ceiling

Push validates all installed jobs during startup preflight and through a
read-only `push job validate` command. If any installed job is invalid, startup
reports every error and stops before polling or scheduling. A missing jobs
directory and an empty directory are valid. Draft files outside the jobs
directory are not installed jobs and do not affect startup.

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

Scheduled jobs must use `delivery = "primary"` and require a validated primary
destination. Manual-only jobs may use `none`; their result is always printed to
the invoking terminal. Secrets are referenced through the selected profile or
process environment policy, never embedded as special job fields.

### Execution

Manual and cron triggers create the same immutable run request containing a
snapshot hash of the validated job, trigger information, scheduled time,
backend, permission profile, timeout, canonical work directory, and delivery
target. Editing the job affects later runs but not an already claimed run.

Each run starts a fresh Claude or Codex session. Push supplies the composed
`SOUL.md` instructions at instruction priority and the job body as the current
request. Jobs receive neither backend conversation history nor canonical chat
history. Their backend session ids are not saved for reuse. The working
directory is stable across runs, so filesystem state may persist even though
conversation state does not.

Only one run of a given job may be queued or running at a time. A manual start
or scheduled occurrence that loses this atomic claim is recorded as
`skipped_overlap`; it does not wait in an unbounded queue. Different jobs may
run concurrently subject to a configured global worker limit.

Once scheduling ships, the long-running gateway is the sole executor and owns
the global worker limit. `push job run` is a short-lived local producer: it
validates the requested job, inserts a manual `queued` run into the same SQLite
ledger, and waits for the gateway to complete it. If no gateway holds the local
runtime lock, the CLI temporarily acquires that lock and executes its own run
through the same claim path. A second gateway process is rejected. This local
coordination does not expose a network listener or create a distributed queue.

Push does not catch up cron occurrences missed while it was stopped. On
startup it schedules the next future occurrence. A unique ledger key over job,
trigger id, and scheduled instant prevents the same occurrence from being
claimed twice after a restart. Push also does not retry failed or timed-out
agent execution automatically because a backend may have completed side
effects before failing. The operator can start a new manual run instead.

### Run ledger and delivery

The SQLite ledger records each run before backend execution. Its minimum
logical fields are:

- run id, job name, job snapshot hash, and trigger kind/id;
- scheduled, queued, started, and finished timestamps;
- backend, permission profile, timeout, canonical work directory, and delivery
  target;
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
the read-only run log. On restart, valid `queued` rows remain eligible for the
gateway to claim because backend execution has not begun. Recovery marks an
interrupted `running` row as failed with an interruption reason and releases
its overlap claim. It resumes pending delivery attempts, but never backend
execution. A queued row whose job snapshot is no longer installed remains
cancelled with a diagnostic rather than running different content.

### Commands and ownership

The first runtime exposes read-only listing and inspection plus explicit
execution:

- `push job validate`
- `push job list`
- `push job show <name>`
- `push job run <name>`
- `push log [--job <name>]`

Push is the only writer to the run ledger: the gateway owns execution state and
the CLI may insert a manual queued request. Installed job files remain
operator-owned. A later draft workflow may let an agent write under
`~/.push/drafts/`, but activation must revalidate and atomically install the
exact approved revision into `~/.push/jobs/`.

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
- Stored outputs may contain sensitive data. Results and errors must be
  bounded, locally permissioned, and excluded from normal logs.

## Rollout

1. Ship parsing, validation, listing, inspection, and one-shot manual execution
   without enabling a scheduler. This transitional command acquires a local
   per-job lock, prints its result, and has no durable run ledger.
2. Add the SQLite run ledger, make the gateway the sole long-running executor,
   route manual requests through durable claims, and expose recent runs through
   `push log`. From this step onward every run is recorded before execution.
3. Add cron evaluation and primary-channel delivery behind that same validated
   execution path.
4. Enable agent-authored drafts only after approval and filesystem isolation
   rules are implemented.

Backout disables scheduling and manual execution while preserving job files
and ledger history. Existing messaging, cursor, and backend-session behavior
does not depend on jobs.

## Open questions

None required before implementing the first manual jobs runtime. Review may
revise the chosen file format, delivery rule, or scheduling semantics.

## Decision
