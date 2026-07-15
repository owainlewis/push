# Running Push as a Service

This guide covers running `push` continuously under a process manager.

The iMessage channel is macOS-only because it reads
`~/Library/Messages/chat.db` and sends replies with `osascript`. Telegram uses
outbound HTTPS long polling and can run under `systemd` on Linux or a VM.

## Before Installing a Service

Build or install `push`, then run doctor from the same user account that will
own the service:

```sh
push init ~/Code/assistant
# Edit ~/.push/config.toml with your channel settings.
push doctor
```

Use absolute paths in service files. The service user needs:

- access to the configured `config.toml`
- write access to `state_path`
- write access to `audit_log_path`
- write access to `database_path`
- filesystem access to `assistant_root` as allowed by the selected agent
- write access to `assistant_root/jobs/` for Push to install approved drafts;
  agent write access can also change installed jobs outside that workflow
- access to the selected `claude`, `codex`, or `pi` executable on `PATH`
- backend login, tokens, settings, MCP config, and project credentials
- for iMessage on macOS, Full Disk Access and `osascript`
- for Telegram, a token in the private config and network access to
  `api.telegram.org`
- for optional voice messages, `OPENAI_API_KEY` in the service environment and
  network access to `api.openai.com`

`state_path` stores independent cursors for each channel and backend session
ids. `database_path` stores the canonical conversation journal. Chat agents run
from `assistant_root`. Keep these paths on durable storage. Restarting the
service resumes after the last completed row and reuses existing backend
sessions when the backend for that thread has not changed.

Keep `assistant_root` in its own Git repository. Keep config secrets, state,
databases, drafts, logs, locks, and service credentials outside it.

## macOS launchd

Create the log directory:

```sh
mkdir -p ~/Library/Logs
```

Create `~/Library/LaunchAgents/com.owainlewis.push.plist`. You can start from
[`examples/launchd/com.owainlewis.push.plist`](https://github.com/owainlewis/push/blob/main/examples/launchd/com.owainlewis.push.plist)
and replace `YOU` with your macOS user name:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.owainlewis.push</string>

  <key>ProgramArguments</key>
  <array>
    <string>/Users/YOU/.local/bin/push</string>
    <string>--config</string>
    <string>/Users/YOU/.push/config.toml</string>
  </array>

  <key>WorkingDirectory</key>
  <string>/Users/YOU/.push</string>

  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/Users/YOU/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
    <key>OPENAI_API_KEY</key>
    <string>replace-with-your-openai-api-key</string>
  </dict>

  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>

  <key>StandardOutPath</key>
  <string>/Users/YOU/Library/Logs/push.out.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/YOU/Library/Logs/push.err.log</string>
</dict>
</plist>
```

Load and inspect it:

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.owainlewis.push.plist
launchctl enable gui/$(id -u)/com.owainlewis.push
launchctl kickstart -k gui/$(id -u)/com.owainlewis.push
launchctl print gui/$(id -u)/com.owainlewis.push
tail -f ~/Library/Logs/push.err.log ~/Library/Logs/push.out.log
```

After changing the plist:

```sh
launchctl bootout gui/$(id -u)/com.owainlewis.push
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.owainlewis.push.plist
launchctl kickstart -k gui/$(id -u)/com.owainlewis.push
```

The plist contains the optional OpenAI key, so protect it with
`chmod 600 ~/Library/LaunchAgents/com.owainlewis.push.plist`. Omit the key when
you do not want voice support.

## Linux systemd

Use this for Telegram-only deployments. The iMessage channel still requires
macOS.

Create the service directories:

```sh
mkdir -p ~/.config/push ~/.config/systemd/user ~/.push
```

Create `~/.config/systemd/user/push.service`. You can start from
[`examples/systemd/push.service`](https://github.com/owainlewis/push/blob/main/examples/systemd/push.service):

```ini
[Unit]
Description=Push personal assistant gateway
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=%h/.local/bin/push --config %h/.push/config.toml
WorkingDirectory=%h/.push
Restart=on-failure
RestartSec=10
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin
EnvironmentFile=-%h/.config/push/env

[Install]
WantedBy=default.target
```

Load and inspect it:

```sh
systemctl --user daemon-reload
systemctl --user enable --now push.service
systemctl --user status push.service
journalctl --user -u push.service -f
```

For voice support, create the optional private environment file:

```sh
printf 'OPENAI_API_KEY=replace-with-your-openai-api-key\n' > ~/.config/push/env
chmod 600 ~/.config/push/env
systemctl --user restart push.service
```

Keep `~/.push/config.toml` at mode `0600` because it contains the Telegram bot
token. Do not commit this file or print it in service logs.

For a user service that survives logout, enable lingering:

```sh
loginctl enable-linger "$USER"
```

## Manual Jobs

`push job run <name>` executes in the invoking terminal process, not in the
managed service. Use the same config file so the CLI and service share
`push.db`, `<assistant_root>/jobs`, and the local per-job lock directory.
Invalid job files are reported and disabled individually; they do not stop the
messaging service.

## Scheduled Jobs

Cron triggers run inside the managed gateway only when `primary_delivery`
resolves. Keep `push.db`, `<assistant_root>/jobs`, and `jobs_run_dir` on
persistent local storage. Restarting the service resumes queued runs and
pending result delivery; it does not catch up missed cron times or rerun
interrupted agent execution. Use `push job runs` to distinguish execution state
from delivery attempts.

## Drafted Jobs

The service prepares `drafts_dir` and the derived assistant `jobs/` directory
with owner-only permissions. Push exposes only the route's origin-specific
drafts inbox, and the agent's configuration decides whether it may write there.
Proposals remain inactive until the exact revision
is approved from its originating allowlisted channel identity. Pending
questions survive service restart.

## Restart Behavior

Push only advances the selected channel cursor after a message is ignored or
completed. If the process stops during an in-flight backend run, that message
can be retried after restart. This avoids silently losing accepted messages,
but it can repeat backend work if the process stops before the result is
persisted. If an outbound reply is already stored, restart delivers that exact
reply without generating a different second response.

Ignored messages, completed rows, and setup failures advance the cursor. Rows
newer than an in-flight row do not push the cursor past it until the earlier row
is completed.

## Security Notes

Managed services run without a person watching the terminal. An allowed sender
can instruct the configured backend to use its tools, subject to your backend
settings. Keep `imessage.allow_from` narrow and configure each selected agent
for unattended use. Push passes no sandbox, approval, or tool overrides. Jobs
are kept away from Push-owned paths by work-directory validation.

Store config files, state files, audit logs, backend credentials, and service
logs with permissions appropriate for the service user. Logs may contain
prompts, backend errors, file paths, handles, or message text when content
logging is enabled.
