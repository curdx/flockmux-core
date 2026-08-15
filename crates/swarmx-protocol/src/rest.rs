//! REST DTOs.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnAgentRequest {
    /// Plugin id from `cli-plugins/<id>.toml`, e.g. "claude" or "codex".
    pub cli: String,
    /// Optional role label shown in the UI; defaults to the cli name.
    #[serde(default)]
    pub role: Option<String>,
    /// Optional workspace path. If absent, server allocates a temp dir.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Workspaces table FK. Optional in Step 1 of the rollout for compatibility;
    /// becomes mandatory in Step 3 once the frontend always passes the
    /// active workspace's id and orphan `+ Claude` clicks are routed through
    /// CreateWizard. The server will error if missing post-Step-3.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Optional direction/thread FK. UI launchers should pass the active
    /// thread. When omitted, the server binds the agent to the workspace's
    /// main thread instead of leaving it orphaned.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Optional model overlay (L5c). Passed to the CLI via its manifest
    /// `model_args` template (e.g. claude/codex `--model <v>`). `None` ⇒ use
    /// the plugin's `default_model`, else the CLI's own default. Decouples
    /// model from CLI id — same `cli`, any model, no forked plugin/role.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnAgentResponse {
    pub agent_id: String,
    pub cli: String,
    pub role: String,
    pub workspace: String,
    /// Set when the requested CLI wasn't installed and the server substituted
    /// an installed fallback engine: the ORIGINALLY requested plugin id
    /// (`cli` above then names the engine that actually spawned). None when
    /// the requested engine itself spawned. Billing red line: the fallback
    /// filter allows API-billed engines (e.g. reasonix), so the UI MUST
    /// surface this instead of letting a paid engine run in disguise.
    #[serde(default)]
    pub fallback_from: Option<String>,
    /// Billing surface of the engine that ACTUALLY spawned, as a kebab-case
    /// token ("interactive-subscription" | "cli-account" | "api-key" | …;
    /// mirrors swarmx-server's BillingSurface serde rename). Lets the UI say
    /// "按 API 计费" on a fallback without hardcoding per-engine knowledge.
    #[serde(default)]
    pub billing_surface: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliPluginInfo {
    pub id: String,
    pub display_name: String,
    pub binary: String,
    /// Whether the plugin's CLI binary is available on the server's augmented
    /// runtime PATH. This is the same PATH family used for GUI-launched child
    /// processes, so the Settings page reports what swarmx can actually run.
    #[serde(default)]
    pub installed: bool,
    /// Absolute path resolved for `binary`, when available.
    #[serde(default)]
    pub resolved_path: Option<String>,
    /// Best-effort `<binary> --version` output. Missing if the CLI is absent,
    /// times out, or doesn't support the flag.
    #[serde(default)]
    pub version: Option<String>,
    /// Official install guidance for known shipped CLIs. Present even when the
    /// CLI is installed so the UI can expose repair/reinstall docs.
    #[serde(default)]
    pub install: Option<CliInstallHint>,
    /// Keystroke-settle delay (ms) the web terminal applies after the CLI
    /// signals ready (codex needs ~300ms; claude 0). Lets the frontend input
    /// policy be data-driven instead of branching on the agent-id prefix.
    #[serde(default)]
    pub input_settle_ms: u64,
    /// Default model this CLI runs when a spawn doesn't override it (L5c).
    /// `None` ⇒ the CLI picks its own default. Surfaced so a future spawn UI
    /// can pre-fill / offer a model picker without hardcoding per-CLI names.
    #[serde(default)]
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliInstallHint {
    pub title: String,
    pub summary: String,
    pub docs_url: String,
    pub commands: Vec<String>,
    #[serde(default)]
    pub verify_command: Option<String>,
    #[serde(default)]
    pub login_command: Option<String>,
}

/// One entry in `GET /api/agent`. Mirrors `SpawnAgentResponse` plus the
/// fields a reconnecting UI needs to render initial status before its
/// WS Hello frame arrives. `killed_at = Some(_)` means the agent's PTY
/// has been torn down — the entry comes from the SQLite history only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub agent_id: String,
    pub cli: String,
    pub role: String,
    pub workspace: String,
    pub shim_ready: bool,
    pub shim_exit: Option<i32>,
    /// Unix-ms timestamp the agent was killed; `None` while live.
    #[serde(default)]
    pub killed_at: Option<i64>,
    /// Unix-ms timestamp the agent was spawned. Always populated for
    /// SQLite-backed rows; `None` for in-memory-only entries (shouldn't
    /// happen in practice since spawn always writes to SQLite first).
    #[serde(default)]
    pub spawned_at: Option<i64>,
    /// Blackboard keys this agent is waiting on. Populated from the
    /// `wake_subs` runtime map (whatever was registered at spell launch
    /// from the agent's role.depends_on or the spell-level override).
    /// Empty for agents that don't subscribe to anything — including
    /// historical SQLite-only rows where the subscription has been torn
    /// down. Frontend uses this to draw the depends_on DAG.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Blackboard key this agent will write when its phase completes —
    /// the inverse of `depends_on`. Read from the agent's role manifest
    /// at list time. Empty if the agent has no `role_ref` (inline-only
    /// agents like critic-loop's writer) or its role has no
    /// `handoff_signal`. Frontend uses this to wire DAG edges from
    /// dependents back to their producers.
    #[serde(default)]
    pub handoff_signal: String,
    /// FK into the workspaces table. `None` for pre-migration rows or for
    /// agents spawned via legacy code paths before Step 3 enforces the
    /// field. Frontend uses this for nav grouping in place of the cwd
    /// (`workspace`) string.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// FK into the spell_runs table — set only for agents that came out of
    /// a spell launch (the planner/scout pair from `init` and every
    /// downstream spell). UI doesn't render this directly yet; reserved
    /// for a future "group by spell run" toggle.
    #[serde(default)]
    pub spell_run_id: Option<String>,
    /// FK into the threads table (a workspace's "direction"). `None` = the
    /// workspace's main thread (legacy rows + pre-thread spawns). Frontend
    /// groups the chat/members/ledger by thread using this.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Derived from `spell_runs.caller_agent_id` of this agent's
    /// `spell_run_id`. Populates the GraphPanel "雇佣关系" (parent → child)
    /// edges. `None` for user-initiated spawns (no spell run) and for
    /// top-level spell launches kicked off from the UI (caller_agent_id
    /// itself is None when a human runs the spell from SpellsLauncher).
    /// Only sub-agents spawned via MCP `swarm_run_spell` from another
    /// agent get a non-None parent.
    #[serde(default)]
    pub parent_agent_id: Option<String>,
    /// In-memory pause flag (not persisted). True means the WakeCoordinator
    /// will skip auto-wake delivery for this agent (BlackboardChanged
    /// events still fire for the swarm, just don't kick this agent's
    /// PTY). Manual operator wakes (`POST .../wake`) bypass this and
    /// still go through. Resets to false on server restart.
    #[serde(default)]
    pub paused: bool,
    /// Unix-ms of the agent's most recent tool-level activity, persisted by the
    /// transcript tailer (migration 0013). `None` for agents that never emitted
    /// a tool event (e.g. the orchestrator, which we don't tail). The UI uses
    /// it to tell "wedged" from "idle" even on a cold page load, where the live
    /// `AgentActivity` WS stream isn't available yet.
    #[serde(default)]
    pub last_activity_at: Option<i64>,
    /// Last "alive but can't work" reason (auth/quota banner or first-response
    /// watchdog), persisted via migration 0022. `None` for healthy agents.
    /// Lets the UI re-render an honest failure card on a cold load — the live
    /// `AgentState::Error` WS event is lossy with no resume, so every health
    /// fact needs a REST snapshot.
    #[serde(default)]
    pub last_error: Option<String>,
    /// Coarse class of `last_error` (auth | rate_limit | fatal) steering which
    /// remedy buttons the failure card offers. `None` when no error.
    #[serde(default)]
    pub last_error_kind: Option<String>,
    /// Unix-ms the `last_error` was recorded. `None` when no error.
    #[serde(default)]
    pub last_error_at: Option<i64>,
    /// True when this agent's handoff FAILED — i.e. instead of writing its
    /// success key (`handoff_signal`), it (or the exit-fallback) wrote
    /// `<handoff_signal>.error`. Computed at list time by checking the
    /// blackboard for the `.error` variant. The DAG uses it to render the
    /// node/edge as a failed delivery rather than a successful one — without
    /// it a worker that aborted looked identical to one that succeeded, since
    /// `handoff_signal` always holds the (declared) success key. `false` for
    /// agents with no handoff_signal or whose `.error` key was never written.
    #[serde(default)]
    pub handoff_failed: bool,
    /// True when this agent EXITED without delivering its declared
    /// `handoff_signal` at all — neither the success key NOR the `.error`
    /// variant is on the blackboard. This is the "premature handoff / silent
    /// drop" failure mode (it left, the work it promised never landed, and
    /// unlike `handoff_failed` there isn't even an `.error` marker to notice).
    /// Only set for agents that have a `handoff_signal`, have exited
    /// (`killed_at` or `shim_exit`), and whose keys are both absent. Live
    /// (still-running) agents are never flagged — they may yet deliver.
    #[serde(default)]
    pub handoff_missing: bool,
    /// True when the agent looks STALLED — alive (shim ready, not exited, not
    /// killed) yet sitting on unread mailbox mail whose oldest message has
    /// outlived a conservative threshold while showing no activity in that
    /// same window (computed at list time in rest.rs; see
    /// `Store::stalled_agents_with_unread`). This is the NeedsYou inbox's
    /// "该醒没醒" (wake chain may be broken) signal, NOT the 0.3.0
    /// "suspected-stuck" heuristic: it only fires when something is provably
    /// WAITING for the agent, so an idle worker (no unread) or a busy one
    /// (fresh activity) never matches. The UI words it honestly as
    /// "可能卡住", never "卡住了".
    #[serde(default)]
    pub stalled: bool,
}

