# Permissions and security

Push turns messages and schedules into agent runs. That is useful precisely
because the backend can access local tools and data, so configuration is a
security boundary rather than a convenience setting.

## Trust model

An accepted sender is an operator of the configured backend. They may be able
to read files, edit repositories, run commands, call MCP servers, or use local
credentials, depending on the route's permission profile and backend settings.

Keep these allowlists narrow:

- `imessage.self_handles` and `imessage.allow_from`
- `telegram.allow_user_ids` and `telegram.allow_chat_ids`

Use stable numeric Telegram IDs. Usernames are mutable and are not accepted as
security identities. Treat a lost phone, shared messaging account, or
compromised allowed sender as access to the assistant.

## Chat permission profiles

| Profile | Claude Code | Codex | Intended use |
| --- | --- | --- | --- |
| `restricted` | Read, Grep, and Glob; Bash and write tools denied | read-only sandbox, approvals disabled | Default inspection and conversation |
| `workspace` | Read and file-edit tools; Bash omitted | workspace-write sandbox, approvals disabled | Contained repository edits and job drafts |
| `inherit` | No Push mode or tool filters | No Push sandbox override | Defer to the operator's backend configuration |
| `full-access` | Backend bypass mode | Backend bypass mode | Rejected for unattended chat routes |

Claude Code does not expose a Codex-equivalent filesystem sandbox. Its
`workspace` mapping allows file tools but deliberately omits shell access.

These profiles control the local filesystem and process capabilities that Push
passes to the backend. They do not rewrite the backend's own integration
configuration. In particular, Codex MCP servers remain available with whatever
read or write capabilities their configuration grants. Audit or disable those
servers before treating a `restricted` or `workspace` route as safe for an
untrusted request.

`inherit` can be the right choice when the backend already has a carefully
designed unattended policy. It can also make every backend-approved tool one
text message away. A headless run cannot pause for an interactive approval.

Custom profiles only alias a capability:

```toml
[permission_profiles.repo-editor]
capability = "workspace"
```

Push rejects raw Claude or Codex permission flags in TOML so a route cannot
quietly bypass the shared local policy model.

## Job permissions

Jobs always inherit the backend's own permission configuration. They do not
select a Push permission profile. Push compensates by requiring a fixed,
existing work directory and rejecting overlap with Push-owned files, including
the loaded config file.

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
| `~/.push/sessions/` | per-thread backend workspaces |
| `~/.push/drafts/` | inactive agent-authored proposals |

Keep them on local durable storage with permissions restricted to the service
user. Keep the assistant directory in its own private Git repository. Never
put real config secrets, state, audit logs, session workspaces, or databases in
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

## Audit log

Push writes structured JSONL events to `audit_log_path`. By default events
include metadata such as row ID, channel, thread, backend, decision, target,
error, and character counts. Message and reply content are omitted unless
`audit_log_content = true`.

The redacted log is still sensitive because it can contain handles, thread
IDs, file paths, and backend errors. Protect and rotate it like a service log.

## Deployment checklist

- allow only identities you control
- start with `restricted`
- run `push doctor` as the service user
- keep backend credentials out of TOML and logs
- use absolute config paths in service files
- keep Push state and job work directories separate
- inspect agent-authored jobs before approving them
- review the audit log after routing or permission changes
