// Mirror of swarmx-protocol's wire types. Hand-maintained for now;
// ts-rs auto-generation lands in M3 once swarmx-protocol grows.

export type ServerControl =
  | {
      type: "hello";
      seq_start: number;
      agent_id: string;
      shim_ready?: boolean;
      shim_exit?: number | null;
    }
  | { type: "shim_ready" }
  | { type: "shim_exit"; code: number }
  | { type: "eof" }
  | { type: "error"; message: string };

export type ClientControl =
  | { type: "resize"; cols: number; rows: number }
  | { type: "ack"; seq: number }
  | { type: "resume"; last_seq: number }
  | { type: "signal"; sig: "SIGINT" | "SIGTERM" | "SIGHUP" }
  | { type: "kill" }
  | { type: "detach" };

export interface CliPluginInfo {
  id: string;
  display_name: string;
  binary: string;
  /** Whether the server can resolve this CLI on its augmented runtime PATH. */
  installed?: boolean;
  /** Absolute executable path, when found. */
  resolved_path?: string | null;
  /** Best-effort `<binary> --version` first line. */
  version?: string | null;
  /** Official install guidance for known bundled CLIs. */
  install?: CliInstallHint | null;
  /** Keystroke-settle delay (ms) the terminal applies after the CLI is ready
   *  (codex ~300, claude 0). Mirrors the backend manifest; drives the input
   *  policy so adding a CLI needs no frontend edit. Optional for back-compat
   *  with an older server that doesn't send it. */
  input_settle_ms?: number;
  /** Default model this CLI runs when a spawn doesn't override it (L5c).
   *  Undefined ⇒ the CLI picks its own default. A spawn UI can use this to
   *  pre-fill / offer a model picker without hardcoding per-CLI names. */
  default_model?: string;
}

export interface CliInstallHint {
  title: string;
  summary: string;
  docs_url: string;
  commands: string[];
  verify_command?: string | null;
  login_command?: string | null;
}

/**
 * Real-usability verdict from `GET /api/plugins/probe`. The honest answer to
 * "can I use this engine right now" — the backend actually STARTS the CLI over
 * PTY (not just "is the binary installed"). See `engine_probe.rs`.
 *   - usable        — started, no auth/quota banner, didn't exit early.
 *   - needs_login   — alive but an auth needle fired (must log in).
 *   - not_installed — binary not on the runtime PATH.
 *   - not_usable    — exited early / spawn failed / timed out (missing key, etc.).
 */
export type EngineProbeState =
  | "usable"
  | "needs_login"
  | "not_installed"
  | "not_usable";

export interface EngineProbe {
  engine: string;
  state: EngineProbeState;
  /** Human-facing detail (why it can't run); absent when usable. */
  reason?: string | null;
  /** Coarse failure class: "auth" | "fatal" | "timeout" | "exit". */
  kind?: string | null;
  /** Unix-ms when the verdict was reached (for staleness). */
  probed_at: number;
  /** How the verdict was reached: "ready" | "health-needle" | "exit" | … */
  method: string;
}

export interface EngineProbeResponse {
  /** A real-usability sweep is running right now (poll until false). */
  probing: boolean;
  engines: EngineProbe[];
}

/**
 * Per-engine readiness the UI renders: install info merged with the probe
 * verdict. `state === "unknown"` = installed but not yet probed — shown
 * neutrally ("已安装（未验证）"), never as a fake green "ready".
 */
export interface EngineReadiness {
  id: string;
  display_name: string;
  installed: boolean;
  install?: CliInstallHint | null;
  state: EngineProbeState | "unknown";
  reason?: string | null;
  kind?: string | null;
  probed_at?: number | null;
  /** How the verdict was reached (probe `method`): "turn-ok" (a real one-turn
   *  check passed), "live-ready" (a live agent came up), "ready" (launch only),
   *  "turn-auth" / "health-needle" / "exit" / … Drives the evidence marker so a
   *  "usable" isn't ambiguous about how strongly it was proven. */
  method?: string | null;
}

