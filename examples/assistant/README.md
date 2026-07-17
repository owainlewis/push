# Example assistant

This example shows how assistant identity, durable context, and a scheduled job
fit together. Copy the files into the matching locations under your configured
`assistant_root`, then create the job's external working directory:

```sh
mkdir -p ~/.push/workspaces/daily-inbox-triage
push job validate
push doctor
```

Configure the selected agent with the email tools and authentication it needs.
Add a valid `[primary_delivery]` destination to Push configuration before
enabling the schedule.