/// One tool-level activity row, served by `GET /api/agent/:id/activity`. Same
/// shape as the `SwarmEvent::AgentActivity` WS frame, held in the transcript
/// tailer's in-memory ring so the drawer's Activity tab can BACKFILL on a cold
/// open — the live WS stream is forward-only and shows nothing for an agent
/// that already did its work before the drawer (or the page) opened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentActivityRecord {
    pub agent_id: String,
    /// "tool" | "system" — mirrors `SwarmEvent::AgentActivity::kind`.
    pub kind: String,
    /// One-line human label, e.g. `Edit src/foo.rs`.
    pub label: String,
    /// "running" | "ok" | "error".
    pub phase: String,
    /// Per-turn sequence number; pairs a `running` row with its later
    /// `ok`/`error`. Backfill and live WS share the SAME tailer seq space, so
    /// the UI can merge them by `seq`.
    pub seq: u32,
    #[serde(default)]
    pub duration_ms: Option<u32>,
    pub at: i64,
}

// ── ad-hoc worker DTOs (Magentic-One 重构) ────────────────────────────────

/// `POST /api/worker` 入参。Orchestrator(或上一级 worker)通过 MCP
/// `swarm_spawn_worker` 工具把这个 payload 发到 server。server 复用
/// `spawn::spawn_agent` 拉起 PTY,然后 PTY bootstrap inject `system_prompt`,
/// 同时写 `workers` 表 + `wake_subs` / `exit_keys` 注册。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnWorkerRequest {
    /// 角色注册表 slug(P0-B),例如 "frontend" / "backend" / "reviewer"。
    /// 服务端对注册表校验:未知 slug → 400 + valid options + did-you-mean。
    /// 携带 default_cli / default_model_tier / produces 等默认值。
    pub role: String,
    /// 完整 system prompt,会通过 PTY bootstrap 注入给 worker。Orchestrator
    /// 负责把任务描述、终止条件写清楚;handoff key 由服务端 mint 后追加注入,
    /// 不再由 LLM 自己编。
    pub system_prompt: String,
    /// 可选 CLI 覆盖("claude"/"codex")。缺省取所选 role 的 default_cli。
    #[serde(default)]
    pub cli: Option<String>,
    /// 可选 model/tier 覆盖。抽象 tier(opus/sonnet/haiku)由服务端按 CLI 经
    /// 模型设置(/api/models)解析成具体模型;具体模型 id 原样透传。缺省取 role
    /// 的 default_model_tier,再无则 plugin.default_model / CLI 自身默认。
    #[serde(default)]
    pub model: Option<String>,
    /// 本 worker 产出的 typed output-kinds(P0-A)。空 → 取 role.produces →
    /// 再空则 ["done"]。服务端按 (workspace,thread,role,kind) mint 黑板 key。
    #[serde(default)]
    pub produces: Vec<String>,
    /// typed 上游依赖(P0-A):本 worker 等的「某角色的某产出种类」。服务端
    /// 解析成 minted key 填入 depends_on,杜绝裸串漂移。
    #[serde(default)]
    pub consumes: Vec<ConsumeRef>,
    /// 谁拉起来的(orchestrator agent_id 或上一级 worker)。MCP 工具调用时
    /// 由 `ToolContext.agent_id` 自动填充;直接 REST 调用时调用方必须填。
    pub caller_agent_id: String,
    /// Worker 所属 workspace。MCP 工具自动从 caller 反查;直接 REST 调用
    /// 时必填。
    pub workspace_id: String,
}