export interface SpawnAgentRequest {
  cli: string;
  role?: string;
  workspace?: string;
  /** FK into the workspaces table. Mandatory post-Step-3 of the
   *  workspace-as-first-class rollout — the orphan `+ Claude` button
   *  is routed through CreateWizard when no active workspace exists. */
  workspace_id: string;
  /** Direction/thread to bind this agent to. Omit to use the workspace's main
   *  thread; UI launchers should pass the active direction when available. */
  thread_id?: string | null;
  /** Optional model overlay (L5c). Passed to the CLI via its manifest
   *  `model_args` template. Omit to use the plugin's default_model / the
   *  CLI's own default. Decouples model from CLI id. */
  model?: string;
}

export interface SpawnAgentResponse {
  agent_id: string;
  cli: string;
  role: string;
  workspace: string;
  /** Set when the requested CLI wasn't installed and the server substituted
   *  an installed fallback engine: the ORIGINALLY requested plugin id (`cli`
   *  then names the engine that actually spawned). Billing red line: the
   *  fallback filter admits API-billed engines (e.g. reasonix), so the UI
   *  must surface this loudly instead of a paid engine running in disguise. */
  fallback_from?: string | null;
  /** Billing surface of the engine that ACTUALLY spawned (kebab-case token:
   *  "interactive-subscription" | "cli-account" | "api-key" | …). */
  billing_surface?: string | null;
}

export interface AgentInfo {
  agent_id: string;
  cli: string;
  role: string;
  workspace: string;
  shim_ready: boolean;
  shim_exit: number | null;
  killed_at?: number | null;
  spawned_at?: number | null;
  /** Blackboard keys this agent is subscribed to (M6b wake-coordinator).
   *  Empty for historical / inline-only agents. */
  depends_on?: string[];
  /** Blackboard key this agent will write when its phase completes.
   *  Empty for roles with no handoff_signal (e.g. planner). */
  handoff_signal?: string;
  /** True when the agent's handoff FAILED — it wrote `<handoff_signal>.error`
   *  instead of the success key (computed server-side from the blackboard).
   *  `handoff_signal` always holds the declared success key, so the DAG needs
   *  this flag to tell a failed delivery from a successful one. */
  handoff_failed?: boolean;
  /** True when the agent EXITED without delivering its declared
   *  `handoff_signal` at all — neither the success key nor the `.error`
   *  variant is on the blackboard (computed server-side). This is the
   *  "premature handoff / silent drop" failure: it left, promised work never
   *  landed, and unlike `handoff_failed` there's not even an `.error` marker.
   *  The chat surfaces this so the user isn't silently misled into thinking a
   *  finished-looking turn actually delivered. Live agents are never flagged. */
  handoff_missing?: boolean;
  /** True when the agent looks STALLED — alive (shim ready, not exited, not
   *  killed, not paused) yet holding unread mailbox mail older than a
   *  conservative threshold (10min) with no activity in that window
   *  (computed server-side; see `stalled_agents_with_unread` in store.rs).
   *  This is the inbox's "该醒没醒" signal: it only fires when something is
   *  provably WAITING for the agent while it shows no sign of life, so an
   *  idle worker (no unread) or a busy one (fresh activity) never matches.
   *  Word it honestly in the UI: "可能卡住", never "卡住了". */
  stalled?: boolean;
  /** FK into the workspaces table. Null for pre-migration rows or for
   *  legacy spawn paths from before Step 3. The frontend groups the
   *  left nav by this — historical rows fall through to the unnamed
   *  bucket and won't appear as orphan tabs. */
  workspace_id?: string | null;
  /** FK into the spell_runs table. Set for every agent spawned by a
   *  spell launch; null for direct `+ Claude` clicks. Reserved for a
   *  future "group by spell run" toggle. */
  spell_run_id?: string | null;
  /** Derived server-side from spell_runs.caller_agent_id. Non-null only
   *  for sub-agents spawned via MCP `swarm_run_spell` from another agent.
   *  Drives the parent → child spawn edges in the DAG view (see
   *  lib/dagEdgeDerivation deriveSpawnEdges). */
  parent_agent_id?: string | null;
  /** FK into the threads table (a workspace's "direction"). Null = the
   *  workspace's main direction (legacy rows + pre-thread spawns). The UI
   *  groups chat/members/ledger by this; null is treated as the main thread. */
  thread_id?: string | null;
  /** In-memory pause state. True when the operator has hit "暂停" — the
   *  WakeCoordinator skips auto-wake for this agent until resume. Manual
   *  ⚡ wakes still work. Resets on server restart. */
  paused?: boolean;
  /** Unix-ms of the agent's most recent tool-level activity, persisted by the
   *  transcript tailer (server migration 0013). Null for agents that never
   *  emitted a tool event (e.g. the orchestrator, which isn't tailed). Used to
   *  tell "wedged" from "idle" even on a cold load, before the live
   *  AgentActivity WS stream has caught up. */
  last_activity_at?: number | null;
  /** Last "alive but can't work" reason — auth/quota banner caught by the
   *  server's HealthScanner, or the first-response watchdog firing (server
   *  migration 0022). Null for healthy agents. Lets the UI re-render an honest
   *  failure card on a cold load, since the live AgentState::Error WS event is
   *  lossy with no resume. */
  last_error?: string | null;
  /** Coarse class of `last_error` (auth | rate_limit | fatal | watchdog)
   *  steering which remedy buttons the failure card offers. Null when healthy. */
  last_error_kind?: string | null;
  /** Unix-ms the `last_error` was recorded. Null when healthy. */
  last_error_at?: number | null;
}

