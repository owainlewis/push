# Push Architecture

## 1. Executive Summary

Push is a local Rust gateway that turns Claude Code, Codex, or Pi into a
personal assistant available through iMessage, Telegram, and Slack. It also
runs validated Markdown jobs on a schedule. Push owns messaging, routing,
durable history, scheduling, recovery, and delivery. The selected agent owns
reasoning, tools, skills, permissions, MCP servers, and authentication.

The central boundary is:

```text
message channel -> Push gateway -> agent CLI -> Push gateway -> message channel
```

Push is one binary and one local process. It opens no inbound server port.
iMessage reads the local Messages database, Telegram uses outbound HTTPS long
polling, and Slack uses an outbound Socket Mode WebSocket.

The main architectural rule is that agent runtimes are disposable adapters.
Push does not implement an agent loop, tool runner, plugin system, or model
client. It gives the selected agent a request, receives a final answer, stores
that answer, and delivers it.

### System Architecture

```text
┌──────────────────────────────────────────────────────────────────────────┐
│                              INPUTS                                      │
│                                                                          │
│  iMessage chat.db       Telegram Bot API       Slack Socket Mode         │
│        │                     │                       │                    │
│        └──────────── poll / receive outbound connections ───────────────┘│
└────────────────────────────────────┬─────────────────────────────────────┘
                                     │
                                     ▼
┌──────────────────────────────────────────────────────────────────────────┐
│                           push process                                   │
│                                                                          │
│  ┌──────────────┐  ┌───────────────┐  ┌───────────────────────────────┐ │
│  │ Channel      │  │ GatewayGroup  │  │ Scheduler                     │ │
│  │ contract     │─▶│ per-channel   │  │ validated jobs + delivery     │ │
│  └──────────────┘  │ loops         │  └───────────────────────────────┘ │
│                    └───────┬───────┘                                    │
│                            ▼                                            │
│                    ┌───────────────┐                                    │
│                    │ per-thread    │                                    │
│                    │ worker queues │                                    │
│                    └───────┬───────┘                                    │
│                            ▼                                            │
│  ┌───────────────────────────────────────┐  ┌──────────────────────────┐ │
│  │ push.db                               │  │ audit.jsonl              │ │
│  │ conversations, jobs, delivery,        │  │ redacted operational     │ │
│  │ channel cursors, backend sessions     │  │ events                   │ │
│  └───────────────────────────────────────┘  └──────────────────────────┘ │
└────────────────────────────────────┬─────────────────────────────────────┘
                                     │ subprocess
                                     ▼
┌──────────────────────────────────────────────────────────────────────────┐
│                          AGENT RUNTIMES                                  │
│                                                                          │
│       claude -p              codex exec              pi --print          │
└────────────────────────────────────┬─────────────────────────────────────┘
                                     │ final response
                                     ▼
                          originating channel only
```

### Module Dependency Hierarchy

Push is a single crate. Module boundaries, not separate packages, keep
responsibilities isolated.

```text
main
 ├── paths + config + doctor + assistant init
 ├── gateway
 │    ├── channel
 │    │    ├── imessage
 │    │    ├── telegram
 │    │    └── slack
 │    ├── agent
 │    │    ├── claude
 │    │    ├── codex
 │    │    └── pi
 │    ├── store
 │    ├── history
 │    ├── audit
 │    ├── prompt composer
 │    ├── voice
 │    └── approval answer handling
 └── jobs
      ├── catalog + validation
      ├── scheduler
      ├── ledger in push.db
      ├── agent runner
      └── evaluator
```

The gateway coordinates the modules. Channel implementations do not call agent
backends directly, and backend adapters do not know about messaging providers.

---

## 2. Runtime Contracts

Push divides ownership among three systems:

| Owner | Responsibilities |
| --- | --- |
| Push runtime | channels, allowlists, routing, scheduling, canonical history, cursors, session mappings, audit, retries, and delivery |
| Assistant repository | user-owned `SOUL.md`, shared instructions, context, evals, jobs, optional project skills, and the Push-managed capability skill |
| Agent runtime | reasoning, models, tools, global skills, MCP, authentication, sandbox, and interactive permissions |

### Channel Contract

Every built-in channel implements the closed `ChannelContract` in
[`src/channel.rs`](src/channel.rs). The `Channel` enum provides static
dispatch. This is an internal compile-time boundary, not a dynamic plugin API.

A channel owns:

- polling after an opaque monotonic cursor;
- first-start cursor discovery so old messages are skipped;
- sender, chat, group, content, and loopback acceptance rules;
- a stable channel-qualified thread key;
- the exact reply target;
- routing aliases for parent and legacy thread identities;
- outbound chunking, formatting, and provider limits;
- typing indicators;
- voice download and upload;
- delivery timeouts and retries;
- worker shutdown grace.

The shared gateway never branches on provider names for polling, routing,
worker ordering, reply recovery, or shutdown.

### Agent Contract

The gateway calls backends through the boundary in
[`src/agent.rs`](src/agent.rs):

```rust
Request {
    session_id,
    is_new,
    work_dir,
    instructions,
    prompt,
}

RunOutput {
    reply,
    session_id,
}
```

`RunError` separates timeouts, missing sessions, and other failures so the
worker can recover or produce the correct stored fallback response.

For conversation and job runs, the prompt composer owns both string fields.
`instructions` always contains the same ordered system sections: Push-owned
base policy, user-owned system identity, and Push-resolved workspace paths.
`prompt` contains one fresh message context section. Claude Code receives
`instructions` through its system-prompt flag, Codex through developer
instructions, and Pi through its system-prompt flag. All three receive
`prompt` through their ordinary prompt input. Restricted evaluator runs keep
their separate evaluator-specific instruction contract.

### CLI Automation Contract

Human-readable CLI output remains the default. Read-only automation commands
can select a versioned JSON envelope with `--json`. Success writes one document
to stdout. Failure writes one structured error to stderr with a stable category
and exit code. JSON mode does not initialize tracing, print progress, or mix
tables into stdout.

Diagnostic JSON reports credential presence without values. Job run history
reports state and content-presence flags without stored result, evaluation, or
error text. Commands that mutate runtime state reject JSON mode because Push
cannot always know whether an interrupted external mutation is safe to retry.
Resolved-path output reads every Push-owned runtime location from the loaded
`PushPaths` owner. It adds the selected config and user-owned assistant paths
without reconstructing runtime defaults. Although job runs now share `push.db`
with channel cursors and backend sessions, their JSON projection queries only
job-run rows and never exposes co-located session IDs or conversation content.

### Durable State Contract

Push uses separate stores because they have different update and query needs:

| Store | Purpose |
| --- | --- |
| `$PUSH_HOME/state.json` | retained legacy cursor and session source for one-time migration |
| `$PUSH_HOME/push.db` | transactional conversation, delivery, approval compatibility, job-run history, channel cursors, and channel-qualified backend sessions |
| `$PUSH_HOME/state.json.slack-inbox.db` | Slack Socket Mode inbox committed before envelope acknowledgement |
| `$PUSH_HOME/audit.jsonl` | append-only operational audit events |
| `$PUSH_HOME/run/` | advisory job locks |
| `$PUSH_HOME/cache/` | disposable agent handoff files |
| assistant Git repository | user-owned identity, context, evals, jobs, and project resources |

`state_path` remains a compatibility input for one-time migration. Push never
uses the legacy JSON file as a live source of truth after its migration marker
commits. The Slack inbox remains separate because its rows are an
acknowledgement queue, not gateway cursor or session state.

Runtime databases, secrets, locks, sessions, and logs must stay outside the
assistant repository.

[`src/paths.rs`](src/paths.rs) is the single owner of runtime locations.
`PUSH_HOME` selects the root and defaults to `~/.push`. Config, startup, jobs,
history, audit, and channels consume the resolved `PushPaths` value instead of
reconstructing filenames. The config, database, state, audit, run-lock, inbox,
and cache locations are derived from that root. Explicit legacy state,
database, audit, and run-lock settings replace only their matching derived
locations. `assistant_root` remains independent, and overlap with the runtime
root fails closed.

---

## 3. Process Lifecycle

The entry point is [`src/main.rs`](src/main.rs).

### Step 1: Parse the Command

The binary supports the long-running gateway plus `init`, `doctor`, `reload`,
and job management commands. Commands that need configuration use `--config`
when present. `--config` selects a file without changing `PUSH_HOME`;
otherwise the config is `$PUSH_HOME/config.toml`.

### Step 2: Load and Validate Configuration

[`src/config.rs`](src/config.rs) parses TOML, flattens supported provider
sections, expands paths, migrates compatible legacy layout, and rejects:

- unsupported channels or backends;
- missing allowlists and credentials;
- removed Push-owned agent permission settings;
- unsafe overlaps between the assistant repository and runtime state;
- unsafe job work directories;
- provider tokens stored inside the assistant repository;
- invalid routes, durations, and primary delivery targets.

`push doctor` adds environment checks for agent binaries, database access,
channel credentials, local paths, job validity, and optional voice support.
The gateway runs the required preflight checks before starting.

### Step 3: Build Shared State

`GatewayGroup::new` opens one shared transactional `Store` and one shared
`History` connection to `push.db`, one runner per required backend, and one
serialized audit-log lock. Opening the store also performs any one-time legacy
`state.json` migration before polling can begin. It then creates one `Gateway`
for each enabled channel.

Each gateway owns its own:

- channel adapter;
- polling loop;
- acknowledgement tracker;
- per-thread queue map;
- worker handles.

### Step 4: Start Channel and Scheduler Tasks

`GatewayGroup::run` starts:

1. one task per enabled channel;
2. one scheduler task;
3. one coordinator waiting for shutdown or channel termination.

The scheduler starts in full mode only when `primary_delivery` resolves to an
enabled, allowlisted target. Otherwise it disables new cron enqueues while
continuing recovery of previously queued runs and persisted delivery work.

### Step 5: Establish Initial Cursors

On the first run for a channel, Push asks the adapter for its latest cursor and
persists it. Existing backlog is skipped. Later starts resume from the stored
cursor.

### Step 6: Poll Until Shutdown

Each gateway polls on its configured interval. Missed timer ticks are skipped,
so a slow poll does not create a burst of catch-up polls. A failed provider
poll is logged and retried on the next interval.

### Step 7: Drain

SIGINT or SIGTERM broadcasts shutdown. Pending poll futures are dropped, queue
senders are closed, accepted per-thread work drains for the channel's grace
period, and remaining workers are aborted. The scheduler also gets a bounded
grace period before releasing delivery claims and recovering interrupted work.

---

## 4. Message Pipeline

The main pipeline lives in [`src/gateway/mod.rs`](src/gateway/mod.rs) and
[`src/gateway/worker.rs`](src/gateway/worker.rs).

```text
 1. POLL                 read messages after durable channel cursor
 2. DEDUPLICATE          skip old or already in-flight channel rows
 3. AUTHORIZE            apply provider allowlist and message rules
 4. ANSWER RESOLUTION    consume a matching durable numbered answer, if any
 5. HISTORY INSERT       persist accepted inbound message in push.db
 6. ROUTE                select backend by exact thread, parent, channel, default
 7. ENQUEUE              place message on its per-thread bounded queue
 8. PREPARE              resolve session, workspace, identity, voice, and images
 9. COMPOSE              frame system sections and untrusted fresh context
10. RUN BACKEND          invoke the selected agent CLI with a timeout
11. HISTORY INSERT       persist generated outbound response
12. SESSION SAVE         persist backend-owned session ID when required
13. DELIVER              send durable chunks through the originating channel
14. ACK CURSOR           advance across the contiguous completed row prefix
```

### Authorization Before Work

Channel `accept` runs before transcription, routing, backend dispatch, or
delivery. Rejected, group, looped-back, empty, unsupported, or non-allowlisted
messages are audited and completed without reaching an agent.

### Persist Before Dispatch

Accepted inbound messages are inserted in `push.db` before local commands or
backend execution. `channel_event_id` is unique, so a retried provider event
resolves to the same canonical inbound row.

### Routing Order

Backend selection has this precedence:

1. exact thread or topic route;
2. parent Telegram private-chat route for a topic;
3. channel route;
4. root `agent`.

Session storage uses the final channel-qualified thread key. A backend change
for the same thread starts a new session instead of resuming another runtime's
session.

### Per-Thread Queue

Each thread gets an MPSC queue with depth 32. One worker drains it in order.
Different threads and channels may run concurrently. If a queue is full, Push
stores and sends a gateway-authored retry message rather than silently dropping
the input. If a worker closes unexpectedly, retained jobs are moved to a
replacement worker.

### Agent Preparation

For every turn, the worker:

1. canonicalizes `assistant_root`;
2. validates the optional `context/` boundary;
3. loads the user-owned `SOUL.md` identity;
4. resolves the assistant, context, evals, jobs, and working paths;
5. resolves or creates the backend session mapping.

The prompt composer renders these sections in order:

| Section | Owner and precedence | Transport |
| --- | --- | --- |
| Push-owned base policy | Push; highest among composed sections | native system or developer instructions |
| User-owned system identity | `SOUL.md`; below Push policy | native system or developer instructions |
| Resolved workspace paths | Push-owned path data | native system or developer instructions |
| Fresh message context | untrusted current-turn data | ordinary prompt input |

The base policy is limited to Push delivery, instruction-boundary, context,
identity/eval editing, and job-validation invariants. `SOUL.md` and path values
are JSON strings so delimiter-like text cannot create a new section.
Push does not inject every context file. The selected backend decides what to
inspect in the assistant repository.

### Session Rehydration

Every turn's fresh context is a JSON object containing only the channel,
channel-qualified thread, reply-delivery mode, current user message, and
optional bounded history. Sender text, handles, message content, history, and
provider metadata are untrusted prompt data. Normal resumed turns have an empty
history array. Fresh sessions include at most 20 recent messages from the exact
channel-qualified conversation. Each historical message is capped at 4 KiB and
the history array at 16 KiB.

If a backend reports that a resumed session is missing, Push rotates the stored
session and retries once as a fresh, rehydrated session.

### Persist Before Delivery

Backend, command, timeout, interruption, and failure responses are inserted as
outbound messages before channel delivery. Generation state and delivery state
are separate. Worker-managed replies retry the stored outbound record, and
restart recovery resends it without rerunning the agent.

The queue-full fallback is a deliberate exception. It records the reply, makes
one delivery attempt, and completes the input row even if that attempt fails.
This prevents an overloaded thread from blocking its channel cursor.

Each successful outbound chunk advances a durable chunk checkpoint. A crash
between provider acceptance and checkpoint persistence may create an
at-least-once duplicate, because provider sends and local SQLite commits cannot
form one atomic transaction.

### Cursor Advancement

Poll rows can finish out of order because separate thread workers run in
parallel. `AckState` tracks in-flight and completed row IDs. Push advances the
channel cursor only across a contiguous completed prefix, so unfinished older
work is never skipped.

---

## 5. Channel System

### iMessage

Files: [`src/imessage/`](src/imessage/)

- Reads the macOS Messages `chat.db` database.
- Accepts one-to-one self chats and explicitly allowed senders.
- Uses local row IDs as the monotonic cursor.
- Sends through `osascript`.
- Keeps legacy unprefixed route and session aliases for migration.
- Supports text only.

### Telegram

File: [`src/telegram.rs`](src/telegram.rs)

- Uses outbound Bot API long polling.
- Authorizes stable numeric user and private-chat IDs.
- Preserves private-chat topic IDs in thread keys and reply targets.
- Splits rich Markdown within Telegram's 4,096-character limit.
- Refreshes typing activity during long runs.
- Downloads voice notes only after allowlist acceptance.
- Downloads supported images only after allowlist acceptance, validates their
  signatures, and removes private handoff files after the Codex turn.
- Sends text and optional generated voice replies.

### Slack

File: [`src/slack.rs`](src/slack.rs)

- Connects with an outbound authenticated Socket Mode WebSocket.
- Verifies the workspace with `auth.test`.
- Authorizes stable member IDs.
- Accepts app direct messages and replies in the exact originating Slack
  message thread.
- Commits accepted envelopes to a dedicated SQLite inbox before acknowledging
  them to Slack.
- Persists only file IDs and safe size and MIME type hints, then resolves and
  downloads supported images with the bot token after allowlist acceptance.
- Applies the provider-neutral image count, byte, and signature checks before
  passing private temporary paths to Claude Code, Codex, or Pi.
- Continues receiving while the gateway processes earlier messages.
- Deduplicates stable Slack `event_id` values.

### Adding a Channel

A new channel requires:

1. one `ChannelContract` implementation;
2. one `Channel` and `ChannelKind` variant;
3. configuration validation and doctor checks;
4. provider-specific contract and integration tests;
5. documentation for identity, allowlists, limits, and failure behavior.

It should not require provider-name branches in the shared gateway or worker.

---

## 6. Agent Adapters

### Claude Code

File: [`src/claude.rs`](src/claude.rs)

| Operation | Invocation |
| --- | --- |
| New chat | `claude -p --session-id <uuid>` |
| Resumed chat | `claude -p --resume <uuid>` |
| Instructions | `--append-system-prompt <composed system sections>` |
| Images | base64 content blocks over `--input-format stream-json` stdin |
| Unattended job | `--permission-mode bypassPermissions` |
| Evaluator | safe mode, no tools, no MCP, no Chrome, no session persistence |

