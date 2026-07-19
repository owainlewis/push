---
title: Push documentation
hide:
  - toc
---

<div class="push-hero" markdown>

<span class="push-kicker">Open source · Runs on your machine</span>

# Your coding agent has a phone now.

Text Claude Code, Codex, or Pi from iMessage, Telegram, or Slack. Ask for work,
walk away, and get the result back in chat.

<p class="push-actions">
  <a class="md-button md-button--primary" href="getting-started/">Build your assistant</a>
  <a class="md-button" href="#what-can-it-do">See what it can do&nbsp; ↓</a>
</p>

<p class="push-install">curl -fsSL https://raw.githubusercontent.com/owainlewis/push/main/install.sh | sh</p>

<ul class="push-signals">
  <li><strong>Use your existing agent</strong>No new model stack</li>
  <li><strong>Your machine</strong>Push state stays under your control</li>
  <li><strong>Beyond the terminal</strong>Available from the chat apps you use</li>
</ul>

</div>

<section class="push-demo" markdown>

<div class="push-section-heading" markdown>

<span class="push-section-label">A task in Push</span>

## Send the work. Get on with your day.

Push carries the conversation between your phone and the coding agent already
set up on your machine.

</div>

<div class="push-chat" markdown="0">
<div class="push-chat-bar"><span>Telegram</span><span><i></i> Push is online</span></div>
<div class="push-chat-body">
<div class="push-chat-message push-chat-message--user"><span>You · 08:42</span><p>Check why the nightly build failed and send me the likely fix.</p></div>
<div class="push-chat-status"><span>Push → Codex</span><span>Working on your machine</span></div>
<div class="push-chat-message push-chat-message--assistant"><span>Push · 08:47</span><p>The failure comes from an expired fixture in the integration suite. I prepared the change and ran the focused tests: 18 passed.</p></div>
</div>
<div class="push-chat-footer"><span>Delivered in chat</span><span>Conversation saved</span></div>
</div>

</section>

<section class="push-outcomes" id="what-can-it-do" markdown>

<span class="push-section-label">Your personal assistant</span>

## Useful work, without reopening the terminal.

<div class="push-use-cases" markdown="0">
  <article>
    <span>01</span>
    <h3>Start work from anywhere</h3>
    <p>Ask your agent to inspect a repository, investigate a failure, or prepare an update while you are away from your desk.</p>
  </article>
  <article>
    <span>02</span>
    <h3>Wake up to a useful brief</h3>
    <p>Run Markdown routines every morning, evening, or week and deliver the result automatically to your primary chat.</p>
  </article>
  <article>
    <span>03</span>
    <h3>Keep the conversation moving</h3>
    <p>Continue across messages with durable history instead of rebuilding context every time you open a terminal.</p>
  </article>
  <article>
    <span>04</span>
    <h3>Bring the tools you trust</h3>
    <p>Use the MCP servers, skills, permissions, and integrations already configured in Claude Code, Codex, or Pi.</p>
  </article>
</div>

</section>

<section class="push-model" markdown>

<span class="push-section-label">How it works</span>

## Your agent leaves the terminal.

<div class="push-steps" markdown="0">
  <div><span>01</span><strong>Send a message</strong><p>Use iMessage, Telegram, or Slack from wherever you are.</p></div>
  <div><span>02</span><strong>Your agent does the work</strong><p>Push restores the conversation and hands the task to your chosen coding agent.</p></div>
  <div><span>03</span><strong>The answer comes back</strong><p>Push saves the result and delivers it to the same chat.</p></div>
</div>

Push is a gateway, not another agent runtime. You own a Git-versioned assistant
repository containing identity, context, and jobs. Your backend continues to
own models, tools, MCP servers, skills, permissions, and authentication.

[See the full architecture](architecture.md){ .push-inline-link }

</section>

<section class="push-paths" markdown>

<span class="push-section-label">Set up Push</span>

## Build your assistant

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

-   :material-account-cog-outline:{ .lg .middle } **Design your assistant**

    ---

    Shape identity, durable context, reusable skills, jobs, and evaluation
    criteria without duplicating instructions or committing secrets.

    [:octicons-arrow-right-24: Design the repository](designing-an-assistant.md)

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

<section class="push-doc-map" markdown>

<span class="push-section-label">Documentation</span>

## Find what you need

| If you need to… | Read… |
| --- | --- |
| install and run one working channel | [Quickstart](getting-started.md) |
| design identity, context, skills, and jobs | [Designing an assistant](designing-an-assistant.md) |
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
