//! `/ws/swarm` event stream. All fan-out (lifecycle, messages, blackboard
//! changes) flows through this single broadcast channel so a subscriber can
//! see the complete cross-agent picture in one connection.

use serde::{Deserialize, Serialize};

use crate::rest::{ThoughtTrace, ThoughtTraceStep};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SwarmEvent {
    /// A per-agent lifecycle transition (spawning → ready → idle → exited).
    AgentState { agent_id: String, state: AgentState },
    /// An agent-to-agent message was persisted. Subscribers can rehydrate
    /// the recipient's inbox without going back to SQLite.
    Message {
        id: i64,
        from_agent: String,
        to_agent: String,
        kind: String,
        body: String,
        sent_at: i64,
        /// Parent message id when this is a reply. Optional so old clients
        /// (which serialize without the field) still round-trip.
        #[serde(default)]
        in_reply_to: Option<i64>,
        /// Direction (thread) this message belongs to; `None` = main/untagged.
        /// Lets the UI hard-gate a direction's chat on live messages too.
        #[serde(default)]
        thread_id: Option<String>,
        /// Structured metadata for system-generated messages (Slack
        /// `event_payload`-style), e.g. `{"subtype":"wake","reason":"blackboard",
        /// "key":"…"}` or `{"subtype":"completion","signal":"reviewer.done"}`.
        /// The UI renders/filters from `meta.subtype` instead of regex-parsing
        /// the prose `body`. `None` for agent free-text (UI falls back to
        /// heuristics there). Optional so old clients still round-trip.
        #[serde(default)]
        meta: Option<serde_json::Value>,
        /// Product-level reasoning / execution summary for this message.
        #[serde(default)]
        thought_trace: Option<ThoughtTrace>,
    },
    /// A batch of messages was marked read on behalf of `to_agent`. Used by
    /// the UI to decrement the unread badge live without a REST poll.
    MessageRead {
        ids: Vec<i64>,
        to_agent: String,
        at: i64,
    },
    /// A blackboard file changed. `agent_id == None` means the change
    /// came from outside swarmx (the watcher fallback path) — e.g. the
    /// user editing the markdown file in their normal editor.
    BlackboardChanged {
        id: i64,
        agent_id: Option<String>,
        /// "write" (an agent's call) or "external" (filesystem watcher).
        op: String,
        path: String,
        sha256: String,
        at: i64,
    },
    /// A direction (thread) was created, renamed, isolated into a worktree,
    /// degraded back to shared, or deleted — i.e. the workspace's thread list
    /// changed in a way the REST `/api/workspaces` snapshot doesn't push on its
    /// own. Subscribers (the sidebar) refetch workspaces so a direction named/
    /// isolated by `swarm_name_thread` (a server-side PATCH, not a UI action)
    /// shows its new name + branch icon live, without a reload.
    ThreadChanged {
        workspace_id: String,
        thread_id: String,
        /// "created" | "updated" | "isolated" | "deleted".
        op: String,
    },
    /// A worker's tool- or step-level activity. Sourced from claude/codex
    /// PostToolUse hooks (tool-level) or an internal step (system-level).
    /// Drives the live activity block + the member-row "what is it doing right
    /// now" line. Keep it LOW-FREQUENCY: emit one `running` when a tool/step
    /// starts and one `ok`/`error` when it ends — never per-byte. The broadcast
    /// channel drops frames under flood, and the UI rebuilds current state from
    /// the latest event per (agent_id, seq), so a missed mid-frame is benign.
    AgentActivity {
        agent_id: String,
        /// "tool" (a real tool call) | "system" (compacting context, wrapping
        /// up, waiting…, server-side error marks) | "watchdog" (an S5
        /// stuck-watchdog nudge — 系统正在唤醒它). Open string so callers can
        /// extend later.
        kind: String,
        /// Human-facing one-liner, e.g. `Edit src/foo.rs` or `整理上下文`.
        label: String,
        /// "running" | "ok" | "error".
        phase: String,
        /// Per-turn sequence number; pairs a `running` with its later
        /// `ok`/`error` so the UI can group the current round and dedupe.
        seq: u32,
        /// Wall-clock duration once finished; `None` while still running.
        #[serde(default)]
        duration_ms: Option<u32>,
        at: i64,
    },
    /// A live append to an IN-FLIGHT thought trace: emitted right after a real
    /// step is persisted to an agent's still-active trace, so the "正在响应"
    /// bubble can grow its step list during the turn instead of only learning
    /// the steps when the agent→user reply finally lands. Carries the FULL
    /// current summary snapshot (not a delta) keyed by `trigger_message_id`, so
    /// the UI just replaces by that key — a dropped frame self-heals on the
    /// next append. Same low frequency as `AgentActivity` (one per tool).
    ThoughtTraceEvent {
        trigger_message_id: i64,
        agent_id: String,
        steps: Vec<ThoughtTraceStep>,
        at: i64,
    },
    /// A coarse bootstrap-stage heartbeat while an agent COLD-STARTS, so the
    /// UI can show a narrative stage bar ("启动 CLI → 挂载 swarm 工具 → 注入
    /// 任务") instead of a silent 30s spinner that reads as "wedged". Emitted
    /// once per transition at: `shim_ready` (PTY up), `mcp_ready` (swarm tools
    /// visible to the model), `bootstrap_injected` (first prompt submitted).
    /// The "first turn" boundary needs no event — the first `AgentActivity`
    /// after `bootstrap_injected` IS that signal. LOW-FREQUENCY by design.
    AgentStage {
        agent_id: String,
        /// "shim_ready" | "mcp_ready" | "bootstrap_injected". Open string so
        /// future stages don't need a schema bump.
        stage: String,
        at: i64,
    },
    /// A workspace's budget brake changed state: TRIPPED (estimated all-time
    /// spend crossed the cap → the server paused the workspace's live agents)
    /// or LIFTED (budget raised above the estimate, or cleared → the agents
    /// the brake paused get resumed). Subscribers refetch
    /// `GET /api/workspaces/:id/budget` instead of caching the numbers. Every
    /// amount here is an ESTIMATE from transcript scraping, never the
    /// subscription invoice.
    BudgetChanged {
        workspace_id: String,
        exceeded: bool,
        /// The cap now in force (None = unlimited / cleared).
        budget_usd: Option<f64>,
        /// Estimated all-time spend at event time.
        cost_usd: f64,
        at: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Spawning,
    Ready,
    Thinking,
    Idle,
    /// Waiting on an unmet dependency — a `consumes`/`depends_on` blackboard
    /// key that isn't ready yet. WakeCoordinator flips this to Thinking/Ready
    /// once the dependency lands.
    WaitingDep,
    /// Terminated abnormally (non-zero shim exit, or a `<role>.error` fallback).
    /// Distinct from `Exited` (clean stop) so the UI can surface it (red,
    /// sorted to top). Error DETAIL rides a separate `AgentActivity`
    /// (kind="system", phase="error") so this enum stays `Copy`.
    Error,
    Exited,
}