Push chooses Claude's initial session ID and marks it started before execution
so a partial create failure is not retried as another create. Text-only turns
keep the single-result JSON command contract. Image turns use one streaming JSON
user message and parse the final result event without putting image data in
process arguments.

### Codex

File: [`src/codex.rs`](src/codex.rs)

| Operation | Invocation |
| --- | --- |
| New chat | `codex exec --json` |
| Resumed chat | `codex exec resume <thread-id> --json` |
| Instructions | `-c developer_instructions=<composed system sections>` |
| Images | repeated `--image <private-temporary-path>` on new and resumed chats |
| Unattended job | full access with approval prompts disabled |
| Evaluator | read-only, ephemeral, tools and project instructions disabled |

Codex owns its thread ID. Push reads `thread.started.thread_id` from JSONL and
stores it after the first successful turn.

### Pi

File: [`src/pi.rs`](src/pi.rs)

| Operation | Invocation |
| --- | --- |
| New chat | `pi --print --mode json` |
| Resumed chat | `pi --print --mode json --session <session-id>` |
| Instructions | `--append-system-prompt <composed system sections>` |
| Images | repeated `@<private-temporary-path>` arguments on new and resumed chats |
| Unattended job | `--approve` only when the project resources are trusted |
| Evaluator | no approval, tools, extensions, skills, templates, context, or session |

Pi owns its session ID and reports it in the JSON event stream. Prompts are
written to stdin so large requests do not depend on command-line argument
limits.

### Project Skill Contract

`push init` installs one canonical capability at `skills/push/`. Relative
directory links expose it at `.agents/skills/push` for Codex and Pi and at
`.claude/skills/push` for Claude Code. This matches each runtime's project
discovery rules without copying instruction bodies or making Pi discover the
same skill twice.

The canonical directory contains a versioned checksum manifest. Initialization
creates missing provider directories and links idempotently. It refreshes an
older skill only when the installed content still matches its recorded
checksum. A modified skill, unexpected provider path, symlinked canonical file,
or newer managed version is preserved and returns actionable guidance instead
of being overwritten. `SOUL.md`, `AGENTS.md`, and user-created skills remain
user-owned.

### Adding a Backend

A new backend adapter must preserve the `Request` and `RunOutput` contract,
support fresh and resumed chat sessions, provide a missing-session
classification, terminate when its future is dropped, and define explicit
configured, unattended, and evaluator modes.

---

## 7. Durable Data Model

### Runtime State in `push.db`

[`src/store.rs`](src/store.rs) owns channel cursors and backend session
mappings in the canonical SQLite database. Cursor advancement uses a
monotonic upsert, so concurrent or repeated writes cannot move a channel
backwards. Session reads, backend changes, backend-owned ID updates, and
`/clear` rotation use transactions keyed by channel and thread.

The first startup after upgrade imports the legacy document configured by
`state_path`:

```json
{
  "last_row_id": 123,
  "cursors": {
    "imessage": 123,
    "telegram": 456,
    "slack": 789
  },
  "sessions": {
    "imessage:self:you@icloud.com": {
      "uuid": "backend-session-id",
      "started": true,
      "backend": "codex"
    }
  }
}
```

`last_row_id` remains an iMessage fallback only when the JSON has no explicit
`cursors.imessage` value. Unqualified session keys are imported as iMessage
keys; an explicit channel-qualified key wins when both forms exist. Empty
session IDs are preserved so the next request starts fresh.

The imported cursor/session rows and a source-path migration marker commit in
one immediate transaction. A crash or error before commit leaves no partial
import, and startup retries from the unchanged JSON file. After commit, later
starts ignore that source even though Push deliberately leaves it in place as
a private recovery copy. Restoring a pre-migration database, or removing the
database after preserving other needed data, allows the retained file to be
imported again. Corrupt JSON stops startup with recovery guidance.

### `push.db`

[`src/history.rs`](src/history.rs) owns the schema and forward migrations.
SQLite foreign keys are enabled and the connection uses a five-second busy
timeout.

| Table | Responsibility |
| --- | --- |
| `conversations` | unique channel and thread identity |
| `messages` | canonical inbound and outbound content, generation state, delivery state, chunk checkpoint |
| `approval_questions` | retained durable question and answer state |
| `job_runs` | immutable job claims, bounded results, evaluation, scheduling, and delivery |
| `job_schedule_reviews` and `job_schedule_events` | exact-revision schedule proposals, decisions, activation, and history |
| `gateway_control_actions` | idempotent control actions such as `/stop` targets |
| `channel_cursors` | monotonic per-channel polling checkpoints |
| `backend_sessions` | current backend session for each channel and thread |
| `legacy_state_migrations` | atomic record of each imported legacy JSON source |

