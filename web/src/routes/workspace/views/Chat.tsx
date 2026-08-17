/**
 * Chat view — the messages tab inside WorkspaceShell.
 *
 * Reduced from the previous chat.tsx route to just two regions:
 *   - center: MessagesPanel (composer + bubbles)
 *   - right:  members sidebar
 *
 * Workspace state, sidebar, channel header, tab bar all live in the
 * parent Shell. We pull what we need (members, unread, live events,
 * composer override) via useWorkspaceContext().
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import i18n from "@/i18n";
import { api } from "../../../api/http";
import type { AgentInfo } from "../../../api/types";
import {
  useEngineReadiness,
  type EngineReadinessState,
} from "../../../hooks/useEngineReadiness";
import { engineStatusKey } from "../../../lib/engineEvidence";
import { notifySpawnFallbacks } from "../../../lib/engineFallback";
import { MessagesPanel } from "../../../components/MessagesPanel";
import { OrchestratorFailureCard } from "../../../components/workspace/OrchestratorFailureCard";
import { BootstrapChecklistCard } from "../../../components/chat/BootstrapChecklistCard";
import { PlanStickyCard } from "../../../components/chat/PlanStickyCard";
import { CostChip } from "../../../components/chat/CostChip";
import { PulseRail } from "../../../components/workspace/PulseRail";
import { MemberStrip } from "../../../components/workspace/MemberStrip";
import { parsePlan, type ParsedPlan } from "../../../lib/parsePlan";
import {
  TaskActivity,
  type TaskActivity as TaskActivityT,
} from "../../../components/TaskActivity";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  ConfirmActionDialog,
  type ConfirmActionState,
} from "@/components/ConfirmActionDialog";
import {
  Boxes,
  FolderOpen,
  GitBranch,
  Loader2,
  PlugZap,
  Radio,
  TriangleAlert,
  Users,
} from "lucide-react";
import { cn } from "@/lib/cn";
import { toast } from "@/lib/toast";
import {
  roleColorClass as roleColor,
  roleDisplayName,
  roleLabelAmong,
  resolveMemberVisual,
  formatActivityLine,
  agentIsAlive,
  agentIsWorkable,
  agentIsErrored,
} from "@/lib/agent";
import type {
  AgentLiveState,
  BlackboardEntry,
  BlackboardSnapshot,
  MessageRecord,
  SwarmAgentState,
} from "../../../api/types";
import {
  useLiveBbChanges,
  useSwarmRefresh,
} from "../../../hooks/useSwarmProjection";
import { useWorkspaceContext } from "../Shell";

/** Pull every `<workspace_id>/<thread_slug>/<role>.progress.md` breadcrumb the
 *  workers in this DIRECTION have written, newest first. `prefix` is the
 *  direction key prefix (`<workspace_id>/<thread_slug>/`). Same source the
 *  Ledger "近况" card uses — rendered slim inline in the chat sidebar so users
 *  don't switch tabs to know a worker is alive during npm install / build. */
function useBreadcrumbs(prefix: string) {
  const [rows, setRows] = useState<
    { role: string; content: string; at: number }[]
  >([]);
  const reload = useCallback(async () => {
    try {
      const all = (await api.listBlackboard()) as BlackboardEntry[];
      const suffix = ".progress.md";
      const candidates = all.filter(
        (e) => e.path.startsWith(prefix) && e.path.endsWith(suffix),
      );
      const snaps = await Promise.all(
        candidates.map(async (e) => {
          try {
            const snap = (await api.readBlackboard(e.path)) as BlackboardSnapshot | null;
            if (!snap) return null;
            const role = e.path.slice(prefix.length, -suffix.length);
            return { role, content: snap.content.trim(), at: snap.at };
          } catch {
            return null;
          }
        }),
      );
      const out = snaps.filter(
        (s): s is { role: string; content: string; at: number } => s !== null,
      );
      out.sort((a, b) => b.at - a.at);
      setRows(out);
    } catch {
      setRows([]);
    }
  }, [prefix]);
  useEffect(() => {
    reload();
  }, [reload]);
  useSwarmRefresh((s) => s.reconnectGen, () => {
    void reload();
  });
  useLiveBbChanges((ev) => {
    if (!ev.path.startsWith(prefix)) return;
    if (!ev.path.endsWith(".progress.md")) return;
    void reload();
  });
  return rows;
}

/** Pull the captain's STRUCTURED plan (`<ws>/<thread>/plan.json`) for the
 *  PlanStickyCard. Mirrors useBreadcrumbs: checks existence via listBlackboard
 *  (so no 404 when absent), reads + defensively parses the JSON, and live-
 *  refreshes on blackboard_changed for that exact key. Returns null when there
 *  is no plan or the JSON can't be trusted (parsePlan decides that). */
function usePlan(prefix: string): ParsedPlan | null {
  const [plan, setPlan] = useState<ParsedPlan | null>(null);
  const path = `${prefix}plan.json`;
  const reload = useCallback(async () => {
    try {
      const all = (await api.listBlackboard()) as BlackboardEntry[];
      if (!all.some((e) => e.path === path)) {
        setPlan(null);
        return;
      }
      const snap = (await api.readBlackboard(path)) as BlackboardSnapshot | null;
      setPlan(snap ? parsePlan(snap.content) : null);
    } catch {
      setPlan(null);
    }
  }, [path]);
  useEffect(() => {
    reload();
  }, [reload]);
  useSwarmRefresh((s) => s.reconnectGen, () => {
    void reload();
  });
  useLiveBbChanges((ev) => {
    if (ev.path !== path) return;
    void reload();
  });
  return plan;
}

function fmtBreadcrumbAgo(at: number, now: number): string {
  const sec = Math.max(0, Math.floor((now - at) / 1000));
  if (sec < 60) return i18n.t("chat.ago.seconds", { defaultValue: "{{n}}s 前", n: sec });
  const min = Math.floor(sec / 60);
  if (min < 60) return i18n.t("chat.ago.minutes", { defaultValue: "{{n}}m 前", n: min });
  const hr = Math.floor(min / 60);
  return i18n.t("chat.ago.hours", { defaultValue: "{{n}}h 前", n: hr });
}

/** 成员列表一行的"AI 当前在干啥"视觉。优先真实 swarm state(`live`),缺失
 *  时回退到消息流推导(向后兼容)。优先级与色彩集中在 lib/agent
 *  resolveMemberVisual 里——这里只把 i18n 文案喂进去。
 *  - typing 动画(··· 闪烁)  → running / thinking / responding / working
 *  - 绿点                  → idle / awaiting_user (默认 "在线")
 *  - 琥珀点 + "可能卡住"     → 工具 running 卡太久(F3)
 *  - 琥珀点 + "无响应"       → worker 起来后长时间零活动(从未起跑/卡死)
 *  - 灰点 + "等依赖"        → waiting_dep
 *  - 红点 + "异常退出"      → state=error(且未被主动 kill)
 *  - 灰点 + "已终止/已下线"  → killed_at / shim_exit
 *  - 黄点 + "启动中"        → shim 还没 ready */
