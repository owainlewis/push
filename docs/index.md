---
title: Push documentation
hide:
  - toc
---

<div class="push-hero" markdown>

<span class="push-kicker">Always-on assistant infrastructure</span>

# Put your coding agent on call.

Push turns Claude Code, Codex, or Pi into a personal assistant you can message and
schedule. It runs on your machine, keeps durable state, and sends results back
to iMessage or Telegram.

[Get started](getting-started.md){ .md-button .md-button--primary }
[See the architecture](architecture.md){ .md-button }

</div>

## Choose a path

<div class="grid cards" markdown>

-   :material-rocket-launch-outline:{ .lg .middle } **Run Push for the first time**

    ---

    Install the binary, connect one channel, configure a backend, and validate
    the setup.

    [:octicons-arrow-right-24: Quickstart](getting-started.md)

-   :material-message-processing-outline:{ .lg .middle } **Connect your chat**

    ---

    Set up private [iMessage](channels/imessage.md) or
    [Telegram](telegram.md) conversations with narrow sender allowlists.

    [:octicons-arrow-right-24: Configure channels](configuration.md#channels)

-   :material-calendar-clock:{ .lg .middle } **Automate recurring work**

    ---

    Write Markdown runbooks, run them manually, or add cron triggers and send
    stored results to your primary chat.

    [:octicons-arrow-right-24: Jobs and schedules](jobs.md)

-   :material-server-security:{ .lg .middle } **Operate it continuously**

    ---

    Choose permissions, inspect local state, and run Push under `launchd` or
    `systemd`.

    [:octicons-arrow-right-24: Operations guide](services.md)

</div>

## The mental model

Push is a gateway, not an agent runtime:

```text
message or cron trigger
        ↓
Push: filter → route → persist → schedule → deliver
        ↓
Claude Code, Codex, or Pi: reason → use tools → produce result
```

You own one Git-versioned assistant repository containing identity, context,
and jobs. Push owns channels, history, scheduling, approvals, security, and
delivery. The selected backend owns models, tools, MCP servers, skills, and
authentication. That boundary keeps Push small and lets the backend change
without rebuilding your assistant.

## Documentation map

| If you need to… | Read… |
| --- | --- |
| install and run one working channel | [Quickstart](getting-started.md) |
| understand every TOML setting | [Configuration](configuration.md) |
| add recurring or manual work | [Jobs and schedules](jobs.md) |
| choose backend permissions safely | [Permissions and security](security.md) |
| keep Push online after logout or reboot | [Run as a service](services.md) |
| inspect commands and outputs | [CLI reference](reference/cli.md) |
| understand or extend the code | [Architecture](architecture.md) and [contributing](contributing.md) |

!!! note "Canonical source"

    These pages are generated directly from the Markdown in the repository's
    `docs/` directory. If the site and source ever disagree, update the
    Markdown source and rebuild the site.