Important constraints:

- channel event IDs deduplicate inbound messages;
- each inbound message has at most one outbound response;
- one active job run exists per job;
- scheduled occurrence identity is unique;
- control actions are idempotent by inbound message.

Job files are authored directly. Enabled schedule activation uses the durable
question path and a separate activation ledger. A question is bound to one
allowlisted channel identity and the validated content revision, file identity,
effective backend, timeout, work directory, enabled triggers, and delivery
target. Answer selection and the schedule decision are recoverable across a
crash, and the scheduler still revalidates the exact revision before planning.

### Slack Inbox

The Slack receiver stores accepted events in `<state_path>.slack-inbox.db`
before acknowledging Socket Mode envelopes. Its local row ID becomes the
gateway cursor. Ignored envelopes keep redacted rejection metadata rather than
message or attachment content. Accepted image events persist only file IDs and
safe size and MIME type hints. Private download URLs and file bytes are never
stored.

### Audit Log

[`src/audit.rs`](src/audit.rs) appends JSON Lines under a process-wide lock.
Events include message metadata, routing, backend starts and failures, answer
outcomes, delivery results, and completion. Content is omitted by default.
`audit_log_content = true` opts into message and reply text.

Schedule lifecycle events first enter `job_schedule_events` as a SQLite
outbox. The gateway syncs each JSONL append before acknowledging it in SQLite
and retries pending rows on startup and scheduler or conversation activity.
Replay is at least once, so a crash between append and acknowledgement may
duplicate the stable schedule `event_id`.

---

## 8. Job System

Jobs are regular UTF-8 Markdown files directly under
`<assistant_root>/jobs`. [`src/jobs.rs`](src/jobs.rs) contains validation,
execution, evaluation, scheduling, and durable delivery.

### Runbook Format

```markdown
+++
version = 1
timeout = "5m"
backend = "codex"
evals = ["task-completion"]

[[triggers]]
id = "weekday-morning"
kind = "cron"
schedule = "0 8 * * 1-5"
timezone = "Europe/London"
enabled = true
+++

Run the morning review and return the three most important actions.
```

Validation rejects unknown frontmatter fields, unsafe work directories,
symlinks, subdirectories, invalid names, invalid UTF-8, oversized evals, bad
timeouts, invalid cron expressions, duplicate triggers, and unsupported
backends.

### Manual Run Pipeline

```text
1. LOAD          validate the requested runbook
2. LOCK          acquire a non-blocking per-job OS file lock
3. REREAD        validate the exact file again after locking
4. CLAIM         insert the running row in an immediate SQLite transaction
5. EXECUTE       start one fresh unattended backend session
6. STORE RESULT  persist bounded output before evaluation
7. EVALUATE      optionally run a fresh restricted evaluator
8. FINISH        persist terminal execution and evaluation state
9. PRINT         return the result to the invoking CLI
```

Manual runs never reuse chat sessions or proactively deliver to a channel.

### Scheduled Run Pipeline

The gateway ticks the scheduler once per second:

```text
1. RECOVER       inspect stale runs and delivery claims
2. CATALOG       reload and validate installed jobs
3. REVIEW        reconcile the exact revision and require durable activation
4. PLAN          calculate next cron occurrence in its IANA timezone
5. ENQUEUE       record one due occurrence in push.db
6. CLAIM         take work up to jobs_max_workers
7. EXECUTE       run a fresh unattended backend session
8. EVALUATE      optionally run the restricted evaluator
9. STORE         commit result, error, and evaluation state
10. CLAIM SEND   take due delivery work across gateway processes
11. DELIVER      send stored chunks and checkpoint progress
```

Push does not catch up occurrences missed while offline. A clock jump queues at
most one occurrence, daylight-saving gaps are skipped, and repeated local
times run once at their first instant.

Direct Markdown authoring, validation, inspection, disabled triggers, and
manual runs do not require schedule activation. A new or changed enabled
revision is proposed but omitted from planning until the exact owner-bound
review is approved. Any later validation failure, content change, path or
symlink replacement, or change to effective execution or delivery settings
invalidates it. A version-11 database migration activates only valid enabled
schedules whose exact revisions are captured by the first config-aware open
after upgrade, so upgrades preserve existing intended recurrence without
creating a later grandfathering window. When that capture has no valid primary
destination, the migration records an empty baseline and closes without
activating schedules.