// ── M3 swarm DTOs ────────────────────────────────────────────────────────

/** Structured metadata the server stamps on system-generated messages
 *  (typed-payload pattern; see swarmx-storage migration 0012). The UI
 *  renders / filters from `subtype` instead of regex-parsing the prose body.
 *  Absent (undefined/null) for agent free-text messages. */
export interface MessageMeta {
  /** "wake" (coordination ping) | "completion" (worker delivered + disbanded). */
  subtype?: string;
  /** For wakes: "blackboard" (auto, redundant → UI filters) | "manual" (operator). */
  reason?: string;
  /** For blackboard wakes: the raw blackboard key the agent must check. */
  key?: string;
  /** For completions: the handoff signal that was delivered. */
  signal?: string;
  /** For "dispatch": the spawned worker's agent id, so the card can open it. */
  child_agent?: string;
  /** For "dispatch": the spawned worker's role label, shown on the card. */
  child_role?: string;
  /** For "dispatch": the worker's registry slug (diagnostic). */
  role_slug?: string;
  /** For "dispatch": set when the worker's requested engine wasn't installed
   *  and a fallback engine was spawned instead (billing red line — the card
   *  shows the substitution so a paid-API engine never runs in disguise). */
  fallback_from?: string;
  /** For "dispatch": the fallback engine that actually spawned. */
  fallback_cli?: string;
  /** For "dispatch": billing surface of the engine that actually spawned
   *  (kebab-case token, e.g. "api-key" = billed per API key). */
  billing_surface?: string;
}

export interface MessageRecord {
  id: number;
  from_agent: string;
  to_agent: string;
  kind: string;
  body: string;
  sent_at: number;
  delivered_at: number | null;
  read_at: number | null;
  in_reply_to: number | null;
  /** Direction (thread) this message belongs to; null = main / untagged. */
  thread_id?: string | null;
  meta?: MessageMeta | null;
  thought_trace?: ThoughtTrace | null;
}

export interface ThoughtTraceStep {
  phase: string;
  label: string;
  source: string;
  at: number;
}

export interface ThoughtTrace {
  id: string;
  trigger_message_id: number;
  response_message_id?: number | null;
  agent_id: string;
  workspace_id?: string | null;
  thread_id?: string | null;
  status: "active" | "done" | "expired" | "error" | string;
  started_at: number;
  completed_at?: number | null;
  summary: ThoughtTraceStep[];
  updated_at: number;
}

export interface SendMessageRequest {
  from?: string;
  to: string;
  kind: string;
  body: string;
  in_reply_to?: number;
}

export interface MarkReadRequest {
  to: string;
  ids: number[];
}

export interface MarkReadResponse {
  marked: number[];
  at: number;
}

export interface BlackboardHistoryEntry {
  id: number;
  agent_id: string | null;
  op: string;
  path: string;
  sha256: string;
  at: number;
  content?: string | null;
}

