# Push v0.7.0

- Render Markdown as Telegram HTML for chat and scheduled-job output, with safe
  plain-text fallback when Telegram rejects formatting.
- Add reusable agent evaluations for scheduled jobs and persist evaluation,
  execution, and delivery state separately.
- Add `help`, `reload`, `restart`, and `version` commands, including `--help`
  and `--version` flags that work without loading configuration.
- Recover stalled message queues, support `/stop`, and harden scheduled delivery
  with durable chunk progress, bounded retries, and impossible-cron validation.
- Add configurable voice credentials and voices. Removed runtime settings now
  fail with migration guidance, backend commands resolve from `PATH`, and
  Telegram environment credentials use `TELEGRAM_BOT_TOKEN`.

**Full changelog:** https://github.com/owainlewis/push/compare/v0.6.0...v0.7.0