/// Typed 上游依赖引用(P0-A)。orchestrator 引用「角色 + 产出种类」,服务端在
/// spawn 时解析成生产者的 minted 黑板 key —— 人读稳定、机器侧 typed。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumeRef {
    /// 上游角色 slug。
    pub from_role: String,
    /// 上游角色的产出种类,缺省 "done"。
    #[serde(default = "default_consume_kind")]
    pub kind: String,
}

fn default_consume_kind() -> String {
    "done".to_string()
}

/// `POST /api/worker` 返回 — 新 spawn 的 PTY agent 元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnWorkerResponse {
    pub agent_id: String,
    /// 解析后的实际 CLI(req.cli 覆盖 or role.default_cli)。
    pub cli: String,
    /// UI 显示名,由 role 派生(role.name 或 slug)。
    pub role_label: String,
    pub workspace: String,
    /// 服务端 mint 的主 handoff key(已注入 worker prompt)。无 handoff 留空。
    #[serde(default)]
    pub handoff_signal: String,
    /// consumes 解析后的 minted 依赖 key 列表(已注册到 WakeCoordinator)。
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// 请求的发动机未安装、服务端改用已安装的 fallback 发动机时,填**最初请求的**
    /// plugin id(此时 `cli` 是实际跑起来的发动机)。None = 没发生 fallback。
    /// 计费红线:fallback 过滤器允许 API 计费的发动机(如 reasonix),调用方
    /// (orchestrator LLM / UI)必须把这个变化如实告知用户。
    #[serde(default)]
    pub fallback_from: Option<String>,
    /// 实际 spawn 的发动机的计费面(kebab-case token,镜像 swarmx-server
    /// BillingSurface 的 serde 命名),如 "api-key" = 按 API key 计费。
    #[serde(default)]
    pub billing_surface: Option<String>,
}