export interface BlackboardEntry {
  path: string;
  sha256: string;
  at: number;
  op: string;
}

export interface BlackboardSnapshot {
  path: string;
  content: string;
  sha256: string;
  at: number;
}

export interface WriteBlackboardRequest {
  agent_id?: string;
  content: string;
}

// ── M5c spell DTOs ───────────────────────────────────────────────────────

export interface SpellAgentInfo {
  role: string;
  cli: string;
}

export interface SpellInfo {
  name: string;
  description: string;
  agents: SpellAgentInfo[];
}

export interface RunSpellRequest {
  name: string;
  task: string;
  /** Absolute path to a shared workspace directory. Honoured by spells
   *  whose manifest declares `shared_workspace = true` (e.g. the M6a
   *  fullstack-feature spell where FE/BE/Test work in one monorepo).
   *  Ignored by per-agent spells. Server defaults to the resolved
   *  workspace's `cwd` if omitted. */
  workspace_dir?: string;
  /** FK into the workspaces table. The UI's launcher passes this
   *  whenever the user starts a spell from outside an existing agent
   *  context (CreateWizard's init launch, the top-bar SpellsLauncher).
   *  MCP `swarm_run_spell` calls leave it empty and pass `caller_agent_id`
   *  instead — server reverse-resolves both into the same field. */
  workspace_id?: string;
  /** agent_id of the caller. Only set by the MCP tool; UI callers leave
   *  this empty. */
  caller_agent_id?: string;
  /** The direction (thread) the spell should run in. UI launchers set this to
   *  target a specific direction; omitted resolves to the main direction. */
  thread_id?: string;
  /** Engine (cli-plugin id) for the spell's orchestrator/captain, overriding
   *  the role's `default_cli`. Lets a workspace run with e.g. opencode as the
   *  captain. Omitted ⇒ role default (claude). */
  captain_cli?: string;
}

/** `POST /api/workspaces/:id/threads` — open a new direction. `name` optional
 *  (zero-friction: the orchestrator names it from the first message). `branch`
 *  opens an EXISTING branch as the direction (attach a worktree to it) instead
 *  of creating a fresh one. */
export interface CreateThreadRequest {
  name?: string | null;
  branch?: string | null;
}

/** One local branch of a workspace's repo (`GET /api/workspaces/:id/branches`),
 *  for the "open existing branch as a direction" picker. */
export interface BranchInfo {
  name: string;
  /** Already checked out in some worktree → can't be attached again. */
  checked_out: boolean;
}

export interface RunSpellAgent {
  role: string;
  cli: string;
  agent_id: string;
  /** Set when this agent's requested CLI wasn't installed and a fallback
   *  engine was spawned instead (same contract as SpawnAgentResponse). */
  fallback_from?: string | null;
  /** Billing surface of the engine that actually spawned (kebab-case token). */
  billing_surface?: string | null;
}

export interface RunSpellResponse {
  spell: string;
  agents: RunSpellAgent[];
}

// ── workspaces (workspace-as-first-class refactor) ───────────────────────

/** An attached dependency-source root folder. The workspace's primary
 *  project dir is `Workspace.cwd`; these are extra source roots agents may
 *  read directly (so they don't have to decompile/guess a dependency).
 *  `role` is "dependency" | "tool" today (kept open as string). */
export interface WorkspaceRoot {
  /** Server-assigned id. Omitted on create/add payloads (server fills it). */
  id?: string;
  path: string;
  /** "project" (a top-level peer project) | "dependency" | "tool". */
  role: string;
  label?: string | null;
  /** Logical-tree parent: another root's id, or null/undefined for a
   *  top-level node. Decoupled from physical path — a node's `path` can live
   *  anywhere; parent_id only expresses the "depends on / belongs under"
   *  relationship the user chose. */
  parent_id?: string | null;
  /** Branch currently checked out at `path`, filled by the workspaces list for
   *  the sidebar's live branch chip. null for a non-git dir / detached HEAD. */
  branch?: string | null;
}