function statusDot(
  a: AgentInfo,
  live: AgentLiveState | undefined,
  messages: MessageRecord[],
  t: (k: string) => string,
) {
  const labels = {
    spawning: t("chat.starting"),
    ready: t("chat.online"),
    thinking: "",
    idle: "",
    exited: t("chat.exited"),
    waiting_dep: t("chat.status.waitingDep"),
    error: t("chat.status.error"),
    shimExit: t("chat.shimExit"),
    starting: t("chat.starting"),
    stalled: t("chat.status.stalled"),
    noResponse: t("chat.status.noResponse"),
  } satisfies Record<
    SwarmAgentState | "exited" | "shimExit" | "starting" | "stalled" | "noResponse",
    string
  >;
  const v = resolveMemberVisual(a, live, messages, labels);
  return { typing: v.typing, className: v.dotClass, label: v.label, isError: v.isError };
}

/** 成员栏第二行的活动 elapsed —— 规格图示 "0:42"。<1h 用 m:ss,≥1h 用 h:mm。 */
function fmtActivityElapsed(ms: number): string {
  const sec = Math.max(0, Math.floor(ms / 1000));
  const m = Math.floor(sec / 60);
  const s = sec % 60;
  if (m < 60) return `${m}:${String(s).padStart(2, "0")}`;
  const h = Math.floor(m / 60);
  return `${h}:${String(m % 60).padStart(2, "0")}`;
}

/** 三点 typing 动画 —— 跟微信"对方正在输入"风格一致。三个圆点错相位
 *  bounce,纯 CSS,不依赖外部库。 */
function TypingDots() {
  const { t } = useTranslation();
  const typingLabel = t("chat.typingIndicator", { defaultValue: "AI 正在输入" });
  return (
    <span
      className="inline-flex items-center gap-0.5"
      aria-label={typingLabel}
      title={typingLabel}
    >
      <span className="inline-block size-1.5 animate-bounce rounded-full bg-accent-primary [animation-delay:-0.3s]" />
      <span className="inline-block size-1.5 animate-bounce rounded-full bg-accent-primary [animation-delay:-0.15s]" />
      <span className="inline-block size-1.5 animate-bounce rounded-full bg-accent-primary" />
    </span>
  );
}

// pending task 兜底超时 — 60s 内没看到任何 AI 反应就放弃 (网络/agent 死)。
// 普通场景：AI reply 几秒后就会 dismiss，触不到这个 timeout。
const TASK_PENDING_TIMEOUT_MS = 60_000;
// task 进入 ready 后展示这么久再自动消失（让用户看清楚已就绪）。
const TASK_READY_DISMISS_MS = 4_000;
// spawning 事件归属到最近 user 消息的窗口。超过这个时长的 spawn 视为
// 独立事件，不归到用户上一条消息。注意是 LLM 节奏:规划收到消息 →
// 唤醒 → 思考 → 调 swarm_spawn_worker —— 实测派活延迟 10s~70s+(思考摘要
// 里明明白白写着 "思考 1m 10s")。15s 窗几乎永远挂不上,60s 窗照样被
// 71s 的真实派活甩掉。180s 覆盖 p99 派活,同时仍小于"无关联自发 spawn"
// 混入的现实间隔;误挂的代价只是多一张很快就绪消失的卡。
const TASK_ATTACH_WINDOW_MS = 180_000;

