# Push v0.10.0

- Derive private runtime storage from one `PUSH_HOME`, while keeping the
  Git-versioned assistant repository and its jobs outside that boundary.
- Add a stable, redacted JSON CLI contract for paths, status, diagnostics,
  job inspection, validation, runs, and schedule reviews.
- Compose backend instructions from explicit policy, identity, workspace,
  request, and history sections.
- Install one versioned Push capability skill for Claude Code, Codex, and Pi.
- Move channel cursors and backend session mappings into the canonical SQLite
  database with crash-safe, repeatable migration from `state.json`.
- Require durable owner review before a new or changed enabled schedule can
  activate, including fail-closed queued-run checks and replayable audit events.

## Upgrade notes

Upgrade in this order:

1. Stop the Push service. Back up `PUSH_HOME`, the assistant repository, and
   any explicit runtime paths that live outside `PUSH_HOME`.
2. Before the first v0.10 command that opens job state, such as `push doctor`,
   confirm `primary_delivery` names an enabled, allowlisted destination if
   existing enabled schedules should receive the one-time migration baseline.
3. Install v0.10.0 and set one absolute `PUSH_HOME` in the service definition.
4. Run `push init <assistant_root>` to complete an older configured assistant
   layout and install or update the managed Push skill.
5. Run `push doctor` and `push job validate`, then restart the service and
   confirm `push status`.

- Existing `assistant_root` configurations continue to load jobs from
  `<assistant_root>/jobs`. Default runtime paths remain under `PUSH_HOME`.
- Explicit compatibility overrides such as `state_path`, `database_path`,
  `audit_log_path`, and `jobs_run_dir` remain authoritative. Back up and
  restore those paths separately when they point outside `PUSH_HOME`.
- If an older `$PUSH_HOME/jobs` directory remains after adopting
  `assistant_root`, its files are not active. Move any jobs you still want into
  `<assistant_root>/jobs`, then archive the legacy directory.
- Push imports legacy cursor and session state from `state.json` once and keeps
  the JSON file as a recovery copy.
- Existing valid enabled schedules receive a one-time migration baseline only
  when the first v0.10 command that opens job state has a valid primary delivery
  destination. If it does not, migration closes without grandfathering those
  schedules and they require review. Later schedule changes require approval
  of the exact revision.
- The service and interactive CLI must use the same `PUSH_HOME` so they share
  the runtime database and lock directory.

**Full changelog:** https://github.com/owainlewis/push/compare/v0.9.0...v0.10.0