/** One "direction" inside a workspace: its own orchestrator + worker subtree +
 *  dual ledger + (optionally) an isolated git worktree. Mirrors the server
 *  `ThreadInfo`. `slug` is the per-direction blackboard / URL segment; the main
 *  direction's slug is always `main`. */
export interface ThreadInfo {
  id: string;
  workspace_id: string;
  slug: string;
  name?: string | null;
  /** "shared" | "worktree" */
  isolation: string;
  branch?: string | null;
  /** Working dir agents in this direction run in (= workspace cwd for a shared
   *  thread; the worktree path once isolated). */
  cwd: string;
  /** "ready" | "preparing" | "failed" */
  state: string;
  /** Per-direction model override (abstract tier opus|sonnet|haiku or a concrete
   *  model id). Null = use the global default. Set via the chat model picker. */
  model_tier?: string | null;
  /** Per-direction reasoning/thinking effort (abstract low|medium|high|max).
   *  Null = the model's own default. Set via the chat model picker. */
  reasoning_effort?: string | null;
  /** Live (computed server-side at list time): does `cwd` have uncommitted
   *  changes? Drives the sidebar's dirty dot. */
  dirty?: boolean;
  /** Live (computed server-side, purely local — no fetch): commits this
   *  direction's branch is ahead of / behind the workspace's base branch.
   *  Null for the main/shared direction or a non-git tree. */
  ahead?: number | null;
  behind?: number | null;
  created_at: number;
}

/** Preview of what a direction changed, before merging it back to the main line
 *  (`GET /api/workspaces/:id/threads/:tid/diff`). */
export interface ThreadDiff {
  /** Base branch (the workspace cwd's current branch). */
  base?: string | null;
  /** The direction's own branch. */
  branch?: string | null;
  /** Repo-relative files this direction changed vs the merge-base. */
  files: string[];
  /** Base work tree has uncommitted changes → a merge would be refused. */
  base_dirty: boolean;
}

/** Result of `POST /api/workspaces/:id/threads/:tid/merge`. `status` discriminates:
 *  - "merged": clean merge into `base` (`files` = changed-file count).
 *  - "resolving": conflicts; an AI agent (`agent_id`) was spawned to resolve. */
export type MergeResult =
  | { status: "merged"; base: string; files: number }
  | { status: "resolving"; agent_id: string; files: string[] };

export interface Workspace {
  id: string;
  /** First 8 chars of `id`. Used as the URL slug `/chat/:slug`. */
  slug: string;
  name: string;
  cwd: string;
  /** Branch currently checked out at `cwd` (the agent's terminal home), filled
   *  by the workspaces list for the sidebar's branch chip. null for a non-git
   *  cwd / detached HEAD. */
  cwd_branch?: string | null;
  accent?: string | null;
  created_at: number;
  /** Live agents whose `workspace_id` points here. Computed server-side
   *  at list time; not persisted. */
  member_count: number;
  /** Attached dependency-source roots (excludes the primary `cwd`). */
  roots?: WorkspaceRoot[];
  /** The workspace's directions (always ≥1: an auto-created `main`).
   *  Oldest-first; the first entry is the main thread. */
  threads?: ThreadInfo[];
}

export interface CreateWorkspaceRequest {
  name: string;
  cwd: string;
  accent?: string;
  /** Attached dependency-source roots to persist alongside the workspace. */
  roots?: WorkspaceRoot[];
}

// ── M3 recording DTOs ────────────────────────────────────────────────────

export interface RecordingInfo {
  id: string;
  agent_id: string;
  started_at: number;
  finalized_at: number | null;
  duration_ms: number | null;
  cols: number;
  rows: number;
  last_seq: number | null;
}

// ── /ws/swarm event stream ───────────────────────────────────────────────

export type SwarmAgentState =
  | "spawning"
  | "ready"
  | "thinking"
  | "idle"
  | "exited"
  /** Blocked on an upstream handoff (a `consumes` dependency hasn't been
   *  produced yet). The WakeCoordinator will re-wake it once the blackboard
   *  key it depends on is written. */
  | "waiting_dep"
  /** Exited abnormally (non-zero shim_exit / crashed mid-turn). Distinct from
   *  a clean `exited` so the UI can surface it red + float it to the top. */
  | "error";