function WorkspaceStatusStrip({
  workspaceName,
  directionName,
  memberCount,
  sourceCount,
  cwdBranch,
  readiness,
  workspaceId,
  hasError = false,
  preparing = false,
  hasHistory = false,
}: {
  workspaceName: string;
  directionName: string;
  memberCount: number;
  sourceCount: number;
  cwdBranch: string | null;
  readiness: EngineReadinessState;
  /** Workspace id for the CostChip's usage query. */
  workspaceId: string;
  /** True when the direction's orchestrator is in an error state (auth/quota/
   *  watchdog). Keeps the strip's liveness dot honest — without it the strip
   *  would still say "1 个 AI 在线" directly above the failure card. */
  hasError?: boolean;
  /** True while the direction is still bootstrapping (thread.state ===
   *  "preparing",或规划已 spawn 但 PTY 还没 ready 的冷启动窗口)——
   *  the orchestrator is on its way up. */
  preparing?: boolean;
  /** True when this room already has chat history so "AI 未上线" vs
   *  "AI 已下线" can be told apart. */
  hasHistory?: boolean;
}) {
  const { t } = useTranslation();
  // A member that exists but can't work is NOT "online". `hasError` overrides
  // the raw member count so the strip never contradicts the failure card.
  const hasMembers = memberCount > 0 && !hasError;
  const { loading: cliLoading, engines } = readiness;
  const installedEngines = engines.filter((e) => e.installed);
  const usableEngines = engines.filter((e) => e.state === "usable");
  // "No usable engine" only blocks when nothing is even installed — an
  // installed-but-unverified engine can still be tried (the captain spawn will
  // surface the real error), so we don't hard-gate on the probe verdict here.
  const noCliReady =
    !cliLoading && engines.length > 0 && installedEngines.length === 0;
  // Prefer verified-usable names for the chip; fall back to installed names
  // (shown without a "ready" claim) when nothing's been probed yet.
  const usableNames = usableEngines.map((p) => p.display_name).join(" / ");
  const cliNames = installedEngines.map((p) => p.display_name).join(" / ");
  // Hover detail for the engine chip: every installed engine + how it was
  // verified (已验证回合 / 使用中 / 仅启动 / 需登录 …), one per line. Lets the
  // compact chip stay short while the full per-engine evidence is one hover away.
  const engineHover = installedEngines
    .map((e) => `${e.display_name} — ${t(engineStatusKey(e))}`)
    .join("\n");
  return (
    <div className="shrink-0 border-b border-border-subtle bg-surface-primary px-3 py-2">
      <div className="mx-auto flex w-full max-w-[1040px] flex-col gap-2 rounded-lg border border-border-subtle bg-surface-elevated px-3 py-2 shadow-sm sm:flex-row sm:items-center">
        <div className="flex min-w-0 flex-1 items-center gap-2">
          <span className="flex size-8 shrink-0 items-center justify-center rounded-md bg-accent-primary-soft text-accent-primary">
            <Boxes className="size-4" />
          </span>
          <div className="min-w-0">
            <div className="flex min-w-0 items-center gap-1.5">
              <span className="truncate font-heading text-sm font-semibold text-foreground-primary">
                {workspaceName}
              </span>
              <span className="text-foreground-tertiary">/</span>
              <span className="truncate font-caption text-xs text-foreground-secondary">
                {directionName}
              </span>
            </div>
            <div className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-1 font-caption text-[11px] text-foreground-tertiary">
              <span className="inline-flex items-center gap-1">
                <span
                  className={cn(
                    "size-1.5 rounded-full",
                    hasError
                      ? "bg-status-danger"
                      : hasMembers
                        ? "bg-status-running"
                        : "bg-state-warning",
                  )}
                />
                {hasError
                  ? t("chat.workspaceStripFailed", "AI 启动失败")
                  : hasMembers
                    ? t("chat.workspaceStripOnline", { count: memberCount })
                    : hasHistory
                      ? t("chat.workspaceStripAway", { defaultValue: "AI 已下线" })
                      : t("chat.workspaceStripIdle")}
              </span>
              <span className="inline-flex items-center gap-1">
                <FolderOpen className="size-3" />
                {t("chat.workspaceStripSources", { count: sourceCount })}
              </span>
              {cwdBranch && (
                <span className="inline-flex min-w-0 items-center gap-1">
                  <GitBranch className="size-3 shrink-0" />
                  <span className="truncate font-mono">{cwdBranch}</span>
                </span>
              )}
              {/* 成本可见性是一等功能(R1):常驻估算花费,点击进 /usage。
                  组件自带诚实规则:零事件不渲染、未计价加 ≥ 前缀。 */}
              <CostChip workspaceId={workspaceId} />
              {/* Engine chip: only when members are up but engines aren't
                  verified usable yet. Empty room already has EmptyState;
                  online+usable is redundant next to「N 人在线」. */}
              {!hasError &&
                !cliLoading &&
                hasMembers &&
                installedEngines.length > 0 &&
                !usableNames && (
                <span
                  className="inline-flex min-w-0 cursor-default items-center gap-1"
                  title={engineHover}
                >
                  <PlugZap className="size-3 shrink-0" />
                  <span className="truncate">
                    {t("chat.workspaceStripCliInstalled", { names: cliNames })}
                  </span>
                </span>
              )}
            </div>
          </div>
        </div>
        {noCliReady ? (
          <div className="flex flex-col gap-2 sm:max-w-[440px] sm:flex-row sm:items-center">
            <div className="font-caption text-[11px] leading-5 text-state-warning">
              <p>
                <TriangleAlert className="mr-1 inline size-3 align-[-2px]" />
                {t("chat.workspaceStripCliMissing")}
              </p>
              <p className="text-foreground-tertiary">
                {t("chat.workspaceStripCliInstallHelp")}
              </p>
            </div>
            <Button asChild size="sm" variant="outline" className="h-8 shrink-0 gap-1.5">
              <Link to="/settings/plugins">
                <PlugZap className="size-3.5" />
                {t("chat.workspaceStripSetupCli")}
              </Link>
            </Button>
          </div>
        ) : !hasMembers && preparing ? (
          // 方向正在 bootstrap（orchestrator 正在起来）——不渲染唤起按钮，
          // 否则用户会在这个窗口里再点一次，spawn 出第二个 orchestrator。
          <div className="flex items-center gap-2 sm:max-w-[430px]">
            <Loader2 className="size-3.5 shrink-0 animate-spin text-foreground-tertiary" />
            <p className="font-caption text-[11px] leading-5 text-foreground-tertiary">
              {t("chat.workspaceStripPreparing", { defaultValue: "正在准备…" })}
            </p>
          </div>
        ) : null}
        {/* 空房间「发消息即可」只由 EmptyState 说一次；这里再写就重复。 */}
        {/* Online + optional engines missing → don't nag「补齐 OpenCode」beside
            an already-running swarm (cognitive load). Settings remains the place. */}
      </div>
    </div>
  );
}

