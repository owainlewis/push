# Designing an assistant

An assistant repository is the durable, portable part of your setup. It should
explain who the assistant is, what it knows, how recurring work runs, and how
to judge good results. Keep it small enough that you can inspect and version
every important instruction.

Push creates the starting structure:

```text
assistant/
├── SOUL.md
├── AGENTS.md
├── CLAUDE.md
├── README.md
├── context/
├── evals/
├── jobs/
├── skills/
│   └── push/
│       └── SKILL.md
├── .agents/skills/
│   └── push -> ../../skills/push
└── .claude/skills/
    └── push -> ../../skills/push
```

`AGENTS.md` is the shared instruction source. `CLAUDE.md` contains only
`@AGENTS.md`, so Claude Code and Codex receive the same repository guidance
without maintaining two copies.

## Start with identity, not a long prompt

Use `SOUL.md` for stable identity and working style:

```markdown
# SOUL

You are my personal assistant. Be direct, practical, and honest.

## Working style

- State uncertainty instead of guessing.
- Confirm before external side effects.
- Prefer short answers with enough evidence to trust them.
```

Write rules that should apply to almost every conversation. Project details,
temporary priorities, contact information, and task procedures belong
elsewhere. A short identity file is easier to reason about and less likely to
contain conflicting instructions.

Push supplies `SOUL.md` to every conversation and job as the user-owned system
identity section. It does not rewrite the file.

## Understand prompt ownership and precedence

Push composes every Claude Code, Codex, and Pi conversation or job request from
the same ordered sections:

| Order | Section | Owner | Meaning |
| --- | --- | --- | --- |
| 1 | Push-owned base policy | Push | Small delivery, boundary, context, identity/eval editing, and job-validation invariants |
| 2 | User-owned system identity | You, through `SOUL.md` | Stable identity and working style that cannot override Push policy |
| 3 | Resolved workspace paths | Push | Absolute assistant, working, context, evals, and jobs paths |
| 4 | Fresh message context | Channel or scheduler | Untrusted current-turn data described below |

The first three sections use each backend's native system or developer
instruction mechanism. Fresh message context uses the ordinary prompt input.
Push JSON-encodes identity, paths, current messages, and history, so text that
looks like a heading or delimiter remains data inside its owning section.
Normal resumed sessions receive an empty history array. New or rebuilt sessions
receive bounded canonical history in that same untrusted section.

For conversations, fresh context contains the channel, channel-qualified
thread, text or voice delivery mode, current message, and optional bounded
history. For jobs, it contains the job name, configured delivery behavior, and
runbook body as the current message. Both forms remain ordinary prompt content.

This boundary means sender text, message text, history, handles, and provider
metadata never become Push policy through prompt framing. Content in
`SOUL.md` is still system-level guidance, so keep it trusted and under version
control.

## Organize durable context

Use `context/` for information that should survive across conversations:

```text
context/
├── README.md
├── preferences.md
├── people.md
├── projects/
│   ├── push.md
│   └── website.md
└── processes/
    └── publishing.md
```

Keep each file focused. Record facts, decisions, preferences, and current state,
not complete chat transcripts. Include dates when information will become
stale, and remove obsolete notes rather than accumulating contradictions.

`context/README.md` should act as the index. Push tells the backend to begin
there when user context is relevant, but it does not inject every context file
into every prompt.

## Put reusable capabilities in skills

A skill packages one repeatable workflow. Keep its instructions, helper
scripts, references, examples, and assets together:

```text
skills/
└── youtube/
    ├── SKILL.md
    ├── scripts/
    ├── references/
    └── assets/
```

Tooling that exists only to support the workflow belongs under the skill's
`scripts/` directory. Shared external capabilities such as an email connector
or issue tracker remain configured through the selected agent's MCP or tool
configuration. Never put credentials in `SKILL.md` or a helper script.

Every skill needs a `SKILL.md` with a name, a description that explains when it
should run, and the workflow instructions. For example:

```markdown
---
name: youtube
description: Plan and prepare a technical YouTube video from a topic, transcript, or rough notes.
---

# YouTube

1. Identify the one useful lesson for the intended viewer.
2. Produce a clear title, opening script, lesson outline, and recording plan.
3. Use scripts and references in this skill only when the task needs them.
```

Save that file as `skills/youtube/SKILL.md`. Keep the description specific
because each supported runtime uses it to decide when the skill is relevant.

