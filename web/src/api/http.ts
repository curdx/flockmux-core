import type {
  AgentActivity,
  AgentInfo,
  BlackboardEntry,
  BlackboardHistoryEntry,
  BlackboardSnapshot,
  BranchInfo,
  CliPluginInfo,
  AddGoalEvidenceRequest,
  CreateGoalRequest,
  CreateThreadRequest,
  CreateWorkspaceRequest,
  EngineProbeResponse,
  GoalStatus,
  GoalEvidenceResponse,
  GoalsResponse,
  MarkReadResponse,
  MergeResult,
  MessageRecord,
  ModelConfig,
  ModelsResponse,
  RecordingInfo,
  RunSpellRequest,
  RunSpellResponse,
  SendMessageRequest,
  SpawnAgentRequest,
  SpawnAgentResponse,
  SpellInfo,
  ThreadDiff,
  ThreadInfo,
  CronListResp,
  CreateFusionRequest,
  FusionBatch,
  FusionDecideRequest,
  FusionDecideResponse,
  FusionConsultRequest,
  FusionConsultResponse,
  FusionJudgeResponse,
  FileListResp,
  FileReadResp,
  TasksResponse,
  UsagePricingResponse,
  UsagePricingRule,
  UsageSummary,
  Workspace,
  WorkspaceRoot,
  WriteBlackboardRequest,
} from "./types";

import i18n from "@/i18n";
import { HTTP_BASE } from "../lib/apiBase";
import { isTauriWindow } from "@/lib/tauriWindowChrome";
import { dedupe } from "@/lib/requestDedupe";
import { apiRoutes } from "./endpoints";
import type { ApiEndpoint } from "./endpoints";

/** User-facing copy when fetch never reaches the server. Packaged App ≠
 *  `cargo run`: never mention loopback ports or "make sure swarmx is running". */
export function networkErrorCopy(): string {
  if (isTauriWindow()) {
    return i18n.t("common.networkErrorApp", {
      defaultValue:
        "暂时出了点问题。若顶部有「重启」，点一下；还不行就关掉再打开应用。",
    });
  }
  return i18n.t("common.networkErrorDev", {
    defaultValue: "暂时连不上本地服务。开发时请确认后端已启动，然后重试。",
  });
}

/** Thrown by `request()` on any non-2xx response. Carries the HTTP `status`
 *  (so callers can special-case 404 → empty-state instead of a red error)
 *  and a `detail` string that is the server's `{ error }` envelope unwrapped
 *  (or the raw body text when not JSON). `.message` is also user-safe for
 *  network failures (status 0); for HTTP errors it keeps a compact
 *  `METHOD path → status: detail` form for logs — prefer `.detail` in UI. */
export class ApiError extends Error {
  status: number;
  detail: string;
  constructor(status: number, detail: string, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.detail = detail;
  }
}

async function request<T>(
  method: string,
  path: string,
  body?: unknown,
  signal?: AbortSignal,
): Promise<T> {
  // `signal` lets a caller abort an in-flight request when its component
  // unmounts (pass an AbortController.signal). Components that don't care just
  // omit it and instead guard their setState with a mounted-ref. Aborting
  // rejects the fetch with an AbortError the caller can ignore.
  let res: Response;
  try {
    res = await fetch(HTTP_BASE + path, {
      method,
      headers: body ? { "content-type": "application/json" } : undefined,
      body: body ? JSON.stringify(body) : undefined,
      signal,
    });
  } catch (e) {
    // An aborted fetch rejects with an AbortError — that's a caller-initiated
    // cancellation, not a failure, so re-throw it untouched for the caller to
    // ignore. Everything else here is a connection-layer failure (fetch rejects
    // with a TypeError like "Failed to fetch" / "Load failed"): the request
    // never reached the server, so there's no Response and the `!res.ok` →
    // ApiError path below never runs.
    //
    // BOTH `detail` and `message` must be user-safe: many toast/UI sites show
    // `e.message`. Technical crumbs (method/path/TypeError) go to console only
    // — packaged App users must never see 127.0.0.1:7777 or GET /api/….
    if (e instanceof DOMException && e.name === "AbortError") throw e;
    const original = (e as Error)?.message || String(e);
    const friendly = networkErrorCopy();
    // eslint-disable-next-line no-console
    console.warn(`[swarmx] network error ${method} ${path}:`, original);
    throw new ApiError(0, friendly, friendly);
  }
  if (!res.ok) {
    // Read the body stream EXACTLY ONCE. The old code did `res.json()` then
    // `res.text()` in the catch — but `res.json()` already consumes the
    // stream, so on a non-JSON error body (empty 404, HTML 5xx from a proxy)
    // the `res.text()` retry threw "body stream already read" and the real
    // status code was lost. Read text first, then try to parse.
    const raw = await res.text().catch(() => "");
    let detail = raw;
    try {
      const parsed = JSON.parse(raw);
      if (parsed && typeof parsed.error === "string") detail = parsed.error;
    } catch {
      /* not JSON — keep raw text */
    }
    throw new ApiError(
      res.status,
      detail,
      `${method} ${path} → ${res.status}: ${detail || res.statusText}`,
    );
  }
  // 204 No Content
  if (res.status === 204) return undefined as T;
  return res.json() as Promise<T>;
}