export default function ChatView() {
  const { t } = useTranslation();
  const {
    workspace,
    threadSlug,
    activeThread,
    // Members + room are scoped to the ACTIVE direction (thread), not the whole
    // workspace, so each direction is its own self-contained chat.
    threadMembers: activeMembers,
    handoffMissingAgents,
    allAliveAgents,
    threadAgentIds,
    liveMessages,
    liveRead,
    agentStateById,
    agentStageById,
    agentActivityById,
    reasoningById,
    unreadByFrom: activeWorkspaceUnread,
    jumpUnreadTick,
    openAgent,
    refreshAgents,
    // unreadByFrom in OutletContext is workspace-filtered; the right-side
    // members list wants raw agent-id → count so it can show the small
    // red badge per row. We re-derive it by indexing into the filtered
    // map (same keys, just used differently).
  } = useWorkspaceContext();

  const [confirm, setConfirm] = useState<ConfirmActionState | null>(null);
  // Real engine readiness: install info merged with the on-demand usability
  // probe (`engine_probe.rs`). "installed" alone is NOT "usable" — the EmptyState
  // and the strip render the honest verdict, never a fake "ready".
  const readiness = useEngineReadiness();

  // ── Orchestrator failure (honesty fix) ───────────────────────────────
  // The backend now flips the orchestrator to AgentState::Error when its CLI
  // can't work (HealthScanner caught "Not logged in", or the first-response
  // watchdog fired) and persists the reason to last_error. Surface that in the
  // MAIN chat view — not just the ≥1536px member rail — so an empty room is
  // never silently sitting behind a fake green dot.
  const orchestratorFailure = useMemo(() => {
    const orch = activeMembers.find(
      (m) => m.role === "orchestrator" && m.killed_at == null,
    );
    if (!orch) return null;
    const live = agentStateById[orch.agent_id];
    // Prefer the live error label (instant, from the AgentActivity event); fall
    // back to the persisted last_error so the card survives a page reload.
    const liveErr =
      live?.state === "error" && live.activity?.phase === "error"
        ? live.activity.label
        : null;
    const reason = liveErr ?? orch.last_error ?? null;
    // Recovery guard: a liveness signal newer than the recorded error means the
    // orchestrator came back to work (user ran /login in its terminal, or a slow
    // first turn finally produced output). The backend tailer clears last_error
    // + publishes a non-error state, but this covers the window before that
    // lands and a lossy-WS drop of the clear — otherwise the failure card would
    // outlive the recovery the card's own "打开终端登录" button caused. Can't
    // false-clear: only fires when there's activity strictly newer than the error.
    const errAt = orch.last_error_at ?? null;
    const freshSignalAt = Math.max(
      orch.last_activity_at ?? 0,
      live?.activity?.at ?? 0,
    );
    const recoveredSinceError = errAt != null && freshSignalAt > errAt;
    const isError =
      (live?.state === "error" || orch.last_error != null) &&
      !recoveredSinceError;
    if (!isError || !reason) return null;
    const kind =
      orch.last_error_kind ??
      (/(未登录|not logged in|\/login)/i.test(reason) ? "auth" : null);
    const loginCommand =
      readiness.engines.find((e) => e.id === orch.cli)?.install
        ?.login_command ?? null;
    return { agentId: orch.agent_id, reason, kind, loginCommand };
  }, [activeMembers, agentStateById, readiness.engines]);

  // ── Orchestrator bootstrap (honest startup checklist) ─────────────────
  // P0-5/P0-6: an orchestrator that exists but hasn't produced its first
  // response yet (PTY launching / engine booting) shows an honest startup
  // checklist instead of a bare "暂无消息" or a fake green dot. Suppressed once
  // it's in a failure state (the failure card takes over in place). The
  // empty-room gate is the emptyStateOverride slot itself (only rendered when
  // there are no messages), so reaching here means no first response landed.
  // The 90s first-response watchdog is the backstop: if nothing arrives it
  // flips the orchestrator to Error and the failure card replaces this — we do
  // NOT self-time it.
  const bootstrapState = useMemo(() => {
    if (orchestratorFailure) return null;
    const orch = activeMembers.find(
      (m) =>
        m.role === "orchestrator" &&
        m.killed_at == null &&
        m.shim_exit == null,
    );
    if (!orch) return null;
    const engineName =
      readiness.engines.find((e) => e.id === orch.cli)?.display_name ??
      orch.cli ??
      null;
    return {
      shimReady: !!orch.shim_ready,
      spawnedAt: orch.spawned_at ?? null,
      branchLabel: workspace.cwdBranch ?? null,
      engineName,
    };
  }, [orchestratorFailure, activeMembers, readiness.engines, workspace.cwdBranch]);

  const [retrying, setRetrying] = useState(false);
  const retryOrchestrator = useCallback(async () => {
    if (retrying) return;
    setRetrying(true);
    try {
      // Kill the wedged/errored orchestrator(s) in this direction, then re-run
      // init to spawn a fresh one (mirrors the model-switch restart path).
      const orchs = activeMembers.filter(
        (m) => m.role === "orchestrator" && m.killed_at == null,
      );
      await Promise.allSettled(orchs.map((o) => api.killAgent(o.agent_id)));
      await api.runSpell({
        name: "init",
        task: "",
        workspace_dir: workspace.path,
        workspace_id: workspace.workspaceId,
        thread_id: activeThread?.id,
      });
      refreshAgents();
    } catch (e) {
      // eslint-disable-next-line no-console
      console.warn("retry orchestrator failed", e);
    } finally {
      setRetrying(false);
    }
  }, [
    retrying,
    activeMembers,
    workspace.path,
    workspace.workspaceId,
    activeThread?.id,
    refreshAgents,
  ]);

  // ── Per-direction model switch ───────────────────────────────────────
  // The chat model picker calls this. We persist the direction's model_tier,
  // then restart any live orchestrator so the new model takes effect now (it
  // re-bootstraps from the ledger). Workers spawned afterward inherit the tier
  // at spawn time. Passive otherwise (null tier = follow the global default).
  const [modelBusy, setModelBusy] = useState(false);
  const applyDirectionModel = useCallback(
    async (cfg: { tier?: string | null; reasoning?: string | null }) => {
      if (!activeThread || modelBusy) return;
      setModelBusy(true);
      // Send the COMPLETE desired state: apply the changed knob, keep the
      // other from the current thread.
      const nextTier =
        cfg.tier !== undefined ? cfg.tier : activeThread.model_tier ?? null;
      const nextReasoning =
        cfg.reasoning !== undefined
          ? cfg.reasoning
          : activeThread.reasoning_effort ?? null;
      const label =
        (nextTier ?? t("chat.modelDefault", "默认")) +
        (nextReasoning ? `·${nextReasoning}` : "");
      const willRestart = activeMembers.some((m) => m.role === "orchestrator");
      const run = (async () => {
        await api.setThreadModel(workspace.workspaceId, activeThread.id, {
          tier: nextTier,
          reasoning: nextReasoning,
        });
        const orchs = activeMembers.filter((m) => m.role === "orchestrator");
        if (orchs.length > 0) {
          await Promise.allSettled(orchs.map((o) => api.killAgent(o.agent_id)));
          const resp = await api.runSpell({
            name: "init",
            task: "",
            workspace_dir: workspace.path,
            workspace_id: workspace.workspaceId,
            thread_id: activeThread.id,
          });
          // Engine fallback is never silent (billing red line) — see CreateWizard.
          notifySpawnFallbacks(t, resp.agents);
        }
      })();
      // L5: surface the REAL outcome instead of swallowing it into console.warn
      // (the chip used to just revert with no explanation). toast.promise rides
      // the loading → success / error states off the actual operation.
      toast.promise(run, {
        loading: willRestart
          ? t("chat.modelSwitchingRestart", {
              label,
              defaultValue: "正在切换到 {{label}} 并重启规划…",
            })
          : t("chat.modelSwitching", {
              label,
              defaultValue: "正在切换到 {{label}}…",
            }),
        success: t("chat.modelSwitched", {
          label,
          defaultValue: "已切换到 {{label}}",
        }),
        error: (e) =>
          t("chat.modelSwitchFailed", {
            msg: (e as Error)?.message ?? t("common.unknownError", "未知错误"),
            defaultValue: "切换模型失败：{{msg}}",
          }),
      });
      try {
        await run;
        refreshAgents();
      } catch (e) {
        // The toast already reported it; keep a console breadcrumb for triage.
        // eslint-disable-next-line no-console
        console.warn("set direction model failed", e);
      } finally {
        setModelBusy(false);
      }
    },
    [
      activeThread,
      modelBusy,
      workspace.workspaceId,
      workspace.path,
      activeMembers,
      refreshAgents,
      t,
    ],
  );

  // P0-10: gate the model switch behind a confirm when a live orchestrator
  // would actually be restarted + its in-flight reply interrupted. A no-op
  // re-select or a member-less direction applies silently.
  const setDirectionModel = useCallback(
    (cfg: { tier?: string | null; reasoning?: string | null }) => {
      if (!activeThread || modelBusy) return;
      const curTier = activeThread.model_tier ?? null;
      const curReasoning = activeThread.reasoning_effort ?? null;
      if (cfg.tier !== undefined && (cfg.tier ?? null) === curTier) return;
      if (cfg.reasoning !== undefined && (cfg.reasoning ?? null) === curReasoning)
        return;
      const hasLiveOrch = activeMembers.some(
        (m) => m.role === "orchestrator" && m.killed_at == null,
      );
      if (!hasLiveOrch) {
        void applyDirectionModel(cfg);
        return;
      }
      setConfirm({
        title: t("messages.modelConfirmTitle", "换模型会重启规划"),
        description: t(
          "messages.modelConfirmBody",
          "当前在跑的回复会被打断，规划会用新模型重新开始。",
        ),
        confirmLabel: t("messages.modelConfirmYes", "确认切换"),
        onConfirm: () => {
          void applyDirectionModel(cfg);
        },
      });
    },
    [activeThread, modelBusy, activeMembers, applyDirectionModel, t],
  );

  // 发消息即上线：工作空间没有活的 orchestrator 时，用户直接发消息——自动
  // 拉起 orchestrator(init spell)、把这条消息投给它、唤醒它干活，省去先手点
  // 「唤醒调度」。MessagesPanel 在 defaultRecipient 为空(无活成员)时调用本函数。
  const bootstrapRef = useRef(false);
  const sendBootstrappingOrchestrator = useCallback(
    async (body: string) => {
      if (bootstrapRef.current) return;
      bootstrapRef.current = true;
      try {
        const resp = await api.runSpell({
          name: "init",
          task: "",
          workspace_dir: workspace.path,
          workspace_id: workspace.workspaceId,
          thread_id: activeThread?.id,
        });
        // Engine fallback is never silent (billing red line) — see CreateWizard.
        notifySpawnFallbacks(t, resp.agents);
        const orch =
          resp.agents.find((a) => a.role === "orchestrator") ?? resp.agents[0];
        if (orch) {
          // Persist the user's message to the freshly-spawned orchestrator so it
          // shows in chat AND lands in its mailbox. The server's POST /api/message
          // handler auto-wakes the recipient when an EXTERNAL sender (user/system)
          // messages a LIVE agent (W0-2) — the orchestrator was just spawned by the
          // spell above, so it's registered and gets woken by this send alone.
          // We therefore DO NOT also call api.wakeAgent here: that produced a
          // double-kick (auto-wake + explicit wake ~20ms apart → two identical
          // "操作员唤醒" notes in the captain's mailbox, the exact regression W0-2
          // removed from MessagesPanel's live-send path). One source of wake only.
          await api.sendMessage({
            from: "user",
            to: orch.agent_id,
            kind: "note",
            body,
          });
        }
        refreshAgents();
      } catch (e) {
        // eslint-disable-next-line no-console
        console.warn("send-to-revive orchestrator failed", e);
        throw e; // let MessagesPanel surface the error inline
      } finally {
        bootstrapRef.current = false;
      }
    },
    [workspace.path, workspace.workspaceId, activeThread?.id, refreshAgents, t],
  );

  // ── Task activity state machine ──────────────────────────────────
  // 解决业界说的 "doomscrolling gap" — 用户发完消息后 5-30 秒 UI 黑盒。
  // 简化版状态机:
  //   1. user 发消息 → push pending TaskActivity
  //   2. agent_state=spawning 且 8s 内有 pending task → 把 role 加进去，
  //      升级到 spawning 状态
  //   3. 所有 spawning agents 都 ready → 升 ready，4s 后自动 dismiss
  //   4. pending 超过 15s 没 spawning → expired (默认 dismiss)
  // user 发的是普通聊天消息 (没派活) 时 15s timeout 兜底，无副作用。
  const [tasks, setTasks] = useState<TaskActivityT[]>([]);
  const tasksRef = useRef<TaskActivityT[]>([]);
  useEffect(() => {
    tasksRef.current = tasks;
  }, [tasks]);

  // 已知 agent_id 集合 — 用来判定 spawning 事件是不是新 agent (老 agent
  // 重启也会发 spawning 但不算新派活)。惰性初始化(渲染期一次性种子):
  // 之前是 useEffect(...,[allAliveAgents]) 每次花名册变化都先把所有现存
  // agent 标记为 known,而发现新 agent 的 effect 声明在它后面 —— 同一次
  // 提交里先标 known 再查新,永远查不到,task card(和冷启动阶段条)从
  // 不显示。改成渲染期懒初始化后,新增只由下面的 effect 自己登记,竞态消除。
  const knownAgentIdsRef = useRef<Set<string> | null>(null);
  if (knownAgentIdsRef.current === null) {
    knownAgentIdsRef.current = new Set(allAliveAgents.map((a) => a.agent_id));
  }

  // 轻量 messages cache —— 给成员列表的语义状态推导喂数据。MessagesPanel
  // 也自己拉一份(它需要展示 200 条 + filter + 排序),这里只需要"最近一条
  // inbound/outbound per agent"这点信息,但写两套缓存不值,直接 fetch 一次
  // + liveMessage append。每次切 workspace 重新拉(workspace.id 变化)。
  const [recentMessages, setRecentMessages] = useState<MessageRecord[]>([]);
  useEffect(() => {
    let cancelled = false;
    api
      .listMessages({ limit: 200 })
      .then((rows) => {
        if (!cancelled) setRecentMessages(rows);
      })
      .catch(() => {
        /* best-effort — statusDot falls back to "online" */
      });
    return () => {
      cancelled = true;
    };
  }, [workspace.id]);
  useEffect(() => {
    if (liveMessages.length === 0) return;
    setRecentMessages((prev) => {
      const have = new Set(prev.map((m) => m.id));
      const fresh = liveMessages.filter((m) => !have.has(m.id));
      if (fresh.length === 0) return prev;
      // Bounded like the other live caches (MessagesPanel / SwarmPanel /
      // useWorkspaceShellData all slice(-200)). This is a *derived* cache that
      // only feeds statusDot's "last inbound/outbound per agent" — it never
      // needs more than the recent tail. Without the cap it grew unbounded for
      // the whole session, and statusDot scans it per-member per-render, so the
      // member sidebar got O(members × session-length) and degraded over time.
      const next = [...prev, ...fresh];
      return next.length > 200 ? next.slice(-200) : next;
    });
  }, [liveMessages]);
  // worker 心跳 — orchestrator 让每个 worker 每完成一步覆写
  // `<workspace_id>/<role>.progress.md`,我们订阅这些 key 实时显示在右栏。
  // 同样的数据 Ledger 视图也用,但 chat 这边给个 slim 列表,用户不用切 tab。
  const breadcrumbsRaw = useBreadcrumbs(
    `${workspace.workspaceId}/${threadSlug}/`,
  );
  const plan = usePlan(`${workspace.workspaceId}/${threadSlug}/`);

  // 每 5s tick 让 statusDot 重新评估时间窗(responding/working 都有时间窗,
  // 没消息进来时也要让它们自然衰减成 idle/awaiting)。
  const [statusTick, setStatusTick] = useState(0);
  useEffect(() => {
    const i = window.setInterval(() => setStatusTick((t) => t + 1), 5000);
    return () => window.clearInterval(i);
  }, []);
  void statusTick; // referenced via re-render

  // Re-resolve "XX 秒前" labels on each statusTick so the side panel
  // stays current even when no new event lands.
  const breadcrumbs = useMemo(
    () =>
      breadcrumbsRaw.map((b) => ({
        ...b,
        ago: fmtBreadcrumbAgo(b.at, Date.now()),
      })),
    // eslint-disable-next-line react-hooks/exhaustive-deps -- intentional tick dep
    [breadcrumbsRaw, statusTick],
  );

  const dismissTask = useCallback((id: string) => {
    setTasks((prev) => prev.filter((tsk) => tsk.id !== id));
  }, []);

  // pending task 兜底过期 + ready task 自动消失
  useEffect(() => {
    if (tasks.length === 0) return;
    const now = Date.now();
    const timers: number[] = [];
    for (const task of tasks) {
      if (task.status === "ready") {
        // P2-4: count the auto-dismiss window from when the task became ready,
        // not from the user message that triggered it. A slow spawn (e.g. 30s)
        // used to land here with `now - startedAt` already past the 4s window,
        // so the ready card flashed and vanished instantly. `readyAt` (stamped
        // at the ready transition) gives the operator the full glimpse window.
        const anchor = task.readyAt ?? task.startedAt;
        const remaining = TASK_READY_DISMISS_MS - (now - anchor);
        timers.push(
          window.setTimeout(() => dismissTask(task.id), Math.max(0, remaining)),
        );
      } else if (task.status === "pending") {
        const remaining = TASK_PENDING_TIMEOUT_MS - (now - task.startedAt);
        timers.push(
          window.setTimeout(() => dismissTask(task.id), Math.max(0, remaining)),
        );
      }
    }
    return () => {
      for (const id of timers) window.clearTimeout(id);
    };
  }, [tasks, dismissTask]);

  // 状态机重新设计 (基于用户反馈 — chat typing indicator 已经表达"AI 正
  // 在响应"，task card 不要重复，只在 chat 不能表达的事才出现：
  //
  //   ✗ 普通 single-agent reply → 不显示 task card (chat typing 够了)
  //   ✓ 派活拉起了新 agent → 显示 "拉起 X 个 agent · roles..."
  //     这是 chat 看不到的信息：成员栏多了几个人但没人特别提示
  //   ✓ 所有新 agent ready → 升 ready (✓)，4s 后消失
  //   ✓ 兜底 60s 没 ready 就 expired (网络/agent 死)
  //
  // 触发：记录最近 user msg 时间，新 agent 出现且在 user msg 后 15s 内
  // 才认为是"派活触发的"，否则视为 agent 自发 spawn (比如别人启动的) 不
  // 显示。
  const lastUserMsgRef = useRef<{ at: number; body: string; id: number } | null>(null);
  useEffect(() => {
    if (liveMessages.length === 0) return;
    // Take the most-recent user message in this batch (highest id).
    let latestUser: MessageRecord | null = null;
    for (const m of liveMessages) {
      if (m.from_agent === "user" && (!latestUser || m.id > latestUser.id)) {
        latestUser = m;
      }
    }
    if (latestUser) {
      lastUserMsgRef.current = {
        at: latestUser.sent_at,
        body: latestUser.body.slice(0, 32),
        id: latestUser.id,
      };
    }
  }, [liveMessages]);

  // allAliveAgents 出现新 agent → 如果最近 60s 内有 user msg → 创建/更新
  // spawning task；新 agent 进入 ready 时升 task 状态
  useEffect(() => {
    const known = knownAgentIdsRef.current;
    if (!known) return;
    const newAgents: AgentInfo[] = [];
    for (const a of allAliveAgents) {
      if (!known.has(a.agent_id)) {
        newAgents.push(a);
        known.add(a.agent_id);
      }
    }
    if (newAgents.length === 0) {
      // 没新 agent — 检查现有 spawning task 内的 agent 是否都 ready，是
      // 就升级 task。
      setTasks((prev) =>
        prev.map((tsk) => {
          if (tsk.status !== "spawning") return tsk;
          const matching = allAliveAgents.filter((a) =>
            tsk.spawnedRoles.includes(a.role),
          );
          if (matching.length === 0) return tsk;
          // M1: "Ready" must mean the agent can actually WORK, not just that its
          // PTY launched (shim_ready). A spawn wedged on a login prompt / MCP
          // loop reports shim_ready but carries last_error — agentIsWorkable
          // gates on the real health signal so the card never flips green over
          // an agent the watchdog will fail 45–300s later.
          const allReady = matching.every((a) => agentIsWorkable(a));
          if (!allReady) return tsk;
          // P2-4: stamp readyAt only on the actual flip (preserve any prior
          // stamp). This effect re-runs on every roster change; re-stamping
          // would keep pushing the auto-dismiss window forward and the card
          // would never disappear.
          return { ...tsk, status: "ready" as const, readyAt: tsk.readyAt ?? Date.now() };
        }),
      );
      return;
    }
    const lastUser = lastUserMsgRef.current;
    const now = Date.now();
    if (!lastUser || now - lastUser.at > TASK_ATTACH_WINDOW_MS) {
      // 新 agent 不是用户消息触发的 (>15s 前 / 没 user msg) — 不显示 task
      // card，agent 加入会通过成员栏体现。
      return;
    }
    // M2: a just-spawned agent that immediately FAILED (last_error set) is
    // still in the alive roster — don't count it as dispatched work / inflate
    // the "正在派活 N" chip. Only not-yet-failing roles get listed.
    const fresh = newAgents.filter((a) => !agentIsErrored(a));
    if (fresh.length === 0) return;
    setTasks((prev) => {
      const taskId = `task-${lastUser.id}`;
      const existing = prev.find((t) => t.id === taskId);
      // M1: "ready" gates on agentIsWorkable (PTY up + alive + not failing),
      // not bare shim_ready — same honest signal as the no-new-agents path.
      const allReady = fresh.every((a) => agentIsWorkable(a));
      if (existing) {
        // 已经为这条 user msg 创建过 task — append roles
        return prev.map((tsk) =>
          tsk.id === taskId
            ? {
                ...tsk,
                status: (allReady ? "ready" : "spawning") as TaskActivityT["status"],
                // P2-4: stamp readyAt on the flip to ready, preserving any prior
                // stamp; clear it if a freshly-appended role drops us back to
                // spawning so the dismiss window restarts when ready returns.
                readyAt: allReady ? tsk.readyAt ?? Date.now() : undefined,
                spawnedRoles: [...tsk.spawnedRoles, ...fresh.map((a) => a.role)],
                spawnedAgentIds: [
                  ...tsk.spawnedAgentIds,
                  ...fresh.map((a) => a.agent_id),
                ],
              }
            : tsk,
        );
      }
      return [
        ...prev,
        {
          id: taskId,
          startedAt: lastUser.at,
          status: (allReady ? "ready" : "spawning") as TaskActivityT["status"],
          // P2-4: a task born already-ready gets its dismiss window from now.
          readyAt: allReady ? Date.now() : undefined,
          trigger: lastUser.body,
          spawnedRoles: fresh.map((a) => a.role),
          spawnedAgentIds: fresh.map((a) => a.agent_id),
        },
      ];
    });
  }, [allAliveAgents]);

  // P1-08 补漏:规划已 spawn 但 PTY 还没 ready(shim_ready=false)的 5–20s
  // 冷启动窗口里 agentIsWorkable 为 false —— strip 的「有无成员」若只看
  // workable,窗口内会判定「无成员」重新放出「唤起 orchestrator」按钮,
  // 连点 = 同方向双规划。只要方向里还有活着的、没报错的 orchestrator
  // (agentIsAlive 语义:未 kill 未退出),就按「正在准备」处理。
  const orchestratorStarting = activeMembers.some(
    (m) =>
      m.role === "orchestrator" &&
      agentIsAlive(m) &&
      !m.shim_ready &&
      !agentIsErrored(m),
  );

  const directionName =
    activeThread?.slug === "main"
      ? t("chat.mainDirection")
      : activeThread?.name?.trim() ||
        activeThread?.slug ||
        t("chat.directionUnnamed");

  return (
    <div className="flex min-h-0 flex-1">
      {/* Onboarding 已由「空态即引导」接管(MessagesPanel 的 EmptyState:
          starter 一键填充 + 引擎就绪自检),不再弹 4 步浮层 —— 浮层说完就忘
          还挡点击,任务式引导让用户 30s 内看到 agent 真的跑起来。 */}
      <section className="flex min-w-0 flex-1 flex-col">
        <WorkspaceStatusStrip
          workspaceName={workspace.name}
          directionName={directionName}
          // L1: "N 个 AI 在线" counts only agents that can actually work — an
          // errored/wedged member (last_error set) isn't "online". (was:
          // activeMembers.length, which counted failing agents as online.)
          memberCount={activeMembers.filter(agentIsWorkable).length}
          sourceCount={workspace.roots.length + 1}
          cwdBranch={workspace.cwdBranch}
          readiness={readiness}
          workspaceId={workspace.workspaceId}
          hasError={orchestratorFailure != null}
          preparing={activeThread?.state === "preparing" || orchestratorStarting}
          hasHistory={recentMessages.length > 0}
        />
        {/* P2: the captain's structured plan, pinned above the conversation —
            accurate ✓/◐/○ from plan.json, not guessed from prose. */}
        {plan && (
          <PlanStickyCard
            plan={plan}
            dismissKey={`${workspace.workspaceId}:${activeThread?.id ?? "main"}`}
            // 计划诚实门只盯「还活着却没交」——已死的 worker 归 NeedsYou，
            // 别用琥珀条永久占着聊天顶（清完 .error 后尤其像撒谎）。
            undeliveredHandoffs={handoffMissingAgents.filter(
              (a) => a.killed_at == null && a.shim_exit == null,
            ).length}
          />
        )}
        {/* <xl 紧凑成员条:<1280px 时右侧 PulseRail(≥xl)与完整成员面板(≥2xl)
            都不渲染,「谁在线 / 谁卡住」整体蒸发 —— 靠这一横排诚实点(逻辑同
            PulseRail,不重构它)保住核心信号;点击开成员抽屉。≥xl 由 rail 接管
            (xl:hidden 不占高)。无成员时不渲染 —— 空房间的引导由 MessagesPanel
            的 EmptyState 负责,一条空横条只是噪音。 */}
        {activeMembers.length > 0 && (
          <div className="shrink-0 border-b border-border-subtle bg-surface-secondary xl:hidden">
            <MemberStrip
              members={activeMembers}
              agentStateById={agentStateById}
              recentMessages={recentMessages}
              unreadByFrom={activeWorkspaceUnread}
              onOpenAgent={openAgent}
            />
          </div>
        )}
        <MessagesPanel
          liveMessages={liveMessages}
          liveRead={liveRead}
          unreadByFrom={activeWorkspaceUnread}
          activeMembers={activeMembers}
          allAliveAgents={allAliveAgents}
          handoffMissingAgents={handoffMissingAgents}
          workspaceAgentIds={threadAgentIds}
          workspaceSlug={workspace.id}
          activeThreadId={activeThread?.id}
          jumpUnreadTick={jumpUnreadTick}
          onOpenAgent={openAgent}
          onSend={sendBootstrappingOrchestrator}
          modelTier={activeThread?.model_tier ?? null}
          reasoningEffort={activeThread?.reasoning_effort ?? null}
          onSetModel={setDirectionModel}
          modelBusy={modelBusy}
          agentActivityById={agentActivityById}
          agentLiveStateById={agentStateById}
          reasoningById={reasoningById}
          cliReadiness={{
            loading: readiness.loading,
            probing: readiness.probing,
            engines: readiness.engines,
            onProbe: readiness.probe,
          }}
          emptyStateOverride={
            orchestratorFailure ? (
              <OrchestratorFailureCard
                reason={orchestratorFailure.reason}
                kind={orchestratorFailure.kind}
                loginCommand={orchestratorFailure.loginCommand}
                onOpenTerminal={() => openAgent(orchestratorFailure.agentId)}
                onRetry={retryOrchestrator}
                retrying={retrying}
              />
            ) : bootstrapState ? (
              <BootstrapChecklistCard
                branchLabel={bootstrapState.branchLabel}
                engineName={bootstrapState.engineName}
                shimReady={bootstrapState.shimReady}
                spawnedAt={bootstrapState.spawnedAt}
              />
            ) : undefined
          }
          taskActivityBelow={
            <TaskActivity tasks={tasks} onDismiss={dismissTask} stageById={agentStageById} />
          }
        />
      </section>

      {/* surface-secondary (not -elevated) so this rail matches the left
          workspace rail — both side rails read as panels, keeping the
          3-column layout balanced instead of the right one blending into
          the white center canvas. No-op in dark (both resolve slate-800). */}
      {/* Members is auxiliary — hide below xl so the chat keeps a usable width
          on half-screen / narrow windows instead of overflowing off-screen
          (R2-004). Agent status is also visible in the DAG + agent drawer. */}
      {/* P0-12: collapsed pulse rail for the 1280–1535px range, where the full
          members panel (≥1536px below) doesn't render — so member health is no
          longer invisible on a laptop. Click a dot → opens that member's drawer
          (focus). Pure breakpoint visibility, additive to the full panel. */}
      <aside className="hidden w-14 shrink-0 flex-col border-l border-border-subtle bg-surface-secondary xl:flex 2xl:hidden">
        <PulseRail
          members={activeMembers}
          agentStateById={agentStateById}
          recentMessages={recentMessages}
          unreadByFrom={activeWorkspaceUnread}
          onOpenAgent={openAgent}
        />
      </aside>
      <aside className="hidden w-[320px] shrink-0 flex-col border-l border-border-subtle bg-surface-secondary 2xl:flex">
        <div className="flex h-12 shrink-0 items-center gap-2 border-b border-border-subtle px-4">
          <Users className="size-4 text-foreground-tertiary" />
          <h2 className="font-heading text-xs font-semibold uppercase tracking-wider text-foreground-tertiary">
            {t("chat.members")}
          </h2>
          <span className="ml-auto font-caption text-xs text-foreground-tertiary">
            {activeMembers.length}
          </span>
        </div>
        <div className="flex-1 overflow-y-auto px-2 py-2">
          {activeMembers.length === 0 && (
            <div className="mx-2 mt-2 rounded-xl border border-border-subtle bg-surface-elevated px-4 py-4">
              <p className="font-caption text-xs leading-6 text-foreground-tertiary">
                {t(
                  recentMessages.length > 0
                    ? "chat.noMembersAway"
                    : "chat.noMembers",
                  recentMessages.length > 0
                    ? "成员已下线。再发一条消息，规划会回来。"
                    : "还没有成员在线。在对话里发一条消息，规划就会上线。",
                )}
              </p>
            </div>
          )}
          {(() => {
            // 排序优先级:
            //   1. 异常退出(state=error 且未被主动 kill)→ 顶到最前,出错
            //      自动抢眼。killed_at 的成员不算 error(主动 kill 不顶)。
            //   2. orchestrator → 次置顶。用户在 workspace 里唯一的常驻
            //      对接人 (Magentic-One PM 角色),worker 完工后回头找它。
            //   3. 其余按原顺序。
            const isErr = (a: AgentInfo) =>
              a.killed_at == null &&
              a.shim_exit == null &&
              agentStateById[a.agent_id]?.state === "error";
            const rank = (a: AgentInfo) =>
              isErr(a) ? 0 : a.role === "orchestrator" ? 1 : 2;
            const sorted = [...activeMembers].sort((a, b) => rank(a) - rank(b));
            return sorted;
          })().map((a) => {
            const live = agentStateById[a.agent_id];
            const dot = statusDot(a, live, recentMessages, t);
            const activity = formatActivityLine(live);
            const unread = activeWorkspaceUnread[a.agent_id] ?? 0;
            const isOrchestrator = a.role === "orchestrator";
            return (
              <div
                key={a.agent_id}
                onClick={() => openAgent(a.agent_id)}
                className={cn(
                  "flex cursor-pointer items-center gap-3 rounded-md px-3 py-2 hover:bg-surface-tertiary",
                )}
              >
                <Avatar
                  className={cn(
                    "size-8 shrink-0",
                    // 金边告诉用户"这个是调度,长期在线"。没有 ring
                    // 的就是临时 worker — 干完一件事就会消失。
                    isOrchestrator &&
                      "ring-2 ring-accent-primary ring-offset-2 ring-offset-surface-secondary",
                  )}
                  title={a.role}
                >
                  <AvatarFallback
                    className={cn(
                      "text-xs font-medium text-foreground-on-accent",
                      roleColor(a.role),
                    )}
                  >
                    {a.role.slice(0, 1).toUpperCase()}
                  </AvatarFallback>
                </Avatar>
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="truncate font-heading text-sm text-foreground-primary">
                      {roleLabelAmong(a, activeMembers)}
                    </span>
                    {isOrchestrator && (
                      <span className="shrink-0 rounded-full bg-accent-primary-soft px-1.5 py-0.5 font-caption text-[9px] font-semibold text-accent-primary">
                        {t("chat.pmBadge", "调度")}
                      </span>
                    )}
                    {dot.typing ? (
                      <TypingDots />
                    ) : (
                      <>
                        {dot.className && (
                          <span
                            className={cn("size-1.5 rounded-full", dot.className)}
                            title={dot.label || undefined}
                          />
                        )}
                        {dot.label && (
                          <span
                            className={cn(
                              "font-caption text-[10px]",
                              dot.isError
                                ? "text-status-danger"
                                : "text-foreground-tertiary",
                            )}
                          >
                            {dot.label}
                          </span>
                        )}
                      </>
                    )}
                  </div>
                  {/* 第二行 = "此刻在干嘛"。有实时活动就展示它(规格图示
                      "正在调用 Edit · src/App.vue 0:42"),否则回退 cli·id。
                      跑完(ok)→✓,出错→✕(红);running 用 at→now elapsed,
                      ok/error 用 duration_ms。 */}
                  {activity ? (
                    <div className="flex items-center gap-1.5 truncate font-mono text-[10px]">
                      <span
                        className={cn(
                          "shrink-0",
                          activity.phase === "error"
                            ? "text-status-danger"
                            : activity.phase === "ok"
                              ? "text-status-success"
                              : "text-accent-primary",
                        )}
                      >
                        {activity.phase === "error"
                          ? "✕"
                          : activity.phase === "ok"
                            ? "✓"
                            : "▸"}
                      </span>
                      <span
                        className={cn(
                          "min-w-0 flex-1 truncate",
                          activity.phase === "error"
                            ? "text-status-danger"
                            : "text-foreground-secondary",
                        )}
                        title={activity.label}
                      >
                        {activity.phase === "running"
                          ? t("chat.activity.running", { label: activity.label })
                          : activity.label}
                      </span>
                      <span className="shrink-0 text-foreground-tertiary">
                        {fmtActivityElapsed(activity.elapsedMs)}
                      </span>
                    </div>
                  ) : (
                    <div className="truncate font-mono text-[10px] text-foreground-tertiary">
                      {a.cli} · {a.agent_id.slice(-8)}
                    </div>
                  )}
                </div>
                {unread > 0 && (
                  <Badge
                    variant="destructive"
                    className="rounded-full px-1.5 py-0.5 text-[10px]"
                  >
                    {unread}
                  </Badge>
                )}
              </div>
            );
          })}
        </div>
        {breadcrumbs.length > 0 && (
          <div className="shrink-0 border-t border-border-subtle">
            <div className="flex items-center gap-2 px-4 pt-3 pb-2">
              <Radio className="size-3.5 text-accent-primary" />
              <span className="font-heading text-[11px] font-semibold uppercase tracking-wider text-foreground-tertiary">
                {t("chat.breadcrumbsTitle", "近况")}
              </span>
              <span className="ml-auto font-caption text-[10px] text-foreground-tertiary">
                {breadcrumbs.length}
              </span>
            </div>
            <ul className="flex max-h-[35vh] flex-col gap-1.5 overflow-y-auto px-2 pb-3">
              {breadcrumbs.map((b) => (
                <li
                  key={b.role}
                  className="rounded-md bg-surface-tertiary px-3 py-2"
                >
                  <div className="flex items-baseline gap-2">
                    <span className="shrink-0 font-mono text-[10px] font-semibold text-accent-primary">
                      {roleDisplayName(b.role)}
                    </span>
                    <span className="ml-auto shrink-0 font-caption text-[9px] text-foreground-tertiary">
                      {b.ago}
                    </span>
                  </div>
                  <p className="mt-0.5 break-words font-body text-[11px] leading-snug text-foreground-primary">
                    {b.content}
                  </p>
                </li>
              ))}
            </ul>
          </div>
        )}
      </aside>
      {/* Mounted outside the ≥1536px members aside so confirms triggered from
          the full-width chat — e.g. the P0-10 model-switch confirm — work at
          any window width (the aside, and its old copy of this dialog, only
          render at 2xl). */}
      <ConfirmActionDialog
        action={confirm}
        onOpenChange={(next) => {
          if (!next) setConfirm(null);
        }}
      />
    </div>
  );
}
