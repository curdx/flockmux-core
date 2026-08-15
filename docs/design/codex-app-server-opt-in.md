# Codex app-server opt-in — design boundary

> Status: **design only** (2026-08 优化审计 P2). Do **not** replace the default
> Codex PTY spawn path until this document's exit criteria are met and an
> explicit opt-in flag ships.

## Why this exists

OpenAI's **Codex app-server** is the official client↔Codex JSON-RPC surface
(stdio / experimental WS / unix socket). It powers the VS Code extension and
exposes threads, turns, approvals, and streamed agent events. MCP remains the
tool plane; app-server is the *client control* plane.

swarmx already drives Codex over PTY + Stop hooks + MCP swarm tools. That path
preserves:

- CLI-account / ChatGPT login behaviour
- asciicast recording
- the same wake / reaper / registry lifecycle as every other engine

App-server is attractive for **structured Activity** (tool calls, diffs,
approvals) without scraping a TUI — but it is an additive transport, not a
replacement for agent-to-agent mailbox orchestration.

## Non-goals

| Do not | Why |
|---|---|
| Switch Claude to ACP / Agent SDK / `claude -p` by default | Billing red line (`.agents/skills/swarmx-agent-upgrades/SKILL.md`) |
| Revive deleted `acp.rs` / `acp_engine.rs` as the multi-engine default | Zed ACP = editor↔single agent, not swarm orchestration |
| Replace mailbox / blackboard with Google A2A | Wrong problem (cross-org peers); local SQLite mailbox stays |
| Make OpenCode ACP the default instead of TUI HTTP | Large-prompt + PTY lifecycle already solved via `/tui/*` |
| Depend on Claude Channels for wake | Research preview + allowlist + unstable; Stop hook + kick remain |

## Proposed shape (when implemented)

1. **Flag**: e.g. `SWARMX_CODEX_TRANSPORT=app-server` or plugin field
   `transport = "app-server"` with default `"pty"`.
2. **Spawn**: still register an `AgentSlot` (reaper / live-cap / kill). Prefer
   keeping a thin PTY wrapper around `codex app-server` **or** a dedicated
   non-PTY channel only after reaper/kill semantics are proven equivalent.
3. **Driver**: JSON-RPC `initialize` → `initialized` → `thread/start` →
   `turn/start`; map notifications → `SwarmEvent::AgentActivity` /
   `ThoughtTrace`.
4. **Wake**: prefer app-server turn/steer (or interrupt + new turn) when idle;
   **PTY kick remains fallback** until the driver is complete.
5. **MCP swarm tools**: unchanged — still inject via per-agent `CODEX_HOME`
   MCP config so the model keeps `swarm_*`.
6. **Billing copy**: UI must say this is still CLI-account Codex, not API-key
   billing, and that the experiment can fall back to PTY.

## Exit criteria before flipping any default

- [ ] Focused protocol unit tests (framing, initialize handshake, turn events)
- [ ] Opt-in route only; default spawn stays PTY
- [ ] Wake + Stop-equivalent turn boundary covered
- [ ] Reaper / kill / live-agent cap behave identically
- [ ] Browser smoke: spawn codex → activity cards → blackboard wake
- [ ] Document rollback: unset flag → PTY

## References

- https://developers.openai.com/codex/app-server
- https://openai.com/index/unlocking-the-codex-harness/
- Internal: `docs/opencode-integration-plan.md` (ACP section **废止**)
- Internal: `.agents/skills/swarmx-agent-upgrades/SKILL.md`
- Internal: [`docs/research/codex-desktop-vs-swarmx-2026-08.md`](../research/codex-desktop-vs-swarmx-2026-08.md)（官方 Electron 桌面 vs swarmx Tauri 壳/控制面差距）
