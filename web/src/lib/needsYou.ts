/**
 * needsYou — 把「需要用户看一眼」的 agent 聚合成一个全局收件箱( NeedsYouBar
 * 的数据源)。判定尽量复用成员栏同一套视觉管线(`resolveMemberVisual`),
 * 杜绝「成员点显示红、收件箱却没看见」两处真相。
 *
 * 三类(按严重度排序,不重复计数)——只放「需要你做个决定」的事:
 *   1. error        — 异常退出/持久错误(未登录、限流、看门狗),用户要决策
 *                     (重拉 / kill / 去登录)。
 *   2. handoff      — worker 退出了但没交付约定的产出。服务端两种旗:
 *                     `handoff_missing`(silent,无 `.error`) 与
 *                     `handoff_failed`(写了 `<key>.error`,常见于 kill/崩溃)。
 *                     **两者都要进人收件箱** —— 只认 missing 会漏掉真实用户
 *                     杀进程路径(见 UX-016)。
 *   3. stalled      — 「该醒没醒」(server 算)。**不展示给用户点** —— Shell 用
 *                     `useAutoNudgeStalled` 自动催；收件箱只留需要人做决定的
 *                     error / handoff。
 *
 * stalled 不是 0.3.0 被撤下的「疑似卡住」:那版只看「多久没活动」,而现代
 * agent 的回合可以合法跑十几二十分钟(长思考、大构建、API 重试),阈值再宽
 * 也是猜 —— 误报一次(实测:codex 明明还在出 exec 事件,被标「疑似卡住」)
 * 用户就再也不信这条收件箱了。新版信号是「有未读的东西在等它 + 它没动静」的
 * 交集:空闲等活的 worker 没有未读邮件,不命中;正在干活的 worker 活动是
 * 新的,不命中。纯「慢但没邮件等它」依旧不收 —— 软提示留在成员栏的琥珀点
 * (那边只是提示、不要求行动)。waiting_dep / paused 同理不算。
 */

import type { AgentInfo, AgentLiveState } from "@/api/types";
import type { MessageRecord } from "@/api/types";
import { resolveMemberVisual } from "./agent";

export type NeedsYouKind = "error" | "handoff" | "stalled";

export interface NeedsYouItem {
  agent: AgentInfo;
  kind: NeedsYouKind;
}

// resolveMemberVisual 需要一份 label 表(它给成员栏显示用);这里只要视觉
// 分类(isError),label 字符串用不上,传占位。labels 是它的第 4 个参数
// (下标 3)。
const PLACEHOLDER_LABELS = new Proxy(
  {} as Record<string, string>,
  { get: () => "" },
) as Parameters<typeof resolveMemberVisual>[3];

/** Undelivered handoff of either flavor — silent missing or explicit `.error`. */
export function hasUndeliveredHandoff(a: AgentInfo): boolean {
  return !!(a.handoff_missing || a.handoff_failed);
}

function isLiveOrchestrator(a: AgentInfo): boolean {
  return (
    a.role.toLowerCase() === "orchestrator" &&
    a.killed_at == null &&
    a.shim_exit == null
  );
}

export function hasLiveOrchestrator(members: AgentInfo[]): boolean {
  return members.some(isLiveOrchestrator);
}

export function deriveNeedsYou(
  members: AgentInfo[],
  liveById: Record<string, AgentLiveState | undefined>,
  messages: MessageRecord[],
  now: number = Date.now(),
): NeedsYouItem[] {
  const captainLive = hasLiveOrchestrator(members);
  const out: NeedsYouItem[] = [];
  for (const a of members) {
    // handoff 必须先于退出过滤:server 对已退出 agent 置 missing/failed。
    // 先 skip 已退出会让「worker 没交付就死了」永远进不了收件箱。
    if (hasUndeliveredHandoff(a)) {
      // UX-034: silent missing（.error 已清/从未写）+ 规划也挂了 →
      // 「跟规划说」没对象，条会永久赖着。有 .error 的 failed 仍亮，
      // 或规划还活着时仍亮（真的能跟规划说）。
      const silentMissing = !!a.handoff_missing && !a.handoff_failed;
      if (silentMissing && !captainLive) {
        continue;
      }
      out.push({ agent: a, kind: "handoff" });
      continue;
    }
    // 已退出(主动 kill / shim 退出)的 agent 不参与 error/stalled 判定:
    // 刻意 kill 且无 handoff 契约不是 needs-you;server 也只对活着的 agent 置 stalled。
    if (a.killed_at != null || a.shim_exit != null) continue;
    const v = resolveMemberVisual(a, liveById[a.agent_id], messages, PLACEHOLDER_LABELS, now);
    if (v.isError) {
      // error 优先于 stalled:同一个 agent 既报错又有滞留邮件时,真正要人
      // 处理的是错误(去登录/等限流),不是唤醒。
      out.push({ agent: a, kind: "error" });
    } else if (a.stalled) {
      out.push({ agent: a, kind: "stalled" });
    }
  }
  // 排序:确凿的故障(error / handoff,工作已受损或无法进行)排在疑似
  // (stalled,唤醒链「可能」断了)前面 —— 收件箱的注意力分配与文案的
  // 诚实度梯度一致。
  const rank: Record<NeedsYouKind, number> = { error: 0, handoff: 1, stalled: 2 };
  return out.sort((x, y) => rank[x.kind] - rank[y.kind]);
}
