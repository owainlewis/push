+++
version = 1
timeout = "10m"
workdir = "~/.push/workspaces/daily-inbox-triage"

[[triggers]]
id = "weekday-morning"
kind = "cron"
schedule = "0 8 * * 1-5"
timezone = "Europe/London"
# Enable only after configuring the agent's email tools and primary delivery.
enabled = false
+++

Using the email tools configured for this agent, review unread messages received
since the previous day.

Prioritize:

1. Messages that need a reply today.
2. Time-sensitive requests, account alerts, and commitments.
3. Messages blocking active work.
4. Useful information that can wait.

Return these sections:

1. Needs reply today
2. Time-sensitive
3. Blocking active work
4. FYI

For each actionable message, include the sender, subject, why it matters, and
the next action. Group newsletters, receipts, and automated notifications under
FYI. Do not include message bodies or private details unless needed to explain
the required action. Do not send, delete, archive, label, or modify email.
