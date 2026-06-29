# STANDARDS.md

These standards define repo health for push.

## Purpose

push must clearly explain its mission:
provide a small personal assistant gateway over iMessage while keeping coding-agent backends replaceable.

## Repository Contract

- Factory repo contract files live under `.factory/`.
- Standards, workflows, objectives, and journal entries are owned by this repo.
- Factory may run workflows from this repo, but the repo defines what good means.

## Rust Code

- Keep module boundaries clear.
- `src/config.rs` owns config shape, defaults, and validation.
- `src/gateway.rs` owns message loop orchestration.
- `src/imessage/` owns iMessage read and send behavior.
- `src/claude.rs` and `src/codex.rs` own agent backend behavior.
- `src/store.rs` owns persisted session state.
- Prefer explicit errors with context.
- Do not add large dependencies without human review.

## Testing

- New behavior must include focused tests when practical.
- Bug fixes must include regression tests.
- `cargo test --locked` must pass.
- Tests that need macOS Messages, Full Disk Access, or live agent CLIs should be isolated or documented as manual checks.

## Formatting and Linting

- `cargo fmt --check` must pass.
- `cargo clippy --all-targets -- -D warnings` must pass.
- Code should stay idiomatic and simple.

## Build

- `cargo build --locked` must pass.
- Release builds should use `cargo build --locked --release`.
- `Cargo.lock` must stay committed.

## Documentation

- README must explain the current scope honestly.
- README must document install, run, config, safety, and release commands.
- `config.example.json` must match the actual config shape.
- Public claims must be backed by code, docs, tests, issues, or releases.
- Security and permission risks must be plain, especially iMessage access and headless agent permissions.

## CI

- Pull requests should run Rust format, clippy, tests, and build.
- CI should not require secrets for normal pull request checks.
- CI should use the locked dependency graph.

## Release

- Releases should use tags.
- Release notes should explain user-visible changes.
- Release artifacts should include the binary, README, LICENSE, and `config.example.json`.
- Do not publish releases without human review.

## Safety

- Do not broaden `allow_from` behavior without human review.
- Do not weaken reply loop prevention without human review.
- Do not change default agent permission modes without human review.
- Do not commit secrets, private assistant memory, or local config.
- Do not merge pull requests automatically.
- Do not push directly to default branches.

## Human Review Required

Human review is required for:

- merging
- releases
- public claims
- pricing
- product strategy
- deleting features
- changing repo purpose
- changing licenses
- changing safety defaults
- changing iMessage permission assumptions
- changing default backend permission behavior

