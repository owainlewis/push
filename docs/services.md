# Running Push as a Service

This guide covers running `push` continuously under a process manager.

The iMessage channel is macOS-only because it reads
`~/Library/Messages/chat.db` and sends replies with `osascript`. Telegram uses
outbound HTTPS long polling. Slack uses outbound Socket Mode. Both can run
under `systemd` on Linux or a VM.

## Before Installing a Service

Build or install `push`, then run doctor from the same user account that will
own the service:

```sh
push init ~/Code/assistant
# Edit $PUSH_HOME/config.toml with your channel settings.
push doctor
```

Set one absolute `PUSH_HOME` in the service definition. It defaults to
`~/.push` for interactive commands. The service user needs:

- read and write access to `PUSH_HOME`
- access to the configured `config.toml`
- read access and owner control of an existing legacy `state_path` for migration
- write access to `audit_log_path`
- write access to `database_path`
- write access to `jobs_run_dir`
- filesystem access to `assistant_root` as allowed by the selected agent
- agent write access to `assistant_root/jobs/` when jobs should be created from chat
- access to the selected `claude`, `codex`, or `pi` executable on `PATH`
- backend login, tokens, settings, MCP config, and project credentials
- for iMessage on macOS, Full Disk Access and `osascript`
- for Telegram, a token in the private config and network access to
  `api.telegram.org`
- for Slack, app and bot tokens in the private config or service environment,
  plus network access to `slack.com`
- for optional voice messages, `voice.openai_api_key` in the private config or
  `OPENAI_API_KEY` in the service environment, plus network access to
  `api.openai.com`

`database_path` stores the canonical conversation journal, channel cursors, and
backend session mappings. `state_path` is only a legacy JSON migration source
and retained recovery copy; Push does not write live state to it. The audit
log, Slack recovery inbox, job locks, and cache remain separate paths derived
from `PUSH_HOME`, unless their documented compatibility settings override
them. Chat agents run from `assistant_root`. Keep these paths on durable
storage. Restarting the service resumes after the last completed row and
reuses existing backend sessions when the backend for that thread has not
changed.

Keep `assistant_root` in its own Git repository. Keep config secrets, state,
databases, logs, locks, and service credentials outside it.

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
  </array>

  <key>WorkingDirectory</key>
  <string>/Users/YOU/.push</string>

  <key>EnvironmentVariables</key>
  <dict>
    <key>PUSH_HOME</key>
    <string>/Users/YOU/.push</string>
    <key>PATH</key>
    <string>/Users/YOU/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
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

After editing `~/.push/config.toml`, restart the gateway with:

```sh
push reload
```

After changing the plist:

```sh
launchctl bootout gui/$(id -u)/com.owainlewis.push
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.owainlewis.push.plist
launchctl kickstart -k gui/$(id -u)/com.owainlewis.push
```

For voice support, prefer `voice.openai_api_key` in the private Push config. An
`OPENAI_API_KEY` entry in `EnvironmentVariables` remains available as an
override when service-level secret injection is preferred.

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
ExecStart=%h/.local/bin/push
WorkingDirectory=%h/.push
Restart=on-failure
RestartSec=10
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin
Environment=PUSH_HOME=%h/.push
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

After editing `~/.push/config.toml`, restart the gateway with:

```sh
push reload
```

For voice support, prefer `voice.openai_api_key` in `~/.push/config.toml`. As an
alternative, create the optional private environment file:

```sh
printf 'OPENAI_API_KEY=replace-with-your-openai-api-key\n' > ~/.config/push/env
chmod 600 ~/.config/push/env
systemctl --user restart push.service
```

Keep `~/.push/config.toml` at mode `0600` because it may contain messaging or
OpenAI credentials. Do not commit this file or print it in
service logs.

For a user service that survives logout, enable lingering:

```sh
loginctl enable-linger "$USER"
```

## Manual Jobs

`push job run <name>` executes in the invoking terminal process, not in the
managed service. Use the same `PUSH_HOME` so the CLI and service share
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

## Backup and state migration recovery

Stop the managed service before taking a filesystem copy of `push.db`, or use
SQLite's online backup tooling. The database contains conversation history,
job and delivery state, channel cursors, and backend session mappings. Back up
the audit log and assistant repository separately. Slack's durable inbox stays
in `<state_path>.slack-inbox.db`; include it when preserving unprocessed Slack
events.

After an upgrade, Push imports an existing configured `state_path` in one
transaction and leaves that JSON file unchanged. Keep it as a private recovery
copy until a verified database backup exists. If migration fails, fix or
restore the JSON and restart; Push will not poll while the import is
incomplete. If the post-migration database is lost, restore `push.db` from
backup. As a last resort, move the unusable database aside and restart with the
retained JSON to recover its older cursors and sessions, understanding that
conversation, job, and delivery records not present in JSON will be absent.

## Agent-created jobs

When asked, the agent writes jobs directly under `<assistant_root>/jobs` and
runs `push job validate`. There is no approval step. The agent's configuration
decides whether it may write to the assistant repository.

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

## Backup and Recovery

Stop the service before taking a filesystem-level backup. Back up the complete
`PUSH_HOME` directory as one unit so config, cursors, the Slack inbox, canonical
history, audit events, and job delivery state stay consistent. Back up
`assistant_root` separately through its Git repository because it is
user-owned and must not live under `PUSH_HOME`.

To restore, stop the service, restore both locations to separate directories,
set the service `PUSH_HOME` to the restored runtime root, confirm
`assistant_root` in the restored config, then run `push doctor` before starting
the service. If a compatible older config sets `state_path`, `database_path`,
`audit_log_path`, or `jobs_run_dir`, back up and restore those explicit
locations too. The cache directory is disposable and can be omitted.

## Security Notes

Managed services run without a person watching the terminal. An allowed sender
can instruct the configured backend to use its tools, subject to your backend
settings. Keep `imessage.allow_from` narrow and configure each selected agent
for unattended use. Push preserves backend permissions for chats. Codex and
Claude jobs bypass interactive permissions so they can finish without an
operator. Jobs are kept away from Push-owned paths by work-directory validation.

Store config files, state files, audit logs, backend credentials, and service
logs with permissions appropriate for the service user. Logs may contain
prompts, backend errors, file paths, handles, or message text when content
logging is enabled.
