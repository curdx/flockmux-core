/**
 * WorkspaceShell — 4 个工作空间内 view (chat / dag / replays / context) 的
 * 共享 chrome。React Router 拿它当 layout route：所有 /chat/:wsId/* 都进这
 * 一层，子 view 通过 <Outlet/> 渲染。
 *
 * 解决的核心问题：之前每个子 view 是独立的 top-level route，切 tab → 整页
 * 卸载 + 重画，连工作空间列表都不在了，用户感觉自己"跳了页面"而不是
 * "切了视图"。Shell 化之后切 tab 只重渲染 Outlet：
 *   - 左侧工作空间列表常驻
 *   - Channel header（workspace 名 + 路径 + 未读 + 复制）常驻
 *   - Tab bar 常驻
 *   - swarm event 订阅常驻 → 切走再回来 unread/agent state 不丢
 *
 * Outlet context 把 activeWs / agents / liveMessage 等下发给子 view，
 * 避免每个 view 自己 listAgents / 开 swarm subscription（之前协作图
 * 和录像页各开了一个，重复请求 + 重复 ws 连接）。
 *
 * 拆分（god-file 治理）：侧栏 + 根源树 + 管理对话框搬到 WorkspaceSidebar，
 * tab 栏 + 视图过渡搬到 WorkspaceToolbar，WorkspaceSummary 类型搬到 ./types，
 * 数据编排（agents/workspaces/unread + swarm 订阅 + 级联删除）搬到
 * useWorkspaceShellData hook。本文件只剩"布局 + 导航 + Outlet context 组装"。
 * WorkspaceList / WorkspaceSummary 在此 re-export，既有 import 站点
 * （chat/Home）无需改动。
 */