// ── swarm REST DTOs (M3 #18) ─────────────────────────────────────────────

/// `POST /api/message` payload. `from` is optional so a system-emitted
/// notification can omit it; the server fills in a sentinel if it's
/// missing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageRequest {
    #[serde(default)]
    pub from: Option<String>,
    pub to: String,
    pub kind: String,
    pub body: String,
    /// Optional parent message id; threads this message as a reply.
    #[serde(default)]
    pub in_reply_to: Option<i64>,
}

/// One user-facing stage in a product-level reasoning / execution summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThoughtTraceStep {
    pub phase: String,
    pub label: String,
    pub source: String,
    pub at: i64,
}

/// Coarse thought trace attached to chat messages. This is not raw
/// chain-of-thought; it is a stable user-facing progress summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThoughtTrace {
    pub id: String,
    pub trigger_message_id: i64,
    #[serde(default)]
    pub response_message_id: Option<i64>,
    pub agent_id: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    pub status: String,
    pub started_at: i64,
    #[serde(default)]
    pub completed_at: Option<i64>,
    pub summary: Vec<ThoughtTraceStep>,
    pub updated_at: i64,
}

/// Returned by `POST /api/message` so the client knows the persisted row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    pub id: i64,
    pub from_agent: String,
    pub to_agent: String,
    pub kind: String,
    pub body: String,
    pub sent_at: i64,
    pub delivered_at: Option<i64>,
    pub read_at: Option<i64>,
    #[serde(default)]
    pub in_reply_to: Option<i64>,
    /// Direction (thread) this message belongs to; `None` = main / untagged.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Structured metadata for system-generated messages (see
    /// `swarmx_storage::NewMessage::meta`). The UI renders / filters from
    /// `meta.subtype` instead of regex-parsing the prose `body`.
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
    /// Product-level reasoning / execution summary for this reply.
    #[serde(default)]
    pub thought_trace: Option<ThoughtTrace>,
}

/// `POST /api/message/read` payload. The server enforces `to` matches each
/// id's actual recipient, so callers can't mark someone else's mail read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkReadRequest {
    pub to: String,
    pub ids: Vec<i64>,
}

/// `POST /api/message/read` response. `marked` is the subset that this call
/// actually updated (idempotent — repeats return empty).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkReadResponse {
    pub marked: Vec<i64>,
    pub at: i64,
}

/// One consumed wake's inline payload (B1 wake-latency fix).
///
/// History: the Stop hook used to be told only "you have N wakes — go list",
/// so the captain burned 3–5 serial LLM round-trips (`swarm_list_messages` →
/// `swarm_read_blackboard` → …) before it could say anything. `consume_wakes`
/// already claims the rows atomically, so the server snapshots their content
/// here for (nearly) free and `wake-check` can digest it straight into the
/// continuation prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumeWakeItem {
    /// Id of the consumed `kind="wake"` message.
    pub id: i64,
    pub from_agent: String,
    pub sent_at: i64,
    /// The wake mailbox note body, e.g. "共享区 `X` 有更新，请查看".
    #[serde(default)]
    pub body: String,
    /// Blackboard key the wake points at (from the message's `meta.key`),
    /// when the wake carries one.
    #[serde(default)]
    pub key: Option<String>,
    /// Snapshot of that key's content at consume time. `None` when the wake
    /// has no key / the key no longer exists / the response's total payload
    /// budget was already spent — callers fall back to
    /// `swarm_read_blackboard` for those.
    #[serde(default)]
    pub content: Option<String>,
    /// True when `content` (or `body`) was cut at the per-entry cap: the
    /// prefix is a teaser, read the key in full via `swarm_read_blackboard`.
    #[serde(default)]
    pub truncated: bool,
}

/// `POST /api/message/consume_wakes` response. `wakes` is additive —
/// `#[serde(default)]` keeps pre-B1 clients (which read only `count`)
/// deserializing fine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumeWakesResponse {
    pub to: String,
    pub count: i64,
    pub ids: Vec<i64>,
    /// Inline payload snapshots for (a bounded prefix of) the consumed wakes,
    /// oldest-first. May be shorter than `ids` — or empty — under the
    /// response's caps or when enrichment fails soft; `ids`/`count` are
    /// always the full consumed set.
    #[serde(default)]
    pub wakes: Vec<ConsumeWakeItem>,
}