/** A step-level activity event the server derives by tailing the CLI's
 *  session JSONL: one per tool call (Edit/Bash/Read…) or system step. The
 *  member list shows the latest one as "what this worker is doing right now";
 *  the AgentDrawer activity tab streams the whole round.
 *
 *  - `kind`     — "tool" (Edit/Bash/…) or "system" (a non-tool step).
 *  - `label`    — human-facing line, e.g. "Edit src/foo.rs".
 *  - `phase`    — "running" (in flight, no duration yet) → "ok" / "error".
 *  - `seq`      — monotonic per-agent activity index.
 *  - `duration_ms` — wall time once the step settles; absent while running.
 *  - `at`       — unix-ms the event was emitted. */
export interface AgentActivity {
  agent_id: string;
  kind: "tool" | "system";
  label: string;
  phase: "running" | "ok" | "error";
  seq: number;
  duration_ms?: number;
  at: number;
}

/** Per-agent live slice the swarm WS accumulates client-side (state + latest
 *  activity), keyed by agent_id. The REST `AgentInfo` row carries no live
 *  state/activity — those only arrive over `/ws/swarm` — so this is the only
 *  source of truth for them, with `inferAgentStatus` as the back-compat
 *  fallback when an event hasn't been seen yet. */
export interface AgentLiveState {
  state?: SwarmAgentState;
  activity?: AgentActivity;
}

export type SwarmEvent =
  | { type: "agent_state"; agent_id: string; state: SwarmAgentState }
  | ({ type: "agent_activity" } & AgentActivity)
  | {
      type: "message";
      id: number;
      from_agent: string;
      to_agent: string;
      kind: string;
      body: string;
      sent_at: number;
      in_reply_to?: number | null;
      thread_id?: string | null;
      meta?: MessageMeta | null;
      thought_trace?: ThoughtTrace | null;
    }
  | {
      type: "message_read";
      ids: number[];
      to_agent: string;
      at: number;
    }
  | {
      type: "blackboard_changed";
      id: number;
      agent_id: string | null;
      op: string;
      path: string;
      sha256: string;
      at: number;
    }
  | {
      type: "thread_changed";
      workspace_id: string;
      thread_id: string;
      op: string;
    }
  | {
      // A live append to an in-flight thought trace — lets the "正在响应" bubble
      // grow its step list during the turn (keyed by trigger_message_id) instead
      // of waiting for the reply. Full current snapshot, not a delta.
      type: "thought_trace_event";
      trigger_message_id: number;
      agent_id: string;
      steps: ThoughtTraceStep[];
      at: number;
    }
  | {
      // Cold-start stage heartbeat (see the rust-side doc): shim_ready →
      // mcp_ready → bootstrap_injected. Drives the narrative stage bar on a
      // pending agent card instead of a silent 30s spinner.
      type: "agent_stage";
      agent_id: string;
      stage: string;
      at: number;
    };

// ── F1 model settings: per-CLI tier→concrete-model mapping ──────────────────
export interface CliModels {
  /** Concrete model id when a spawn resolves to NO tier. Empty = CLI default. */
  default: string;
  /** tier (opus/sonnet/haiku) → concrete model id. Empty value = CLI default
   *  for that tier; absent key = fall back to `default`. */
  tiers: Record<string, string>;
  /** Global default reasoning effort (low|medium|high|max). Empty = model's own
   *  default. A per-direction override (chat picker) wins over this. */
  effort?: string;
}
export interface ModelConfig {
  version: number;
  clis: Record<string, CliModels>;
}
export interface ModelCliInfo {
  id: string;
  display_name: string;
  /** false ⇒ the CLI declares no model_args; the UI greys it out. */
  supports_model: boolean;
  /** true ⇒ the opus/sonnet/haiku tier names ARE this CLI's own model aliases
   *  (only claude). The page shows the tier rows only when true; other CLIs
   *  (codex = gpt-5.x) get just a default-model row. */
  native_tiers?: boolean;
}
export interface ModelsResponse {
  config: ModelConfig;
  clis: ModelCliInfo[];
}

