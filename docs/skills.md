# Skills

Skills are the assistant's reusable workflows. They belong in the assistant
repository when they capture how this assistant performs a task across
conversations and scheduled jobs.

Push uses the open [Agent Skills specification](https://agentskills.io/specification/).
Each skill is a directory with a required `SKILL.md` and optional scripts,
references, and assets:

```text
skills/
└── youtube-research/
    ├── SKILL.md
    ├── scripts/
    │   ├── fetch-videos.py
    │   └── rank-videos.py
    ├── references/
    │   └── ranking-method.md
    └── assets/
        └── report-template.md
```

For a new repository, `push init` creates `skills/` as the canonical source. It
also creates `.agents/skills` and `.claude/skills` links so Codex and Claude
Code discover the same files without duplicate copies. Push passes the
canonical directory to Pi explicitly.

Re-running init never replaces an existing backend-specific skills directory
or a link to another location. It stops before writing and explains how to
consolidate those skills into `skills/`. Remove the old discovery path only
after checking that the canonical directory contains everything you need.

## Skills and tools

A skill is a reusable procedure. A tool is a capability the procedure uses.
Keep a script inside a skill when it exists only for that workflow. For
example, `youtube-research/scripts/rank-videos.py` can normalize metrics and
rank results deterministically instead of asking the model to do arithmetic.

Use a shared CLI or MCP server when several skills need the same capability,
or when the integration needs its own lifecycle, authentication, or service.
The selected backend remains responsible for executing scripts and tools,
enforcing permissions, and providing authenticated integrations. Push does not
implement another tool runner or plugin system.

## Write a skill

Create one directory beneath `skills/`:

```sh
mkdir -p skills/youtube-research/scripts
```

Add `skills/youtube-research/SKILL.md`:

```markdown
---
name: youtube-research
description: Find and rank YouTube videos for content research. Use when comparing topics, channels, titles, or recent video performance.
compatibility: Requires Python 3, network access, and YOUTUBE_API_KEY.
---

# YouTube research

1. Read the current content priorities from `context/`.
2. Run `scripts/fetch-videos.py` for the requested channels or searches.
3. Run `scripts/rank-videos.py` on the returned JSON.
4. Check unusual results against the source data.
5. Return the ranked opportunities and one recommendation.
```

Keep `SKILL.md` focused on the workflow. Put deterministic fetching,
normalization, validation, and ranking in scripts. Put detailed methodology in
`references/` and output templates in `assets/`.

## Use a skill from a job

A job defines a particular outcome and schedule. A skill defines the reusable
method:

```markdown
Research high-performing AI engineering videos published this week.

Use the `youtube-research` skill. Return the five strongest opportunities and
recommend one video to make next.
```

Jobs start fresh backend sessions. Name the skill explicitly when the job
depends on it, and keep the requested outcome and constraints in the job. See
[Jobs and schedules](jobs.md) for the runbook format.

## Dependencies and secrets

Document required commands, network access, and environment variable names in
the skill's `compatibility` field or instructions. Keep actual credentials out
of the assistant repository.

Push does not load an assistant-root `.env` automatically. Supply secrets
through the service environment, the backend's authentication store, or a
local secret manager. An `.env.example` may document variable names, but it
must never contain real values.
