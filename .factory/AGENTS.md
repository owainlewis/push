# AGENTS.md

This repository defines push.

push is a tiny messaging gateway that turns coding agents into a personal assistant you text.
It reads allowed iMessage threads, routes messages to Claude Code or Codex, and sends replies back through Messages.

## Agent Rules

- Keep changes small and easy to review.
- Prefer simple Rust code and clear module boundaries.
- Follow the current async Tokio style.
- Do not push to `main`.
- Do not merge pull requests.
- Do not change safety defaults without human review.
- Do not invent product claims, metrics, pricing, or roadmap promises.
- Stop when the requested issue or objective is unclear.

## Important Context

- `src/main.rs` owns CLI startup and runtime wiring.
- `src/config.rs` owns config loading and validation.
- `src/gateway.rs` owns polling, routing, and reply flow.
- `src/imessage/` owns Messages database reads and sends.
- `src/claude.rs` and `src/codex.rs` own backend adapters.
- `src/store.rs` owns durable conversation state.
- `assistant/*.example.md` and `config.example.json` are user-facing templates.
- `site/` is the static project site.

## Verification

Run these checks before opening a pull request when code changes:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --locked
```

