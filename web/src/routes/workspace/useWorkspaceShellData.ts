/**
 * useWorkspaceShellData — the data-orchestration layer behind WorkspaceShell.
 *
 * Extracted from Shell.tsx (which, after the earlier UI split, still carried
 * ~260 lines of state + fetching + the swarm subscription + the optimistic
 * cascade-delete all inline). Pulling it into a hook leaves Shell as a thin
 * layout/nav component and makes this logic independently reasoned-about.
 *
 * Owns: the agents / workspaces / unread state, the three refreshers, the
 * single `/ws/swarm` subscription (the only one in the app — child views read
 * derived data via Outlet context), and the derived view-models
 * (`workspaces` / `activeWs` / per-workspace unread). Navigation stays in the
 * component: `deleteWorkspace` performs the kill+delete+optimistic-drop and
 * RETURNS where to navigate (or null), so this hook has no router dependency.
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
  SwarmEvent,
  ThreadInfo,
  Workspace,
} from "../../api/types";
import { useSwarmFeed } from "../../hooks/useSwarmFeed";
import { accentToCssVar, splitWorkspacePath } from "../../lib/workspace";
import { agentInThread, mainThreadOf } from "../../lib/thread";
import { dlog } from "../../lib/debugLog";
import { countsAsUserUnread } from "../../lib/unread";
import type { ReasoningSummary } from "../../components/MessagesPanel";
import type { WorkspaceSummary } from "./types";

/** Cap on per-agent activity history kept in memory for the drawer Activity
 *  tab. Bounded so a long-running worker can't grow this unboundedly. */
const MAX_ACTIVITY = 100;

