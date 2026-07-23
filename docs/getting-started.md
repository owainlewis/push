# Quickstart

This guide gets one private chat working with one coding-agent backend. Start
with Telegram, Slack, or Missive on macOS or Linux, or iMessage on macOS. Add multiple channels,
routes, and scheduled jobs after the basic path passes `push doctor`.

## 1. Check the requirements

You need:

- Apple Silicon macOS or x86_64 Linux for the current prebuilt release
- macOS for iMessage, or macOS/Linux for Telegram, Slack, or Missive
- Claude Code, Codex, or Pi installed, authenticated, and runnable by the same
  user that will run Push
- Git for the assistant repository created by `push init`
- `curl`, `tar`, and either `shasum` or `sha256sum` for the release installer

Push uses the backend's existing login, settings, tools, MCP servers, global
skills, and backend configuration. Each chat runs from `assistant_root`, so the
backend can discover project instructions and repository-scoped skills and work
with the assistant's context directly. Confirm the selected command works
before starting Push:

=== "Codex"

    ```sh
    codex --version
    ```

=== "Claude Code"

    ```sh
    claude --version
    ```

=== "Pi"

    ```sh
    pi --version
    ```

## 2. Install Push

On Apple Silicon macOS or x86_64 Linux, install the latest prebuilt release:

```sh
curl -fsSL https://raw.githubusercontent.com/owainlewis/push/main/install.sh | sh
```

The installer verifies the archive against its published SHA-256 checksum
before extracting it. On macOS, it then clears the downloaded binary's
provenance restriction so the verified command can run.

The binary goes to `~/.local/bin` by default. Add that directory to `PATH` if
your shell does not already include it. The installer recognizes Intel macOS
and ARM Linux, but it exits unless the latest GitHub release contains a matching
archive.

## Build from source

Use this path on other Rust-supported architectures or when testing `main`:

```sh
git clone https://github.com/owainlewis/push.git
cd push
cargo build --locked --release
mkdir -p ~/.local/bin
install -m 755 target/release/push ~/.local/bin/push
```

## 3. Create your assistant repository

```sh
push init ~/Code/assistant
```

Push creates one Git-versioned repository containing `SOUL.md`, shared
instructions in `AGENTS.md`, a `CLAUDE.md` reference to those instructions,
`README.md`, `context/`, and empty `evals/` and `jobs/` directories. It records
the canonical root in the selected config file. A new config starts with
Telegram, Codex, and an empty `telegram.allow_user_ids` list that you must fill
in. Edit `SOUL.md` to define identity and operating style, then add durable
user context under `context/`.
Push reads these files at run time and never writes machine-specific paths into
the repository. Read [Designing an assistant](designing-an-assistant.md) for a
practical structure for identity, context, shared skills, jobs, and evals.

## 4. Configure a channel

=== "Telegram"

    Create a bot with Telegram's `@BotFather`, send it one message, and find
    your stable numeric user ID. Then edit `~/.push/config.toml`:

    ```toml
    channel = "telegram"
    agent = "codex"
    assistant_root = "~/Code/assistant"

    [telegram]
    bot_token = "token-from-BotFather"
    allow_user_ids = [123456789]
    ```

    Read the [Telegram guide](telegram.md) for token storage, allowlisting,
    topics, and first-run cursor behavior.

=== "iMessage"

    Give the terminal or service host Full Disk Access in macOS System
    Settings, then edit `~/.push/config.toml`:

    ```toml
    channel = "imessage"
    agent = "codex"
    assistant_root = "~/Code/assistant"

    [imessage]
    self_handles = ["you@icloud.com"]
    ```

    `self_handles` is for a private conversation with yourself. Use
    `allow_from` to accept one-to-one messages from another trusted handle.
    Read the [iMessage guide](channels/imessage.md) for database permissions
    and filtering behavior.

=== "Slack"

    Create a Slack app with Socket Mode, `connections:write`, `im:history`,
    `chat:write`, and the `message.im` bot event. Set the two tokens in the
    service environment, then edit `~/.push/config.toml`:

    ```toml
    channel = "slack"
    agent = "codex"
    assistant_root = "~/Code/assistant"

    [slack]
    allow_user_ids = ["U012ABCDEF"]
    ```

    Read the [Slack guide](slack.md) for app setup, scopes, token storage,
    filtering, and recovery behavior.

=== "Missive"

    Create a Missive API token, choose one private conversation, and collect
    the stable IDs for that conversation and the trusted comment author. Put
    the token in the service environment, then edit `~/.push/config.toml`:

    ```toml
    channel = "missive"
    agent = "codex"
    assistant_root = "~/Code/assistant"
    poll_interval = "3s"

    [missive]
    conversation_ids = ["00000000-0000-0000-0000-000000000000"]
    allow_user_ids = ["00000000-0000-0000-0000-000000000000"]
    ```

    Set `MISSIVE_API_TOKEN` for the service. Read the [Missive
    guide](missive.md) for the comments-only command model, rate limit,
    allowlists, and first-run behavior.

Replace `codex` with `claude` for Claude Code or `pi` for Pi. Pi must already
have a configured model provider or authenticated account for the service user.

If you replace the config file created by `push init`, keep its
`assistant_root` setting. Running the same init command again is safe for a
complete assistant repository and restores the setting without overwriting
user files.

## 5. Validate and run

```sh
push doctor
push
```

Send a new message after the gateway starts. Telegram deliberately discards
the pending backlog on first run, so an older setup message will not execute.

Try:

> Summarize `/absolute/path/to/my-project/README.md`. Do not change anything.

Replace the example path with a file the service user can read. Push does not
override the agent's sandbox, approval mode, or tool list. The selected agent's
configuration decides what the request can do. Read [permissions and
security](security.md) before running the gateway unattended.

## 6. Keep it online

A foreground process stops when its terminal closes. Follow [run as a
service](services.md) to install Push under `launchd` on macOS or `systemd` for
a Telegram- or Slack-only Linux host.

## Next steps

- [Design your assistant repository](designing-an-assistant.md)
- [Configure both channels and per-thread routes](configuration.md)
- [Create a manual or scheduled job](jobs.md)
- [Inspect every CLI command](reference/cli.md)
- [Understand durable state and recovery](architecture.md)
