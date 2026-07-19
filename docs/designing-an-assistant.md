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
└── jobs/
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

Push supplies `SOUL.md` to every conversation and job. It appends its own
gateway safety rules in memory and does not rewrite the file.

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
because both Codex and Claude use it to decide when the skill is relevant.

### Share one skill between Codex and Claude

Codex discovers repository skills under `.agents/skills/`. Claude Code uses
`.claude/skills/`. Both support skill directories that are symbolic links, so
one canonical skill can serve both backends. See the official [Codex skills
guide](https://learn.chatgpt.com/docs/build-skills.md) and [Claude Code skills
guide](https://code.claude.com/docs/en/skills) for their discovery rules.

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

After creating the canonical skill, expose it to both agents from the assistant
root:

```sh
mkdir -p .agents/skills .claude/skills
ln -s ../../skills/youtube .agents/skills/youtube
ln -s ../../skills/youtube .claude/skills/youtube
```

Use relative links so the repository remains portable when cloned elsewhere.
Commit the canonical skill and the links. `push init` does not currently create
or synchronize skill links, and Pi skill discovery remains controlled by Pi's
own configuration.

## Use jobs for scheduled outcomes

A skill describes how to perform a reusable workflow. A job describes one
specific manual or scheduled outcome:

```text
skills/youtube/          reusable publishing workflow
jobs/morning-brief.md    scheduled request with timeout and delivery
```

Keep job bodies self-contained because every job starts a fresh backend
session without conversation history. Chat turns start in `assistant_root`, but
jobs start in the job's configured work directory, which must stay outside the
assistant repository. A job therefore does not automatically discover skills
linked under the assistant root. Keep required procedures in the job body, or
make the skill available through the backend's global skill location or the
job work directory.

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
- `context/README.md` indexes focused, current context files.
- each skill owns its instructions and supporting tooling.
- each job requests one self-contained outcome.
- evals describe observable pass or fail conditions.
- credentials and runtime state stay outside version control.
- agent permissions match the side effects an allowed sender may request.