/// One row from `GET /api/blackboard-history/*path`. `content` is omitted by
/// default (`?include_content=true` to include) so listing 50 versions of a
/// large file doesn't blow up the JSON payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlackboardHistoryEntry {
    pub id: i64,
    #[serde(default)]
    pub agent_id: Option<String>,
    pub op: String,
    pub path: String,
    pub sha256: String,
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// `PUT /api/blackboard/:path` payload. The path itself rides in the URL;
/// the body carries the new content. `agent_id` lets the caller attribute
/// the write — `None` ⇒ system / external.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteBlackboardRequest {
    #[serde(default)]
    pub agent_id: Option<String>,
    pub content: String,
}

/// Returned by `GET /api/blackboard/:path` (snapshot of the latest content).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlackboardSnapshot {
    pub path: String,
    pub content: String,
    pub sha256: String,
    pub at: i64,
}

/// Returned by `GET /api/blackboard` — one entry per known path, latest op.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlackboardEntry {
    pub path: String,
    pub sha256: String,
    pub at: i64,
    pub op: String,
}

// ── recording DTOs (M3 #19) ──────────────────────────────────────────────

/// One entry in `GET /api/recording`. The .cast file content is *not*
/// inlined — clients hit `GET /api/recording/:id` to stream the bytes.
/// `finalized_at = None` means the recording is still live (its agent's
/// PTY hasn't EOFed yet).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingInfo {
    pub id: String,
    pub agent_id: String,
    pub started_at: i64,
    #[serde(default)]
    pub finalized_at: Option<i64>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
    pub cols: i64,
    pub rows: i64,
    #[serde(default)]
    pub last_seq: Option<i64>,
}

/// Public summary of a loaded spell. Returned by `GET /api/spells` so
/// the UI can populate a launcher dropdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpellInfo {
    pub name: String,
    pub description: String,
    /// Roles the spell will spawn, in declaration order. UI shows this so
    /// users know what the spell will fork on their machine before clicking
    /// "run".
    pub agents: Vec<SpellAgentInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpellAgentInfo {
    pub role: String,
    pub cli: String,
}

/// `POST /api/spell/run` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSpellRequest {
    pub name: String,
    /// Free-form task description; substituted into each agent's
    /// `system_prompt` wherever `{task}` appears.
    pub task: String,
    /// Optional workspace directory (absolute path). When the spell
    /// has `shared_workspace = true` (M6a fullstack-feature), every
    /// agent runs with cwd set to this path. When omitted, the server
    /// generates a fresh `<workspaces_root>/spell-<uuid>/` shared dir
    /// so a launch never silently no-ops. Ignored for spells that
    /// don't set `shared_workspace` (each agent gets its own subdir
    /// under workspaces_root as before).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_dir: Option<String>,
    /// Workspaces table FK the spawned agents should be associated with.
    /// When omitted, the server tries to reverse-look-up via
    /// `caller_agent_id`; if neither is available the request is rejected
    /// (post-Step-3). This is what fixes the "orphan workspace tab" bug —
    /// every spell agent inherits the caller's workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// agent_id of the caller, transparently injected by the
    /// `swarm_run_spell` MCP tool from its `ToolContext`. Server reverse-
    /// resolves it to the caller's `workspace_id`. Direct REST callers
    /// (CreateWizard, top-bar launcher) leave this empty and pass
    /// `workspace_id` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_agent_id: Option<String>,
    /// The thread (direction) the spell should run in. When omitted, the
    /// server resolves the caller's own thread (via `caller_agent_id`), else
    /// falls back to the workspace's main thread. Lets a UI launcher target a
    /// specific direction explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Optional engine (cli-plugin id) for the spell's `orchestrator` agent,
    /// overriding its role `default_cli`. Lets a workspace be created with e.g.
    /// `opencode` as the captain instead of the default `claude`. Only the
    /// orchestrator agent is overridden; worker agents keep their roles'
    /// engines. Ignored (with a warning) if it names no known plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captain_cli: Option<String>,
}

/// `POST /api/spell/run` response. Lists the agents the runner actually
/// spawned (role → agent_id), so the UI can deep-link directly into them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSpellResponse {
    pub spell: String,
    pub agents: Vec<RunSpellAgent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSpellAgent {
    pub role: String,
    pub cli: String,
    pub agent_id: String,
    /// Set when this agent's requested CLI wasn't installed and the server
    /// substituted an installed fallback engine (then `cli` is the engine that
    /// actually spawned). None = no fallback. Same billing-red-line contract
    /// as `SpawnAgentResponse::fallback_from`: the UI must surface it loudly.
    #[serde(default)]
    pub fallback_from: Option<String>,
    /// Billing surface of the engine that actually spawned (kebab-case token,
    /// e.g. "api-key" = billed per API key).
    #[serde(default)]
    pub billing_surface: Option<String>,
}

// ── workspaces (Step 1 of workspace-as-first-class refactor) ─────────────

