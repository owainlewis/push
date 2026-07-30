---
name: push
description: Operate a Push personal assistant, inspect its health and jobs, and author or validate Push job runbooks. Use when a request concerns Push setup, CLI commands, assistant files, scheduled jobs, or delivery behavior.
license: MIT
compatibility: Requires the Push CLI and an initialized assistant repository.
metadata:
  push-managed-version: "2"
---

# Push

Use Push as the gateway around the configured Claude Code, Codex, or Pi
runtime. Push owns channels, scheduling, history, security checks, and delivery.
The agent runtime owns reasoning, tools, skill execution, permissions, MCP
servers, and authentication.

## Work in the assistant repository

- Treat `SOUL.md` as user-owned identity and `AGENTS.md` as user-owned repository
  guidance. Do not edit either unless the user asks.
- Read `context/README.md` first when durable user context is relevant. Keep
  useful durable notes under `context/` and evaluation criteria under `evals/`.
- Store complete job runbooks under `jobs/`.
- Treat `skills/push/` and the `push` links under `.agents/skills/` and
  `.claude/skills/` as Push-managed. Do not edit them. Codex and Pi share the
  `.agents/skills/` discovery path.
- Keep credentials, configuration containing credentials, sessions, databases,
  audit logs, and job runtime files outside the assistant repository.

## Use the current CLI

Run `push help` when the accepted command forms are unclear. The stable commands
are:

- `push help`
- `push version`
- `push init [path]`
- `push`
- `push doctor`
- `push reload` or `push restart`
- `push job validate`
- `push job list`
- `push job show <name>`
- `push job run <name>`
- `push job runs [<name>]`
- `push job reviews [<name>]`

All commands accept `--config <path>`. Do not assume machine-readable output
unless `push help` documents it in the installed version. Never expose tokens,
message content, or sensitive runtime state in diagnostics or replies.

## Author jobs safely

1. Read the job format and existing runbooks before editing.
2. Write the complete Markdown runbook directly under `jobs/` when the user
   asks to create or change a job.
3. Keep secrets out of the runbook. Use the configured service or agent
   environment for credentials.
4. Run `push job validate` after every job change. Do not claim success if
   validation fails.
5. Saving a new or changed enabled schedule does not activate it. Tell the user
   that Push will present the exact revision for separate owner review.
6. Use `push job show <name>` to inspect the installed result,
   `push job reviews <name>` to inspect schedule activation state, and
   `push job runs <name>` to inspect execution and delivery history.
7. Run `push job run <name>` only when the user asked for the job to execute or
   when execution is a clearly authorized part of the task.

## Reply normally

For an ordinary conversation, return the final reply normally. Push sends it
back through the originating channel. Do not invoke the Push CLI merely to send
a chat reply.
