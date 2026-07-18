# CLI reference

Push has one gateway command, one diagnostic command, and a small set of job
commands. All commands accept `--config <path>` anywhere in the argument list.
The default is `~/.push/config.toml`.

| Command | Purpose |
| --- | --- |
| `push help`, `push --help` | Print command and option help without loading config or changing files |
| `push version`, `push --version`, `push -V` | Print the installed Push version without starting the gateway |
| `push init [path]` | Create and Git-initialize the one assistant repository; defaults to `./assistant` |
| `push` | Start the configured channel gateway and scheduler |
| `push doctor` | Validate config, paths, channel requirements, and required backend binaries |
| `push reload`, `push restart` | Restart the managed gateway to load updated config |
| `push job validate` | Validate every installed job; exits non-zero if any are invalid |
| `push job list` | List valid and invalid jobs with backend or error |
| `push job show <name>` | Print the parsed installed job |
| `push job run <name>` | Claim and run one job in the CLI process |
| `push job runs [<name>]` | Print run and delivery history, optionally for one job |

Examples:

```sh
push init ~/Code/assistant
push help
push version
push doctor
push
push reload
push job validate
push job run repo-review
push job runs repo-review
```

Unknown commands and missing values fail with the accepted command forms. The
CLI does not currently provide shell completion or separate help pages for
subcommands. A `--help` flag anywhere in the argument list prints the global
help shown by `push --help`.

`push reload` and its `push restart` alias target the service definitions documented by Push:
`com.owainlewis.push` under launchd on macOS and the `push.service` user unit
under systemd on Linux. The service definition controls its config path,
environment, and executable; `--config` does not override the service definition
for this command. Run `push doctor` separately when you want to validate those
settings from the current shell.

`push init` accepts an empty target, the selected config by itself, or a
complete existing assistant layout. It refuses unrelated and partial non-empty
directories, never overwrites an existing assistant file, persists one
canonical `assistant_root`, and initializes Git when needed. A new assistant
also gets an enabled `morning-ai-brief` job scheduled for 8:00 each day in the
machine's local IANA timezone. Delivery starts after `primary_delivery` is
configured.

## Commands sent in chat

These messages are handled by the gateway before backend dispatch:

| Message | Effect |
| --- | --- |
| `/clear`, `/new`, `/reset` | Start a fresh backend session for that conversation |
| `/stop` | Stop the active request; already queued messages continue in order |
| `/help` | Return the available chat commands |

Starting a fresh session preserves canonical history. Push can seed the new
backend session with bounded recent turns from the exact channel-qualified
conversation.
