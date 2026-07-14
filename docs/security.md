# Permissions and security

Push turns messages and schedules into agent runs. That is useful precisely
because the backend can access local tools and data, so configuration is a
security boundary rather than a convenience setting.

## Trust model

An accepted sender is an operator of the configured backend. They may be able
to read files, edit repositories, run commands, call MCP servers, or use local
credentials, depending on the selected agent's settings.

Keep these allowlists narrow:

- `imessage.self_handles` and `imessage.allow_from`
- `telegram.allow_user_ids` and `telegram.allow_chat_ids`

Use stable numeric Telegram IDs. Usernames are mutable and are not accepted as
security identities. Treat a lost phone, shared messaging account, or
compromised allowed sender as access to the assistant.

## Agent permissions

Push does not pass sandbox, approval-policy, permission-mode, or tool-list
overrides to Claude Code, Codex, or Pi. The selected agent's own configuration
is the sole permission source for chats and jobs.

This makes Push behave like the agent you already configured, but it also means
every agent-approved capability may be one accepted message away. Review the
agent's tools, MCP servers, filesystem access, shell access, and unattended
approval behavior before running Push as a service. Pi has no native filesystem
sandbox or interactive permission prompt, so its configured tools deserve
particular care.

Push rejects old `permission_profile`, `permission_profiles`, and raw backend
permission fields with a migration error. Remove those keys and configure the
selected agent instead.

## Job permissions

Jobs use the selected agent's own permission configuration. Push requires a
fixed, existing work directory and rejects overlap with Push-owned files,
including the loaded config file.

Do not place secrets in a job body. Make them available through the backend or
service environment using the narrowest policy that works.

## Files to protect

| Path | Contains |
| --- | --- |
| `config.toml` | allowlists, routes, paths, and possibly credentials |
| `<assistant_root>/` | Git-versioned `SOUL.md`, durable context, and installed jobs |
| `~/.push/state.json` | channel cursors and backend session IDs |
| `~/.push/push.db` | conversation history, approvals, and job runs |
| `~/.push/audit.jsonl` | metadata, errors, handles, and optional content |
| `~/.push/drafts/` | inactive agent-authored proposals |

Keep them on local durable storage with permissions restricted to the service
user. Keep the assistant directory in its own private Git repository. Never
put real config secrets, state, audit logs, or databases in
that repository. An explicit `assistant_root` config stored inside it cannot
contain an inline Telegram token; use `telegram.bot_token_env` or move the
config outside.

## Network exposure

Push opens no inbound server port. iMessage reads local state. Telegram uses
outbound HTTPS long polling against the Bot API and does not need a webhook.
This reduces exposure, but it does not make an allowed message harmless.

## Durable approval

Agent-drafted jobs remain inactive until the exact revision is approved from
the originating allowed identity. Approval questions are stored before
delivery, survive restart, expire, and can be consumed once. Mismatched,
duplicate, ambiguous, cancelled, and expired answers do not reach an agent.
This protects proposals submitted through Push's drafts directory. It is not a
filesystem sandbox. If the selected agent can write to `assistant_root`, it can
also change `SOUL.md` or installed jobs directly. Restrict that access in the
agent's own configuration when the approval boundary must be enforced.

## Audit log

Push writes structured JSONL events to `audit_log_path`. By default events
include metadata such as row ID, channel, thread, backend, decision, target,
error, and character counts. Message and reply content are omitted unless
`audit_log_content = true`.

The redacted log is still sensitive because it can contain handles, thread
IDs, file paths, and backend errors. Protect and rotate it like a service log.

## Deployment checklist

- allow only identities you control
- configure the selected agent for unattended use
- run `push doctor` as the service user
- keep backend credentials out of TOML and logs
- use absolute config paths in service files
- keep Push state and job work directories separate
- inspect agent-authored jobs before approving them
- review the audit log after routing or agent permission changes
