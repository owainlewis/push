+++
version = 1
timeout = "10m"
workdir = "~/.push/workspaces/daily-inbox-triage"

[[triggers]]
id = "weekday-morning"
kind = "cron"
schedule = "0 8 * * 1-5"
timezone = "Europe/London"
enabled = true
+++

Read `context/README.md` and `context/inbox-triage.md` from the assistant
repository identified in your instructions.

Using the email tools configured for this agent, review unread messages received
since the previous day. Return these sections:

1. Needs reply today
2. Time-sensitive
3. Blocking active work
4. FYI

For each actionable message, include the sender, subject, why it matters, and
the next action. Do not send, delete, archive, label, or modify email.
