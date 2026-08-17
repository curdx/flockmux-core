import { describe, expect, it } from "vitest";
import { countsAsUserUnread } from "./unread";

describe("countsAsUserUnread — canonical user-unread predicate", () => {
  it("counts a real agent→user reply", () => {
    expect(countsAsUserUnread("claude-abc", "reply", null)).toBe(true);
    expect(countsAsUserUnread("codex-9", "note", undefined)).toBe(true);
  });

  it("excludes system event cards", () => {
    expect(countsAsUserUnread("system", "reply", null)).toBe(false);
  });

  it("excludes coordination wakes", () => {
    expect(countsAsUserUnread("system", "wake", { subtype: "wake" })).toBe(false);
    // even if some non-system sender emitted kind=wake, it's still noise
    expect(countsAsUserUnread("claude-abc", "wake", null)).toBe(false);
  });

  it("excludes a worker's completion delivery card — the drift that made the badge lie", () => {
    // This is the exact case the top-bar tally excluded but the in-list divider
    // used to count, producing "badge says 0 / divider says 1".
    expect(
      countsAsUserUnread("codex-9276dd15", "farewell", { subtype: "completion" }),
    ).toBe(false);
  });

  it("still counts an agent reply that carries unrelated meta", () => {
    expect(countsAsUserUnread("claude-abc", "reply", { reason: "manual" })).toBe(true);
  });

  it("excludes cron prompts (they are operator instructions, not chat)", () => {
    expect(countsAsUserUnread("cron", "note", null)).toBe(false);
  });
});
