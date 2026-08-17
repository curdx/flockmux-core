/**
 * useWorkspaceShellData — REST roster/workspaces + derived view-models.
 *
 * Live `/ws/swarm` reduction lives in SwarmProjection. This hook reads that
 * snapshot and still owns the REST snapshots (listAgents / listWorkspaces)
 * plus derived workspace/thread view-models. Child views should also read
 * the projection (or generation counters) instead of opening another
 * event-switch.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "@/lib/toast";
import { api } from "../../api/http";
import type {
  AgentActivity,
  AgentInfo,
  AgentLiveState,
  MessageRecord,
  ThreadInfo,
  Workspace,
} from "../../api/types";
import { useSwarmRefresh, useSwarmSnapshot } from "../../hooks/useSwarmProjection";
import { type LiveRead } from "../../lib/swarmProjection";
import { hydrateSwarmUnread } from "../../lib/swarmProjectionStore";
import { accentToCssVar, splitWorkspacePath } from "../../lib/workspace";
import { agentInThread, mainThreadOf } from "../../lib/thread";
import type { WorkspaceSummary } from "./types";

export type { LiveRead };

export interface WorkspaceShellData {
  agents: AgentInfo[];
  workspaces: WorkspaceSummary[];
  activeWs: WorkspaceSummary | null;
  /** The active direction (thread) resolved from the URL `:threadSlug` param,
   *  defaulting to the workspace's main thread. `null` only for a legacy/empty
   *  workspace with no thread rows. */
  activeThread: ThreadInfo | null;
  /** The workspace's main direction (slug `main`, else oldest). `null` only for
   *  a legacy/empty workspace. Views use it to fold `thread_id == null` agents
   *  into main when scoping by direction. */
  mainThread: ThreadInfo | null;
  /** Resolved slug of the active direction — `"main"` when none/unresolved.
   *  Used to scope blackboard keys `{workspace_id}/{threadSlug}/…`. */
  activeThreadSlug: string;
  allAliveAgents: AgentInfo[];
  workspaceAgentIds: string[];
  /** Historical id set (alive + killed) of agents in the ACTIVE direction.
   *  MessagesPanel filters by it so each direction is a self-contained room.
   *  For the main direction, `thread_id == null` agents fold in. */
  threadAgentIds: string[];
  /** Alive agents in the active direction (subset of `activeWs.members`). */
  threadMembers: AgentInfo[];
  /** Active-direction agents that exited without delivering their declared
   *  handoff (`handoff_missing` 或 `handoff_failed`). Empty in the healthy case. */
  handoffMissingAgents: AgentInfo[];
  /** 「需要我」收件箱(NeedsYouBar)的输入:活着的成员 + 本工作空间已退出但
   *  未交付 handoff 的 agent(`handoff_missing` / `handoff_failed`)。后者不在
   *  members 里 —— 不补上,「worker 没交付就死了」永远到不了 deriveNeedsYou。
   *  stalled 不用单独补:server 只对活着的 agent 置位,已在 members 里。 */
  needsYouMembers: AgentInfo[];
  liveMessages: MessageRecord[];
  liveRead: LiveRead | null;
  /** Per-agent live state + latest activity, accumulated incrementally from
   *  the swarm WS (NOT from REST — `AgentInfo` carries no state/activity).
   *  Keyed by agent_id; each slice is replaced independently so a member row
   *  only re-renders when its own agent's event lands. Falls back to
   *  `inferAgentStatus` downstream when an agent has no slice yet. */
  agentStateById: Record<string, AgentLiveState>;
  /** Per-agent bounded activity stream, accumulated from the swarm WS so the
   *  drawer's Activity tab survives close/reopen/remount (NOT ephemeral). */
  agentActivityById: Record<string, AgentActivity[]>;
  /** Latest cold-start stage per agent (shim_ready → mcp_ready →
   *  bootstrap_injected), from `agent_stage` events. Drives the pending-card
   *  stage bar; cleared on first activity or terminal state. */
  agentStageById: Record<string, { stage: string; at: number }>;
  /** Live in-flight reasoning steps keyed by agent id, fed by
   *  `thought_trace_event` so the pending bubble grows its steps mid-turn. */
  reasoningById: Record<string, { steps: string[]; durationMs: number | null }>;
  /** Unread tally already filtered to the active workspace's senders. */
  activeWorkspaceUnread: Record<string, number>;
  totalUnread: number;
  refreshAgents: () => void;
  refreshWorkspaces: () => Promise<void>;
  /** True once the first listWorkspaces has resolved — distinguishes "still
   *  loading" from "loaded, genuinely zero workspaces", so a stale URL can be
   *  normalized to /chat without bouncing a valid wsId mid-load. */
  wsLoaded: boolean;
  /** True when the last listWorkspaces failed (backend unreachable) — lets the
   *  sidebar show "连不上后端" instead of the fake "还没有工作空间". */
  wsError: boolean;
  /** Kill the workspace's live agents, soft-delete it, optimistically drop it
   *  from local state. Returns a path to navigate to when the ACTIVE workspace
   *  was deleted (`/chat/<next>` or `/chat`), else `null` (no nav needed). */
  deleteWorkspace: (workspaceId: string) => Promise<string | null>;
}


