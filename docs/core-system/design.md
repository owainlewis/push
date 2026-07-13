# Core Assistant System

**Status:** Approved

**Author:** Owain Lewis  **Date:** 2026-07-11

## Summary

Push is a local personal-assistant gateway. It owns channels, conversation
history, routing, permissions, scheduling, approvals, and delivery while
delegating reasoning and tool use to disposable agent runtimes such as Claude
Code and Codex. The durable assistant is one user-owned Git repository with
`SOUL.md`, `context/`, and `jobs/`. Push stores the canonical conversation
history in SQLite outside that repository.

## Goals

- Keep exactly one portable assistant across channels and agent backends.
- Make Push, rather than a backend vendor, the owner of conversation history.
- Keep identity legible and directly editable in one Markdown file.
- Preserve existing backend sessions as a fast path without depending on them
  for durable history.
- Leave a simple path to compact long-term memory without putting summarisation
  on the message-response path.
- Keep the backend boundary narrow enough to add or replace runtimes.

## Non-goals

- Generate or reconcile `MEMORY.md` in the first implementation.
- Inject an entire conversation transcript into every request.
- Build embeddings, semantic retrieval, or a general knowledge system.
- Build a new filesystem sandbox. Push uses the existing named permission
  profiles and each backend's filesystem controls.
- Build a custom agent loop, tool runner, plugin system, or MCP layer in Push.
- Build assistant registries, IDs, selection, or multi-assistant commands.
- Define autonomous memory write-back here.

## Constraints

- Push remains one local Rust process with no inbound server port.
- Telegram and iMessage conversations must remain channel-qualified.
- Claude Code, Codex, and Pi retain different session and instruction mechanisms.
- A failed history write must not result in an unrecorded request being sent to
  an agent.
- A failed future reconciliation run must not block replies.
- Conversation content is sensitive local data. The database and assistant
  files must not be logged or exposed to unallowlisted senders.
- Current backend permission modes cannot guarantee that a powerful agent
  process will not edit files readable by the local user.

## Proposed design

### Ownership boundary

```text
Push runtime                    Assistant repository          Agent runtime
channels, scheduling, history   SOUL.md, context, jobs         reasoning, tools,
security, delivery              user-owned and Git-versioned   skills, MCP, auth
```

The gateway owns durable runtime state. The user owns the assistant repository.
Agent runtimes own execution. A
backend session is a cache of conversational context, not the source of truth.
Losing a Claude Code, Codex, or Pi session must not lose the conversation record.

### Identity

`assistant_root` is the one configured assistant repository. `SOUL.md` beneath
that root is the single user-owned identity source. The file contains
personality, communication style, principles, and stable behavioural
boundaries. Identity does not live in TOML fields. `push init [path]` creates
the repository and defaults to `./assistant` when no path is given.

At runtime, Push composes the file with a gateway-owned footer rather than
modifying it on disk:

```text
<contents of SOUL.md>

Assistant root: /resolved/path/to/assistant
Context: /resolved/path/to/assistant/context
Jobs: /resolved/path/to/assistant/jobs

Begin with context/README.md when user context is relevant.
Do not modify SOUL.md or installed jobs directly.
Propose job changes through Push's approval workflow.
```

Claude Code and Pi receive the composed text as appended system instructions.
Codex receives it as developer instructions. Push never writes resolved machine paths
into `SOUL.md` and does not inject all context files into every prompt. The
backend decides which files to inspect. Conversation routes receive `context/`
as an additional workspace subject to their permission profile; `SOUL.md` and
installed jobs remain protected by the existing sandbox and approval boundary.

Push owns the footer so customising `SOUL.md` cannot remove repository
locations or ownership rules. Runtime sessions, drafts, databases, audit logs,
locks, delivery state, configuration secrets, and authentication stay outside
the assistant repository.

This is a deliberate replacement for the structured `[assistant]` identity
fields and automatic `User.md` and `Memory.md` injection. The implementation
removes those inputs rather than merging multiple identity sources. Existing
users move any identity they want to keep into `SOUL.md`. Legacy
`assistant_dir` and `jobs_dir` settings remain compatible only when the jobs
directory is exactly `<assistant_dir>/jobs`; divergent layouts receive an
actionable migration error rather than silently losing identity or jobs.

### Canonical conversation history

`~/.push/push.db` stores every accepted inbound message and every user-visible
outbound message, whether produced by a backend or by the gateway. The minimum
logical model is:

```text
conversations
  id, channel, thread_key, created_at, updated_at

messages
  id, conversation_id, direction, origin, content, backend,
  channel_event_id, generation_status, delivery_status, created_at
```

The exact schema is an implementation decision, but these invariants are not:

- Conversation identity includes the channel-qualified thread key.
- An inbound message has a stable identity unique on channel and channel event
  id. Retrying the same channel event finds the existing row rather than
  inserting another user turn.
- The accepted incoming message is stored before the backend or gateway command
  handler runs.