// ── usage / cost (GET /api/usage) ──────────────────────────────────────────
export interface UsageModelRow {
  model: string | null;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  events: number;
  cost_usd: number;
  /** false when this model isn't in the server pricing table (tokens only). */
  priced: boolean;
  /** The model's static context-window cap (tokens); null for unknown models. */
  context_window: number | null;
  /** Estimated peak context occupancy (tokens) — how full the window got. */
  context_peak: number;
}
export interface UsageDayRow {
  day: string;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
}
export interface UsageAgentRow {
  agent_id: string;
  role?: string | null;
  workspace_id?: string | null;
  thread_id?: string | null;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  events: number;
}
export interface UsageSummary {
  totals: {
    input_tokens: number;
    output_tokens: number;
    cache_read_tokens: number;
    cache_write_tokens: number;
    events: number;
    cost_usd: number;
    priced: boolean;
  };
  by_model: UsageModelRow[];
  by_day: UsageDayRow[];
  by_agent: UsageAgentRow[];
}
export interface UsagePricingRate {
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
}
export interface UsagePricingRule {
  id: string;
  provider: string;
  label: string;
  matchers: string[];
  context_window: number | null;
  rates_usd_per_mtok: UsagePricingRate;
  note: string;
}
export interface UsagePricingResponse {
  unit: string;
  source: "default" | "user" | string;
  path: string;
  rules: UsagePricingRule[];
  /** Models no rule matches fall through to LiteLLM (embedded / disk / refreshed). */
  fallback?: {
    source: string;
    models: number;
    origin?: "embedded" | "disk" | "refreshed" | string;
    auto_refresh?: boolean;
    cache_path?: string;
  };
}

// ── goals (GET/POST/PATCH /api/goals) ─────────────────────────────────────
export type GoalStatus = "active" | "paused" | "blocked" | "complete" | "archived";
export interface GoalRecord {
  id: string;
  workspace_id: string;
  thread_id: string | null;
  objective: string;
  success_criteria: string[];
  status: GoalStatus;
  budget_tokens: number | null;
  created_at: number;
  updated_at: number;
  completed_at: number | null;
}
export interface GoalEvidenceRecord {
  id: string;
  goal_id: string;
  kind: string;
  summary: string;
  source_agent_id: string | null;
  blackboard_path: string | null;
  command: string | null;
  created_at: number;
}
export interface GoalsResponse {
  goals: GoalRecord[];
}
export interface GoalEvidenceResponse {
  evidence: GoalEvidenceRecord[];
}
export interface CreateGoalRequest {
  workspace_id: string;
  thread_id?: string | null;
  objective: string;
  success_criteria?: string[];
  budget_tokens?: number | null;
  status?: GoalStatus;
}
export interface AddGoalEvidenceRequest {
  kind: string;
  summary: string;
  source_agent_id?: string | null;
  blackboard_path?: string | null;
  command?: string | null;
}

// ── kanban tasks (GET /api/tasks) ──────────────────────────────────────────
export interface TaskRow {
  /** Effective status (human override else derived from lifecycle). */
  status: string;
  /** True when `status` is a human override (vs derived). */
  overridden: boolean;
  agent_id: string;
  parent_agent_id: string;
  role_label: string;
  role_slug: string | null;
  handoff_signal: string | null;
  task_status: string | null;
  spawned_at: number;
  killed_at: number | null;
  shim_exit_code: number | null;
  last_activity_at: number | null;
  workspace_id: string | null;
  thread_id: string | null;
  handoff_done: boolean;
  error_present: boolean;
}
export interface TasksResponse {
  tasks: TaskRow[];
}

// ── file browser (GET /api/files/*) ────────────────────────────────────────
export interface FileEntry {
  name: string;
  is_dir: boolean;
  size: number;
}
export interface FileListResp {
  dir: string;
  parent: string | null;
  entries: FileEntry[];
}
export interface FileReadResp {
  path: string;
  binary: boolean;
  size: number;
  content: string | null;
  truncated: boolean;
}

