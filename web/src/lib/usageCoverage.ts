/**
 * 用量采集覆盖范围（F1 诚实性补丁的唯一事实源）。
 *
 * 后端只 tail 三种 CLI 的 session transcript 来累计 token / 费用:
 * crates/swarmx-server/src/transcript.rs `spawn_tailer` 按
 * `cli.contains("codex" | "claude" | "kimi")` 匹配 flavor,其余引擎直接
 * no-op 返回 —— opencode / reasonix / zulu 等引擎的 agent 永远不会有任何
 * usage 记录。这不是「没花钱」,是「没采集」。
 *
 * 因此 UI 侧凡涉及费用 / 用量的表面(/usage、CostChip、战报、预算刹车)
 * 都必须把未采集引擎标成「不计入 / 暂不统计」,绝不渲染成 0 或隐藏 ——
 * 否则读起来像这些成员免费。预算刹车同样覆盖不到它们(无 usage 事件就
 * 永不触发),文案要如实告知。
 */

import type { AgentInfo } from "../api/types";

/** 与 transcript.rs 的 contains 匹配保持一致(子串,不是精确相等 ——
 *  插件 id 可能带后缀,后端同样用 contains 判 flavor)。 */
const COLLECTED_FLAVORS = ["claude", "codex", "kimi"] as const;

/** 该引擎(cli plugin id)的用量是否被后端 transcript 采集覆盖。 */
export function usageIsCollected(cli: string | null | undefined): boolean {
  const c = (cli ?? "").toLowerCase();
  return COLLECTED_FLAVORS.some((f) => c.includes(f));
}

/** 一组 agent 里未被采集的引擎 id,去重且保持稳定先后顺序(供文案拼接)。 */
export function uncollectedEngines(
  agents: ReadonlyArray<Pick<AgentInfo, "cli">>,
): string[] {
  const seen = new Set<string>();
  for (const a of agents) {
    if (!usageIsCollected(a.cli)) seen.add(a.cli);
  }
  return [...seen];
}
