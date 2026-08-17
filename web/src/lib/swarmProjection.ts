/**
 * SwarmProjection — one in-memory reduction of `/ws/swarm`.
 *
 * Pure reducer + snapshot types. The immortal pump that feeds this lives
 * in `swarmProjectionStore`. Views read the store (or a generation
 * counter). They do not each re-reduce `SwarmEvent`. REST snapshots
 * (agent roster, workspaces) stay REST.
 */

import type {
  AgentActivity,
  AgentLiveState,
  MessageRecord,
  SwarmEvent,
} from "../api/types";
import { countsAsUserUnread } from "./unread";

export const MAX_LIVE_MESSAGES = 200;
export const MAX_ACTIVITY = 100;
export const MAX_BB_CHANGES = 50;

export interface LiveRead {
  ids: number[];
  to_agent: string;
  at: number;
}

export interface LiveBbChange {
  id: number;
  path: string;
  agent_id: string | null;
  op: string;
}

export interface LiveReasoning {
  steps: string[];
  durationMs: number | null;
}

export interface SwarmSnapshot {
  liveMessages: MessageRecord[];
  liveRead: LiveRead | null;
  bbChanges: LiveBbChange[];
  agentStateById: Record<string, AgentLiveState>;
  agentActivityById: Record<string, AgentActivity[]>;
  agentStageById: Record<string, { stage: string; at: number }>;
  reasoningById: Record<string, LiveReasoning>;
  unreadByFrom: Record<string, number>;
  /** Bumps when live agent state actually changes (spawn / lifecycle). */
  rosterGen: number;
  /** Bumps on every `blackboard_changed`. */
  bbGen: number;
  /** Bumps on every `thread_changed`. */
  threadGen: number;
  /** Bumps on every `budget_changed` (brake tripped/lifted). */
  budgetGen: number;
  /** Bumps when an agent exits (recordings tab). */
  recordingsGen: number;
  /** Bumps on socket (re)open. */
  reconnectGen: number;
}

export const emptySwarmSnapshot: SwarmSnapshot = {
  liveMessages: [],
  liveRead: null,
  bbChanges: [],
  agentStateById: {},
  agentActivityById: {},
  agentStageById: {},
  reasoningById: {},
  unreadByFrom: {},
  rosterGen: 0,
  bbGen: 0,
  threadGen: 0,
  budgetGen: 0,
  recordingsGen: 0,
  reconnectGen: 0,
};

export interface UnreadCtx {
  idToFrom: Map<number, string>;
  countedUnread: Set<number>;
}

export function emptyUnreadCtx(): UnreadCtx {
  return { idToFrom: new Map(), countedUnread: new Set() };
}

