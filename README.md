# Push

**Your coding agent, on call 24/7.**

[![CI](https://github.com/owainlewis/push/actions/workflows/ci.yml/badge.svg)](https://github.com/owainlewis/push/actions/workflows/ci.yml)
[![Docs](https://img.shields.io/badge/docs-read-12756f)](https://owainlewis.github.io/push/)
[![License: MIT](https://img.shields.io/badge/license-MIT-111417)](LICENSE)

Push turns Claude Code, Codex, or Pi into an always-on personal assistant. Run
one small process on your own machine, message it over iMessage, Telegram, or Slack,
and schedule work with plain Markdown runbooks.

It can review repositories before you wake up, watch pull requests, prepare a
daily brief, or pick up a conversation from your phone. You own one portable,
Git-versioned assistant repository containing identity, context, skills, and
jobs. Push owns channels, schedule evaluation, history, approvals, security,
and result delivery. Each job file owns its schedule definition. Your coding
agent owns reasoning and execution.

[Read the documentation](https://owainlewis.github.io/push/) ·
[Get started](#quickstart) ·
[View releases](https://github.com/owainlewis/push/releases)

## What Push gives you

- **An assistant that is actually available.** Push runs continuously under
  `launchd` or `systemd`, not only while a terminal window is open.
- **The agents you already use.** Keep Claude Code, Codex, or Pi, including
  their model access, tools, MCP servers, global skills, login, and backend
  configuration.
- **Conversations from your phone.** Use private iMessage, Telegram, or Slack app DMs.
  Each thread keeps its own backend session and canonical history.
- **Work that starts without you.** A five-field cron trigger can run a
  Markdown job and send the stored result back to your primary chat.
- **State you own.** `SOUL.md`, durable context, reusable project skills, and
  installed jobs live in one assistant repository. History is local SQLite and
  configuration is TOML.
- **A small local control layer.** Sender allowlists constrain chat access.
  Agent permissions, tools, and MCP servers stay in the agent configuration.
  Telegram uses outbound long polling and Slack uses outbound Socket Mode. Neither opens a port.

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

## Quickstart

### Requirements

- Apple Silicon macOS or x86_64 Linux for the current prebuilt release
- macOS for iMessage, or macOS/Linux for Telegram
- [Codex](https://developers.openai.com/codex/cli/),
  [Claude Code](https://docs.anthropic.com/en/docs/claude-code/overview), or
  [Pi](https://pi.dev/) installed, authenticated, and runnable by the user that
  will run Push
- Git for the assistant repository created by `push init`
- `curl`, `tar`, and either `shasum` or `sha256sum` for the release installer

First confirm that your chosen backend works. For example:

```sh
codex --version
```

### Install

Install the latest prebuilt release to `~/.local/bin/push`:

```sh
curl -fsSL https://raw.githubusercontent.com/owainlewis/push/main/install.sh | sh
```

The installer verifies the release archive against its published SHA-256
checksum. If `~/.local/bin` is not on `PATH`, add it before continuing.

To build from source instead, install the stable Rust toolchain first:

```sh
git clone https://github.com/owainlewis/push.git
cd push
cargo build --locked --release
mkdir -p ~/.local/bin
install -m 755 target/release/push ~/.local/bin/push
```

### Set up an assistant

Create the assistant repository and default config:

```sh
push init ~/Code/assistant
```

This creates a Git repository containing `SOUL.md`, `AGENTS.md`, `context/`,
portable `skills/`, `evals/`, and `jobs/`, then records its path in
`~/.push/config.toml`. New configs use Telegram and Codex by default. Edit the
config to add your Telegram bot token and numeric user ID:

```toml
channel = "telegram"
agent = "codex"
assistant_root = "~/Code/assistant"

[telegram]
bot_token = "token-from-BotFather"
allow_user_ids = [123456789]
```

For iMessage, use the [iMessage setup guide](docs/channels/imessage.md). For
Slack, use the [Slack direct-message guide](docs/slack.md). For
Claude Code or Pi, set `agent = "claude"` or `agent = "pi"` after confirming
that backend is authenticated for the same user.

Validate the setup, then start the gateway:

```sh
push doctor
push
```

Send a new message after the gateway starts. Telegram discards pending updates
on its first run, so resend any message you used while creating the bot.

## Use Push

Chat messages go to the configured coding agent with the assistant repository
as its working directory. Try a read-only first request:

> Summarize `/absolute/path/to/my-project/README.md`. Do not change anything.

Push leaves sandbox, approval, and tool permissions to the selected backend.
Review [permissions and security](docs/security.md) before allowing unattended
work.

Chat commands:

| Message | Effect |
| --- | --- |
| `/clear`, `/new`, `/reset` | Start a fresh backend session for this conversation |
| `/stop` | Stop the active request; queued messages continue in order |
| `/help` | Show available chat commands |

Jobs are Markdown runbooks stored in the assistant repository. Common commands
are:

```sh
push job validate
push job list
push job run repo-review
push job runs repo-review
```

See [jobs and schedules](docs/jobs.md) for the runbook format and scheduling,
and [running as a service](docs/services.md) to keep Push online with `launchd`
or `systemd`.

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

- iMessage on macOS, plus Telegram private chats and Slack app DMs on macOS or Linux
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

## Contributing

Push is a Rust project. After cloning the repository, run the same core checks
used by CI:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
cargo test --locked
```

Documentation changes should also pass `mkdocs build --strict` after installing
`requirements-docs.txt`. Read the full [contributing guide](docs/contributing.md)
before submitting a pull request.

## License

Push is available under the [MIT License](LICENSE).
