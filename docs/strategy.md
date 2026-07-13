# Push Strategy

## The Bet

The agent runtime is becoming the commodity layer.

Claude Code, Codex, Cursor, AMP, Pi-style agents, in-house agents, and small
open-source agents are all racing to own the same capabilities: reasoning,
coding, file edits, shell commands, MCP, plugins, permissions, repo context, and
task execution.

Push should not compete there.

Push should own the durable gateway layer: messages, routing, scheduling,
history, security, and delivery. The user should own one portable assistant
repository with identity, context, and jobs. The agent runtime should be
replaceable.

## Product Thesis

Push is a personal assistant gateway, not an agent runtime.

The gateway answers these questions:

- Who is allowed to talk to the assistant?
- Which conversation is this?
- Where is the single configured assistant repository?
- Which runtime should handle the work?
- What should be sent back to the user?
- What state should persist for next time?

The backend agent answers a smaller question:

- Given this message and assistant context, what work should be done and what
  response should be sent back?

That contract keeps the hard and fast-moving agent work inside products with
large teams behind them, while Push owns the layer that makes the interaction a
personal assistant.

## What Personal Assistant Means

This is not just a coding bot over iMessage.

A personal assistant needs:

- Durable user preferences.
- Business and project context.
- A memory model the user can inspect and correct.
- Access to tools through the selected runtime.
- Channel-aware replies.
- A stable identity across backend changes.
- Permission and routing rules that match the user's life.

Push supports exactly one assistant. `push init [path]` creates its user-owned,
Git-versioned repository with `SOUL.md`, `context/`, and `jobs/`. One canonical
root is configured; the other paths are derived. There are no assistant names,
IDs, registries, or selection flows. This stays small, legible, and stable
across backends and machines.

## What The Gateway Owns

- Channel polling and sending.
- Allowlist and reply-loop filtering.
- Conversation ids.
- Backend session ids.
- Assistant root loading and runtime instruction composition.
- Job validation, approval, scheduling, history, and delivery.
- Runtime selection.
- User-visible delivery.
- The audit trail in plain files and JSON state.

## What The Runtime Owns

- Model behavior.
- Coding workflow.
- Tool execution.
- MCP servers.
- Plugins and skills.
- Shell permissions.
- Repo context.
- Long-running task mechanics.

The assistant repository owns `SOUL.md`, editable context, and installed job
runbooks. The runtime owns skills, tools, MCP, and authentication. Push runtime
state and secrets remain outside the repository.

The gateway should not rebuild these unless there is no reliable backend
contract for the job.

## Backend Contract

The backend seam should stay small:

```text
input:
  user message
  assistant context
  resolved assistant, context, and jobs locations
  conversation/session id if one exists
  working directory
  timeout and permission config

output:
  final reply
  backend session id if created by the runtime
```

Claude Code and Codex already fit this shape:

- Claude Code accepts a gateway-generated session id with `--session-id` and
  resumes with `--resume`.
- Codex creates its own thread id through `codex exec`; Push stores it and later
  resumes with `codex exec resume`.

The state store must therefore track backend-owned session ids, not just
gateway-owned UUIDs.

## Positioning

Hermes and OpenClaw are useful comparisons, but the critique should be precise.

Hermes is powerful because it builds a full agent runtime and memory system. The
cost is complexity and a second agent layer between the user and the underlying
model or tool runtime.

Push takes the opposite bet. It delegates agent quality to first-party or
specialized coding agents and focuses on the personal gateway layer.

This means Push can be smaller and more durable:

- When Claude Code improves, the Claude backend improves.
- When Codex improves, the Codex backend improves.
- When another agent becomes better, Push can add an adapter instead of
  rewriting the product.

## Current Direction

Lock in these choices:

- Keep iMessage as the first channel, not the whole product.
- Keep markdown memory as the first memory model.
- Support multiple runtimes early so the architecture does not harden around
  one agent.
- Avoid a plugin system in the gateway.
- Avoid a custom agent loop.
- Treat runtime config as adapter-specific.
- Keep the core gateway state backend-neutral.

## Next Actions

1. Add a second channel after the backend seam has settled.
2. Add memory write-back only with an audit trail and explicit user review.
3. Add richer runtime routing, such as task-type routing.
4. Publish and test the first binary release.
5. Add a Homebrew formula after release assets are stable.
