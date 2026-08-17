/**
 * BudgetBrakeBanner — 工作空间预算刹车横幅。
 *
 * 只在「刹车已触发」(exceeded) 时可见：估算花费到达上限,服务端已暂停本空间
 * 所有成员,并拒绝对新 spawn / 新回合(fail-closed)。数据来自
 * GET /api/workspaces/:id/budget;live 刷新走 swarm projection 的 budgetGen
 * (服务端 trip/lift 时广播 budget_changed) + reconnectGen(断线重连后兜底),
 * 不轮询。
 *
 * 一键恢复(F2):刹车触发时用户情绪峰值,不该逼他跳四步去 /usage 改数字。
 * 横幅直接给「调高到 $X 并恢复」按钮 —— X = max(当前估算 × 2, 当前上限 + $5),
 * PUT 同一个 budget 接口;提高上限后服务端自动恢复刹车自己暂停的成员。
 * 想填别的数仍可点「去调整预算」跳 /usage。
 *
 * 诚实红线:所有金额都是估算(transcript 抓取 + 价目表),不等于订阅账单。
 */

import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { CircleDollarSign, Loader2 } from "lucide-react";
import { api, ApiError } from "@/api/http";
import type { WorkspaceBudget } from "@/api/types";
import { useSwarmRefresh } from "@/hooks/useSwarmProjection";
import { toast } from "@/lib/toast";

function fmtCost(n: number): string {
  if (n === 0) return "$0";
  if (n < 0.01) return "<$0.01";
  return `$${n.toFixed(2)}`;
}

export function BudgetBrakeBanner({ workspaceId }: { workspaceId: string }) {
  const { t } = useTranslation();
  const [budget, setBudget] = useState<WorkspaceBudget | null>(null);
  const [raising, setRaising] = useState(false);

  const load = useCallback(async () => {
    try {
      setBudget(await api.getWorkspaceBudget(workspaceId));
    } catch (e) {
      // 读失败必须保持上次状态(或隐藏),绝不假装"没超支"也绝不假装"超支"。
      console.warn("getWorkspaceBudget failed", e);
    }
  }, [workspaceId]);

  useEffect(() => {
    setBudget(null);
    void load();
  }, [load]);

  // 服务端 trip/lift 各广播一次 budget_changed → budgetGen;断线重连后重读一次
  // 兜底(断开期间的刹车状态变化不能靠漏收的事件脑补)。
  useSwarmRefresh((s) => s.budgetGen, load);
  useSwarmRefresh((s) => s.reconnectGen, load);

  if (!budget?.exceeded) return null;

  // 一键恢复的目标上限:给足余量(估算翻倍)且至少比旧上限多 $5;向上取整到
  // 分,免得 9.999999 这种浮点尾巴写进预算。cap 为 null 时理论上不会 exceeded,
  // 防御性隐藏按钮。
  const cap = budget.budget_usd;
  const raiseTo =
    cap != null
      ? Math.ceil(Math.max(budget.current_cost_usd * 2, cap + 5) * 100) / 100
      : null;

  const raiseAndResume = async () => {
    if (raiseTo == null || raising) return;
    setRaising(true);
    try {
      const b = await api.putWorkspaceBudget(workspaceId, raiseTo);
      setBudget(b);
      // 服务端在抬高上限后会自动恢复刹车暂停的成员;但若最新估算仍压过新
      // 上限(极端竞态),就如实说"仍超上限",不假装已恢复。
      toast.success(
        b.exceeded
          ? t("budget.bannerRaisedStillExceeded", {
              amount: fmtCost(raiseTo),
              defaultValue: "已调高到 {{amount}},但最新估算仍超上限",
            })
          : t("budget.bannerRaised", {
              amount: fmtCost(raiseTo),
              defaultValue: "预算已调高到 {{amount}},成员恢复运行",
            }),
      );
    } catch (e) {
      toast.error(t("budget.bannerRaiseFailed", { defaultValue: "调高预算失败" }), {
        description: e instanceof ApiError ? e.detail : (e as Error)?.message,
      });
    } finally {
      setRaising(false);
    }
  };

  return (
    <div
      role="status"
      aria-live="polite"
      className="flex h-9 shrink-0 items-center gap-2 border-b border-status-warning/40 bg-status-warning-soft/60 px-3"
    >
      <CircleDollarSign className="size-3.5 shrink-0 text-status-warning" aria-hidden />
      <span className="shrink-0 font-caption text-[11px] font-medium text-status-warning">
        {t("budget.bannerTitle", { defaultValue: "已达预算上限(估算),成员已暂停" })}
      </span>
      <span className="min-w-0 flex-1 truncate font-caption text-[11px] text-foreground-tertiary">
        {t("budget.bannerDetail", {
          cost: fmtCost(budget.trip_cost_usd ?? budget.current_cost_usd),
          budget: budget.budget_usd != null ? fmtCost(budget.budget_usd) : "—",
          defaultValue:
            "trip 时估算 {{cost}} / 上限 {{budget}};金额为估算,不等于订阅账单。调高预算或清除上限后成员自动恢复。",
        })}
      </span>
      {raiseTo != null && (
        <button
          type="button"
          onClick={raiseAndResume}
          disabled={raising}
          className="inline-flex shrink-0 items-center gap-1 rounded px-1.5 py-1 font-caption text-[11px] font-medium text-status-warning transition-colors hover:bg-surface-tertiary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent-primary/40 disabled:opacity-50"
        >
          {raising && <Loader2 className="size-3 animate-spin" aria-hidden />}
          {t("budget.bannerRaiseResume", {
            amount: fmtCost(raiseTo),
            defaultValue: "调高到 {{amount}} 并恢复",
          })}
        </button>
      )}
      <Link
        to="/usage"
        className="shrink-0 rounded px-1.5 py-1 font-caption text-[11px] font-medium text-status-warning transition-colors hover:bg-surface-tertiary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent-primary/40"
      >
        {t("budget.bannerAction", { defaultValue: "去调整预算" })}
      </Link>
    </div>
  );
}