### Share one skill between supported runtimes

Codex discovers repository skills under `.agents/skills/`. Claude Code uses
`.claude/skills/`. Pi uses `.pi/skills/` and also recognizes
`.agents/skills/`. These project locations support skill directories, and
Codex and Claude Code explicitly follow directory links, so one canonical skill
can serve every supported backend. See the official [Codex skills
guide](https://developers.openai.com/codex/skills), [Claude Code skills
guide](https://code.claude.com/docs/en/skills), and [Pi skills
guide](https://pi.dev/docs/latest/skills) for their discovery rules.

```text
assistant/
├── skills/
│   └── youtube/
├── .agents/
│   └── skills/
│       └── youtube -> ../../skills/youtube
└── .claude/
    └── skills/
        └── youtube -> ../../skills/youtube
```

After creating a canonical user-owned skill, expose it to all three agents with
two links from the assistant root. Pi and Codex share `.agents/skills/`:

```sh
mkdir -p .agents/skills .claude/skills
ln -s ../../skills/youtube .agents/skills/youtube
ln -s ../../skills/youtube .claude/skills/youtube
```

Use relative links so the repository remains portable when cloned elsewhere.
Commit the canonical skill and the links.

Push itself manages only `skills/push/` and the two `push` exposure links.
Its hidden manifest records the installed content version and checksum. A later
`push init` refreshes an older copy only when its checksum still matches the
managed content. If you modify the managed skill or replace a provider link,
Push preserves it and asks you to move your changes to a differently named
skill or restore the managed path. Push does not edit `SOUL.md`, `AGENTS.md`, or
skills you create.

## Use jobs for scheduled outcomes

A skill describes how to perform a reusable workflow. A job describes one
specific manual or scheduled outcome:

```text
skills/youtube/          reusable publishing workflow
jobs/morning-brief.md    scheduled request with timeout and delivery
```

Keep job bodies self-contained because every job starts a fresh backend
session without conversation history. Chat turns start in `assistant_root`, and
jobs use `assistant_root` as their default work directory. Jobs can therefore
discover project instructions and skills linked there. An explicit alternate
work directory changes that discovery context and must not overlap Push-owned
runtime paths. Keep every required procedure in the job body, or make the
needed skill available through the backend's global skill location or the
selected work directory.

Put stable preferences in `SOUL.md` or `context/`, and put the schedule, work
directory, constraints, required procedure, and requested output in the job.
See [Jobs and schedules](jobs.md) for the complete runbook format.

## Define what good looks like

Use `evals/` for reusable checks applied to completed jobs:

```markdown
# Writing quality

Fail work that contains unsupported claims, missing source links, or needlessly
complex language.
```

Good evals describe observable properties of the result. Avoid vague goals such
as "make it excellent." A job can assign several focused evals, such as factual
support, writing style, and task completion.

## Keep credentials out of the repository

Commit an `.env.example` only when it helps document required variable names:

```dotenv
YOUTUBE_API_KEY=
```

Provide real values through the service environment, the selected agent's
authentication store, or another local secret manager. Push does not load an
assistant-root `.env` file automatically. A gitignored `.env` used directly by
a helper tool is still sensitive local state; restrict it to the service user
and do not assume `.gitignore` prevents accidental disclosure.

Never commit tokens, OAuth data, session state, conversation databases, audit
logs, or Push configuration containing credentials. Read [Permissions and
security](security.md) before running an assistant unattended.

## Grow the assistant deliberately

Use this order when adding a capability:

1. Try the task in a normal conversation.
2. Record stable personal or project facts under `context/`.
3. Extract a repeated workflow into one skill.
4. Add a job only when the outcome should run manually or on a schedule.
5. Add an eval when success can be checked consistently.
6. Review the repository diff before committing the change.

This keeps identity stable and prevents one large instruction file from
becoming a mixture of preferences, procedures, schedules, and secrets.

## Design checklist

- `SOUL.md` contains only durable identity and working style.
- `AGENTS.md` is the shared repository guidance.
- `CLAUDE.md` references `AGENTS.md` instead of copying it.
- `skills/push/` and its provider links remain Push-managed.
- `context/README.md` indexes focused, current context files.
- each skill owns its instructions and supporting tooling.
- each job requests one self-contained outcome.
- evals describe observable pass or fail conditions.
- credentials and runtime state stay outside version control.
- agent permissions match the side effects an allowed sender may request.