function messageRecordFromEvent(
  ev: Extract<SwarmEvent, { type: "message" }>,
): MessageRecord {
  return {
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
}

function dropStage(
  stages: SwarmSnapshot["agentStageById"],
  agentId: string,
): SwarmSnapshot["agentStageById"] {
  if (!(agentId in stages)) return stages;
  const next = { ...stages };
  delete next[agentId];
  return next;
}

/** REST snapshot of unread for the rows returned. Does not consult live ctx. */
export function unreadFromMessages(rows: MessageRecord[]): {
  unreadByFrom: Record<string, number>;
  ctx: UnreadCtx;
} {
  const unreadByFrom: Record<string, number> = {};
  const ctx = emptyUnreadCtx();
  for (const m of rows) {
    ctx.idToFrom.set(m.id, m.from_agent);
    if (
      m.to_agent === "user" &&
      m.read_at === null &&
      countsAsUserUnread(m.from_agent, m.kind, m.meta)
    ) {
      unreadByFrom[m.from_agent] = (unreadByFrom[m.from_agent] ?? 0) + 1;
      ctx.countedUnread.add(m.id);
    }
  }
  return { unreadByFrom, ctx };
}

/**
 * REST is truth for ids it returned (so a missed `message_read` during
 * disconnect is corrected). Live-only ids not in the REST window are kept
 * so a stale last-N fetch cannot wipe a message the socket already counted.
 */
export function hydrateUnread(
  rows: MessageRecord[],
  prevCtx: UnreadCtx,
): {
  unreadByFrom: Record<string, number>;
  ctx: UnreadCtx;
} {
  const rest = unreadFromMessages(rows);
  const restIds = new Set(rows.map((m) => m.id));
  const unreadByFrom = { ...rest.unreadByFrom };
  const ctx: UnreadCtx = {
    idToFrom: new Map(rest.ctx.idToFrom),
    countedUnread: new Set(rest.ctx.countedUnread),
  };
  for (const id of prevCtx.countedUnread) {
    if (restIds.has(id)) continue;
    const from = prevCtx.idToFrom.get(id);
    if (!from) continue;
    ctx.countedUnread.add(id);
    ctx.idToFrom.set(id, from);
    unreadByFrom[from] = (unreadByFrom[from] ?? 0) + 1;
  }
  return { unreadByFrom, ctx };
}

/**
 * Pure event reduction. Mutates `ctx` maps (same contract as the old refs:
 * they are the projection's private index, not React state). Returns a new
 * snapshot object; unchanged slices keep their previous references.
 */
export function applySwarmEvent(
  prev: SwarmSnapshot,
  ev: SwarmEvent,
  ctx: UnreadCtx,
): SwarmSnapshot {
  switch (ev.type) {
    case "agent_state": {
      const cur = prev.agentStateById[ev.agent_id];
      const agentStateById =
        cur?.state === ev.state
          ? prev.agentStateById
          : { ...prev.agentStateById, [ev.agent_id]: { ...cur, state: ev.state } };
      const terminal = ev.state === "error" || ev.state === "exited";
      const agentStageById = terminal
        ? dropStage(prev.agentStageById, ev.agent_id)
        : prev.agentStageById;
      const recordingsGen =
        ev.state === "exited" && cur?.state !== "exited"
          ? prev.recordingsGen + 1
          : prev.recordingsGen;
      if (
        agentStateById === prev.agentStateById &&
        agentStageById === prev.agentStageById &&
        recordingsGen === prev.recordingsGen
      ) {
        return prev;
      }
      return {
        ...prev,
        agentStateById,
        agentStageById,
        recordingsGen,
        rosterGen: prev.rosterGen + 1,
      };
    }
    case "agent_activity": {
      const activity: AgentActivity = {
        agent_id: ev.agent_id,
        kind: ev.kind,
        label: ev.label,
        phase: ev.phase,
        seq: ev.seq,
        duration_ms: ev.duration_ms,
        at: ev.at,
      };
      const agentStateById = {
        ...prev.agentStateById,
        [ev.agent_id]: { ...prev.agentStateById[ev.agent_id], activity },
      };
      const agentStageById = dropStage(prev.agentStageById, ev.agent_id);
      const cur = prev.agentActivityById[ev.agent_id] ?? [];
      const idx = cur.findIndex((s) => s.seq === activity.seq);
      let nextActs: AgentActivity[];
      if (idx >= 0) {
        nextActs = cur.slice();
        nextActs[idx] = activity;
      } else {
        nextActs = cur.length >= MAX_ACTIVITY ? cur.slice(1) : cur.slice();
        nextActs.push(activity);
      }
      return {
        ...prev,
        agentStateById,
        agentStageById,
        agentActivityById: { ...prev.agentActivityById, [ev.agent_id]: nextActs },
      };
    }
    case "message": {
      if (ctx.idToFrom.has(ev.id)) {
        return prev;
      }
      const rec = messageRecordFromEvent(ev);
      const dropped =
        prev.liveMessages.length >= MAX_LIVE_MESSAGES
          ? prev.liveMessages[0]
          : undefined;
      const liveMessages =
        dropped === undefined
          ? [...prev.liveMessages, rec]
          : [...prev.liveMessages.slice(1), rec];
      if (dropped && !ctx.countedUnread.has(dropped.id)) {
        ctx.idToFrom.delete(dropped.id);
      }
      ctx.idToFrom.set(ev.id, ev.from_agent);
      let reasoningById = prev.reasoningById;
      if (ev.from_agent !== "user" && ev.to_agent === "user") {
        if (ev.from_agent in reasoningById) {
          reasoningById = { ...reasoningById };
          delete reasoningById[ev.from_agent];
        }
      }
      let unreadByFrom = prev.unreadByFrom;
      if (
        ev.to_agent === "user" &&
        countsAsUserUnread(ev.from_agent, ev.kind, ev.meta) &&
        !ctx.countedUnread.has(ev.id)
      ) {
        ctx.countedUnread.add(ev.id);
        unreadByFrom = {
          ...unreadByFrom,
          [ev.from_agent]: (unreadByFrom[ev.from_agent] ?? 0) + 1,
        };
      }
      return { ...prev, liveMessages, reasoningById, unreadByFrom };
    }
    case "message_read": {
      const liveRead: LiveRead = {
        ids: ev.ids,
        to_agent: ev.to_agent,
        at: ev.at,
      };
      const decByFrom: Record<string, number> = {};
      for (const id of ev.ids) {
        if (!ctx.countedUnread.has(id)) continue;
        ctx.countedUnread.delete(id);
        const from = ctx.idToFrom.get(id);
        if (!from) continue;
        decByFrom[from] = (decByFrom[from] ?? 0) + 1;
      }
      if (Object.keys(decByFrom).length === 0) {
        return { ...prev, liveRead };
      }
      const unreadByFrom = { ...prev.unreadByFrom };
      for (const [from, dec] of Object.entries(decByFrom)) {
        const cur = (unreadByFrom[from] ?? 0) - dec;
        if (cur <= 0) delete unreadByFrom[from];
        else unreadByFrom[from] = cur;
      }
      return { ...prev, liveRead, unreadByFrom };
    }
    case "blackboard_changed": {
      const change: LiveBbChange = {
        id: ev.id,
        path: ev.path,
        agent_id: ev.agent_id,
        op: ev.op,
      };
      const next = [...prev.bbChanges, change];
      const bbChanges =
        next.length > MAX_BB_CHANGES ? next.slice(-MAX_BB_CHANGES) : next;
      return { ...prev, bbChanges, bbGen: prev.bbGen + 1 };
    }
    case "thread_changed":
      return { ...prev, threadGen: prev.threadGen + 1 };
    case "budget_changed":
      return { ...prev, budgetGen: prev.budgetGen + 1 };
    case "thought_trace_event": {
      const steps = ev.steps.map((s) => s.label).filter(Boolean);
      return {
        ...prev,
        reasoningById: {
          ...prev.reasoningById,
          [ev.agent_id]: { steps, durationMs: null },
        },
      };
    }
    case "agent_stage":
      return {
        ...prev,
        agentStageById: {
          ...prev.agentStageById,
          [ev.agent_id]: { stage: ev.stage, at: ev.at },
        },
      };
    default:
      return prev;
  }
}
