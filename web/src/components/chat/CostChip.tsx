/**
 * CostChip — the workspace's all-time estimated spend, pinned in the chat
 * status strip. Read-only; click through to /usage for the full breakdown.
 *
 * Honesty rules (same convention as the usage page):
 *  - `priced === false` → "≥" prefix: unpriced models exist, so real spend
 *    is at least the shown number, never less.
 *  - zero events → render nothing (a fresh room must not show a fake $0) —
 *    EXCEPT when the workspace has live members on uncollected engines
 *    (opencode/reasonix/zulu…, see lib/usageCoverage.ts): their spend is
 *    never scraped, so the chip shows 「暂不统计」 instead of vanishing —
 *    disappearing would read as "these agents are free" (F1).
 *  - tooltip states the number is an estimate, not the subscription bill,
 *    and names any uncollected engines whose spend is NOT included.
 *
 * Refresh: mount + 30s poll. Cost only changes at turn boundaries, so a slow
 * poll beats wiring another WS gen for a read-only indicator.
 */

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import { Coins } from "lucide-react";
import { api } from "../../api/http";
import { agentIsAlive } from "../../lib/agent";
import { uncollectedEngines } from "../../lib/usageCoverage";

function fmtCost(n: number): string {
  if (n === 0) return "$0";
  if (n < 0.01) return "<$0.01";
  return `$${n.toFixed(2)}`;
}

export function CostChip({ workspaceId }: { workspaceId: string }) {
  const { t } = useTranslation();
  const [totals, setTotals] = useState<{
    cost: number;
    priced: boolean;
    events: number;
  } | null>(null);
  // Engines of LIVE members whose usage is never collected — their real spend
  // is absent from `totals` and never trips the budget brake.
  const [uncollected, setUncollected] = useState<string[]>([]);

  useEffect(() => {
    let alive = true;
    const load = async () => {
      try {
        const d = await api.getUsage(workspaceId);
        if (alive) {
          setTotals({
            cost: d.totals.cost_usd,
            priced: d.totals.priced,
            events: d.totals.events,
          });
        }
      } catch {
        /* best-effort: a failed fetch keeps the chip hidden, never fake data */
      }
      try {
        const list = await api.listAgents();
        if (alive) {
          setUncollected(
            uncollectedEngines(
              list.filter((a) => a.workspace_id === workspaceId && agentIsAlive(a)),
            ),
          );
        }
      } catch {
        /* members fetch failing only drops the coverage note, not the number */
      }
    };
    void load();
    const timer = window.setInterval(load, 30_000);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [workspaceId]);

  if (!totals) return null;

  // Pure-uncollected workspace: zero usage events yet live members are burning
  // money — say 「暂不统计」 instead of hiding the chip entirely.
  if (totals.events === 0) {
    if (uncollected.length === 0) return null;
    return (
      <Link
        to="/usage"
        className="-mx-1 inline-flex items-center gap-1 rounded px-1 font-mono transition-colors hover:bg-surface-tertiary hover:text-foreground-secondary"
        title={t("chat.costChipUncollectedOnly", {
          engines: uncollected.join(" / "),
          defaultValue:
            "{{engines}} 引擎暂不纳入用量统计:实际花费不显示,也不触发预算刹车。点击查看用量",
        })}
      >
        <Coins className="size-3 shrink-0" />
        {t("chat.costChipNotCounted", { defaultValue: "暂不统计" })}
      </Link>
    );
  }

  const hint =
    t("chat.costChipHint", {
      defaultValue: "本工作空间累计估算花费（不等于订阅账单）· 点击查看用量",
    }) +
    (uncollected.length > 0
      ? `\n${t("chat.costChipUncollectedHint", {
          engines: uncollected.join(" / "),
          defaultValue: "其中 {{engines}} 引擎暂不纳入统计,其实际花费不含在内",
        })}`
      : "");

  return (
    <Link
      to="/usage"
      className="-mx-1 inline-flex items-center gap-1 rounded px-1 font-mono transition-colors hover:bg-surface-tertiary hover:text-foreground-secondary"
      title={hint}
    >
      <Coins className="size-3 shrink-0" />
      {totals.priced ? "≈ " : "≥ "}
      {fmtCost(totals.cost)}
    </Link>
  );
}