import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import {
  Outlet,
  useLocation,
  useNavigate,
  useOutletContext,
  useParams,
  useSearchParams,
} from "react-router-dom";
import { useTranslation } from "react-i18next";
import { FolderOpen } from "lucide-react";
import { api, ApiError } from "../../api/http";
import type {
  AgentActivity,
  AgentInfo,
  AgentLiveState,
  MessageRecord,
  ThreadInfo,
} from "../../api/types";
import { setActiveWorkspaceId } from "../../lib/activeWorkspace";
import { notifySpawnFallbacks } from "../../lib/engineFallback";
import { ErrorBoundary } from "../../components/ErrorBoundary";
import { Welcome } from "../../components/Welcome";
import { toast } from "@/lib/toast";
import { useAutoNudgeStalled } from "@/hooks/useAutoNudgeStalled";
import { TooltipProvider } from "@/components/ui/tooltip";
import {
  Sheet,
  SheetContent,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import type { ReasoningSummary } from "../../components/MessagesPanel";
import type { WorkspaceSummary } from "./types";
import { WorkspaceList } from "./WorkspaceSidebar";
import { WorkspaceToolbar, ViewTransition, buildTabs } from "./WorkspaceToolbar";
import { NeedsYouBar } from "@/components/workspace/NeedsYouBar";
import { useWorkspaceShellData } from "./useWorkspaceShellData";

const AgentDrawer = lazy(() =>
  import("@/components/agent/AgentDrawer").then((m) => ({
    default: m.AgentDrawer,
  })),
);
const CreateWizard = lazy(() =>
  import("@/components/workspace/CreateWizard").then((m) => ({
    default: m.CreateWizard,
  })),
);

// Re-exported so existing import sites keep working unchanged (chat/Home.tsx
// imports `WorkspaceList` + `WorkspaceSummary` from here; the child views below
// import `useWorkspaceContext`).
export type { WorkspaceSummary } from "./types";
export { WorkspaceList } from "./WorkspaceSidebar";

// ── Outlet context ─────────────────────────────────────────────────────

/** Threaded down to children via <Outlet context={...}/>. Anything a child
 *  view needs that the Shell already computed lives here so we don't run
 *  redundant fetches / subscriptions. */
export interface ShellOutletContext {
  workspace: WorkspaceSummary;
  /** The active direction (thread), resolved from the URL `:threadSlug`
   *  param, defaulting to the workspace's main thread. `null` only for a
   *  legacy/empty workspace with no thread rows. */
  activeThread: ThreadInfo | null;
  /** The workspace's main direction. Views fold `thread_id == null` agents
   *  into main when filtering their own agent fetches by direction. */
  mainThread: ThreadInfo | null;
  /** Resolved slug of the active direction — `"main"` when none. Views build
   *  blackboard keys as `{workspace.workspaceId}/{threadSlug}/…`. */
  threadSlug: string;
  /** Alive agents in the active workspace (= workspace.members alias). */
  activeMembers: AgentInfo[];
  /** Alive agents in the ACTIVE direction (subset of activeMembers). The
   *  members list + DAG render this so each direction is self-contained. */
  threadMembers: AgentInfo[];
  /** Every alive agent across all workspaces — composer needs it to
   *  resolve cross-workspace mentions ("planner is responding…"). */
  allAliveAgents: AgentInfo[];
  /** Historical id set of agents that ever lived in this workspace
   *  (alive + killed). Workspace-wide; kept for any non-thread consumers. */
  workspaceAgentIds: string[];
  /** Historical id set (alive + killed) of agents in the ACTIVE direction.
   *  MessagesPanel filters by it so each direction is a self-contained room. */
  threadAgentIds: string[];
  /** Agents in the active direction that EXITED without delivering their
   *  declared handoff (server-computed `handoff_missing`). Empty in the common
   *  healthy case. The chat shows a quiet, actionable banner when non-empty. */
  handoffMissingAgents: AgentInfo[];
  /** Append-only bounded buffer of live swarm messages. Children merge by id —
   *  never a single slot (batched arrivals would overwrite all but the last). */
  liveMessages: MessageRecord[];
  /** Latest message_read event, or null. */
  liveRead: { ids: number[]; to_agent: string; at: number } | null;
  /** Per-agent live state + latest activity from the swarm WS, keyed by
   *  agent_id. The members list reads it for the real-time status dot +
   *  "what this worker is doing right now" line. */
  agentStateById: Record<string, AgentLiveState>;
  /** Latest cold-start stage per agent (shim_ready → mcp_ready →
   *  bootstrap_injected) — drives the chat's pending-card stage bar. */
  agentStageById: Record<string, { stage: string; at: number }>;
  /** Bounded per-agent activity history from the swarm WS. Chat uses it to
   *  patch late tool events into visible thought summaries. */
  agentActivityById: Record<string, AgentActivity[]>;
  /** Live in-flight reasoning steps keyed by agent id, fed by
   *  `thought_trace_event`, so the pending bubble grows its steps mid-turn. */
  reasoningById: Record<string, ReasoningSummary>;
  /** Unread tally, already filtered to this workspace's senders. */
  unreadByFrom: Record<string, number>;
  /** Click → bump this counter, MessagesPanel scrolls to first unread. */
  jumpUnreadTick: number;
  /** Open the right-side AgentDrawer (writes ?agent=<id> into URL). */
  openAgent: (agentId: string) => void;
  /** Imperative refresh handle child views can call after mutations
   *  (e.g. wake-agent button → listAgents() to update spinner state). */
  refreshAgents: () => void;
}

/** Convenience hook so child views don't import the context object. */
export function useWorkspaceContext(): ShellOutletContext {
  return useOutletContext<ShellOutletContext>();
}

// ── Shell ──────────────────────────────────────────────────────────────

export default function WorkspaceShell() {
  const { t } = useTranslation();
  const { wsId, threadSlug: threadSlugParam } = useParams<{
    wsId: string;
    threadSlug?: string;
  }>();
  const navigate = useNavigate();
  const location = useLocation();
  const [searchParams, setSearchParams] = useSearchParams();

  // Right-side AgentDrawer state lives in URL (?agent=<id>) so the user
  // can deep-link / refresh. Shell owns it so any view can open it.
  const drawerAgentId = searchParams.get("agent");
  const openAgent = useCallback(
    (id: string) => {
      setSearchParams((prev) => {
        const next = new URLSearchParams(prev);
        next.set("agent", id);
        return next;
      });
    },
    [setSearchParams],
  );
  const closeAgent = useCallback(() => {
    setSearchParams((prev) => {
      const next = new URLSearchParams(prev);
      next.delete("agent");
      next.delete("tab"); // the drawer's tab param is meaningless without an agent
      return next;
    });
  }, [setSearchParams]);

  // CreateWizard opens from sidebar + ⌘K (window event).
  const [wizardOpen, setWizardOpen] = useState(false);
  const [mobileNavOpen, setMobileNavOpen] = useState(false);
  useEffect(() => {
    const onOpen = () => setWizardOpen(true);
    window.addEventListener("swarmx:open-wizard", onOpen as EventListener);
    return () =>
      window.removeEventListener("swarmx:open-wizard", onOpen as EventListener);
  }, []);

  // jumpUnread is a pure UI signal to MessagesPanel (scroll to first unread).
  const [jumpUnreadTick, setJumpUnreadTick] = useState(0);

  // All data orchestration (agents / workspaces / unread, the single swarm
  // subscription, the cascade delete) lives in the hook — Shell stays layout.
  const {
    agents,
    workspaces,
    activeWs,
    activeThread,
    mainThread,
    activeThreadSlug,
    allAliveAgents,
    workspaceAgentIds,
    threadAgentIds,
    threadMembers,
    handoffMissingAgents,
    needsYouMembers,
    liveMessages,
    liveRead,
    agentStateById,
    agentStageById,
    agentActivityById,
    reasoningById,
    activeWorkspaceUnread,
    totalUnread,
    refreshAgents,
    refreshWorkspaces,
    wsLoaded,
    wsError,
    deleteWorkspace,
  } = useWorkspaceShellData(wsId, threadSlugParam);

  // Stalled agents = auto-nudge (no user "催一下" button). Cooldown inside.
  useAutoNudgeStalled({
    members: needsYouMembers,
    liveById: agentStateById,
    messages: liveMessages,
  });

  // deleteWorkspace performs the kill+delete+optimistic-drop and returns where
  // to navigate when the ACTIVE workspace was removed (router stays here).
  const onDeleteWorkspace = useCallback(
    async (workspaceId: string) => {
      const navTo = await deleteWorkspace(workspaceId);
      if (navTo) navigate(navTo, { replace: true });
    },
    [deleteWorkspace, navigate],
  );

  // ── Open a new direction (zero-friction) ───────────────────────────
  // Create an unnamed shared thread, jump into it, then launch the
  // orchestrator there (run_spell with thread_id → backend runs it in the
  // direction's cwd). The orchestrator greets and names the direction from
  // the user's first message (swarm_name_thread → background git isolation).
  //
  // In-flight guard: a double-click would otherwise fire two createThread +
  // two run_spell calls → two empty directions, each with its own orchestrator.
  const creatingDirRef = useRef(false);
  // Directions created with a name go "preparing" and isolate in the
  // background; the backend re-roots their orchestrator into the worktree on
  // success but spawns NOTHING on a failed isolation (it degrades to shared).
  // We track such directions here so the settle-watcher effect below can
  // recover them — otherwise the user lands in an empty room with no
  // orchestrator and zero feedback. Map<threadId, {workspaceId, path}>.
  const pendingIsolationRef = useRef<
    Map<string, { workspaceId: string; path: string }>
  >(new Map());
  // Guards the recovery spawn from firing twice for the same thread across the
  // re-renders that happen between scheduling and resolving it.
  const recoveringDirRef = useRef<Set<string>>(new Set());
  const onNewDirection = useCallback(
    async (ws: WorkspaceSummary, name?: string, branch?: string) => {
      if (creatingDirRef.current) return;
      creatingDirRef.current = true;
      try {
        // `branch` opens an EXISTING branch as the direction (attach a
        // worktree); otherwise a fresh named/unnamed direction.
        const th = await api.createThread(
          ws.workspaceId,
          branch ? { branch } : name ? { name } : {},
        );
        await refreshWorkspaces();
        navigate(`/chat/${ws.id}/t/${th.slug}`);
        // A *named* direction isolates immediately: the backend marks it
        // "preparing", does a background `git worktree add`, and spawns the
        // orchestrator IN THE NEW WORKTREE CWD once isolation completes. In
        // that case the frontend must NOT also spawn — doing so produced a
        // second orchestrator for the same thread, and in the wrong cwd
        // (ws.path is the main project, not the worktree). An *unnamed*
        // direction comes back "ready" with no isolation, so the frontend
        // owns the spawn (isolation happens later via swarm_name_thread).
        if (th.state !== "preparing") {
          const resp = await api.runSpell({
            name: "init",
            task: "",
            workspace_dir: ws.path,
            workspace_id: ws.workspaceId,
            thread_id: th.id,
          });
          // Engine fallback is never silent (billing red line) — see CreateWizard.
          notifySpawnFallbacks(t, resp.agents);
        } else {
          // Watch this one for the failed-isolation case (degraded → no
          // orchestrator). The settle-watcher effect resolves it.
          pendingIsolationRef.current.set(th.id, {
            workspaceId: ws.workspaceId,
            path: ws.path,
          });
        }
        refreshAgents();
      } catch (e) {
        // Dialog already closed before this rejects, so a silent failure would
        // leave the user staring at the old room with no idea their new
        // direction never happened. Surface it.
        // eslint-disable-next-line no-console
        console.warn("new direction failed", e);
        toast.error(
          t("chat.newDirectionFailed", {
            defaultValue: "新建方向失败",
          }),
          { description: e instanceof ApiError ? e.detail : (e as Error)?.message },
        );
      } finally {
        creatingDirRef.current = false;
      }
    },
    [navigate, refreshWorkspaces, refreshAgents, t],
  );

  // ── Recover a direction whose background git isolation failed ────────
  // A named direction goes "preparing" while the backend does a `git worktree
  // add`. On success the backend re-roots an orchestrator into the worktree.
  // On FAILURE it degrades the direction to shared/ready and spawns NOTHING —
  // so without this the user is dropped into a room with no orchestrator and
  // no explanation (the P2 "empty room" bug). We watch each tracked direction
  // for its settle: worktree → done (backend spawned); degraded → the
  // isolation failed, so spawn the orchestrator in the shared cwd ourselves
  // (the same path an unnamed direction takes) and warn the user that
  // isolation didn't happen.
  useEffect(() => {
    if (pendingIsolationRef.current.size === 0) return;
    for (const [threadId, info] of pendingIsolationRef.current) {
      const th = workspaces
        .find((w) => w.workspaceId === info.workspaceId)
        ?.threads.find((x) => x.id === threadId);
      // Thread vanished (deleted mid-isolation) → stop tracking.
      if (!th) {
        pendingIsolationRef.current.delete(threadId);
        recoveringDirRef.current.delete(threadId);
        continue;
      }
      if (th.state === "preparing") continue; // still isolating — wait
      pendingIsolationRef.current.delete(threadId);
      // Isolation succeeded: the backend already re-rooted the orchestrator
      // into the worktree. Nothing for us to do.
      if (th.isolation === "worktree") continue;
      // Isolation failed (degraded) → no orchestrator was spawned. Recover by
      // launching one in the shared cwd, guarded so it fires once.
      if (recoveringDirRef.current.has(threadId)) continue;
      // Already has a live orchestrator (e.g. a manual retry)? Don't double up.
      const hasOrchestrator = agents.some(
        (a) =>
          a.thread_id === threadId &&
          a.role === "orchestrator" &&
          a.killed_at == null &&
          a.shim_exit == null,
      );
      if (hasOrchestrator) continue;
      recoveringDirRef.current.add(threadId);
      toast.warning(
        t("chat.directionIsolationDegraded", {
          defaultValue: "方向隔离失败,已改为与主线共用目录",
        }),
      );
      void api
        .runSpell({
          name: "init",
          task: "",
          workspace_dir: info.path,
          workspace_id: info.workspaceId,
          thread_id: threadId,
        })
        .then((resp) => {
          // Engine fallback is never silent (billing red line) — see CreateWizard.
          notifySpawnFallbacks(t, resp.agents);
          refreshAgents();
        })
        .catch((e) => {
          recoveringDirRef.current.delete(threadId);
          // eslint-disable-next-line no-console
          console.warn("recover degraded direction failed", e);
          toast.error(
            t("chat.newDirectionIncomplete", {
              defaultValue: "方向创建未完成,请重试",
            }),
            {
              description:
                e instanceof ApiError ? e.detail : (e as Error)?.message,
            },
          );
        });
    }
  }, [workspaces, agents, refreshAgents, t]);

  // ── Delete a direction ──────────────────────────────────────────────
  // Server kills the direction's live agents first (kill-first), then
  // soft-deletes + removes its worktree. If we were viewing it, fall to main.
  const onDeleteThread = useCallback(
    async (ws: WorkspaceSummary, threadId: string) => {
      try {
        await api.deleteThread(ws.workspaceId, threadId);
      } catch (e) {
        // Confirm dialog already closed, so a silent return would look like the
        // delete succeeded while the direction is still right there. Surface it.
        // eslint-disable-next-line no-console
        console.warn("delete direction failed", e);
        toast.error(
          t("chat.deleteDirectionFailed", {
            defaultValue: "删除方向失败",
          }),
          { description: e instanceof ApiError ? e.detail : (e as Error)?.message },
        );
        return;
      }
      if (activeThread?.id === threadId) {
        navigate(`/chat/${ws.id}`, { replace: true });
      }
      await refreshWorkspaces();
      refreshAgents();
    },
    [activeThread, navigate, refreshWorkspaces, refreshAgents, t],
  );

  // ── Record the active workspace for the global tool pages ───────────
  // Files / terminal / tasks / usage live outside this shell and default to
  // whatever workspace you last viewed here. Persist the UUID (not the slug).
  useEffect(() => {
    if (activeWs) setActiveWorkspaceId(activeWs.workspaceId);
  }, [activeWs]);

  // ── ⌘1-4 global shortcut ───────────────────────────────────────────
  useEffect(() => {
    if (!activeWs) return;
    const tabs = buildTabs(activeWs.id, activeThreadSlug);
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey)) return;
      if (e.target instanceof HTMLElement) {
        const tag = e.target.tagName;
        // 别和 IME / 表单组合键冲突 — 输入框里 ⌘1 仍走原生 (浏览器切 tab)。
        if (tag === "INPUT" || tag === "TEXTAREA" || e.target.isContentEditable) {
          return;
        }
      }
      const n = Number.parseInt(e.key, 10);
      if (!Number.isInteger(n) || n < 1 || n > tabs.length) return;
      e.preventDefault();
      navigate(tabs[n - 1].to);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [activeWs, activeThreadSlug, navigate]);

  // ── Redirect a stale / unknown wsId to the first workspace ──────────
  // MUST be an effect, not render-phase. Calling navigate() while rendering
  // triggers React's "Cannot update a component (BrowserRouter) while
  // rendering a different component (WorkspaceShell)" warning and is unsafe
  // under React 18 concurrent rendering. This fires when a bookmark / refresh
  // points at a workspace that was since deleted while others still exist.
  useEffect(() => {
    // Only normalize once the workspace list has loaded — otherwise the brief
    // pre-fetch window (workspaces == []) would bounce a perfectly valid wsId.
    if (activeWs || !wsId || !wsLoaded) return;
    if (workspaces.length === 0) {
      // No workspaces at all: a stale /chat/<id>[/t/<slug>] should land on the
      // Welcome screen at /chat instead of sitting on a dead address.
      navigate("/chat", { replace: true });
    } else if (!workspaces.some((w) => w.id === wsId || w.workspaceId === wsId)) {
      navigate(`/chat/${workspaces[0].id}`, { replace: true });
    }
  }, [activeWs, workspaces, wsId, wsLoaded, navigate]);

  // ── Redirect a stale / unknown :threadSlug to the workspace's main ──
  // A `/chat/:wsId/t/<typo>/…` URL otherwise silently shows main content while
  // the address bar + tabs point at a non-existent direction. Strip the
  // `/t/<slug>` segment (keep the view + query) so URL and content agree.
  useEffect(() => {
    if (
      activeWs &&
      threadSlugParam &&
      !activeWs.threads.some((th) => th.slug === threadSlugParam)
    ) {
      const fixed = location.pathname.replace(`/t/${threadSlugParam}`, "");
      navigate(fixed + location.search, { replace: true });
    }
  }, [activeWs, threadSlugParam, location.pathname, location.search, navigate]);

  // ── Render ─────────────────────────────────────────────────────────
  if (!activeWs) {
    // wsId 在 URL 但 listAgents 还没回 / 已经 evicted。渲染 sidebar +
    // "找不到工作空间" 提示；真正的跳转由上面的 useEffect 负责（render
    // 阶段不能 navigate）。
    return (
      <TooltipProvider delayDuration={300}>
        <div className="flex h-full min-h-0">
          <WorkspaceList
            workspaces={workspaces}
            activeId={wsId ?? null}
            isLoading={!wsLoaded}
            wsError={wsError}
            onOpenWizard={() => setWizardOpen(true)}
            onDelete={onDeleteWorkspace}
            onRootsChanged={refreshWorkspaces}
          />
          {!wsLoaded ? (
            <section className="flex min-w-0 flex-1 flex-col items-center justify-center gap-3 bg-surface-primary text-foreground-tertiary">
              <FolderOpen className="size-10 opacity-40" />
              <p className="font-caption text-sm">
                {t("common.loading")}
              </p>
            </section>
          ) : workspaces.length === 0 ? (
            // 完全空：展示 Welcome 屏，跟 /chat 主入口体验一致。
            <Welcome onCreateWorkspace={() => setWizardOpen(true)} />
          ) : (
            // 有别的 ws 但 URL 指的这个不存在 — 给个安静提示就行，重定向
            // 已经在 useEffect 里发车。
            <section className="flex min-w-0 flex-1 flex-col items-center justify-center gap-3 bg-surface-primary text-foreground-tertiary">
              <FolderOpen className="size-10 opacity-40" />
              <p className="font-caption text-sm">
                {t("chat.selectWorkspace")}
              </p>
            </section>
          )}
          {wizardOpen && (
            <Suspense fallback={null}>
              <CreateWizard
                open={wizardOpen}
                onClose={() => setWizardOpen(false)}
                onCreated={(ws) => {
                  refreshAgents();
                  // Await the workspace refetch BEFORE navigating — otherwise the
                  // new slug isn't in `workspaces` yet and the not-found redirect
                  // effect bounces us straight back to the previous workspace.
                  void refreshWorkspaces().then(() => {
                    if (ws) navigate(`/chat/${ws.slug}`);
                  });
                }}
              />
            </Suspense>
          )}
        </div>
      </TooltipProvider>
    );
  }

  const ctx: ShellOutletContext = {
    workspace: activeWs,
    activeThread,
    mainThread,
    threadSlug: activeThreadSlug,
    activeMembers: activeWs.members,
    threadMembers,
    handoffMissingAgents,
    allAliveAgents,
    workspaceAgentIds,
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
  };

  return (
    <TooltipProvider delayDuration={300}>
      <div className="flex h-full min-h-0">
        <WorkspaceList
          workspaces={workspaces}
          activeId={activeWs.id}
          activeThreadSlug={activeThreadSlug}
          isLoading={!wsLoaded}
          onOpenWizard={() => setWizardOpen(true)}
          onDelete={onDeleteWorkspace}
          onRootsChanged={refreshWorkspaces}
          onNewDirection={onNewDirection}
          onDeleteThread={onDeleteThread}
        />
        <section className="flex min-w-0 flex-1 flex-col bg-surface-primary">
          <WorkspaceToolbar
            workspace={activeWs}
            threadSlug={activeThreadSlug}
            agentCount={threadMembers.length}
            totalUnread={totalUnread}
            onJumpUnread={() => setJumpUnreadTick((v) => v + 1)}
            onCleanupThread={(threadId) => onDeleteThread(activeWs, threadId)}
            onOpenWorkspaceNav={() => setMobileNavOpen(true)}
          />
          {/* 「需要你」:只放 error / handoff（要人决定）。stalled 由
              useAutoNudgeStalled 自动催，不进这条栏。 */}
          <NeedsYouBar
            members={needsYouMembers}
            liveById={agentStateById}
            messages={liveMessages}
            onOpenAgent={openAgent}
          />
          <ViewTransition>
            {/* View-level boundary: a crash in one tab (malformed ledger
                markdown, ReactFlow state, …) shows a contained fallback while
                the sidebar + tab bar stay intact. Keyed by wsId+view so a
                tab switch clears a held error. */}
            <ErrorBoundary resetKey={`${activeWs.id}:${location.pathname}`}>
              <Outlet context={ctx} />
            </ErrorBoundary>
          </ViewTransition>
        </section>

        {drawerAgentId && (
          <Suspense fallback={null}>
            <AgentDrawer
              agentId={drawerAgentId}
              activities={agentActivityById[drawerAgentId] ?? []}
              onClose={closeAgent}
            />
          </Suspense>
        )}
        <Sheet open={mobileNavOpen} onOpenChange={setMobileNavOpen}>
          <SheetContent side="left" className="w-full max-w-none p-0 sm:max-w-none">
            <SheetHeader className="border-b border-border-subtle">
              <SheetTitle>{t("chat.workspaces")}</SheetTitle>
            </SheetHeader>
            <WorkspaceList
              mobile
              workspaces={workspaces}
              activeId={activeWs.id}
              activeThreadSlug={activeThreadSlug}
              isLoading={!wsLoaded}
              onOpenWizard={() => {
                setMobileNavOpen(false);
                setWizardOpen(true);
              }}
              onDelete={onDeleteWorkspace}
              onRootsChanged={refreshWorkspaces}
              onNewDirection={onNewDirection}
              onDeleteThread={onDeleteThread}
              onAfterNavigate={() => setMobileNavOpen(false)}
            />
          </SheetContent>
        </Sheet>
        {wizardOpen && (
          <Suspense fallback={null}>
            <CreateWizard
              open={wizardOpen}
              onClose={() => setWizardOpen(false)}
              onCreated={(ws) => {
                refreshAgents();
                // Await the workspace refetch BEFORE navigating — otherwise the
                // new slug isn't in `workspaces` yet and the not-found redirect
                // effect bounces us straight back to the previous workspace.
                void refreshWorkspaces().then(() => {
                  if (ws) navigate(`/chat/${ws.slug}`);
                });
              }}
            />
          </Suspense>
        )}
      </div>
    </TooltipProvider>
  );
}