/// One node in a workspace's user-defined LOGICAL tree. The PRIMARY project
/// dir lives in `Workspace.cwd` (the implicit root); these rows are all OTHER
/// nodes. Semantics depend on `(role, parent_id)`:
/// - role="project", parent_id=None → a top-level PEER project (sibling of the
///   primary), physical path anywhere.
/// - role="dependency"|"tool", parent_id=None → a source mount under the
///   PRIMARY (cwd) project.
/// - any role, parent_id=Some(other root's id) → a child of that node (e.g. a
///   lib mounted under a peer project). Physical path is arbitrary / unrelated
///   to the parent's path — that decoupling is the whole point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRoot {
    /// Server-assigned id; clients omit it on create/add (filled from the DB).
    #[serde(default)]
    pub id: String,
    pub path: String,
    pub role: String, // "project" | "dependency" | "tool"
    #[serde(default)]
    pub label: Option<String>,
    /// Logical parent (another WorkspaceRoot's `id`) or `None` for a top-level
    /// node. Decoupled from physical path nesting.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// The branch currently checked out at `path`, filled at list time for the
    /// sidebar's live branch chip. `None` for a non-git dir / detached HEAD, and
    /// always omitted on create/add payloads (server computes it). Same
    /// fill-on-output, skip-on-input contract as `id`.
    #[serde(default)]
    pub branch: Option<String>,
}

/// One "direction" inside a workspace: its own orchestrator + worker subtree
/// + dual ledger + (optionally) an isolated git worktree. Mirrors the storage
/// `ThreadRecord` minus `deleted_at` (the API only surfaces alive threads).
/// `slug` is the blackboard / URL segment; the main thread's slug is `main`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadInfo {
    pub id: String,
    pub workspace_id: String,
    pub slug: String,
    #[serde(default)]
    pub name: Option<String>,
    /// "shared" | "worktree"
    pub isolation: String,
    /// Git branch backing the worktree (None until/unless isolated).
    #[serde(default)]
    pub branch: Option<String>,
    /// Working dir agents in this thread run in. Equals the workspace cwd for a
    /// shared thread; the worktree path once isolated.
    pub cwd: String,
    /// "ready" | "preparing" | "failed"
    pub state: String,
    /// Per-direction model override (abstract tier opus|sonnet|haiku or a
    /// concrete model id). None = use the global default. Set via the chat
    /// header model picker.
    #[serde(default)]
    pub model_tier: Option<String>,
    /// Per-direction reasoning/thinking effort (abstract low|medium|high|max).
    /// None = the model's own default. Set via the chat model picker.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Live (computed at list time, not persisted): does `cwd` have uncommitted
    /// changes? Lets the sidebar flag "this direction's agent has unsaved work"
    /// with a dirty dot. `false` for a clean/non-git/errored tree.
    #[serde(default)]
    pub dirty: bool,
    /// Live (computed at list time, not persisted): commits this direction's
    /// branch is ahead of / behind the workspace's base branch (the main
    /// worktree's current branch). Purely local — never fetches. `None` for the
    /// main/shared direction, a non-git tree, or detached HEAD.
    #[serde(default)]
    pub ahead: Option<i64>,
    #[serde(default)]
    pub behind: Option<i64>,
    pub created_at: i64,
}

/// One row from `GET /api/workspaces`. `member_count` is computed at list
/// time (live agents whose `workspace_id` points here). Soft-deleted
/// workspaces aren't included by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    /// First 8 chars of `id`, used as the URL slug for `/chat/:slug`.
    pub slug: String,
    pub name: String,
    pub cwd: String,
    /// Branch currently checked out at `cwd` (the agent's terminal落脚点), filled
    /// at list time for the sidebar's live branch chip. `cwd` is a synthetic
    /// tree node — not a `WorkspaceRoot` row — so its branch rides here rather
    /// than on `roots`. `None` for a non-git cwd / detached HEAD.
    #[serde(default)]
    pub cwd_branch: Option<String>,
    #[serde(default)]
    pub accent: Option<String>,
    pub created_at: i64,
    pub member_count: i64,
    /// Attached dependency-source roots (empty when none). The primary
    /// project dir stays in `cwd` and is NOT duplicated here.
    #[serde(default)]
    pub roots: Vec<WorkspaceRoot>,
    /// The workspace's "directions" (always ≥1: an auto-created `main`).
    /// Oldest-first; the first entry is the main thread.
    #[serde(default)]
    pub threads: Vec<ThreadInfo>,
}

/// `POST /api/workspaces` payload. CreateWizard sends this before launching
/// the `init` spell so the new workspace is persisted up-front (replaces
/// the old blackboard `workspace.name.<slug>` / `workspace.accent.<slug>`
/// keys).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub cwd: String,
    #[serde(default)]
    pub accent: Option<String>,
    /// Dependency-source root folders to attach at creation time. Empty by
    /// default. The primary project dir goes in `cwd`, not here.
    #[serde(default)]
    pub roots: Vec<WorkspaceRoot>,
}

