# Memory

Durable facts the assistant should remember across every conversation. This is
plain text you own: edit it, correct it, commit it. Copy to `Memory.md` (which
is git-ignored) to use it.

In v1 this file is read-only to the assistant (the gateway injects it but does
not write to it). v2 will let the assistant append here.

## Facts

- (example) My main machine is a MacBook Pro; default shell is zsh.
- (example) I prefer Go for small tools and TypeScript for web.

## Decisions

- (example) push is iMessage + Claude Code only for v1.
