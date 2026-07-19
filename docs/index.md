---
title: Push documentation
hide:
  - toc
---

<div class="push-hero" markdown>

<span class="push-kicker">Always-on assistant infrastructure</span>

# Your coding agent, always within reach.

Message and schedule Claude Code, Codex, or Pi from iMessage, Telegram, or
Slack, with durable state on your machine.

<p class="push-actions">
  <a class="md-button md-button--primary" href="getting-started/">Get started</a>
  <a class="md-button" href="architecture/">See how it works&nbsp; →</a>
</p>

<p class="push-install">curl -fsSL https://raw.githubusercontent.com/owainlewis/push/main/install.sh | sh</p>

<ul class="push-signals">
  <li><strong>One small binary</strong>No new agent runtime</li>
  <li><strong>Your machine</strong>State stays under your control</li>
  <li><strong>No inbound port</strong>Private by default</li>
</ul>

</div>

<section class="push-paths" markdown>

<span class="push-section-label">Start here</span>

## Choose a path

<div class="grid cards" markdown>

-   :material-rocket-launch-outline:{ .lg .middle } **Run Push for the first time**

    ---

    Install the binary, connect one channel, configure a backend, and validate
    the setup.

    [:octicons-arrow-right-24: Quickstart](getting-started.md)

-   :material-message-processing-outline:{ .lg .middle } **Connect your chat**

    ---

    Set up private [iMessage](channels/imessage.md), [Telegram](telegram.md), or
    [Slack](slack.md) conversations with narrow sender allowlists.

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

</section>

<section class="push-model" markdown>

<span class="push-section-label">The mental model</span>

## A gateway, not another agent.

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

</section>

<section class="push-doc-map" markdown>

<span class="push-section-label">Documentation</span>

## Find what you need

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

</section>