// ── cron (GET/POST/DELETE/PATCH /api/cron) ─────────────────────────────────
export interface CronJob {
  id: string;
  workspace_id: string;
  name: string;
  cron_expr: string;
  prompt: string;
  enabled: boolean;
  created_at: number;
  last_run_at: number | null;
  /** Minutes east of UTC the expression is written in (0 = UTC, +480 = UTC+8). */
  tz_offset_minutes: number;
  /** Next fire time (unix ms, UTC) the scheduler will pick; null if disabled or no upcoming occurrence. Server-computed. */
  next_run: number | null;
}
export interface CronListResp {
  jobs: CronJob[];
}

// ── Fusion: multi-model competition DTOs ─────────────────────────────────
// Wire mirror of crates/swarmx-protocol/src/rest.rs Fusion* structs. A fusion
// batch fans one `need` out to N isolated contestant directions, then a judge
// stage compares their diffs and the user decides a winner (optionally merged).

/** `POST /api/workspaces/:id/fusion` body. `labels` is 2..4 contestant names
 *  (typically CLI/model names like "claude", "codex", "deepseek"). */
export interface CreateFusionRequest {
  need: string;
  labels: string[];
  /** Optional objective gate run in each contestant's worktree by the judge. */
  check_cmd?: string;
  /** Optional label → "cli[:model]" auto-implement panel. */
  panel?: Record<string, string>;
  /** Full-auto: server picks the panel + labels, implements, judges, merges. */
  autopilot?: boolean;
}

/** A fusion competition binding N contestant directions (+ later a judge).
 *  `status`: "running" | "judging" | "needs_decision" | "done" | "failed". */
export interface FusionBatch {
  id: string;
  workspace_id: string;
  slug: string;
  need: string;
  contestant_thread_ids: string[];
  judge_thread_id: string | null;
  status: string;
  winner_thread_id?: string | null;
  created_at: number;
}

/** One contestant's changeset, gathered for the judge: which direction, its
 *  branch, and the repo-relative files it changed vs the base. */
export interface FusionContestantDiff {
  thread_id: string;
  slug: string;
  name: string | null;
  branch: string | null;
  files: string[];
  /** True if the contestant never got an isolated worktree (degraded to
   *  shared) — its work isn't separable. */
  degraded: boolean;
  /** Objective check outcome, present when the batch carried a check_cmd and
   *  the judge stage ran it in this contestant's worktree (auto AND manual
   *  judging both run the gate). null/undefined = no gate ran. */
  check_passed?: boolean | null;
  /** Truncated stdout/stderr tail of the check — WHY it failed, not just that. */
  check_output?: string | null;
}

/** `POST /api/workspaces/:id/fusion/:bid/judge` response. */
export interface FusionJudgeResponse {
  batch: FusionBatch;
  judge_thread_id: string;
  base: string | null;
  contestants: FusionContestantDiff[];
  /** Auto-judge only: the spawned CLI agent driving the verdict (`?auto=true`).
   *  null in manual mode (no agent spawned). */
  judge_agent_id?: string | null;
}

/** `POST /api/workspaces/:id/fusion/:bid/decide` body. `merge` defaults true. */
export interface FusionDecideRequest {
  winner_thread_id: string;
  merge?: boolean;
}

/** `POST /api/workspaces/:id/fusion/:bid/decide` response. `merge_status`:
 *  "merged" | "resolving" | "nothing_to_merge" | null (when merge was false). */
export interface FusionDecideResponse {
  batch: FusionBatch;
  winner_thread_id: string;
  merge_status: string | null;
  base: string | null;
  files: string[];
  resolver_agent_id: string | null;
}

// ── Answer/research fusion (panel → judge → synthesis) ──────────────────────
export interface FusionConsultRequest {
  question: string;
  panel?: string[];
  judge_model?: string;
  synthesis_model?: string;
}
export interface FusionPanelAnswer {
  model: string;
  answer: string;
  ok: boolean;
  elapsed_ms: number;
}
export interface FusionJudgeAnalysis {
  consensus: string[];
  contradictions: string[];
  unique_insights: string[];
  blind_spots: string[];
  note?: string | null;
}
export interface FusionConsultResponse {
  question: string;
  panel: FusionPanelAnswer[];
  analysis: FusionJudgeAnalysis;
  synthesis: string;
  cost_note: string;
}
