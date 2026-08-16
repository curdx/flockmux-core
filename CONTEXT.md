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
The continuation prompt (what the engine is told) is **one** recipe:
`ConsumeWakesResponse::continuation`, filled as `reason` on consume.
Engine adapters do not author their own "You were woken up" strings.
The mailbox note body is a separate, operator-facing record.

## Handoff

Server-minted blackboard key a worker writes on success. Format:

- First live producer of a role in a direction (class key):
  `<workspace>/<thread>/<role>.<kind>`
- Additional parallel producers (instance key):
  `<workspace>/<thread>/<role>.<instance>.<kind>`

`consumes: {from_role, kind}` binds to **currently live** producers of that
role (AND: wait for all of them). If none are live, the consumer reserves
the class key — the next first producer will write it. Already-spawned
consumers are not retrofitted when a later same-role worker appears.
A producer that exits without writing the success key gets `<signal>.error`;
that write is the replan signal (same-role replacement spawn is allowed —
it mints a new instance key). Agents never name keys. EngineAdapter does
not mint Handoff keys.

## EngineAdapter (`CliAdapter`)

Per-CLI **pre-spawn** patches only (MCP inject, trust, stop hook, argv/env
tweaks). Port allocation, serve drivers, and live turn delivery are **not**
owned by this adapter today — those live in spawn orchestration and
`TurnDelivery`. Do not pretend CliAdapter is a full engine facade.

## LiveDelivery

How a *running* Agent accepts turns (zulu serve / reasonix serve / opencode TUI
/ PTY keystroke). Stored on the AgentSlot at spawn from the plugin's declared
`InputDelivery` plus the allocated port/handle. Callers read the stored field;
they do not re-derive the channel from `serve_http_port` / `tui_http_port` /
`zulu` — those Options are handles only, and zulu and reasonix share
`serve_http_port`. Implementation detail of TurnDelivery — not a public seam.