function requestEndpoint<T>(
  route: ApiEndpoint,
  body?: unknown,
  signal?: AbortSignal,
): Promise<T> {
  return request<T>(route.method, route.path, body, signal);
}

export interface ListMessagesQuery {
  to?: string;
  from?: string;
  q?: string;
  limit?: number;
  thread_id?: string;
  only_undelivered?: boolean;
}

/** One detected runtime (node / npm / uv) for the MCP env probe. */
export interface RuntimeInfo {
  present: boolean;
  version: string | null;
  /** node only: present AND version >= the LTS minimum the npx MCP servers need.
   *  `false` for a present-but-too-old node (e.g. v14). Absent for npm/uv. */
  adequate?: boolean;
  /** node only: the minimum major version required (for the warning copy). */
  minMajor?: number;
}
export interface McpEnv {
  node: RuntimeInfo;
  npm: RuntimeInfo;
  uv: RuntimeInfo;
}
/** Per-server API-key state: set?, masked preview, and whether claude/codex agree. */
export interface McpKeyState {
  present: boolean;
  masked: string | null;
  consistent: boolean;
}
/** Names of MCP servers already configured per CLI (user scope) + key states. */
export interface McpStatus {
  claude: string[];
  codex: string[];
  keys?: Record<string, McpKeyState>;
}

