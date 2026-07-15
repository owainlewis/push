# Push

**Your coding agent, on call 24/7.**

[![CI](https://github.com/owainlewis/push/actions/workflows/ci.yml/badge.svg)](https://github.com/owainlewis/push/actions/workflows/ci.yml)
[![Docs](https://img.shields.io/badge/docs-read-12756f)](https://owainlewis.github.io/push/)
[![License: MIT](https://img.shields.io/badge/license-MIT-111417)](LICENSE)

Push turns Claude Code, Codex, or Pi into an always-on personal assistant. Run
one small process on your own machine, message it over iMessage or Telegram,
and schedule work with plain Markdown runbooks.

It can review repositories before you wake up, watch pull requests, prepare a
daily brief, or pick up a conversation from your phone. You own one portable,
Git-versioned assistant repository containing identity, context, and jobs. Push
owns channels, schedules, history, approvals, security, and result delivery.
Your coding agent owns the intelligence and tools.

[Read the documentation](https://owainlewis.github.io/push/) ·
[Install Push](https://owainlewis.github.io/push/getting-started/) ·
[View releases](https://github.com/owainlewis/push/releases)

## What Push gives you

- **An assistant that is actually available.** Push runs continuously under
  `launchd` or `systemd`, not only while a terminal window is open.
- **The agents you already use.** Keep Claude Code, Codex, or Pi, including
  their model access, tools, MCP servers, skills, login, and backend
  configuration.
- **Conversations from your phone.** Use private iMessage or Telegram chats.
  Each thread keeps its own backend session and canonical history.
- **Work that starts without you.** A five-field cron trigger can run a
  Markdown job and send the stored result back to your primary chat.
- **State you own.** `SOUL.md`, durable context, and installed jobs live in
  one assistant repository. History is local SQLite and configuration is TOML.
- **A small local control layer.** Sender allowlists constrain chat access.
  Agent permissions, tools, and MCP servers stay in the agent configuration.
  Telegram uses outbound long polling and opens no port.

## The model

```text
you by iMessage or Telegram ─┐
                            ├─> Push ─> Claude Code, Codex, or Pi ─> reply to you
cron-triggered Markdown job ┘
```

Push is deliberately not another agent loop. Coding agents are already good
at reading repositories, running tools, and completing technical work. Push
makes one of those agents persistent, reachable, schedulable, and accountable.

The backend can change. Your assistant repository remains portable, while
Push keeps conversations, run history, schedules, and delivery routes durable.

## Push and Hermes Agent

[Hermes Agent](https://hermes-agent.nousresearch.com/docs/) is a broad,
batteries-included autonomous agent platform. Push is a smaller orchestration
layer for people who already want Claude Code, Codex, or Pi to do the work.

| | Push | Hermes Agent |
| --- | --- | --- |
| Product shape | Small gateway and scheduler around external coding agents | Full autonomous agent runtime and platform |
| Agent runtime | Claude Code, Codex, or Pi | Hermes's integrated agent and tool system |
| Main focus | A durable 24/7 assistant over private chat and Markdown jobs | A broad agent environment with many tools, channels, skills, and deployment modes |
| Tools and context | Come from your existing backend configuration | Managed as part of the Hermes ecosystem |
| State | Local TOML, Markdown, JSON, and SQLite owned by Push | Managed by the Hermes runtime |
| Best fit | You trust a coding agent already and want it always available | You want an all-in-one autonomous agent platform |

The projects are not forks and do not try to solve the same layer. Hermes is a
useful choice when breadth and an integrated runtime matter. Push is useful
when you want a narrow, inspectable bridge between your existing coding agent
and the rest of your day.

## What works today

- iMessage on macOS and Telegram private chats on macOS or Linux
- Telegram voice notes with OpenAI transcription and spoken replies
- Claude Code, Codex, and Pi backends, selectable by channel or conversation
- One Git-versioned assistant repository containing `SOUL.md`, `context/`, and
  approved `jobs/`
- Durable conversation history and backend session recovery
- Agent-owned permissions with no gateway sandbox or tool overrides
- Manual and scheduled Markdown jobs with a durable run ledger
- An agent-drafted job workflow that approves the exact revision in chat
- Local structured audit logs with message content redacted by default

Push is early software. Its scope is intentionally smaller than a general
agent platform, and its security depends on a tight sender allowlist plus the
permissions you give your backend. Push does not override agent permissions, so
write access to the assistant repository can also change `SOUL.md` or installed
jobs outside the draft approval workflow.

## Start here

Install the latest available prebuilt release:

```sh
curl -fsSL https://raw.githubusercontent.com/owainlewis/push/main/install.sh | sh
```

The current release publishes archives for Apple Silicon macOS and x86_64
Linux. Other Rust-supported architectures can [build from source](docs/getting-started.md#build-from-source).

Then follow the [quickstart](https://owainlewis.github.io/push/getting-started/)
to create your assistant repository, choose a channel, configure a backend,
run `push doctor`, and keep the gateway online.

To use [Pi](https://pi.dev/), install it, complete provider authentication for
the same service user that runs Push, and set `agent = "pi"`. `pi_bin` defaults
to `pi`. Push stores Pi's reported session ID and resumes that exact session on
later turns; `/clear` starts a new one. Push passes no tool override to Pi, so
review Pi's own configuration before allowing unattended access.

## Documentation

The Markdown under [`docs/`](docs/) is the canonical documentation source. It
builds the [Push documentation site](https://owainlewis.github.io/push/) on
every documentation change to `main`.

- [Quickstart](docs/getting-started.md)
- [Configuration](docs/configuration.md)
- [Jobs and schedules](docs/jobs.md)
- [Permissions and security](docs/security.md)
- [Running as a service](docs/services.md)
- [Architecture](docs/architecture.md)
- [Contributing](docs/contributing.md)

## License

Push is available under the [MIT License](LICENSE).
