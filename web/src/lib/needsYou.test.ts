import { describe, expect, it } from "vitest";
import { deriveNeedsYou } from "./needsYou";
import type { AgentInfo, AgentLiveState } from "@/api/types";

const NOW = 1_800_000_000_000;

function agent(partial: Partial<AgentInfo> = {}): AgentInfo {
  return {
    agent_id: "kimi-aaaa",
    cli: "kimi",
    role: "backend",
    workspace: "/tmp/ws",
    shim_ready: true,
    shim_exit: null,
    killed_at: null,
    spawned_at: NOW - 3_600_000,
    ...partial,
  };
}

describe("deriveNeedsYou", () => {
  it("live error state → error", () => {
    const a = agent();
    const items = deriveNeedsYou(
      [a],
      { [a.agent_id]: { state: "error" } },
      [],
      NOW,
    );
    expect(items).toEqual([{ agent: a, kind: "error" }]);
  });

  it("persistent last_error without a newer recovery signal → error", () => {
    const a = agent({ last_error: "未登录", last_error_at: NOW - 60_000 });
    const items = deriveNeedsYou([a], {}, [], NOW);
    expect(items[0]?.kind).toBe("error");
  });

  it("recovery: activity strictly newer than last_error_at clears the error", () => {
    const a = agent({
      last_error: "未登录",
      last_error_at: NOW - 120_000,
      last_activity_at: NOW - 60_000,
    });
    const items = deriveNeedsYou([a], {}, [], NOW);
    expect(items).toEqual([]);
  });

  it("killed agent with error state is excluded (intentional kill is not needs-you)", () => {
    const a = agent({ killed_at: NOW - 1000 });
    const items = deriveNeedsYou(
      [a],
      { [a.agent_id]: { state: "error" } },
      [],
      NOW,
    );
    expect(items).toEqual([]);
  });

  it("stalled-looking worker is NOT needs-you (slow-but-alive is not a decision)", () => {
    // 误报教训:codex 还在正常出 exec 事件,只是回合长 —— 纯「慢」不许进
    // 收件箱(软提示留在成员栏琥珀点)。只有 server 置位 stalled(活着 +
    // 有超期未读邮件 + 同期无活动,见 store.rs)才进 —— 见下方 stalled 用例。
    const a = agent({ parent_agent_id: "orch-1" });
    const live: AgentLiveState = {
      activity: {
        agent_id: a.agent_id,
        kind: "tool",
        label: "Bash npm run build",
        phase: "running",
        seq: 1,
        at: NOW - 301_000,
      },
    };
    expect(deriveNeedsYou([a], { [a.agent_id]: live }, [], NOW)).toEqual([]);
  });

  it("worker that was active then went silent → still not needs-you", () => {
    const a = agent({
      parent_agent_id: "orch-1",
      last_activity_at: NOW - 601_000,
    });
    expect(deriveNeedsYou([a], {}, [], NOW)).toEqual([]);
  });

  it("server-flagged stalled → stalled (该醒没醒:有超期未读 + 无活动)", () => {
    // stalled 是 server 算的精准信号(store.rs stalled_agents_with_unread:
    // 活着 + 最老未读超 10min + last_activity 超阈值或为空 + 无 live 活动),
    // 前端只信标记,不在本地重算时间窗。
    const a = agent({ stalled: true });
    const items = deriveNeedsYou([a], {}, [], NOW);
    expect(items).toEqual([{ agent: a, kind: "stalled" }]);
  });

  it("stalled + error → error wins (先处理错误,不是先唤醒)", () => {
    const a = agent({
      stalled: true,
      last_error: "未登录",
      last_error_at: NOW - 60_000,
    });
    const items = deriveNeedsYou([a], {}, [], NOW);
    expect(items).toEqual([{ agent: a, kind: "error" }]);
  });

  it("stalled on an exited agent is ignored (server 不这么置位,前端兜底)", () => {
    // server 只对活着的 agent 置 stalled(rest.rs 候选过滤 + store SQL 双重
    // 门);snapshot 后 agent 刚好退出时,前端不得把「已退出」显示成「可能卡住」。
    const a = agent({ stalled: true, shim_exit: 0 });
    expect(deriveNeedsYou([a], {}, [], NOW)).toEqual([]);
  });

  it("handoff_missing → handoff (已退出,shim_exit 变体)", () => {
    // 生产事实:server 只对「已退出」的 agent 置 handoff_missing(活着的豁免,
    // 见 rest.rs 的 exited && …),所以 fixture 必须带退出标记 —— 此前
    // shim_exit: null 的 handoff_missing 在生产中不可能出现,恰好掩盖了
    // 「先 skip 已退出 agent」导致的收件箱盲区。
    const orch = agent({ agent_id: "orch-live", role: "orchestrator" });
    const a = agent({
      handoff_missing: true,
      handoff_signal: "backend.done",
      shim_exit: 1,
    });
    const items = deriveNeedsYou([orch, a], {}, [], NOW);
    expect(items).toEqual([{ agent: a, kind: "handoff" }]);
  });

  it("handoff_missing on a killed agent → still handoff (killed_at 变体)", () => {
    // 被 kill 的 worker 同样可能没交付(例如超时看门狗 kill)——
    // server 对 killed_at 也置 handoff_missing,收件箱必须照样亮。
    // 规划还活着时才亮「跟规划说」；见下方 UX-034。
    const orch = agent({
      agent_id: "orch-live",
      role: "orchestrator",
    });
    const a = agent({
      handoff_missing: true,
      handoff_signal: "backend.done",
      killed_at: NOW - 1000,
    });
    expect(deriveNeedsYou([orch, a], {}, [], NOW)).toEqual([
      { agent: a, kind: "handoff" },
    ]);
  });

  it("UX-034: silent handoff_missing + no live captain → not needs-you", () => {
    const deadOrch = agent({
      agent_id: "orch-dead",
      role: "orchestrator",
      killed_at: NOW - 1000,
    });
    const a = agent({
      agent_id: "fixer-dead",
      role: "Fixer",
      handoff_missing: true,
      handoff_failed: false,
      handoff_signal: "fixer.done",
      killed_at: NOW - 5000,
    });
    expect(deriveNeedsYou([deadOrch, a], {}, [], NOW)).toEqual([]);
  });

  it("UX-034: handoff_failed + no live captain → not needs-you (nobody to tell)", () => {
    const a = agent({
      handoff_failed: true,
      handoff_missing: false,
      handoff_signal: "fixer.done",
      killed_at: NOW - 1000,
    });
    expect(deriveNeedsYou([a], {}, [], NOW)).toEqual([]);
  });

  it("handoff_failed (kill/crash wrote .error) → handoff even when missing=false", () => {
    // UX-016: 常见路径是 handoff_failed=true + handoff_missing=false。
    // 只认 missing 会让「没交结果就死了」从不进收件箱。规划必须还活着。
    const orch = agent({ agent_id: "orch-live", role: "orchestrator" });
    const a = agent({
      handoff_failed: true,
      handoff_missing: false,
      handoff_signal: "backend.done",
      killed_at: NOW - 1000,
    });
    expect(deriveNeedsYou([orch, a], {}, [], NOW)).toEqual([
      { agent: a, kind: "handoff" },
    ]);
  });

  it("handoff_failed with shim_exit → handoff", () => {
    const orch = agent({ agent_id: "orch-live", role: "orchestrator" });
    const a = agent({
      handoff_failed: true,
      handoff_signal: "docs.done",
      shim_exit: 1,
    });
    expect(deriveNeedsYou([orch, a], {}, [], NOW)).toEqual([
      { agent: a, kind: "handoff" },
    ]);
  });

  it("healthy orchestrator with no error/stall/handoff → empty", () => {
    const a = agent({ role: "orchestrator" });
    expect(deriveNeedsYou([a], {}, [], NOW)).toEqual([]);
  });

  it("paused agent is excluded (user-caused wait, not needs-you)", () => {
    const a = agent({ paused: true });
    expect(deriveNeedsYou([a], {}, [], NOW)).toEqual([]);
  });

  it("waiting_dep state is excluded (system wait, not needs-you)", () => {
    const a = agent();
    expect(
      deriveNeedsYou([a], { [a.agent_id]: { state: "waiting_dep" } }, [], NOW),
    ).toEqual([]);
  });

  it("orders error before handoff, one item per agent", () => {
    const orch = agent({ agent_id: "orch-live", role: "orchestrator" });
    const err = agent({ agent_id: "a-err", role: "reviewer" });
    const miss = agent({
      agent_id: "a-miss",
      handoff_missing: true,
      handoff_signal: "x.done",
      // handoff_missing ⇒ 已退出(server 只对退出 agent 置位)。
      shim_exit: 1,
    });
    const items = deriveNeedsYou(
      [orch, miss, err],
      { [err.agent_id]: { state: "error" } },
      [],
      NOW,
    );
    expect(items.map((i) => i.kind)).toEqual(["error", "handoff"]);
    expect(new Set(items.map((i) => i.agent.agent_id)).size).toBe(2);
  });

  it("orders error < handoff < stalled (确诊的故障排在疑似前面)", () => {
    const orch = agent({ agent_id: "orch-live", role: "orchestrator" });
    const stalledA = agent({ agent_id: "a-stalled", role: "backend", stalled: true });
    const miss = agent({
      agent_id: "a-miss",
      handoff_missing: true,
      handoff_signal: "x.done",
      shim_exit: 1,
    });
    const err = agent({ agent_id: "a-err", role: "reviewer" });
    const items = deriveNeedsYou(
      [orch, stalledA, miss, err],
      { [err.agent_id]: { state: "error" } },
      [],
      NOW,
    );
    expect(items.map((i) => i.kind)).toEqual(["error", "handoff", "stalled"]);
  });

  it("watchdog stuck mark (last_error_kind=stuck) → stuck lane, not error", () => {
    // S5: server 已自动唤醒两轮仍零活动才置位 —— 进栏让人看一眼,但它是
    // 疑似(琥珀)不是确诊(红),所以不能算进 error 道。
    const a = agent({
      last_error: "疑似卡住：进程还活着…",
      last_error_kind: "stuck",
      last_error_at: NOW - 60_000,
    });
    const items = deriveNeedsYou([a], {}, [], NOW);
    expect(items).toEqual([{ agent: a, kind: "stuck" }]);
  });

  it("stuck mark + activity strictly newer than the mark → recovered, not needs-you", () => {
    // 与成员栏同一条恢复守卫:任何比 last_error_at 新的信号都先算「已恢复」,
    // 不亮 chip(后端下一 tick 也会清掉标记)。
    const a = agent({
      last_error: "疑似卡住：进程还活着…",
      last_error_kind: "stuck",
      last_error_at: NOW - 120_000,
      last_activity_at: NOW - 60_000,
    });
    expect(deriveNeedsYou([a], {}, [], NOW)).toEqual([]);
  });

  it("stuck mark on an exited agent is ignored (前端兜底)", () => {
    const a = agent({
      last_error: "疑似卡住：进程还活着…",
      last_error_kind: "stuck",
      last_error_at: NOW - 60_000,
      shim_exit: 0,
    });
    expect(deriveNeedsYou([a], {}, [], NOW)).toEqual([]);
  });

  it("stuck mark on a paused agent still shows (pause 不改变它曾卡住的事实)", () => {
    // server 不会给 paused agent 新置位,但已置位后被 pause 的,标记依然诚实。
    const a = agent({
      paused: true,
      last_error: "疑似卡住：进程还活着…",
      last_error_kind: "stuck",
      last_error_at: NOW - 60_000,
    });
    const items = deriveNeedsYou([a], {}, [], NOW);
    expect(items).toEqual([{ agent: a, kind: "stuck" }]);
  });

  it("orders error < handoff < stuck < stalled", () => {
    const orch = agent({ agent_id: "orch-live", role: "orchestrator" });
    const stalledA = agent({ agent_id: "a-stalled", role: "backend", stalled: true });
    const stuckA = agent({
      agent_id: "a-stuck",
      role: "frontend",
      last_error: "疑似卡住：进程还活着…",
      last_error_kind: "stuck",
      last_error_at: NOW - 60_000,
    });
    const miss = agent({
      agent_id: "a-miss",
      handoff_missing: true,
      handoff_signal: "x.done",
      shim_exit: 1,
    });
    const err = agent({ agent_id: "a-err", role: "reviewer" });
    const items = deriveNeedsYou(
      [orch, stalledA, stuckA, miss, err],
      { [err.agent_id]: { state: "error" } },
      [],
      NOW,
    );
    expect(items.map((i) => i.kind)).toEqual(["error", "handoff", "stuck", "stalled"]);
  });
});
