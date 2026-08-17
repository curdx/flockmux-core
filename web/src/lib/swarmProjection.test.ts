import { describe, expect, it } from "vitest";
import type { MessageRecord, SwarmEvent } from "../api/types";
import {
  applySwarmEvent,
  emptySwarmSnapshot,
  emptyUnreadCtx,
  hydrateUnread,
  unreadFromMessages,
  type SwarmSnapshot,
} from "./swarmProjection";

function msg(
  over: Partial<Extract<SwarmEvent, { type: "message" }>> & { id: number },
): Extract<SwarmEvent, { type: "message" }> {
  return {
    type: "message",
    from_agent: "worker-1",
    to_agent: "user",
    kind: "reply",
    body: "hi",
    sent_at: 1,
    ...over,
  };
}

describe("applySwarmEvent", () => {
  it("appends a user-unread reply and counts it", () => {
    const ctx = emptyUnreadCtx();
    const next = applySwarmEvent(emptySwarmSnapshot, msg({ id: 7 }), ctx);
    expect(next.liveMessages).toHaveLength(1);
    expect(next.liveMessages[0].id).toBe(7);
    expect(next.unreadByFrom["worker-1"]).toBe(1);
    expect(ctx.countedUnread.has(7)).toBe(true);
  });

  it("does not count a wake toward user unread", () => {
    const ctx = emptyUnreadCtx();
    const next = applySwarmEvent(
      emptySwarmSnapshot,
      msg({ id: 8, kind: "wake", from_agent: "system", meta: { subtype: "wake" } }),
      ctx,
    );
    expect(next.unreadByFrom).toEqual({});
    expect(ctx.countedUnread.has(8)).toBe(false);
  });

  it("message_read decrements only counted ids", () => {
    const ctx = emptyUnreadCtx();
    let s = applySwarmEvent(emptySwarmSnapshot, msg({ id: 1 }), ctx);
    s = applySwarmEvent(
      s,
      msg({ id: 2, from_agent: "worker-1", to_agent: "orch", kind: "note" }),
      ctx,
    );
    expect(s.unreadByFrom["worker-1"]).toBe(1);
    s = applySwarmEvent(
      s,
      { type: "message_read", ids: [1, 2], to_agent: "user", at: 9 },
      ctx,
    );
    expect(s.unreadByFrom["worker-1"]).toBeUndefined();
    expect(s.liveRead).toEqual({ ids: [1, 2], to_agent: "user", at: 9 });
  });

  it("agent_state patches live slice and bumps rosterGen", () => {
    const ctx = emptyUnreadCtx();
    let s = applySwarmEvent(
      emptySwarmSnapshot,
      { type: "agent_state", agent_id: "a", state: "thinking" },
      ctx,
    );
    expect(s.agentStateById.a.state).toBe("thinking");
    expect(s.rosterGen).toBe(1);
    s = applySwarmEvent(
      s,
      { type: "agent_state", agent_id: "a", state: "exited" },
      ctx,
    );
    expect(s.recordingsGen).toBe(1);
    expect(s.agentStageById.a).toBeUndefined();
    expect(s.rosterGen).toBe(2);
    const again = applySwarmEvent(
      s,
      { type: "agent_state", agent_id: "a", state: "exited" },
      ctx,
    );
    expect(again).toBe(s);
    expect(again.rosterGen).toBe(2);
  });

  it("agent_activity replaces same seq and drops the stage bar", () => {
    const ctx = emptyUnreadCtx();
    let s: SwarmSnapshot = {
      ...emptySwarmSnapshot,
      agentStageById: { a: { stage: "mcp_ready", at: 1 } },
    };
    s = applySwarmEvent(
      s,
      {
        type: "agent_activity",
        agent_id: "a",
        kind: "tool",
        label: "Edit",
        phase: "running",
        seq: 1,
        at: 2,
      },
      ctx,
    );
    expect(s.agentStageById.a).toBeUndefined();
    expect(s.agentActivityById.a).toHaveLength(1);
    s = applySwarmEvent(
      s,
      {
        type: "agent_activity",
        agent_id: "a",
        kind: "tool",
        label: "Edit",
        phase: "ok",
        seq: 1,
        duration_ms: 12,
        at: 3,
      },
      ctx,
    );
    expect(s.agentActivityById.a).toHaveLength(1);
    expect(s.agentActivityById.a[0].phase).toBe("ok");
  });

  it("blackboard_changed rings and bumps bbGen without dropping earlier paths", () => {
    const ctx = emptyUnreadCtx();
    let s = applySwarmEvent(
      emptySwarmSnapshot,
      {
        type: "blackboard_changed",
        id: 1,
        agent_id: "a",
        op: "write",
        path: "ws/main/a.progress.md",
        sha256: "x",
        at: 1,
      },
      ctx,
    );
    s = applySwarmEvent(
      s,
      {
        type: "blackboard_changed",
        id: 2,
        agent_id: "a",
        op: "write",
        path: "ws/main/plan.json",
        sha256: "y",
        at: 2,
      },
      ctx,
    );
    expect(s.bbGen).toBe(2);
    expect(s.bbChanges.map((c) => c.path)).toEqual([
      "ws/main/a.progress.md",
      "ws/main/plan.json",
    ]);
  });
});

