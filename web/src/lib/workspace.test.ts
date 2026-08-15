import { describe, expect, it } from "vitest";
import { isScanDoneBlackboardPath } from "./workspace";

// CreateWizard 的进群信号:orchestrator Phase A 写
// `{workspace_id}/{thread_slug}/task.ledger.md`(roles/orchestrator.md 步骤 2)。
// 历史上听 `project.summary.*`(scout 时代的 key,已无写入方),wizard 只能
// 干等 60s 超时 —— 这些用例把新信号钉死。
describe("isScanDoneBlackboardPath", () => {
  const WS = "3f9c1a2b-0000-4000-8000-000000000000";

  it("本工作空间 main 方向的 task.ledger.md → 命中", () => {
    expect(isScanDoneBlackboardPath(`${WS}/main/task.ledger.md`, WS)).toBe(true);
  });

  it("本工作空间其它方向的 task.ledger.md → 命中(方向 slug 不参与匹配)", () => {
    expect(isScanDoneBlackboardPath(`${WS}/dark-mode/task.ledger.md`, WS)).toBe(true);
  });

  it("别的工作空间的 ledger → 不命中(不会误关本次 wizard)", () => {
    const other = "aaaa0000-1111-4222-8333-444455556666";
    expect(isScanDoneBlackboardPath(`${other}/main/task.ledger.md`, WS)).toBe(false);
  });

  it("本工作空间的其它 key(plan.json / progress.ledger.md)→ 不命中", () => {
    // task.ledger.md 是 Phase A 的第一个必写件(也是 Phase A 的跳过标记),
    // 它就是完成信号本身;其它 key 不重复触发。
    expect(isScanDoneBlackboardPath(`${WS}/main/plan.json`, WS)).toBe(false);
    expect(isScanDoneBlackboardPath(`${WS}/main/progress.ledger.md`, WS)).toBe(false);
  });

  it("旧 scout 时代的 project.summary.* → 不命中", () => {
    expect(isScanDoneBlackboardPath("project.summary.Users_me_code_web", WS)).toBe(false);
  });

  it("无命名空间的裸 task.ledger.md → 不命中", () => {
    expect(isScanDoneBlackboardPath("task.ledger.md", WS)).toBe(false);
  });

  it("workspace_id 只作为路径首段生效(子串撞车不算)", () => {
    expect(isScanDoneBlackboardPath(`x${WS}/main/task.ledger.md`, WS)).toBe(false);
  });
});