- Backend replies, local command replies, and user-visible error replies are
  stored with their origin.
- An assistant response is stored after the backend returns a valid reply and
  before Push attempts delivery.
- Delivery status is recorded separately from response generation so a retry
  does not invent a second assistant turn.
- Existing `state.json` cursor and backend-session behaviour remains unchanged
  in this phase. Moving gateway state into SQLite is a separate decision.

On a normal turn, Push resumes the existing backend session and sends only the
new request. When a backend session is missing, cleared, or replaced, Push may
rehydrate a new session from recent canonical history. Rehydration policy is a
performance decision and is not required for the initial history store.

### Deferred memory reconciliation

The SQLite conversation history is the journal. A future reconciler may read
completed exchanges and replace a small `MEMORY.md` in the assistant directory
containing durable preferences, active projects, and confirmed decisions.
`MEMORY.md` is derived context, not another transcript and not a higher-priority
instruction source.

Reconciliation runs outside the reply path. It tracks a message watermark,
updates the file atomically, and remains safe to retry. It should primarily
trust explicit user statements and confirmed decisions, not arbitrary content
retrieved by an agent. The source conversation remains available when a memory
needs to be checked or regenerated.

The first release of the conversation store does not create `MEMORY.md` and
does not run a reconciler. Until that later feature ships, the memory footer is
omitted and backend sessions provide immediate conversational continuity.

### Backend contract

The gateway sends one request and receives one final reply plus an optional
backend session id. The request separates:

- instructions: composed `SOUL.md`, resolved assistant paths, and gateway-owned policy;
- current message: the user's request;
- conversation identity and backend session state;
- working directory, timeout, and permissions.

Conversation history storage happens around this boundary and is independent
of the selected backend. Agent tools, skills, MCP servers, model choice, and
execution loops remain backend-owned.

## Alternatives and tradeoffs

### `User.md` plus `Memory.md` injected on every turn

This is legible but mixes user facts, assistant identity, and memory policy. It
also leaves Push without a complete history from which memory can be audited or
rebuilt. A single `SOUL.md` gives identity one clear owner.

### Append every exchange to `JOURNAL.md`

This is the smallest persistence mechanism, but concurrent writes, structured
queries, delivery state, migrations, and later reconciliation all become more
fragile. SQLite is already embedded in the project and provides a stronger
canonical record. A chronological Markdown journal would duplicate the
database.

### Let each backend own history

This is the current fast path, but it couples assistant memory to vendor
session storage. Switching backends, clearing a session, or losing runtime
state loses continuity. Backend sessions remain useful caches, but not durable
assistant state.

### Build retrieval and summarisation immediately

This could improve recall but adds policy, latency, evaluation, and security
questions before there is evidence that simple session resume plus durable
history is insufficient. Persist the source material first and add retrieval
only when real usage identifies the failure mode.

## Risks

- Storing message content increases local privacy impact. Restrict file
  permissions, avoid content logging, and document backup and deletion.
- SQLite writes add a new failure point. Store the request transactionally
  before dispatch and make response and delivery state explicit.
- Resumed backend sessions may diverge from the canonical record after a crash.
  Treat SQLite as authoritative and make session rotation safe.
- A future reconciler may preserve an incorrect or injected claim. Keep memory
  small, derived, inspectable, replaceable, and lower priority than `SOUL.md`.
- An inherited backend configuration may still grant broad filesystem access.
  Keep `SOUL.md` and installed jobs protected by policy and the existing route
  restrictions, and reserve normal assistant edits for `context/`.

## Rollout

1. Replace structured assistant identity and automatic `User.md`/`Memory.md`
   loading with `SOUL.md`, documenting the manual migration. Add equivalent
   instruction tests for new and resumed Claude and Codex sessions.
2. Introduce SQLite conversation and message persistence. Keep cursors and
   backend session ids in the existing state file.
3. Record user requests, assistant responses, and delivery state around the
   existing backend contract, including idempotent retry tests and
   gateway-generated replies.
4. Add fresh-session rehydration only after the canonical store is proven.
5. Design and ship reconciliation separately if observed history size or
   cross-session recall justifies it.

Backout keeps the existing backend session behaviour and ignores the new
conversation database. No memory file migration is required because
reconciliation is not part of this rollout.

## Open questions

- What retention and deletion controls should the first SQLite store expose?
- Should `/clear` only rotate the backend session or also begin a new logical
  conversation while retaining the old history?
- How much recent history should seed a fresh backend session, if any?

## Decision

Approved by Owain Lewis on 2026-07-11 and updated for the single assistant
repository on 2026-07-13. Adopt `SOUL.md` as the single identity source,
`context/` as the editable assistant workspace, `jobs/` as installed runbooks,
and SQLite as Push's canonical conversation history. Keep backend sessions as a
fast path. Defer generated memory, reconciliation, retrieval, and summarisation
to a separate later design.
