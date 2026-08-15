# swarmx domain language

Shared vocabulary for architecture reviews and deepening work. Prefer these
terms over inventing synonyms.

## Agent

A live coding-CLI process owned by swarmx (claude / codex / opencode /
reasonix / zulu / kimi). One registry slot, one mailbox, optional PTY or
HTTP control plane. Not a cloud "agent" abstraction — the binary is the agent.

## Turn

One unit of prompt delivery to an Agent: bootstrap first prompt, blackboard
wake kick, or operator manual wake. Callers ask TurnDelivery to deliver a
turn; they do not pick PTY vs TUI HTTP vs serve HTTP themselves.

## Wake

Pushing an idle or mid-turn Agent to notice mailbox / blackboard work.
Mailbox `kind=wake` is the source of truth; engine kick is best-effort.
Blackboard subscription wakes and operator ⚡ share the same delivery plane.

## EngineAdapter (`CliAdapter`)

Per-CLI **pre-spawn** patches only (MCP inject, trust, stop hook, argv/env
tweaks). Port allocation, serve drivers, and live turn delivery are **not**
owned by this adapter today — those live in spawn orchestration and
`TurnDelivery`. Do not pretend CliAdapter is a full engine facade.

## LiveDelivery

Internal classification of how a *running* Agent accepts turns (zulu serve /
reasonix serve / opencode TUI / PTY keystroke). Implementation detail of
TurnDelivery — not the public seam for callers.
