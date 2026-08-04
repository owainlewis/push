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
- `slack.allow_user_ids`

Use stable numeric Telegram IDs. Usernames are mutable and are not accepted as
security identities. Treat a lost phone, shared messaging account, or
compromised allowed sender as access to the assistant.
Slack allowlists use stable member IDs, never mutable display names.

## Agent permissions

Push does not pass sandbox, approval-policy, permission-mode, or tool-list
overrides for chats. The selected agent's own configuration is the permission
source for chat requests.

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

Jobs must complete without an interactive approval channel. Push therefore runs
Codex jobs with full filesystem and network access and no approval prompts, and
runs Claude jobs in `bypassPermissions` mode. Pi already has no native
filesystem sandbox or interactive permission prompt. Evaluators remain
read-only with tools disabled.

This makes activated job bodies equivalent to unattended code execution as the
Push service user. New and changed enabled schedules stay inactive until an
allowlisted owner approves the exact revision and effective execution settings.
The assistant repository is the default work directory. An explicit work
directory must exist. Push rejects overlap with runtime state and a loaded
config stored outside the assistant repository. Keep allowed senders and job
definitions trusted, and run the service with only the OS permissions its jobs
require.

Do not place secrets in a job body. Make them available through the backend or
service environment using the narrowest policy that works.

## Files to protect

| Path | Contains |
| --- | --- |
| `$PUSH_HOME/config.toml` | allowlists, routes, paths, and possibly credentials |
| `<assistant_root>/` | Git-versioned identity, context, evals, jobs, and optional project skills |
| `$PUSH_HOME/state.json` | retained legacy cursor and session recovery copy after migration |
| `$PUSH_HOME/push.db` | conversation history, approvals, jobs, delivery, channel cursors, and backend sessions |
| `$PUSH_HOME/state.json.slack-inbox.db` | Slack envelopes accepted before gateway processing |
| `$PUSH_HOME/audit.jsonl` | metadata, errors, handles, and optional content |
| `$PUSH_HOME/run/` | local job lock files |
| `$PUSH_HOME/cache/` | disposable agent handoff files |

Keep them on local durable storage with permissions restricted to the service
user. Keep the assistant directory in its own private Git repository. Never
put real config secrets, state, audit logs, or databases in
that repository. An explicit `assistant_root` config stored inside it cannot
contain inline Telegram, Slack, or OpenAI credentials; use the matching environment
variable or move the config outside. When `voice.openai_api_key` is configured,
`push doctor` requires the config file to be private on Unix:

```sh
chmod 600 "${PUSH_HOME:-$HOME/.push}/config.toml"
```

Accepted Telegram and Slack images are briefly written under `$PUSH_HOME/cache`
with owner-only permissions, passed to the selected agent backend, and
removed when the turn ends. Conversation history retains only the caption or an
image placeholder. Protect the cache directory while Push is running and
review the configured model provider's image data controls.

Slack's recovery inbox persists file IDs and safe size and MIME type hints, but
not private download URLs or file bytes. Push resolves and downloads a Slack
file with the bot token only after workspace, direct-message, and sender
allowlist checks pass. Rejected events retain no message or attachment content.

Push rejects any overlap between `assistant_root` and `PUSH_HOME`, including
overlap through symlinks or existing ancestors. Keep the runtime root private
to Push and keep the assistant as a separate user-owned Git repository.

## Network exposure

Push opens no inbound server port. iMessage reads local state. Telegram uses
outbound HTTPS long polling and Slack uses an authenticated outbound Socket
Mode WebSocket. Neither needs a webhook.
This reduces exposure, but it does not make an allowed message harmless.

## Durable questions

Bounded `ask_user` questions are stored before delivery, survive restart,
expire, and can be consumed once. Mismatched, duplicate, ambiguous, cancelled,
and expired answers do not reach an agent. Direct job-file creation does not
use this mechanism. Enabled schedule activation does: the review binds the
exact allowlisted channel identity to the content hash, file identity,
validated schedules, effective backend, timeout, work directory, and delivery
target. Revalidation and scheduler claims fail closed when those values no
longer match. Schedule reviews require the question UUID with the answer
number; uncorrelated numbers cannot approve a replacement question.

## Audit log

Push writes structured JSONL events to `audit_log_path`. By default events
include metadata such as row ID, channel, thread, backend, decision, target,
error, and character counts. Message and reply content are omitted unless
`audit_log_content = true`.

Schedule lifecycle events use a durable SQLite outbox. Push syncs each JSONL
event before marking it delivered and retries pending events after a write
failure or restart. A crash after append but before acknowledgement can produce
a duplicate with the same `event_id`; consumers should deduplicate that ID.

The redacted log is still sensitive because it can contain handles, thread
IDs, file paths, and backend errors. Protect and rotate it like a service log.

The legacy state migration repairs the JSON file to owner-only permissions and
does not delete or rewrite it. Keep that recovery copy private until your
normal encrypted backup process has captured the migrated `push.db`. The
database is the live source of truth after migration, so backing up only
`state.json` is not sufficient.

## Deployment checklist

- allow only identities you control
- configure the selected agent for unattended use
- run `push doctor` as the service user
- keep agent credentials out of TOML and all credentials out of logs
- protect config credentials with mode `0600` on Unix
- set one absolute `PUSH_HOME` in service files
- keep Push state and job work directories separate
- review agent-authored job changes in the assistant repository
- review the audit log after routing or agent permission changes