### Execution and Delivery Semantics

- A failed or timed-out job is not rerun because the agent may already have
  completed an external side effect.
- Job results are bounded to 64 KiB.
- One OS advisory lock and one SQLite active-run constraint prevent overlap.
- `jobs_max_workers` bounds scheduled execution.
- Delivery uses up to four workers and five persisted attempts.
- Delivery backoff progresses through 30 seconds, 2 minutes, 10 minutes, and
  30 minutes.
- Delivery retries use the stored result and never rerun the backend.
- Partial delivery resumes from the first uncheckpointed chunk.
- Interrupted execution is marked terminal only after the released OS lock
  proves the prior executor is gone.

### Evaluators

Assigned evals come from `<assistant_root>/evals`. After successful execution,
Push starts a fresh restricted session with the original runbook, candidate
response, and evaluation criteria. The evaluator must finish with
`VERDICT: PASS` or `VERDICT: FAIL`.

Evaluation state is separate from execution state. A failed, malformed, or
timed-out evaluator does not rewrite the job result.

---

## 9. Concurrency and Shutdown

### Concurrency Model

| Scope | Model |
| --- | --- |
| Process | one Tokio multi-thread runtime |
| Channels | one independent gateway task per enabled provider |
| Conversations | one worker task per channel-qualified thread |
| Thread messages | serialized through a bounded queue |
| Different threads | concurrent |
| Scheduled jobs | bounded by `jobs_max_workers` |
| Scheduled delivery | at most four workers |
| State access | short shared mutex sections |
| Audit writes | one shared process lock |

This prevents two messages in one conversation from racing the same backend
session while allowing unrelated conversations and providers to progress.

### `/stop`

`/stop` records an idempotent control action, targets the current row in that
conversation, and signals its worker. Dropping the backend future kills the
subprocess. Already queued messages remain and continue after the interrupted
turn stores its response.

### Provider Isolation

One provider may fail, disconnect, or rate-limit without cancelling the other
channel loops. The process exits only when shutdown is requested or every
enabled channel task has stopped.

### Shutdown Guarantees

- pending channel operations must be cancellation-safe;
- accepted work drains within the provider's worker grace;
- workers remaining after the deadline are aborted;
- scheduler execution and delivery workers get a separate 30-second grace;
- delivery claims owned by the process are released during cleanup;
- persisted inbound, outbound, and delivery state remains recoverable.

---

## 10. Security Model

An accepted sender can cause an agent to use local tools and credentials. The
channel allowlist is therefore an operator boundary, not a spam filter.

### Trust Boundaries

| Boundary | Enforcement |
| --- | --- |
| iMessage sender | exact normalized `self_handles` and `allow_from` values |
| Telegram sender/chat | stable numeric user and private-chat IDs |
| Slack sender/workspace | stable member IDs plus authenticated workspace verification |
| Chat agent permissions | selected agent's own configuration |
| Unattended jobs | explicit backend mode with no interactive prompt dependency |
| Evaluators | tools, external integrations, project instructions, and persistence disabled |
| Assistant files | canonical path and symlink checks |
| Runtime state | path separation and owner-only permissions |

### Permission Rule

Push does not override sandbox, approval, or tool configuration for chats.
Claude and Codex jobs bypass interactive permissions so unattended work can
finish. Pi has no native filesystem sandbox or interactive permission prompt.
Every enabled job should be treated as code execution by the Push service user.

### Network Rule

Push accepts no inbound network connection:

- iMessage reads a local SQLite database;
- Telegram uses outbound HTTPS;
- Slack uses an outbound authenticated WebSocket;
- agent CLIs and their tools own any further network access.

### Filesystem Rule

Configuration validation keeps runtime state and secrets outside the
Git-versioned assistant repository. Jobs and evals reject symlinks and unsafe
paths. State, database, audit, inbox, and lock files are restricted to the
service user where supported.

---

## 11. Failure and Recovery

