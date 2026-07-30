# CLI reference

Push has one gateway command, diagnostic and service commands, and a small set
of job commands. All commands accept `--config <path>` anywhere in the argument
list. The default is `~/.push/config.toml`.

| Command | Purpose |
| --- | --- |
| `push help`, `push --help` | Print command and option help without loading config or changing files |
| `push version`, `push --version`, `push -V` | Print the installed Push version without starting the gateway |
| `push init [path]` | Create and Git-initialize the one assistant repository; defaults to `./assistant` |
| `push` | Start the configured channel gateway and scheduler |
| `push doctor` | Validate config, paths, channel requirements, and required backend binaries |
| `push status` | Show whether the installed launchd or systemd gateway service is running |
| `push paths` | Show the resolved config, assistant, job, and runtime storage paths |
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
push status
push paths
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
for this command. `push status` reads the same service definition and also
ignores `--config`. Run `push doctor` separately when you want to validate those
settings from the current shell.

`push init` accepts an empty target, the selected config by itself, or a
complete existing assistant layout. It refuses unrelated and partial non-empty
directories, never overwrites an existing assistant file, persists one
canonical `assistant_root`, and initializes Git when needed.

## JSON contract

Pass the global `--json` option anywhere in the argument list to select the
version 1 JSON contract. Human-readable output remains the default. JSON mode is
available for:

- `help` and `version`
- `doctor`, `status`, and `paths`
- `job validate`, `job list`, `job show`, and `job runs`

Commands that start or mutate runtime state reject `--json`. This includes the
gateway, `init`, `reload`, `restart`, and `job run`. In particular, Push does
not claim that an interrupted mutation is safe to retry when its outcome is
unknown.

A successful command writes exactly one JSON document and a trailing newline to
stdout. It writes nothing to stderr:

```json
{
  "schema_version": 1,
  "ok": true,
  "command": "paths",
  "data": {}
}
```

A failed command writes exactly one JSON document and a trailing newline to
stderr. It writes nothing to stdout:

```json
{
  "schema_version": 1,
  "ok": false,
  "error": {
    "category": "configuration",
    "message": "configuration not found at ~/.push/config.toml",
    "exit_code": 3,
    "retryable": false
  }
}
```

`error.details` is optional and contains command-specific structured evidence,
such as failed doctor checks or invalid jobs. `retryable` is optional. Push
omits it for unexpected failures where it cannot make an honest retry claim.
Diagnostic payloads report whether credentials are configured, never their
values. `job runs` reports content-presence booleans and omits stored result,
evaluation, and error text.

### Exit codes

The category strings and process exit codes are stable within schema version 1:

| Exit code | Category | Meaning |
| --- | --- | --- |
| `0` | success | The command completed |
| `2` | `invalid_input` | Arguments, names, or validated input are invalid |
| `3` | `configuration` | Config or configured local state is missing or invalid |
| `4` | `unavailable_dependency` | A required backend, service manager, or dependency is unavailable |
| `5` | `transient_transport` | A transport failed in a way Push knows is safe to retry |
| `6` | `conflict` | Current state conflicts with the requested operation |
| `70` | `unexpected` | Push cannot classify the failure safely |

### Command data

Every listed field is required unless marked optional. `integer` values are JSON
integers. Path fields are UTF-8 strings; Push replaces invalid filesystem bytes
rather than emitting invalid JSON.

`help` data contains `text` as a string. `version` data contains `name` and
`version` as strings.

`doctor` data:

| Field | Type | Values |
| --- | --- | --- |
| `checks` | array of check objects | All checks in execution order |
| `checks[].name` | string | Stable human-readable check name |
| `checks[].status` | string enum | `pass` or `fail` |
| `checks[].message` | string | Secret-safe explanation |

Failed doctor output places the same object under `error.details`.

`status` data:

| Field | Type | Values |
| --- | --- | --- |
| `manager` | string enum | `launchd` or `systemd` |
| `unit` | string | `com.owainlewis.push` or `push.service` |
| `running` | boolean | `true` only when the normalized state is `active` |
| `state` | string | `active`, `not_loaded`, or a systemd state such as `inactive`, `failed`, `activating`, `deactivating`, or `unknown` |

An operational service-manager failure is an `unavailable_dependency` error,
not a successful inactive status.

`paths` data contains these required string paths:

| Field | Meaning |
| --- | --- |
| `config` | Loaded config file |
| `assistant_root` | User-owned assistant repository |
| `assistant_context` | Assistant `context` directory |
| `assistant_evals` | Assistant `evals` directory |
| `jobs` | Installed Markdown jobs |
| `jobs_run` | Local run-lock directory |
| `state` | Cursor and backend-session state |
| `audit_log` | Structured audit log |
| `database` | Conversation and job-run SQLite database |
| `imessage_database` | Configured Messages database |

`job validate` and `job list` share catalog data:

| Field | Type | Values |
| --- | --- | --- |
| `valid_count` | integer | Number of entries in `valid` |
| `invalid_count` | integer | Number of entries in `invalid` |
| `valid` | array of valid entry objects | Valid installed jobs |
| `valid[].name` | string | Job slug |
| `valid[].status` | string constant | `valid` |
| `valid[].path` | string | Installed Markdown path |
| `valid[].backend` | string enum | `claude`, `codex`, or `pi` |
| `invalid` | array of invalid entry objects | Invalid installed entries |
| `invalid[].name` | string | Best available filename or job slug |
| `invalid[].status` | string constant | `invalid` |
| `invalid[].path` | string | Rejected entry path |
| `invalid[].message` | string | Validation reason |

`job validate` puts catalog data under `error.details` and exits with
`invalid_input` when `invalid_count` is nonzero. `job list` returns the catalog
successfully so callers can inspect valid and invalid entries together.

`job show` data:

| Field | Type | Values |
| --- | --- | --- |
| `name` | string | Job slug |
| `path` | string | Installed Markdown path |
| `backend` | string enum | `claude`, `codex`, or `pi` |
| `timeout_ms` | integer | Validated timeout in milliseconds |
| `workdir` | string | Resolved backend working directory |
| `snapshot_hash` | string | Validated job snapshot SHA-256 |
| `evals` | array of strings | Assigned eval names |
| `triggers` | array of trigger objects | Validated triggers |
| `triggers[].id` | string | Trigger slug |
| `triggers[].kind` | string constant | `cron` |
| `triggers[].schedule` | string | Five-field cron expression |
| `triggers[].timezone` | string | IANA timezone name |
| `triggers[].enabled` | boolean | Whether the scheduler may enqueue it |
| `body` | string | Runbook instruction body |

`job runs` data:

| Field | Type | Values |
| --- | --- | --- |
| `job_name` | string or null | Requested job filter, or null for all jobs |
| `runs` | array of run objects | Up to 100 newest rows |
| `runs[].id` | string | Run UUID |
| `runs[].job_name` | string | Job slug |
| `runs[].state` | string | Persisted execution state |
| `runs[].backend` | string enum | `claude`, `codex`, or `pi` |
| `runs[].queued_at_ms` | integer | Unix epoch milliseconds |
| `runs[].trigger.kind` | string | `manual` or `cron` |
| `runs[].trigger.id` | string or null | Trigger ID for a scheduled run |
| `runs[].trigger.scheduled_at_ms` | integer or null | Scheduled Unix epoch milliseconds |
| `runs[].execution.has_result` | boolean | Whether stored result text exists |
| `runs[].execution.has_error` | boolean | Whether stored execution error text exists |
| `runs[].evaluation.state` | string | Persisted evaluation state |
| `runs[].evaluation.has_result` | boolean | Whether stored evaluation result text exists |
| `runs[].evaluation.has_error` | boolean | Whether stored evaluation error text exists |
| `runs[].delivery.state` | string | Persisted delivery state |
| `runs[].delivery.attempts` | integer | Delivery attempt count |
| `runs[].delivery.has_error` | boolean | Whether stored delivery error text exists |
| `runs[].delivery.channel` | string or null | Delivery channel |
| `runs[].delivery.target` | string or null | Delivery target |

Fields may be added compatibly within version 1. Existing fields, meanings,
category names, and types will not change without a schema-version change.

Shell examples:

```sh
# Read one path.
push paths --json | jq -r '.data.database'

# Fail unless doctor passes, then list failed checks if it does not.
if ! report=$(push doctor --json 2>doctor.json); then
  jq '.error.details.checks[] | select(.status == "fail")' doctor.json
fi

# List valid job names.
push --json job list | jq -r '.data.valid[].name'

# Inspect recent failed run metadata without exposing stored output.
push job runs --json | jq '.data.runs[] | select(.state == "failed")'
```

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
