import { describe, expect, it } from "vitest";
import { filterPreviewLines } from "./castPreview";

describe("filterPreviewLines", () => {
  it("drops 操作员唤醒 injection lines", () => {
    const kept = filterPreviewLines([
      "操作员唤醒——请先查收邮箱里的新消息（可能是用户的新指令），再检查共享区，然后继续。",
      "╭─── Claude Code ───╮",
      "Welcome back!",
    ]);
    expect(kept.some((l) => l.includes("操作员唤醒"))).toBe(false);
    expect(kept.some((l) => l.includes("Welcome back"))).toBe(true);
  });

  it("drops swarm_send_message user-reply boilerplate", () => {
    const kept = filterPreviewLines([
      '必须调用 swarm_send_message(to="user", kind="reply", body=...) 把回复发回',
      "real work output",
    ]);
    expect(kept).toEqual(["real work output"]);
  });
});