/// `POST /api/workspaces/:id/threads` — open a new direction. Zero-friction by
/// design: `name` is optional (the orchestrator names it later from the first
/// message via `swarm_name_thread`). The server mints a placeholder slug.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateThreadRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// Open an EXISTING branch as this direction instead of creating a fresh
    /// one. When set, the worktree attaches this exact branch (the direction's
    /// name/slug derive from it if `name` is absent). swarmx's
    /// worktree-per-direction take on "switch branch" — never an in-place
    /// `git checkout` that would yank the floor out from a running agent.
    #[serde(default)]
    pub branch: Option<String>,
}

/// One local branch of a workspace's repo, for the "open existing branch as a
/// direction" picker (`GET /api/workspaces/:id/branches`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    /// Already checked out in some worktree (the main one or another
    /// direction) → can't be attached again, so the picker disables it.
    pub checked_out: bool,
}

/// `PATCH /api/workspaces/:id/threads/:tid` — (re)name a direction. Setting a
/// real name is ALSO the trigger that kicks off background git isolation: a
/// git project gets a worktree on `<project>-<slug>` so directions don't
/// clobber each other's working tree. Pure rename for non-git / already-named.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateThreadRequest {
    pub name: String,
}

/// `POST /api/workspaces/:id/fusion` — start a multi-model competition. One
/// isolated contestant direction is created per label; the same `need` is sent
/// verbatim to each so the comparison is fair. `labels` are 2..=4 contestant
/// names (typically CLI/model names like "claude", "codex", "deepseek").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFusionRequest {
    pub need: String,
    pub labels: Vec<String>,
    /// Optional objective gate: a shell command run inside each contestant's
    /// worktree by the auto-judge BEFORE the LLM deliberates (e.g. `pytest -q`,
    /// `cargo test`, `npm test`). Contestants whose check fails are objectively
    /// out; pass/fail + output tails are injected into the judge prompt so the
    /// verdict is grounded in execution, not aesthetics. None/empty = no gate
    /// (judge falls back to pure-diff reading — the original behaviour).
    #[serde(default)]
    pub check_cmd: Option<String>,
    /// Optional auto-implement panel: per-label CLI to spawn so each contestant
    /// implements `need` autonomously (OpenRouter-fusion-style full-auto panel).
    /// Key = label (must match an entry in `labels`), value = CLI name
    /// ("claude" | "codex" | "opencode" | "reasonix" | "zulu"), optionally with a
    /// model as `"cli:model"` — e.g. `"zulu:Deepseek V4 Pro"` and
    /// `"zulu:GLM-5.2"` to race different models under one zulu license. Labels
    /// absent from this map get an empty worktree the user drives by hand.
    /// Empty/None = no auto-implement (every contestant is user-driven).
    #[serde(default)]
    pub panel: Option<std::collections::HashMap<String, String>>,
    /// Full-auto "novice" mode. When true the server: picks a model panel itself
    /// if `panel` is empty (usable engines, else N zulu models on one license),
    /// auto-implements every contestant, then — once the contestants settle —
    /// auto-enters the synthesize judge stage, whose watchdog lands the merge.
    /// Zero clicks after submit. `labels` may be empty (the server derives them
    /// from the chosen panel). Ignored by the manual flow.
    #[serde(default)]
    pub autopilot: bool,
}

/// A fusion competition binding N isolated contestant directions (and later a
/// judge direction). Wire mirror of the storage FusionBatchRecord.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionBatch {
    pub id: String,
    pub workspace_id: String,
    pub slug: String,
    pub need: String,
    pub contestant_thread_ids: Vec<String>,
    pub judge_thread_id: Option<String>,
    /// "running" | "judging" | "done" | "failed".
    pub status: String,
    /// The winning contestant direction once the batch is decided (status →
    /// 'done'); None until a verdict is reached.
    #[serde(default)]
    pub winner_thread_id: Option<String>,
    /// The objective check command run per contestant by the auto-judge, if the
    /// batch was created with one. None = no objective gate (pure-diff judging).
    #[serde(default)]
    pub check_cmd: Option<String>,
    pub created_at: i64,
}

/// One contestant's changeset, gathered for the judge: which direction, its
/// branch, and the repo-relative files it changed vs the base. The judge reads
/// these to compare/synthesize without the contestants ever seeing each other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionContestantDiff {
    pub thread_id: String,
    pub slug: String,
    pub name: Option<String>,
    pub branch: Option<String>,
    /// Files this contestant changed vs the base branch (empty if it produced
    /// nothing, or isolation degraded so it has no branch to diff).
    pub files: Vec<String>,
    /// True if the contestant never got an isolated worktree (degraded to
    /// shared) — its work isn't separable, surfaced so the judge/UI is honest.
    pub degraded: bool,
    /// Objective check outcome, present when the batch carried a check_cmd and
    /// the judge stage ran it in this contestant's worktree. Both auto AND
    /// manual judging run the gate (manual used to skip it, silently ignoring
    /// a check_cmd the create form had accepted). None = no gate was run
    /// (pure-diff judging). Some(true) = check passed, Some(false) = failed
    /// (objectively out before any deliberation).
    #[serde(default)]
    pub check_passed: Option<bool>,
    /// Tail of the check command's combined stdout/stderr (truncated), so the
    /// judge + UI can see WHY a contestant failed, not just that it did.
    #[serde(default)]
    pub check_output: Option<String>,
}

