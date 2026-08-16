// swarmx wake plugin for OpenCode (Model A — PTY transport).
//
// OpenCode has NO blocking Stop hook (its only turn-end signal is a read-only
// `session.idle` event whose handler returns void — see opencode
// packages/plugin/src/index.ts `Hooks.event`). claude/codex get re-woken by a
// Stop hook that returns `{"decision":"block"}`; opencode can't. So swarmx
// runs the wake loop AS A PLUGIN: on every `session.idle`, ask swarmx whether
// this agent has pending wakes; if so, start a new turn by re-prompting the
// session with the same steering text the Rust Stop-hook helper
// (`swarmx-mcp wake-check`) feeds claude/codex.
//
// This is the opencode equivalent of crates/swarmx-mcp/src/wake_check.rs.
// It hits the SAME endpoint (`POST /api/message/consume_wakes`), which
// ATOMICALLY claims + marks-read all pending kind="wake" messages and returns
// their count. Because consume is atomic, repeated calls are safe: once the
// wakes are claimed the count is 0, so the agent is NOT re-prompted in a loop
// (the turn we start here ends in another `session.idle`, we ask again, get 0,
// and stop).
//
// Identity + server come from the env swarmx's spawn.rs injects into the
// opencode process: SWARMX_AGENT_ID, SWARMX_SERVER_URL.
//
// STATUS: validated against a live opencode (1.17.7). The per-agent
// OPENCODE_CONFIG merge that loads this plugin is VERIFIED LIVE (see
// cli-plugins/opencode.toml + crates/swarmx-server/src/cli/opencode.rs), the
// tool.execute.* → POST /api/agent/:id/activity path is the server's
// documented production ingress for opencode (routes/rest.rs), and the
// dogfood run (dogfood-output/report.md) drove a live opencode captain
// end-to-end with zero JS errors. The session.idle → consume_wakes →
// client.session.prompt loop mirrors wake_check.rs and is race-guarded
// server-side (wake.rs pre-consumes so kick and plugin don't double-fire).

const AGENT_ID = process.env.SWARMX_AGENT_ID
const SERVER_URL = (process.env.SWARMX_SERVER_URL || "http://127.0.0.1:7777").replace(/\/+$/, "")

// Mirrors the reason string built in wake_check.rs::emit_block so a woken
// opencode worker follows the exact same recovery path as claude/codex.
function wakeReason(count) {
  return [
    `You were woken up: ${count} new wake event(s) just arrived.`,
    `A blackboard key you depend_on was likely written.`,
    `Steps:`,
    `1. Call swarm_list_blackboard, then swarm_read_blackboard on any key you depend on.`,
    `2. If you also have pending non-wake messages, call swarm_list_messages.`,
    `3. Continue with your role's workflow. If you decide to reply, use`,
    `   swarm_send_message with kind:"reply" AND in_reply_to:<id>.`,
    `Do not produce any user-facing output about these wakes outside the swarm tool calls.`,
  ].join("\n")
}

// Build a short one-line label for a tool call (tool name + one salient arg),
// mirroring swarmx's transcript summarize() for claude/codex.
const SALIENT_KEYS = ["key", "path", "file_path", "filePath", "command", "cmd", "pattern", "query", "url", "prompt", "description"]
function toolLabel(tool, args) {
  let label = String(tool || "tool").replace(/^.*?_(?=swarm_)/, "") // strip MCP server prefix before swarm_*
  if (args && typeof args === "object") {
    for (const k of SALIENT_KEYS) {
      const v = args[k]
      if (typeof v === "string" && v) {
        label += " " + v
        break
      }
    }
  }
  label = label.replace(/\s+/g, " ").trim()
  return label.length > 80 ? label.slice(0, 79) + "…" : label
}

async function postActivity(agentId, phase, label, seq, durationMs) {
  try {
    await fetch(`${SERVER_URL}/api/agent/${encodeURIComponent(agentId)}/activity`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ phase, label, seq, duration_ms: durationMs }),
    })
  } catch {
    // Activity is best-effort telemetry; never let it disturb the agent.
  }
}

export const SwarmxWake = async ({ client }) => {
  // Per-agent (per-process) activity state: a monotonic seq pairs a "running"
  // row with its later "ok", keyed by opencode's callID.
  let seq = 0
  const calls = new Map()

  return {
    // Stream tool calls to swarmx so opencode worker steps show in the UI
    // (opencode can't be transcript-tailed — see post_agent_activity in rest.rs).
    "tool.execute.before": async (input, output) => {
      if (!AGENT_ID || !input || !input.callID) return
      const s = seq++
      const label = toolLabel(input.tool, output && output.args)
      calls.set(input.callID, { seq: s, label, start: Date.now() })
      await postActivity(AGENT_ID, "running", label, s)
    },
    "tool.execute.after": async (input) => {
      if (!AGENT_ID || !input || !input.callID) return
      const c = calls.get(input.callID)
      if (!c) return
      calls.delete(input.callID)
      await postActivity(AGENT_ID, "ok", c.label, c.seq, Date.now() - c.start)
    },
    event: async ({ event }) => {
      // Only act at a turn boundary, and only if we know who we are.
      if (!event || event.type !== "session.idle") return
      if (!AGENT_ID) return
      const sessionID = event.properties && event.properties.sessionID
      if (!sessionID) return

      let count = 0
      let text = ""
      try {
        const res = await fetch(
          `${SERVER_URL}/api/message/consume_wakes?to=${encodeURIComponent(AGENT_ID)}`,
          { method: "POST" },
        )
        if (!res.ok) return
        const body = await res.json()
        count = (body && typeof body.count === "number") ? body.count : 0
        if (count <= 0) return
        text =
          (body && typeof body.reason === "string" && body.reason)
            ? body.reason
            : wakeReason(count)
      } catch {
        // Server unreachable → never block the agent; just skip this wake.
        return
      }

      try {
        await client.session.prompt({
          path: { id: sessionID },
          body: { parts: [{ type: "text", text }] },
        })
      } catch {
        // Re-prompt failed → the wakes were already consumed; they will be
        // re-delivered as fresh wakes by swarmx's WakeCoordinator if the
        // dependency is still unsatisfied. Never throw from a hook.
      }
    },
  }
}