export const api = {
  listPlugins: () =>
    dedupe("plugins", 30_000, () => requestEndpoint<CliPluginInfo[]>(apiRoutes.plugins.list())),
  /** Kick a background real-usability sweep. Returns immediately (202); verdicts
   *  land in the probe cache as each engine completes — poll `getEngineProbe`. */
  probeEngines: () =>
    requestEndpoint<{ status: string; engines: number }>(apiRoutes.plugins.probe()),
  /** Cached probe verdicts + in-flight flag. NOT deduped — the readiness hook
   *  polls this while a sweep runs. */
  getEngineProbe: () =>
    requestEndpoint<EngineProbeResponse>(apiRoutes.plugins.probeStatus()),
  /** One-click install: POST the install route and stream its SSE output line by
   *  line via `onEvent`, resolving when the stream closes. The command itself is
   *  server-owned (the engine's whitelisted install command); we only send the id. */
  installPlugin: async (
    id: string,
    onEvent: (
      ev:
        | { type: "line"; text: string }
        | { type: "done"; ok: boolean; installed: boolean; version?: string | null; error?: string },
    ) => void,
  ): Promise<void> => {
    const res = await fetch(HTTP_BASE + apiRoutes.plugins.install(id).path, { method: "POST" });
    if (!res.ok || !res.body) {
      onEvent({ type: "done", ok: false, installed: false, error: `HTTP ${res.status}` });
      return;
    }
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      let sep;
      while ((sep = buf.indexOf("\n\n")) >= 0) {
        const frame = buf.slice(0, sep);
        buf = buf.slice(sep + 2);
        const data = frame.split("\n").find((l) => l.startsWith("data:"));
        if (!data) continue;
        try {
          onEvent(JSON.parse(data.slice(5).trim()));
        } catch {
          /* ignore malformed frame */
        }
      }
    }
  },
  /** Comate Zulu license: read the masked status / write a new value. */
  getComate: () =>
    requestEndpoint<{ configured: boolean; source: string; hint: string }>(
      apiRoutes.comate.get(),
    ),
  putComate: (license: string) =>
    requestEndpoint<{ ok: boolean }>(apiRoutes.comate.put(), { license }),
  /** zulu's available models under the configured license. */
  getZuluModels: () =>
    requestEndpoint<
      { modelId: string; displayName: string; thinking: boolean; image: boolean }[]
    >(apiRoutes.zulu.models()),
  // MCP admin (「快捷装 MCP」页面)
  mcpEnv: () => requestEndpoint<McpEnv>(apiRoutes.mcp.env()),
  mcpStatus: () => requestEndpoint<McpStatus>(apiRoutes.mcp.status()),
  mcpInstall: (name: string, cli: "claude" | "codex", apiKey?: string) =>
    requestEndpoint<{ ok: boolean; output: string }>(apiRoutes.mcp.install(), {
      name,
      cli,
      ...(apiKey ? { api_key: apiKey } : {}),
    }),
  mcpUninstall: (name: string, cli: "claude" | "codex") =>
    requestEndpoint<{ ok: boolean; output: string }>(apiRoutes.mcp.uninstall(), { name, cli }),
  listAgents: () => requestEndpoint<AgentInfo[]>(apiRoutes.agents.list()),
  getUsage: (workspaceId?: string) =>
    requestEndpoint<UsageSummary>(apiRoutes.usage.summary(workspaceId)),
  getUsagePricing: () => requestEndpoint<UsagePricingResponse>(apiRoutes.usage.pricing()),
  putUsagePricing: (rules: UsagePricingRule[]) =>
    requestEndpoint<UsagePricingResponse>(apiRoutes.usage.updatePricing(), { rules }),
  resetUsagePricing: () =>
    requestEndpoint<UsagePricingResponse>(apiRoutes.usage.resetPricing()),
  listGoals: (workspaceId?: string, threadId?: string | null) =>
    requestEndpoint<GoalsResponse>(apiRoutes.goals.list(workspaceId, threadId)),
  createGoal: (req: CreateGoalRequest) =>
    requestEndpoint<{ ok: boolean; id: string }>(apiRoutes.goals.create(), req),
  updateGoalStatus: (id: string, status: GoalStatus) =>
    requestEndpoint<{ ok: boolean }>(apiRoutes.goals.updateStatus(id), { status }),
  listGoalEvidence: (id: string, limit?: number) =>
    requestEndpoint<GoalEvidenceResponse>(apiRoutes.goals.evidence(id, limit)),
  addGoalEvidence: (id: string, req: AddGoalEvidenceRequest) =>
    requestEndpoint<{ ok: boolean; id: string }>(apiRoutes.goals.addEvidence(id), req),
  listTasks: (workspaceId?: string) =>
    requestEndpoint<TasksResponse>(apiRoutes.tasks.list(workspaceId)),
  setTaskStatus: (agentId: string, status: string | null) =>
    requestEndpoint<{ ok: boolean }>(apiRoutes.tasks.setStatus(agentId), { status }),
  // `workspaceId` scopes the browser to that workspace's roots (jailed);
  // `all` is the "browse whole filesystem" escape hatch that lifts the jail.
  filesList: (dir?: string, workspaceId?: string, all?: boolean) =>
    requestEndpoint<FileListResp>(apiRoutes.files.list(dir, workspaceId, all)),
  filesRead: (path: string, workspaceId?: string, all?: boolean) =>
    requestEndpoint<FileReadResp>(apiRoutes.files.read(path, workspaceId, all)),
  compactBlackboard: (path: string) =>
    requestEndpoint<{
      ok: boolean;
      changed: boolean;
      before_tokens: number;
      after_tokens: number;
      note?: string;
    }>(apiRoutes.blackboard.compact(), { path }),
  listCron: () => requestEndpoint<CronListResp>(apiRoutes.cron.list()),
  // Live validation + next-run preview for the create form. next_run is unix ms
  // (UTC) or null (valid but no occurrence within a year), the model evaluates.
  cronPreview: (expr: string, offset: number) =>
    requestEndpoint<{ valid: boolean; next_run: number | null }>(apiRoutes.cron.preview(expr, offset)),
  createCron: (req: {
    workspace_id: string;
    name: string;
    cron_expr: string;
    prompt: string;
    tz_offset_minutes: number;
  }) => requestEndpoint<{ ok: boolean; id?: string; error?: string }>(apiRoutes.cron.create(), req),
  updateCron: (
    id: string,
    req: {
      workspace_id: string;
      name: string;
      cron_expr: string;
      prompt: string;
      tz_offset_minutes: number;
    },
  ) => requestEndpoint<{ ok: boolean; error?: string }>(apiRoutes.cron.update(id), req),
  deleteCron: (id: string) => requestEndpoint<{ ok: boolean }>(apiRoutes.cron.delete(id)),
  toggleCron: (id: string, enabled: boolean) =>
    requestEndpoint<{ ok: boolean }>(apiRoutes.cron.toggle(id), { enabled }),
  runCron: (id: string) =>
    requestEndpoint<{ ok: boolean; skipped?: string }>(apiRoutes.cron.run(id)),
  spawnAgent: (req: SpawnAgentRequest) =>
    requestEndpoint<SpawnAgentResponse>(apiRoutes.agents.spawn(), req),
  killAgent: (id: string) => requestEndpoint<void>(apiRoutes.agents.kill(id)),
  // M6e: 操作者手动 ⚡ 唤醒。后端走 mailbox + PTY-kick（同主路径），
  // body 写"manual wake from operator"。挡在 M6d-6 quiet gate 后面，
  // agent 正在 stream 时只投 mailbox 不戳 PTY。
  wakeAgent: (id: string) => requestEndpoint<void>(apiRoutes.agents.wake(id)),
  // Recent tool-level activity from the transcript tailer's ring — backfills
  // the drawer's Activity tab on a cold open (the live WS stream is
  // forward-only). Shares the live stream's `seq` space so they merge by it.
  getAgentActivity: (id: string) =>
    requestEndpoint<AgentActivity[]>(apiRoutes.agents.activity(id)),
  // 不杀 PTY 的暂停:发 Ctrl-C 取消当前 turn + 设 paused flag。WakeCoordinator
  // 跳过这个 agent 的自动唤醒,manual ⚡ 仍然能用。
  interruptAgent: (id: string) =>
    requestEndpoint<{ ok: boolean; agent_id: string; paused: boolean }>(
      apiRoutes.agents.interrupt(id),
    ),
  resumeAgent: (id: string) =>
    requestEndpoint<{ ok: boolean; agent_id: string; paused: boolean }>(
      apiRoutes.agents.resume(id),
    ),
  // workspace 级"全停"。后端遍历 registry 找匹配 workspace_id 的 live agent,
  // 逐个 interrupt。返回 { interrupted, agent_ids, failed }。
  interruptAllInWorkspace: (workspaceId: string) =>
    requestEndpoint<{
      ok: boolean;
      interrupted: number;
      agent_ids: string[];
      failed: Array<{ agent_id: string; error: string }>;
    }>(apiRoutes.agents.interruptAll(workspaceId)),

  // M3 swarm
  listMessages: (q: ListMessagesQuery = {}) =>
    requestEndpoint<MessageRecord[]>(apiRoutes.messages.list(q)),
  sendMessage: (req: SendMessageRequest) =>
    requestEndpoint<MessageRecord>(apiRoutes.messages.send(), req),
  markMessagesRead: (to: string, ids: number[]) =>
    requestEndpoint<MarkReadResponse>(apiRoutes.messages.markRead(), { to, ids }),
  listBlackboard: () =>
    requestEndpoint<BlackboardEntry[]>(apiRoutes.blackboard.list()),
  readBlackboard: (path: string) =>
    requestEndpoint<BlackboardSnapshot>(apiRoutes.blackboard.read(path)),
  writeBlackboard: (path: string, req: WriteBlackboardRequest) =>
    requestEndpoint<{ id: number; path: string; sha256: string; at: number }>(
      apiRoutes.blackboard.write(path),
      req,
    ),
  listBlackboardHistory: (path: string, limit = 50, includeContent = false) =>
    requestEndpoint<BlackboardHistoryEntry[]>(
      apiRoutes.blackboard.history(path, limit, includeContent),
    ),

  // M3 recordings
  listRecordings: (agentId?: string) =>
    requestEndpoint<RecordingInfo[]>(apiRoutes.recordings.list(agentId)),
  // Prefix HTTP_BASE so direct <a href> / fetch() outside the api.* layer
  // (asciinema-player, castPreview, download link) still resolves in Tauri prod.
  recordingCastUrl: (id: string) =>
    `${HTTP_BASE}${apiRoutes.recordings.cast(id)}`,

  // M5c spells
  listSpells: () => requestEndpoint<SpellInfo[]>(apiRoutes.spells.list()),
  runSpell: (req: RunSpellRequest) =>
    requestEndpoint<RunSpellResponse>(apiRoutes.spells.run(), req),

  // workspaces (workspace-as-first-class refactor)
  listWorkspaces: () =>
    dedupe("workspaces", 0, () => requestEndpoint<Workspace[]>(apiRoutes.workspaces.list())),
  createWorkspace: (req: CreateWorkspaceRequest) =>
    requestEndpoint<Workspace>(apiRoutes.workspaces.create(), req),
  deleteWorkspace: (id: string) =>
    requestEndpoint<void>(apiRoutes.workspaces.delete(id)),
  // threads (directions within a workspace). `id` here is the workspace UUID
  // (workspaceId), matching the server's path param.
  listThreads: (id: string) =>
    requestEndpoint<ThreadInfo[]>(apiRoutes.workspaces.threads(id)),
  // Local branches of a workspace's repo, for the "open existing branch as a
  // direction" picker.
  listBranches: (id: string) =>
    requestEndpoint<BranchInfo[]>(apiRoutes.workspaces.branches(id)),
  createThread: (id: string, req: CreateThreadRequest) =>
    requestEndpoint<ThreadInfo>(apiRoutes.workspaces.createThread(id), req),
  updateThread: (id: string, threadId: string, req: { name: string }) =>
    requestEndpoint<ThreadInfo>(
      apiRoutes.workspaces.updateThread(id, threadId),
      req,
    ),
  // Per-direction model + reasoning override. The body is the COMPLETE desired
  // state: tier = "opus"|"sonnet"|"haiku"|concrete id (null = global default);
  // reasoning = "low"|"medium"|"high"|"max" (null = model default). Takes effect
  // on the next spawn (caller restarts the orchestrator to apply immediately).
  setThreadModel: (
    id: string,
    threadId: string,
    cfg: { tier: string | null; reasoning: string | null },
  ) =>
    requestEndpoint<ThreadInfo>(
      apiRoutes.workspaces.setThreadModel(id, threadId),
      cfg,
    ),
  deleteThread: (id: string, threadId: string) =>
    requestEndpoint<void>(apiRoutes.workspaces.deleteThread(id, threadId)),
  // Preview what a direction changed before merging it back to the main line.
  threadDiff: (id: string, threadId: string) =>
    requestEndpoint<ThreadDiff>(
      apiRoutes.workspaces.threadDiff(id, threadId),
    ),
  // Merge a direction back to the main line. Clean → "merged"; conflicts →
  // "resolving" (an AI agent was spawned to finish the merge).
  mergeThread: (id: string, threadId: string) =>
    requestEndpoint<MergeResult>(
      apiRoutes.workspaces.mergeThread(id, threadId),
    ),
  // ── Fusion: multi-model competition ──────────────────────────────────
  // List alive fusion batches (newest first) for a workspace.
  listFusion: (id: string) =>
    requestEndpoint<FusionBatch[]>(apiRoutes.workspaces.fusion(id)),
  // Start a competition: fan one `need` out to 2..4 isolated contestant
  // directions (one per label). Returns the freshly-created batch.
  createFusion: (id: string, req: CreateFusionRequest) =>
    requestEndpoint<FusionBatch>(apiRoutes.workspaces.createFusion(id), req),
  // Enter the judge stage: spawn a judge direction + return each contestant's
  // diff bundle for review.
  // Enter the judge stage. `auto=true` additionally spawns a real CLI agent in
  // the judge direction that reads each contestant's diff and calls decide on
  // its own (judge_agent_id set); omitted/false = manual (human picks winner).
  judgeFusion: (id: string, bid: string, auto?: boolean, synthesize?: boolean) =>
    requestEndpoint<FusionJudgeResponse>(
      apiRoutes.workspaces.judgeFusion(id, bid, auto, synthesize),
    ),
  // Record the verdict: pick ONE winning contestant; the batch flips to 'done'
  // and (unless merge=false) the winner's branch is merged back into base.
  decideFusion: (id: string, bid: string, req: FusionDecideRequest) =>
    requestEndpoint<FusionDecideResponse>(
      apiRoutes.workspaces.decideFusion(id, bid),
      req,
    ),
  /** Answer/research fusion: panel → judge → synthesis (zulu-backed). */
  fusionConsult: (id: string, req: FusionConsultRequest) =>
    requestEndpoint<FusionConsultResponse>(apiRoutes.workspaces.fusionConsult(id), req),
  // attached dependency-source roots (post-create management)
  addWorkspaceRoot: (id: string, root: WorkspaceRoot) =>
    requestEndpoint<WorkspaceRoot>(apiRoutes.workspaces.roots(id), root),
  /** Remove a root node (by its server id); the backend cascade-deletes any
   *  children mounted under it. */
  deleteWorkspaceRoot: (id: string, rootId: string) =>
    requestEndpoint<{ deleted: number }>(
      apiRoutes.workspaces.deleteRoot(id, rootId),
    ),
  /** Candidate local source deps parsed from a project's manifests
   *  (package.json file:/link:, Cargo path, go.mod replace, uv sources).
   *  `projectPath` scopes the scan to a specific project dir; defaults to the
   *  primary cwd when omitted. */
  rootSuggestions: (id: string, projectPath?: string) =>
    requestEndpoint<WorkspaceRoot[]>(
      apiRoutes.workspaces.rootSuggestions(id, projectPath),
    ),

  // Composer 「优化」 button: one-shot headless prompt rewrite (server runs
  // `claude -p` on a fast tier). Returns the improved text + whether it changed.
  optimizePrompt: (input: string, signal?: AbortSignal) =>
    requestEndpoint<{ optimized: string; changed: boolean }>(
      apiRoutes.prompt.optimize(),
      { input },
      signal,
    ),

  // Upload a pasted/dropped clipboard image: POST raw bytes, get back the saved
  // absolute path (which the composer drops into the message text so agents can
  // read it). Not via request() — the body is binary, not JSON.
  uploadAttachment: async (blob: Blob, name?: string): Promise<{ path: string }> => {
    const res = await fetch(
      `${HTTP_BASE}${apiRoutes.attachments.upload(name)}`,
      {
        method: "POST",
        headers: { "content-type": blob.type || "application/octet-stream" },
        body: blob,
      },
    );
    if (!res.ok) {
      throw new Error((await res.text().catch(() => "")) || `upload → ${res.status}`);
    }
    return res.json() as Promise<{ path: string }>;
  },

  // F1 model settings: per-CLI tier→concrete-model mapping.
  getModels: () => requestEndpoint<ModelsResponse>(apiRoutes.models.get()),
  putModels: (config: ModelConfig) =>
    requestEndpoint<{ config: ModelConfig }>(apiRoutes.models.put(), config),
};