/// `POST /api/workspaces/:id/fusion/:bid/judge` response: the freshly-created
/// judge direction plus every contestant's diff bundle for review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionJudgeResponse {
    pub batch: FusionBatch,
    pub judge_thread_id: String,
    pub base: Option<String>,
    pub contestants: Vec<FusionContestantDiff>,
    /// Set only when the judge was started in auto mode (`?auto=true`): the id
    /// of the real CLI agent spawned inside the judge direction to read every
    /// contestant's diff, pick a winner, and call the decide endpoint itself.
    /// None for the manual flow (judge → human decide), which is unchanged.
    #[serde(default)]
    pub judge_agent_id: Option<String>,
}

/// `POST /api/workspaces/:id/fusion/:bid/decide` — the verdict. The caller picks
/// ONE winning contestant; the batch records it, flips to 'done', and (unless
/// `merge` is false) merges the winner's branch back into the base line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionDecideRequest {
    /// thread.id of the winning contestant. MUST be one of the batch's
    /// contestants (validated server-side, 400 otherwise).
    pub winner_thread_id: String,
    /// Whether to merge the winner's branch into base as part of deciding.
    /// Defaults to true; set false to record the verdict without merging (e.g.
    /// the user wants to review the merge separately).
    #[serde(default = "default_true")]
    pub merge: bool,
}

fn default_true() -> bool {
    true
}

/// `POST /api/workspaces/:id/fusion/:bid/decide` response: the decided batch and
/// the outcome of the winner's merge (absent if `merge` was false).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionDecideResponse {
    pub batch: FusionBatch,
    pub winner_thread_id: String,
    /// The merge result of folding the winner into base. None when the request
    /// asked not to merge. "merged" → clean; "resolving" → conflicts, an AI
    /// resolver agent was spawned (see `resolver_agent_id`).
    pub merge_status: Option<String>,
    /// Base branch the winner was merged into (when merged).
    pub base: Option<String>,
    /// Files the winner changed / that conflicted (depending on merge_status).
    #[serde(default)]
    pub files: Vec<String>,
    /// Set when merge_status == "resolving": the spawned resolver agent's id.
    #[serde(default)]
    pub resolver_agent_id: Option<String>,
}

// ── Answer/research fusion (OpenRouter-Fusion style, zulu-panel backed) ──────
// A DIFFERENT feature from the code-competition fusion above: given a question,
// run a PANEL of models in parallel, have a JUDGE produce a structured analysis
// (consensus/contradictions/…, NOT a vote), then SYNTHESIZE a final answer.

/// `POST /api/workspaces/:id/fusion-consult` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionConsultRequest {
    /// The question / task to deliberate on.
    pub question: String,
    /// 1..8 zulu model ids (or display names) for the panel. Empty → server
    /// picks a sensible default trio.
    #[serde(default)]
    pub panel: Vec<String>,
    /// Model that writes the structured judge analysis. Default: a strong model.
    #[serde(default)]
    pub judge_model: Option<String>,
    /// Model that writes the final synthesis. Default: same as judge.
    #[serde(default)]
    pub synthesis_model: Option<String>,
}

/// One panel member's answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionPanelAnswer {
    pub model: String,
    /// The model's answer text, or an error note when `ok == false`.
    pub answer: String,
    pub ok: bool,
    pub elapsed_ms: u64,
}

/// The judge's structured comparison — consensus/contradictions/coverage/unique
/// insights/blind spots. A COMPARISON, not a merge or a vote.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FusionJudgeAnalysis {
    /// Points all/most panel members agree on (higher-confidence).
    #[serde(default)]
    pub consensus: Vec<String>,
    /// Where members disagree.
    #[serde(default)]
    pub contradictions: Vec<String>,
    /// Insights only one member surfaced.
    #[serde(default)]
    pub unique_insights: Vec<String>,
    /// Gaps none of the members addressed.
    #[serde(default)]
    pub blind_spots: Vec<String>,
    /// Free-text note when the judge couldn't produce clean structure.
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/workspaces/:id/fusion-consult` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionConsultResponse {
    pub question: String,
    pub panel: Vec<FusionPanelAnswer>,
    pub analysis: FusionJudgeAnalysis,
    /// The outer model's final answer, synthesized from the analysis + panel.
    pub synthesis: String,
    /// Rough token-cost note (panel is N× a single call — governance signal).
    pub cost_note: String,
}
