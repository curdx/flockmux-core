import { describe, expect, it } from "vitest";
import {
  GROUP_GAP_MS,
  buildRows,
  collapseChatMessages,
  formatElapsed,
  isOperatorChatNoise,
  resolveRole,
} from "./messageRows";
import type { MessageRecord } from "../api/types";

function msg(partial: Partial<MessageRecord>): MessageRecord {
  return { id: 1, from_agent: "a", to_agent: "b", kind: "note", body: "", sent_at: 0, ...partial } as MessageRecord;
}

describe("buildRows", () => {
  it("first message always shows a header, no divider", () => {
    const rows = buildRows([msg({ id: 1, from_agent: "x", sent_at: 1000 })]);
    expect(rows).toHaveLength(1);
    expect(rows[0].showHeader).toBe(true);
    expect(rows[0].showDividerBefore).toBe(false);
  });

  it("shows a header on every message (no run collapsing) so each is attributed", () => {
    const rows = buildRows([
      msg({ id: 1, from_agent: "x", sent_at: 1000 }),
      msg({ id: 2, from_agent: "x", sent_at: 2000 }),
    ]);
    expect(rows[1].showHeader).toBe(true);
    expect(rows[1].showDividerBefore).toBe(false);
  });

  it("shows the header when the sender changes too", () => {
    const rows = buildRows([
      msg({ id: 1, from_agent: "x", sent_at: 1000 }),
      msg({ id: 2, from_agent: "y", sent_at: 2000 }),
    ]);
    expect(rows[1].showHeader).toBe(true);
  });

  it("inserts a divider + header when the gap exceeds GROUP_GAP_MS", () => {
    const rows = buildRows([
      msg({ id: 1, from_agent: "x", sent_at: 0 }),
      msg({ id: 2, from_agent: "x", sent_at: GROUP_GAP_MS + 1 }),
    ]);
    expect(rows[1].showDividerBefore).toBe(true);
    expect(rows[1].showHeader).toBe(true);
  });

  // Boundary: gap === GROUP_GAP_MS is NOT a divider (the check is `>`, not `>=`).
  it("does not divide exactly at the gap boundary", () => {
    const rows = buildRows([
      msg({ id: 1, from_agent: "x", sent_at: 0 }),
      msg({ id: 2, from_agent: "x", sent_at: GROUP_GAP_MS }),
    ]);
    expect(rows[1].showDividerBefore).toBe(false);
  });
});

describe("resolveRole", () => {
  it("prefers the lookup map", () => {
    expect(resolveRole("claude-abc", new Map([["claude-abc", "captain"]]))).toBe("captain");
  });
  it("falls back to the first id segment", () => {
    expect(resolveRole("claude-abc", new Map())).toBe("claude");
    expect(resolveRole("_writer_xyz", new Map())).toBe("writer");
  });
});

describe("isOperatorChatNoise", () => {
  it("hides cron and wake rows", () => {
    expect(isOperatorChatNoise(msg({ from_agent: "cron", kind: "note" }))).toBe(true);
    expect(isOperatorChatNoise(msg({ from_agent: "system", kind: "wake" }))).toBe(true);
    expect(isOperatorChatNoise(msg({ from_agent: "claude-1", kind: "reply" }))).toBe(false);
  });
});

describe("collapseChatMessages", () => {
  it("drops cron rows and collapses consecutive same-role dispatches", () => {
    const rows = collapseChatMessages([
      msg({ id: 1, from_agent: "cron", kind: "note", body: "don't spawn", sent_at: 1 }),
      msg({
        id: 2,
        from_agent: "system",
        kind: "system",
        sent_at: 2,
        meta: { subtype: "dispatch", child_role: "researcher", child_agent: "a" },
      }),
      msg({
        id: 3,
        from_agent: "system",
        kind: "system",
        sent_at: 3,
        meta: { subtype: "dispatch", child_role: "researcher", child_agent: "b" },
      }),
      msg({
        id: 4,
        from_agent: "system",
        kind: "system",
        sent_at: 4,
        meta: { subtype: "dispatch", child_role: "writer", child_agent: "c" },
      }),
    ]);
    expect(rows).toHaveLength(2);
    expect(rows[0].meta?.child_agent).toBe("b");
    expect(rows[0].meta?.dispatch_count).toBe(2);
    expect(rows[1].meta?.child_role).toBe("writer");
  });
});

describe("formatElapsed", () => {
  it("formats sub-minute / minute / hour (locale-independent)", () => {
    expect(formatElapsed(5_000)).toBe("5s");
    expect(formatElapsed(65_000)).toBe("1m 05s");
    expect(formatElapsed(3_725_000)).toBe("1h 02m");
  });
});
