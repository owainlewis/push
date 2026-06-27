# push

A tiny messaging gateway that turns Claude Code into a personal assistant you
text. It polls iMessage for new messages, runs them through `claude -p`, and
sends the reply back to your phone.

One small Go binary. No daemon framework, no database, no cloud. It runs on your
Mac and reads your local Messages history directly.

## How it works

```
iMessage (chat.db)  ->  push (poll loop)  ->  claude -p  ->  iMessage reply
```

1. Poll `~/Library/Messages/chat.db` for new messages.
2. Keep only messages from allowed senders (yourself, or a config allowlist).
3. Map each conversation to a stable Claude Code session so context persists.
4. Run `claude -p` headless, with your assistant memory injected.
5. Send the answer back over iMessage via AppleScript.

Memory is plain markdown you own (`assistant/User.md`, `assistant/Memory.md`).
The gateway injects it into every run with `--append-system-prompt`, so you can
read, edit, and version your assistant's context by hand.

## Why push: it's actually Claude Code

push is not an agent. It is a thin pipe to the real `claude` binary. Your texted
assistant *is* Claude Code: same model, same tools, same MCP servers, same
permission modes, same `CLAUDE.md` loading, same login. When Claude Code ships a
feature, push has it the same day, for free, with no code change.

This is the opposite of how other gateways work, and it is the point:

- **Hermes** runs its own agent runtime over many providers (OpenRouter, NIM,
  and so on). You get Hermes' abstractions, Hermes' bugs, and a translation
  layer between you and the model.
- **OpenClaw** wraps a third-party agent (`pi`). Again, a layer that is not
  Claude Code sitting between you and the work.

Those layers look like flexibility. In practice they are a disadvantage: you
inherit someone else's agent loop instead of the one Anthropic builds and
maintains. push has no agent loop of its own to inherit. It hands your message
to `claude` and sends back what `claude` says.

A concrete payoff: push bills your **Claude subscription**, because it just runs
`claude` with your environment. If `ANTHROPIC_API_KEY` is set it uses that;
otherwise it uses your logged-in subscription. No wrapper, no separate billing,
no provider config.

## v1 scope

- iMessage only.
- Claude Code only (no direct API).
- Read-only memory (you maintain the files; the gateway injects them).

See [docs/prd.md](docs/prd.md) for the full v1 product spec, and
[docs/architecture.md](docs/architecture.md) for the design and diagrams
(including why push's file-based memory beats an opaque memory database).

## Requirements

- macOS with iMessage signed in.
- **Full Disk Access** for your terminal (System Settings -> Privacy &
  Security -> Full Disk Access) so push can read `chat.db`.
- `claude` (Claude Code CLI) on your `PATH`.
- `sqlite3` and `osascript` (both ship with macOS).
- Go 1.26+ to build.

## Quick start

```sh
git clone https://github.com/owainlewis/push.git
cd push
cp config.example.json config.json
# edit config.json: set self_handles to your own iMessage handles
go build -o push ./cmd/push
./push
```

Then text yourself in Messages. The reply comes back in the same thread.

### Commands you can text

- `/clear` (or `/new`, `/reset`) — start a fresh conversation (rotates the session).
- `/help` — list commands.

## Safety

push runs Claude Code with `--permission-mode bypassPermissions` so it can act
without a human to approve each tool call. Each conversation runs in its own
sandbox directory under `sessions/`. **Only allow senders you trust**: an
inbound text is an instruction to an agent with tool access. Keep the allowlist
tight.