| Failure | Recovery behavior |
| --- | --- |
| Channel poll fails | log and retry on the next poll |
| Cursor save fails | restore in-memory cursor and retry later |
| Canonical inbound write fails | stop processing the batch so the message is retried |
| Thread queue fills | store a retry response, attempt delivery once, then complete the row |
| Worker closes | replace it and recover retained jobs |
| Backend times out | kill it, store a timeout response, do not retry |
| Backend session is missing | rotate once and retry with bounded history |
| Backend fails | store a gateway-authored error response |
| Worker-managed reply send fails | retry the stored outbound without rerunning |
| Process crashes after send | possible at-least-once duplicate on recovery |
| Scheduled execution stops | mark interrupted after lock-based liveness proof |
| Scheduled delivery stops | release or expire the claim and resume stored chunks |
| Invalid job appears | disable that job and continue messaging and other jobs |

The key crash boundary is always persistence before an irreversible next step:
accepted input before execution, generated output before delivery, and job
result before proactive delivery.

---

## 12. Source Map and Change Rules

### Module Reference

| Module | Responsibility |
| --- | --- |
| [`src/main.rs`](src/main.rs) | CLI parsing and process entry |
| [`src/cli_json.rs`](src/cli_json.rs) | versioned JSON envelopes, exit categories, and secret-safe command projections |
| [`src/config.rs`](src/config.rs) | configuration parsing, migration, validation, and routing |
| [`src/gateway/`](src/gateway/) | channel coordination, polling, queues, workers, acknowledgement, delivery |
| [`src/channel.rs`](src/channel.rs) | provider-neutral channel contract and static dispatch |
| [`src/imessage/`](src/imessage/) | Messages database polling and AppleScript sending |
| [`src/telegram.rs`](src/telegram.rs) | Telegram polling, authorization, formatting, voice transfer |
| [`src/slack.rs`](src/slack.rs) | Slack Socket Mode receiver, inbox, identity, and delivery |
| [`src/agent.rs`](src/agent.rs) | provider-neutral backend request and result contract |
| [`src/claude.rs`](src/claude.rs) | Claude Code CLI adapter |
| [`src/codex.rs`](src/codex.rs) | Codex CLI adapter |
| [`src/pi.rs`](src/pi.rs) | Pi CLI adapter |
| [`src/store.rs`](src/store.rs) | transactional SQLite cursor/session state and legacy JSON migration |
| [`src/history.rs`](src/history.rs) | SQLite schema, conversation history, delivery, and migrations |
| [`src/image.rs`](src/image.rs) | image limits, signature validation, and private temporary-file cleanup |
| [`src/jobs.rs`](src/jobs.rs) | runbook validation, execution, evaluation, scheduler, and ledger |
| [`src/audit.rs`](src/audit.rs) | redacted JSONL audit log |
| [`src/prompt.rs`](src/prompt.rs) | system-section and untrusted prompt-content composition |
| [`src/voice.rs`](src/voice.rs) | provider-neutral transcription and speech |
| [`src/assistant.rs`](src/assistant.rs) | assistant repository initialization |
| [`src/doctor.rs`](src/doctor.rs) | environment and deployment checks |

### Change Rules

When changing the system:

1. Keep provider behavior behind `ChannelContract`.
2. Keep agent-specific CLI behavior behind `Runner`.
3. Persist accepted input before execution.
4. Persist generated output before delivery.
5. Do not retry agent execution when external side effects may have occurred.
6. Preserve per-thread ordering and cross-thread concurrency.
7. Keep runtime state outside the assistant repository.
8. Add forward-only SQLite migrations and recovery tests.
9. Document exact delivery, retry, and shutdown semantics.
10. Avoid adding a gateway plugin or tool system unless a concrete capability
    cannot belong to the selected agent.

---

## 13. Build and Verification

Push targets stable Rust on macOS and Linux. iMessage is macOS-only.

The required local checks are:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
cargo test --locked
python3 -m mkdocs build --strict
```

CI runs the documentation build plus formatting, linting, build, installer,
release-tooling, and test checks on Linux and macOS. Security audit runs
separately when Rust dependencies change and on a weekly schedule.

Tests are colocated in module `tests` blocks, with integration checks under
[`tests/`](tests/). Channel and backend adapters use contract-style fakes to
exercise command construction, session behavior, cancellation, retries, and
failure classification without depending on live providers.

---

## 14. Deliberate Non-Goals

Push is intentionally not:

- an agent runtime;
- a model API client;
- an inbound web service;
- a dynamic channel plugin host;
- an MCP server manager;
- a secrets manager;
- a multi-tenant assistant platform;
- a distributed queue or replicated database.

Likely future work includes additional built-in channels, additional agent
adapters, and audited memory write-back. Those additions should preserve the
gateway, channel, and agent boundaries above.
