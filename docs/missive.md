# Missive

Push can use comments in explicitly allowlisted Missive conversations as a
private command channel. It polls Missive's REST API over outbound HTTPS and
returns each assistant reply as a Missive post in the same conversation. It
does not expose a webhook, send email, or create a draft.

## Configure access

Create a Missive API token for the account that will run Push, then put it in
the service environment:

```sh
export MISSIVE_API_TOKEN="..."
```

Choose the exact conversation IDs Push may read and the exact Missive user IDs
whose comments may invoke the assistant:

```toml
channel = "missive"
agent = "codex"
assistant_root = "~/Code/assistant"
poll_interval = "3s"

[missive]
conversation_ids = ["00000000-0000-0000-0000-000000000000"]
allow_user_ids = ["00000000-0000-0000-0000-000000000000"]
```

You may set `missive.api_token` in a private config instead, but the environment
variable is safer for a service. Push rejects an inline Missive token when the
config is inside the Git-versioned assistant repository.

Missive limits continuous polling to one request per second. Push makes one
request per configured conversation during a normal poll, so
`poll_interval` must be at least one second for every configured conversation.
For example, three conversations require `poll_interval = "3s"` or longer.

## Message behavior

- Only new comments are commands. Other email or chat activity is ignored.
- Both the conversation ID and comment author ID must be allowlisted.
- The first successful startup records current comments and does not replay
  them into the assistant.
- Stable Missive comment IDs are deduplicated in
  `<state_path>.missive-inbox.db` before dispatch.
- Rejected comment content is cleared before it is stored locally.
- Replies are Markdown posts in the exact originating conversation. They do
  not send or modify the underlying email.
- Posts longer than Missive's 8,000-character Markdown limit are split into
  durable chunks.

Run the normal preflight after configuring the channel:

```sh
push doctor
push
```

To use scheduled delivery, the target is one configured conversation ID:

```toml
[primary_delivery]
channel = "missive"
target = "00000000-0000-0000-0000-000000000000"
```

Missive voice messages and typing indicators are not supported.
