# Contributing

Push is a small Rust gateway. Changes should preserve that shape: durable
assistant infrastructure belongs in Push; model reasoning, tools, MCP, skills,
and coding workflows belong in the selected backend.

## Set up the repository

```sh
git clone https://github.com/owainlewis/push.git
cd push
cargo build --locked
cargo test --locked
```

Before opening a pull request, run:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
cargo test --locked
```

## Code map

| Area | Location |
| --- | --- |
| CLI and startup | `src/main.rs` |
| TOML loading and validation | `src/config.rs` |
| Channel-neutral boundary | `src/channel.rs` |
| iMessage and Telegram adapters | `src/imessage/`, `src/telegram.rs` |
| Gateway, queues, workers, delivery | `src/gateway/` |
| Claude Code, Codex, and Pi adapters | `src/claude.rs`, `src/codex.rs`, `src/pi.rs` |
| Canonical SQLite history | `src/history.rs` |
| Jobs, scheduling, locks, run ledger | `src/jobs.rs` |
| Agent-authored draft approval | `src/drafts.rs`, `src/approval.rs` |
| Production diagnostics | `src/doctor.rs`, `src/audit.rs` |

Read [architecture](architecture.md) before changing state, crash recovery,
channel cursors, session ownership, scheduling, or delivery semantics.

## Documentation workflow

The Markdown under `docs/` is the only source for the documentation website.
Do not edit generated files under `site/`.

Set up and build the docs:

```sh
python3 -m venv .venv-docs
. .venv-docs/bin/activate
pip install --requirement requirements-docs.txt
mkdocs serve
```

Run the CI-equivalent build before submitting documentation changes:

```sh
mkdocs build --strict
```

The GitHub Pages workflow rebuilds from `docs/` after changes land on `main`.
Keep each fact on one canonical page and link to it elsewhere. The README is a
product overview, not a second configuration manual.

## Release signing

The release workflow signs and notarizes the macOS binary before packaging it.
This requires an active Apple Developer Program membership, a Developer ID
Application certificate, and an App Store Connect team API key with at least
the Developer role.

Export the Developer ID Application certificate and its private key from
Keychain Access as a password-protected `.p12` file. Download the App Store
Connect API key as a `.p8` file. Apple only allows the API key file to be
downloaded once.

Configure these GitHub Actions repository secrets:

| Secret | Value |
| --- | --- |
| `MACOS_CERTIFICATE` | Base64-encoded `.p12` certificate and private key |
| `MACOS_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` file |
| `APPLE_API_KEY` | Base64-encoded App Store Connect team `.p8` API key |
| `APPLE_API_KEY_ID` | App Store Connect API key ID |
| `APPLE_API_ISSUER` | App Store Connect API issuer ID |

From the repository root on macOS, set the file secrets with:

```sh
base64 -i DeveloperIDApplication.p12 | gh secret set MACOS_CERTIFICATE
base64 -i AuthKey_EXAMPLE.p8 | gh secret set APPLE_API_KEY
```

Set the remaining values interactively so they do not enter shell history:

```sh
gh secret set MACOS_CERTIFICATE_PASSWORD
gh secret set APPLE_API_KEY_ID
gh secret set APPLE_API_ISSUER
```

Push a `v*` tag or run the Release workflow with an existing tag. The macOS
job fails before packaging if signing verification or notarization fails. The
Linux release job does not use the Apple secrets.

## Pull requests

Keep changes focused. Explain the problem, impact, root cause, verification,
and risk. Add a regression test for behavior changes when it provides useful
proof. Never include personal config, message history, tokens, assistant
identity, audit logs, or session state.
