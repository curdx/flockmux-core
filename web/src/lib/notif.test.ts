import { describe, expect, it } from "vitest";
import {
  blackboardWorkspaceId,
  humanizeBlackboard,
  isNoisyBlackboard,
} from "./notif";

const t = (k: string, opts?: Record<string, unknown>) => {
  if (k === "notifications.bb.failed") return "一项工作失败了";
  if (k === "notifications.bb.taskLedger") return "任务记录更新";
  if (k === "notifications.bb.update") return `${opts?.name} 更新`;
  if (k === "notifications.bb.mainDir") return "主线";
  return k;
};

describe("isNoisyBlackboard", () => {
  it("hides worker heartbeats", () => {
    expect(isNoisyBlackboard("ws/main/researcher.progress.md")).toBe(true);
    expect(isNoisyBlackboard("ws/main/task.ledger.md")).toBe(false);
  });
});

describe("blackboardWorkspaceId", () => {
  it("takes the first of three path segments", () => {
    expect(blackboardWorkspaceId("abc/main/foo.error")).toBe("abc");
    expect(blackboardWorkspaceId("foo.error")).toBeUndefined();
  });
});

describe("humanizeBlackboard", () => {
  it("does not leak .error keys", () => {
    const h = humanizeBlackboard(
      "abc123/main/researcher.task.done.error",
      [],
      t,
    );
    expect(h.title).toBe("一项工作失败了");
    expect(h.title).not.toContain("researcher");
  });
});
