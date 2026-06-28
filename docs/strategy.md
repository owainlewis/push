# push — Strategy

## The bet

Personal AI agents are becoming a platform category. Google has Spark. Nous has
Hermes. OpenAI and Anthropic have agent tools. But I do not want to bet my
workflow on one assistant app. I want a gateway that lets me talk to whichever
agent is best for the job.

push is that gateway, and it is a long-term hedge against single-provider
lock-in.

## Own the gateway, not the assistant

The durable thing is not the model. Models change every few months. The durable
thing is the layer you own: how messages reach you, how your context and memory
are stored, and how work is routed to an agent. That layer should be yours, not
rented from whichever assistant app is ahead this quarter.

So push splits the world into two parts:

- **The gateway is yours and provider-independent.** Channels (iMessage today,
  more later), your memory as plain files you own and version, session and
  routing state, the poll loop. None of this is tied to a vendor.
- **The agent is a swappable slot.** Today push drives Claude Code, because it
  is the best first-party agent available and push uses it natively rather than
  wrapping it. Tomorrow the same gateway can route to a different agent when a
  different agent is better for the task.

## Why this is not a contradiction

push's other headline is "it's actually Claude Code, not a third-party wrapper."
That is about the agent layer: when you choose Claude, you get the real thing,
not a degraded proxy like Hermes' runtime or OpenClaw's `pi` wrapper. The hedge
is about the gateway layer: the part you own does not depend on Claude, or on
anyone.

Put together: use the best agent natively, but never let any one assistant app
own your workflow. Single-provider apps like Hermes ask you to live inside their
world. push keeps the world yours and lets the agent be a choice you remake
whenever you want.

## What this requires (the honest gap)

Today the agent slot is shaped like Claude Code. The session model leans on
`--session-id` / `--resume`, and memory injection uses `--append-system-prompt`.
That is fine for v1, but it means "swap the agent" is not yet free.

To make the hedge real, the agent needs a clean contract: an `AgentRunner` seam
that takes a prompt plus conversation state and returns a reply, with the
gateway owning the conversation identity rather than borrowing the agent's. The
file-based memory already helps here, because it is injected, not stored inside
any agent. Defining that seam is the main piece of work between "Claude Code
gateway" and "multi-agent gateway."

## Course angle

This doubles as a teaching artifact. The lesson is not "here is how to use one
assistant app." It is "here is how to build a personal AI gateway you own, so no
single provider owns your workflow." push is the worked example: small, legible,
file-based memory, and an agent slot you control.