describe("unreadFromMessages", () => {
  it("hydrates only unread agent→user replies", () => {
    const rows = [
      {
        id: 1,
        from_agent: "w",
        to_agent: "user",
        kind: "reply",
        body: "a",
        sent_at: 1,
        delivered_at: null,
        read_at: null,
        in_reply_to: null,
        thread_id: null,
        meta: null,
        thought_trace: null,
      },
      {
        id: 2,
        from_agent: "w",
        to_agent: "user",
        kind: "reply",
        body: "b",
        sent_at: 2,
        delivered_at: null,
        read_at: 9,
        in_reply_to: null,
        thread_id: null,
        meta: null,
        thought_trace: null,
      },
      {
        id: 3,
        from_agent: "w",
        to_agent: "orch",
        kind: "note",
        body: "c",
        sent_at: 3,
        delivered_at: null,
        read_at: null,
        in_reply_to: null,
        thread_id: null,
        meta: null,
        thought_trace: null,
      },
    ] satisfies MessageRecord[];
    const { unreadByFrom, ctx } = unreadFromMessages(rows);
    expect(unreadByFrom).toEqual({ w: 1 });
    expect(ctx.countedUnread.has(1)).toBe(true);
    expect(ctx.countedUnread.has(2)).toBe(false);
  });
});

describe("unread identity", () => {
  it("does not double-count a WS message already hydrated from REST", () => {
    const rows: MessageRecord[] = [
      rec({ id: 10, from_agent: "w", read_at: null }),
    ];
    const { unreadByFrom, ctx } = hydrateUnread(rows, emptyUnreadCtx());
    expect(unreadByFrom).toEqual({ w: 1 });
    const next = applySwarmEvent(
      { ...emptySwarmSnapshot, unreadByFrom },
      msg({ id: 10, from_agent: "w" }),
      ctx,
    );
    expect(next.unreadByFrom).toEqual({ w: 1 });
    expect(next.liveMessages).toHaveLength(0);
  });

  it("keeps a live-only unread that REST's last-N window missed", () => {
    const liveCtx = emptyUnreadCtx();
    const live = applySwarmEvent(emptySwarmSnapshot, msg({ id: 201 }), liveCtx);
    expect(live.unreadByFrom["worker-1"]).toBe(1);
    const { unreadByFrom, ctx } = hydrateUnread(
      [rec({ id: 1, from_agent: "worker-1", read_at: null })],
      liveCtx,
    );
    expect(unreadByFrom["worker-1"]).toBe(2);
    expect(ctx.countedUnread.has(201)).toBe(true);
    expect(ctx.countedUnread.has(1)).toBe(true);
  });

  it("lets REST win for ids it returned — a now-read row drops the live count", () => {
    const liveCtx = emptyUnreadCtx();
    applySwarmEvent(emptySwarmSnapshot, msg({ id: 10, from_agent: "w" }), liveCtx);
    const { unreadByFrom, ctx } = hydrateUnread(
      [rec({ id: 10, from_agent: "w", read_at: 9 })],
      liveCtx,
    );
    expect(unreadByFrom).toEqual({});
    expect(ctx.countedUnread.has(10)).toBe(false);
  });

  it("does not copy non-unread ids across hydrate (idToFrom stays last-N + live unread)", () => {
    const liveCtx = emptyUnreadCtx();
    applySwarmEvent(
      emptySwarmSnapshot,
      msg({ id: 9, from_agent: "w", to_agent: "orch", kind: "note" }),
      liveCtx,
    );
    const { ctx } = hydrateUnread(
      [rec({ id: 1, from_agent: "w", read_at: null })],
      liveCtx,
    );
    expect(ctx.idToFrom.has(9)).toBe(false);
    expect(ctx.idToFrom.has(1)).toBe(true);
  });
});

function rec(
  over: Pick<MessageRecord, "id" | "from_agent" | "read_at">,
): MessageRecord {
  return {
    id: over.id,
    from_agent: over.from_agent,
    to_agent: "user",
    kind: "reply",
    body: "a",
    sent_at: 1,
    delivered_at: null,
    read_at: over.read_at,
    in_reply_to: null,
    thread_id: null,
    meta: null,
    thought_trace: null,
  };
}