export interface LiveRead {
  ids: number[];
  to_agent: string;
  at: number;
}

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
  reasoningById: Record<string, ReasoningSummary>;
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
  // Lossless append-only buffer of live WS messages (bounded). Consumers merge
  // by id — never a single `liveMessage` slot that batched arrivals overwrite.
  const [liveMessages, setLiveMessages] = useState<MessageRecord[]>([]);
  const [liveRead, setLiveRead] = useState<LiveRead | null>(null);
  const [agentStateById, setAgentStateById] = useState<
    Record<string, AgentLiveState>
  >({});
  const [agentActivityById, setAgentActivityById] = useState<
    Record<string, AgentActivity[]>
  >({});
  // Live, in-flight reasoning steps keyed by AGENT id (an agent has at most one
  // active trace at a time), fed by `thought_trace_event` so the "正在响应"
  // bubble grows its step list during the turn. Keyed by agent — NOT by trigger
  // message id — because the pending bubble's trigger can be a later system
  // "wake" message while the trace is keyed to the user message; agent id is the
  // stable join. Cleared when that agent's reply lands.
  const [reasoningById, setReasoningById] = useState<
    Record<string, ReasoningSummary>
  >({});
  const [unreadByFrom, setUnreadByFrom] = useState<Record<string, number>>({});
  // Latest cold-start stage per agent (`agent_stage` events): drives the
  // narrative stage bar on pending agent cards (see Chat.tsx). Cleared when
  // the agent's first activity lands (first turn started) or the agent exits.
  const [agentStageById, setAgentStageById] = useState<
    Record<string, { stage: string; at: number }>
  >({});
  const idToFromRef = useRef<Map<number, string>>(new Map());
  // The set of message ids we actually counted toward the user's unread badge
  // (agent→user, countsAsUserUnread). Decrement-on-read must only subtract ids
  // that were counted — otherwise an AGENT reading its own inbox (agent↔agent
  // traffic flowing through the same message_read event) would zero out the
  // user's real unread for that sender, making the badge lie "0 unread".
  const countedUnreadRef = useRef<Set<number>>(new Set());

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
      const counts: Record<string, number> = {};
      const ids = new Map<number, string>();
      const counted = new Set<number>();
      for (const m of rows) {
        ids.set(m.id, m.from_agent);
        // `to_agent === "user"` is REQUIRED here: listMessages returns the whole
        // recent stream including agent↔agent traffic. Without this gate the
        // full recompute counts coordination replies as the user's unread —
        // over-reporting, and disagreeing with the live increment (which already
        // gates on to_agent), so the badge flickers between two wrong values.
        if (
          m.to_agent === "user" &&
          m.read_at === null &&
          countsAsUserUnread(m.from_agent, m.kind, m.meta)
        ) {
          counts[m.from_agent] = (counts[m.from_agent] ?? 0) + 1;
          counted.add(m.id);
        }
      }
      idToFromRef.current = ids;
      countedUnreadRef.current = counted;
      if (mountedRef.current) setUnreadByFrom(counts);
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

  useSwarmFeed({
    onEvent: (ev: SwarmEvent) => {
      switch (ev.type) {
        case "agent_state":
          // Patch the agent's live state slice for an immediate visual update
          // (no listAgents roundtrip). We STILL scheduleRefresh because a state
          // transition often coincides with a roster change (new spawn /
          // killed_at) that only the REST row reflects — the patch covers the
          // member dot, the refresh covers membership.
          setAgentStateById((prev) => {
            const cur = prev[ev.agent_id];
            if (cur?.state === ev.state) return prev; // no-op → stable ref
            return { ...prev, [ev.agent_id]: { ...cur, state: ev.state } };
          });
          // Terminal states end the cold-start story either way (error card
          // replaces the stage bar; exit removes the agent).
          if (ev.state === "error" || ev.state === "exited") {
            setAgentStageById((prev) => {
              if (!(ev.agent_id in prev)) return prev;
              const next = { ...prev };
              delete next[ev.agent_id];
              return next;
            });
          }
          scheduleRefresh();
          break;
        case "agent_activity":
          // Pure step-level stream — never touches the agent roster, so no
          // refresh. Replace ONLY this agent's slice so unrelated member rows
          // keep their object identity and don't re-render.
          setAgentStateById((prev) => ({
            ...prev,
            [ev.agent_id]: {
              ...prev[ev.agent_id],
              activity: {
                agent_id: ev.agent_id,
                kind: ev.kind,
                label: ev.label,
                phase: ev.phase,
                seq: ev.seq,
                duration_ms: ev.duration_ms,
                at: ev.at,
              },
            },
          }));
          // First activity = the first turn started → the cold-start stage
          // bar has done its job; drop the stage so it never reappears.
          setAgentStageById((prev) => {
            if (!(ev.agent_id in prev)) return prev;
            const next = { ...prev };
            delete next[ev.agent_id];
            return next;
          });
          // Persistent stream for the drawer's Activity tab — append, with
          // same-seq (running → ok/error) replaced in place, bounded to the
          // last MAX_ACTIVITY. Survives close/reopen since it lives here, not
          // in the (ephemeral) tab component.
          setAgentActivityById((prev) => {
            const cur = prev[ev.agent_id] ?? [];
            const act: AgentActivity = {
              agent_id: ev.agent_id,
              kind: ev.kind,
              label: ev.label,
              phase: ev.phase,
              seq: ev.seq,
              duration_ms: ev.duration_ms,
              at: ev.at,
            };
            const idx = cur.findIndex((s) => s.seq === act.seq);
            let next: AgentActivity[];
            if (idx >= 0) {
              next = cur.slice();
              next[idx] = act;
            } else {
              next = cur.length >= MAX_ACTIVITY ? cur.slice(1) : cur.slice();
              next.push(act);
            }
            return { ...prev, [ev.agent_id]: next };
          });
          break;
        case "message": {
          const rec: MessageRecord = {
            id: ev.id,
            from_agent: ev.from_agent,
            to_agent: ev.to_agent,
            kind: ev.kind,
            body: ev.body,
            sent_at: ev.sent_at,
            delivered_at: null,
            read_at: null,
            in_reply_to: ev.in_reply_to ?? null,
            thread_id: ev.thread_id ?? null,
            meta: ev.meta ?? null,
            thought_trace: ev.thought_trace ?? null,
          };
          dlog("ws.message", {
            id: ev.id,
            from: ev.from_agent,
            to: ev.to_agent,
            kind: ev.kind,
            thread: ev.thread_id ?? null,
          });
          // LOSSLESS relay: APPEND via a functional updater, never overwrite a
          // single slot. When several messages land in one React batch (a burst
          // of replies, a worker reporting, rapid sends), a `setLiveMessage(rec)`
          // single-value setter keeps only the LAST — the rest never reach the
          // chat's append effect and vanish from the UI until a refresh. A
          // functional append is applied once per call even when batched, so all
          // N survive. Bounded so a long session can't grow it unbounded; the
          // consumer drains it into `items` after each commit, so the cap never
          // drops an unconsumed message in any realistic burst.
          setLiveMessages((prev) => {
            const next = [...prev, rec];
            return next.length > 200 ? next.slice(-200) : next;
          });
          idToFromRef.current.set(ev.id, ev.from_agent);
          // F4: this agent's reply landed — the persisted thought_trace on the
          // message takes over, so drop its in-flight live reasoning (keyed by
          // agent) to avoid carrying stale steps into its next turn.
          if (ev.from_agent !== "user" && ev.to_agent === "user") {
            const replier = ev.from_agent;
            setReasoningById((prev) => {
              if (!(replier in prev)) return prev;
              const next = { ...prev };
              delete next[replier];
              return next;
            });
          }
          if (ev.to_agent === "user" && countsAsUserUnread(ev.from_agent, ev.kind, ev.meta)) {
            countedUnreadRef.current.add(ev.id);
            setUnreadByFrom((prev) => ({
              ...prev,
              [ev.from_agent]: (prev[ev.from_agent] ?? 0) + 1,
            }));
          }
          break;
        }
        case "message_read": {
          setLiveRead({ ids: ev.ids, to_agent: ev.to_agent, at: ev.at });
          // Resolve which counted ids to drop HERE — outside the state updater.
          // A `setState` updater must be pure: React StrictMode (dev) and
          // concurrent rendering can invoke it more than once, and mutating
          // `countedUnreadRef` inside it poisons the replay — the first
          // (discarded) pass deletes the id, the second (kept) pass sees it gone
          // and skips the decrement, so the badge never goes down and lies at
          // the cumulative arrival count (live-observed: stuck at "3 未读" with 0
          // actually unread, only a reload reconciled it). Mutate the ref once
          // here, then apply a pure, idempotent updater.
          const decByFrom: Record<string, number> = {};
          for (const id of ev.ids) {
            // Only subtract ids we actually counted as USER unread. Skips
            // agent↔agent reads and non-counted (wake/system/completion) ids,
            // so an agent reading its mailbox can't deflate the user's badge.
            if (!countedUnreadRef.current.has(id)) continue;
            countedUnreadRef.current.delete(id);
            const from = idToFromRef.current.get(id);
            if (!from) continue;
            decByFrom[from] = (decByFrom[from] ?? 0) + 1;
          }
          if (Object.keys(decByFrom).length > 0) {
            setUnreadByFrom((prev) => {
              const next = { ...prev };
              for (const [from, dec] of Object.entries(decByFrom)) {
                const cur = (next[from] ?? 0) - dec;
                if (cur <= 0) delete next[from];
                else next[from] = cur;
              }
              return next;
            });
          }
          break;
        }
        case "blackboard_changed":
          // workspace name / accent now live in the `workspaces` table,
          // not the blackboard, so we don't react to blackboard events
          // for that any more. Member-count changes are picked up via
          // `agent_state` → scheduleRefresh → refreshAgents → recompute.
          break;
        case "thread_changed":
          // A direction was created / renamed / isolated / deleted server-side
          // (e.g. the orchestrator's swarm_name_thread → background worktree
          // isolation). Threads live in the workspaces snapshot, which no other
          // live event refetches — pull it so the sidebar's direction tree
          // reflects the new name + branch icon without a manual reload.
          refreshWorkspaces();
          break;
        case "thought_trace_event": {
          // Live, real steps appended to an in-flight trace — grow the pending
          // bubble's step list mid-turn. Full snapshot, keyed by the trace's
          // agent. (No synthesized steps: the backend only emits this for real,
          // captured tool steps.)
          const steps = ev.steps.map((s) => s.label).filter(Boolean);
          setReasoningById((prev) => ({
            ...prev,
            [ev.agent_id]: { steps, durationMs: null },
          }));
          break;
        }
        case "agent_stage":
          // Cold-start heartbeat (shim_ready → mcp_ready → bootstrap_injected).
          // Just record the latest stage; the stage bar clears on the agent's
          // first activity (see the agent_activity case) or terminal state.
          setAgentStageById((prev) => ({
            ...prev,
            [ev.agent_id]: { stage: ev.stage, at: ev.at },
          }));
          break;
      }
    },
    onReconnect: () => {
      scheduleRefresh();
      recomputeUnread();
      refreshWorkspaces();
    },
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
      Object.entries(unreadByFrom).filter(([from]) => threadSet.has(from)),
    );
  }, [unreadByFrom, activeWs, threadAgentIds]);
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
    liveMessages,
    liveRead,
    agentStateById,
    agentActivityById,
    agentStageById,
    reasoningById,
    activeWorkspaceUnread,
    totalUnread,
    refreshAgents,
    refreshWorkspaces,
    wsLoaded,
    wsError,
    deleteWorkspace,
  };
}