export function useWorkspaceShellData(
  wsId: string | undefined,
  threadSlug: string | undefined,
): WorkspaceShellData {
  const { t } = useTranslation();
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [workspaceRows, setWorkspaceRows] = useState<Workspace[]>([]);
  const [wsLoaded, setWsLoaded] = useState(false);
  // True when the last listWorkspaces FAILED (backend unreachable). The sidebar's
  // empty state is `workspaceRows.length === 0`, which a failed load also
  // produces — without this flag the sidebar lies "还没有工作空间" when the real
  // reason is the backend is down (P0-5 regression).
  const [wsError, setWsError] = useState(false);
  const snap = useSwarmSnapshot();

  // F19: drop async results that resolve after the Shell unmounts.
  const mountedRef = useRef(true);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const refreshWorkspaces = useCallback(async () => {
    try {
      const items = await api.listWorkspaces();
      if (mountedRef.current) {
        setWorkspaceRows(items);
        setWsError(false);
      }
    } catch (err) {
      // eslint-disable-next-line no-console
      console.warn("listWorkspaces failed", err);
      // Flag the error so the sidebar says "连不上后端" instead of the fake
      // "还没有工作空间" — a failed load must not look like an empty account.
      if (mountedRef.current) setWsError(true);
    } finally {
      if (mountedRef.current) setWsLoaded(true);
    }
  }, []);

  const refreshAgents = useCallback(async () => {
    try {
      const items = await api.listAgents();
      if (mountedRef.current) setAgents(items);
    } catch (err) {
      // eslint-disable-next-line no-console
      console.warn("listAgents failed", err);
    }
  }, []);

  const recomputeUnread = useCallback(async () => {
    try {
      const rows = await api.listMessages({ limit: 200 });
      hydrateSwarmUnread(rows);
    } catch {
      /* best-effort */
    }
  }, []);

  useEffect(() => {
    refreshAgents();
    recomputeUnread();
    refreshWorkspaces();
  }, [refreshAgents, recomputeUnread, refreshWorkspaces]);

  // Self-heal the "连不上后端" banner. `wsError` is set by a SINGLE failed
  // listWorkspaces — which can be a mere transient: the webview was App-Napped
  // (window hidden to tray), the backend was briefly busy mid-spawn, a fetch
  // raced a reconnect. Without a retry the banner sticks until a thread_changed
  // event or remount, so a 5-second blip reads as "the backend is down" for
  // minutes. While in the error state, re-poll until the backend answers (each
  // failure is an instant loopback connection-refused, so the poll is cheap),
  // and also re-check the moment the window/tab becomes visible again.
  useEffect(() => {
    if (!wsError) return;
    const id = window.setInterval(() => {
      void refreshWorkspaces();
    }, 3000);
    const onVisible = () => {
      if (document.visibilityState === "visible") void refreshWorkspaces();
    };
    document.addEventListener("visibilitychange", onVisible);
    window.addEventListener("focus", onVisible);
    return () => {
      window.clearInterval(id);
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("focus", onVisible);
    };
  }, [wsError, refreshWorkspaces]);

  const refreshTimerRef = useRef<number | null>(null);
  const scheduleRefresh = useCallback(() => {
    if (refreshTimerRef.current != null) {
      window.clearTimeout(refreshTimerRef.current);
    }
    refreshTimerRef.current = window.setTimeout(() => {
      refreshTimerRef.current = null;
      refreshAgents();
    }, 200);
  }, [refreshAgents]);

  // Live agent_state also patches agentStateById immediately. REST still
  // needed: spawn / killed_at only exist on the roster row.
  useSwarmRefresh((s) => s.rosterGen, scheduleRefresh);
  useSwarmRefresh((s) => s.threadGen, refreshWorkspaces);
  useSwarmRefresh((s) => s.reconnectGen, () => {
    scheduleRefresh();
    void recomputeUnread();
    void refreshWorkspaces();
  });

  // ── Workspaces (server-side, alive only) ────────────────────────────
  // Source of truth: GET /api/workspaces (deleted_at IS NULL only).
  // Agents are grouped onto these via `agent.workspace_id`.
  const workspaces = useMemo<WorkspaceSummary[]>(() => {
    const aliveByWsId = new Map<string, AgentInfo[]>();
    for (const a of agents) {
      if (a.killed_at != null || a.shim_exit != null) continue;
      if (!a.workspace_id) continue;
      const arr = aliveByWsId.get(a.workspace_id) ?? [];
      arr.push(a);
      aliveByWsId.set(a.workspace_id, arr);
    }
    return workspaceRows.map<WorkspaceSummary>((w) => {
      // Use the cwd's basename (the actual project folder) for the caption, not
      // its parent dir — `/tmp` told the user nothing (F2).
      const { name: folder } = splitWorkspacePath(w.cwd);
      return {
        id: w.slug,
        workspaceId: w.id,
        path: w.cwd,
        cwdBranch: w.cwd_branch ?? null,
        name: w.name,
        folder,
        accentColor: accentToCssVar(w.accent),
        members: aliveByWsId.get(w.id) ?? [],
        roots: w.roots ?? [],
        threads: w.threads ?? [],
      };
    });
  }, [workspaceRows, agents]);

  const activeWs = useMemo(
    // A workspace has two identifiers: the slug (`w.id`, used in URLs) and the
    // uuid (`w.workspaceId`, used in API/FK). Resolve by either so a deep link
    // carrying the uuid renders instead of being treated as unknown.
    () => workspaces.find((w) => w.id === wsId || w.workspaceId === wsId) ?? null,
    [workspaces, wsId],
  );

  const allAliveAgents = useMemo(
    () => agents.filter((a) => a.killed_at == null && a.shim_exit == null),
    [agents],
  );

  const workspaceAgentIds = useMemo(() => {
    if (!activeWs) return [];
    return agents
      .filter((a) => a.workspace_id === activeWs.workspaceId)
      .map((a) => a.agent_id);
  }, [agents, activeWs]);

  // ── Active direction (thread) resolution ────────────────────────────
  // Default to the main thread (slug "main", else the oldest row). `null`
  // only for a legacy/empty workspace with no thread rows — callers then fall
  // back to plain workspace-wide scoping (single implicit direction).
  const mainThread = useMemo<ThreadInfo | null>(
    () => (activeWs ? mainThreadOf(activeWs.threads) : null),
    [activeWs],
  );

  const activeThread = useMemo<ThreadInfo | null>(() => {
    if (!activeWs || activeWs.threads.length === 0) return null;
    if (threadSlug) {
      return activeWs.threads.find((th) => th.slug === threadSlug) ?? mainThread;
    }
    return mainThread;
  }, [activeWs, threadSlug, mainThread]);

  const activeThreadSlug = activeThread?.slug ?? "main";

  const agentInActiveThread = useCallback(
    (a: AgentInfo): boolean =>
      !!activeWs && agentInThread(a, activeWs.workspaceId, activeThread, mainThread),
    [activeWs, activeThread, mainThread],
  );

  const threadAgentIds = useMemo(
    () => (activeWs ? agents.filter(agentInActiveThread).map((a) => a.agent_id) : []),
    [agents, activeWs, agentInActiveThread],
  );

  const threadMembers = useMemo(
    () => (activeWs ? activeWs.members.filter(agentInActiveThread) : []),
    [activeWs, agentInActiveThread],
  );

  // Agents in THIS direction that exited without delivering their declared
  // handoff. Server 两种旗都算「没交」:
  //   - handoff_missing — silent(无 `.error`)
  //   - handoff_failed  — 写了 `<key>.error`(kill/崩溃常见路径)
  // 只认 missing 会漏掉真实用户杀进程(UX-016)。已退出 agent 不在
  // activeWs.members,必须从全量 agents 捞。
  const handoffMissingAgents = useMemo(
    () =>
      activeWs
        ? agents.filter(
            (a) =>
              (a.handoff_missing || a.handoff_failed) && agentInActiveThread(a),
          )
        : [],
    [agents, activeWs, agentInActiveThread],
  );

  // 「需要我」收件箱的输入 = 活着的成员 + 本工作空间已退出但未交付 handoff 的
  // agent(missing 或 failed)。不并进输入,「worker 没交付就死了」永远出不来。
  const needsYouMembers = useMemo(() => {
    if (!activeWs) return [];
    const undelivered = agents.filter(
      (a) =>
        (a.handoff_missing || a.handoff_failed) &&
        a.workspace_id === activeWs.workspaceId,
    );
    if (undelivered.length === 0) return activeWs.members;
    const seen = new Set(activeWs.members.map((a) => a.agent_id));
    const extra = undelivered.filter((a) => !seen.has(a.agent_id));
    return [...activeWs.members, ...extra];
  }, [agents, activeWs]);

  // Unread is scoped to the ACTIVE direction (not the whole workspace) so the
  // toolbar badge + per-member counts match the room the user is looking at —
  // a sibling direction's unread doesn't leak into this view. (For a main-only
  // workspace threadAgentIds == workspaceAgentIds, so counts are unchanged.)
  const activeWorkspaceUnread = useMemo(() => {
    if (!activeWs) return {} as Record<string, number>;
    const threadSet = new Set(threadAgentIds);
    return Object.fromEntries(
      Object.entries(snap.unreadByFrom).filter(([from]) => threadSet.has(from)),
    );
  }, [snap.unreadByFrom, activeWs, threadAgentIds]);
  const totalUnread = Object.values(activeWorkspaceUnread).reduce((a, b) => a + b, 0);

  const deleteWorkspace = useCallback(
    async (workspaceId: string): Promise<string | null> => {
      // Kill any live agents belonging to this workspace before deleting the
      // row, otherwise their PTYs survive and keep burning tokens with no UI
      // handle. Per-agent failure is logged but doesn't abort the batch.
      try {
        const all = await api.listAgents();
        const live = all.filter(
          (a) =>
            a.workspace_id === workspaceId &&
            a.killed_at == null &&
            a.shim_exit == null,
        );
        await Promise.all(
          live.map((a) =>
            api.killAgent(a.agent_id).catch((e) => {
              // eslint-disable-next-line no-console
              console.warn("killAgent failed", a.agent_id, e);
            }),
          ),
        );
      } catch (err) {
        // eslint-disable-next-line no-console
        console.warn("listAgents before delete failed", err);
      }
      try {
        await api.deleteWorkspace(workspaceId);
      } catch (err) {
        // 「失败即安全」：后端没删成功 = 数据没丢,我们也没乐观删除列表。
        // 但这个 null 与下面「删的不是当前空间(成功,无需导航)」共用同一返回值,
        // 调用方无法区分 —— 所以失败必须在这里就地 toast,别让它被当成静默成功。
        toast.error(
          t("chat.deleteWorkspaceFailed", {
            defaultValue: "删除工作空间失败",
          }),
          { description: (err as Error)?.message },
        );
        return null;
      }
      // Optimistically drop it locally — the next listWorkspaces refresh would
      // catch it anyway but the UI shouldn't lag a roundtrip.
      const remaining = workspaceRows.filter((w) => w.id !== workspaceId);
      if (mountedRef.current) setWorkspaceRows(remaining);
      // Tell the caller where to navigate if the ACTIVE workspace went away.
      if (activeWs?.workspaceId === workspaceId) {
        const next = remaining[0];
        return next ? `/chat/${next.slug}` : "/chat";
      }
      return null;
    },
    [workspaceRows, activeWs, t],
  );

  return {
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
    liveMessages: snap.liveMessages,
    liveRead: snap.liveRead,
    agentStateById: snap.agentStateById,
    agentActivityById: snap.agentActivityById,
    agentStageById: snap.agentStageById,
    reasoningById: snap.reasoningById,
    activeWorkspaceUnread,
    totalUnread,
    refreshAgents,
    refreshWorkspaces,
    wsLoaded,
    wsError,
    deleteWorkspace,
  };
}
