# CLI reference

Push has one gateway command, one diagnostic command, and a small set of job
commands. All commands accept `--config <path>` anywhere in the argument list.
The default is `config.toml` in the current directory.

| Command | Purpose |
| --- | --- |
| `push init [path]` | Create and Git-initialize the one assistant repository; defaults to `./assistant` |
| `push` | Start the configured channel gateway and scheduler |
| `push doctor` | Validate config, paths, channel requirements, and required backend binaries |
| `push job validate` | Validate every installed job; exits non-zero if any are invalid |
| `push job list` | List valid and invalid jobs with backend or error |
| `push job show <name>` | Print the parsed installed job |
| `push job run <name>` | Claim and run one job in the CLI process |
| `push job runs [<name>]` | Print run and delivery history, optionally for one job |

Examples:

```sh
push init ~/Code/assistant --config ~/.config/push/config.toml
push doctor --config ~/.config/push/config.toml
push --config ~/.config/push/config.toml
push job validate --config ~/.config/push/config.toml
push --config ~/.config/push/config.toml job run repo-review
push job runs repo-review --config ~/.config/push/config.toml
```

Unknown commands and missing values fail with the accepted command forms. The
CLI does not currently provide shell completion or a generated `--help` page.

`push init` accepts an empty target, the selected config by itself, or a
complete existing assistant layout. It refuses unrelated and partial non-empty
directories, never overwrites an existing assistant file, persists one
canonical `assistant_root`, and initializes Git when needed.

## Commands sent in chat

These messages are handled by the gateway before backend dispatch:

| Message | Effect |
| --- | --- |
| `/clear`, `/new`, `/reset` | Start a fresh backend session for that conversation |
| `/help` | Return the available chat commands |

Starting a fresh session preserves canonical history. Push can seed the new
backend session with bounded recent turns from the exact channel-qualified
conversation.
