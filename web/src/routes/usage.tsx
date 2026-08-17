/**
 * Usage / Cost page (`/usage`).
 *
 * swarmx has no spend data from claude/codex (PTY transport, not an API), so
 * the server scrapes per-turn token counts from each worker's session JSONL
 * into `agent_usage` and prices them at query time (GET /api/usage). This page
 * renders that: headline stat cards, a per-day token trend (pure-CSS bars, no
 * chart lib), and per-model / per-agent breakdowns. Cost is an ESTIMATE from a
 * built-in price table; unrecognised models show tokens only.
 *
 * Coverage honesty (F1): the transcript scraper only knows claude/codex/kimi
 * (see lib/usageCoverage.ts). Agents on other engines NEVER appear in the
 * usage data — not "they cost nothing". The per-agent table lists them with
 * 「不计入」 instead of an absent/0 row, and a note near the totals says their
 * real spend is neither shown nor brake-enforced.
 */
import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { Link } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { BarChart3, ChevronDown, Loader2, RefreshCw, RotateCcw, Save } from "lucide-react";
import { api, ApiError } from "@/api/http";
import { roleDisplayName } from "@/lib/agent";
import { fmtTokens } from "@/lib/format";
import { toast } from "@/lib/toast";
import { uncollectedEngines, usageIsCollected } from "@/lib/usageCoverage";
import type {
  AgentInfo,
  UsageAgentRow,
  UsagePricingResponse,
  UsagePricingRule,
  UsageSummary,
  WorkspaceBudget,
} from "@/api/types";
import { cn } from "@/lib/cn";
import { useToolWorkspaces } from "@/lib/useToolWorkspaces";
import { WorkspacePicker } from "@/components/WorkspacePicker";
import { EmptyState } from "@/components/EmptyState";
import {
  ConfirmActionDialog,
  type ConfirmActionState,
} from "@/components/ConfirmActionDialog";

function fmtCost(n: number): string {
  if (n === 0) return "$0";
  if (n < 0.01) return "<$0.01";
  return `$${n.toFixed(2)}`;
}
function fmtRate(n: number): string {
  return Number.isInteger(n) ? String(n) : String(Number(n.toFixed(3)));
}
/** "152.0k / 200.0k" — peak context occupancy vs the model's cap. */
function fmtCtxPeak(peak: number, cap: number | null): string {
  if (!peak) return "—";
  return cap ? `${fmtTokens(peak)} / ${fmtTokens(cap)}` : fmtTokens(peak);
}
/** Prompt-cache hit rate = cache_read / (input + cache_read): the share of
 *  read input tokens served from the (near-free) cache vs full-price fresh
 *  input — the most direct "how much did caching save me" signal. Returns "—"
 *  when there's no read input at all, so an empty window reads as "no data"
 *  rather than a misleading 0%. */
function fmtCacheHit(input: number, cacheRead: number): string {
  const denom = input + cacheRead;
  if (denom <= 0) return "—";
  return `${Math.round((cacheRead / denom) * 100)}%`;
}
/** Collapse a `$HOME` prefix to `~` so the displayed pricing path doesn't leak
 *  the user's home dir (e.g. `/Users/jane/.swarmx/...` → `~/.swarmx/...`).
 *  The browser can't read $HOME, so we pattern-match the platform home roots. */
function foldHome(path: string): string {
  const m = path.match(/^(\/Users\/[^/]+|\/home\/[^/]+|[A-Za-z]:[\\/]Users[\\/][^\\/]+)/);
  return m ? `~${path.slice(m[0].length)}` : path;
}
/** Friendly message from an ApiError (server `detail`) or any thrown error. */
function errMsg(e: unknown): string {
  return e instanceof ApiError ? e.detail : (e as Error).message;
}
/** "2026-06-07" → "6/7" for a compact x-axis tick. */
type DayDatum = { day: string; tokens: number; input: number; output: number };
const UsageTrendChart = lazy(() =>
  import("@/components/UsageTrendChart").then((m) => ({
    default: m.UsageTrendChart,
  })),
);

function StatCard({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return (
    <div className="flex flex-col gap-1 rounded-lg border border-border-subtle bg-surface-secondary px-4 py-3">
      <span className="font-caption text-[11px] uppercase tracking-wide text-foreground-tertiary">
        {label}
      </span>
      <span className="font-mono text-xl text-foreground-primary">{value}</span>
      {hint && <span className="font-caption text-[11px] text-foreground-tertiary">{hint}</span>}
    </div>
  );
}

export default function UsageRoute() {
  const { t } = useTranslation();
  const { workspaces, wsId, setWsId } = useToolWorkspaces();
  const [data, setData] = useState<UsageSummary | null>(null);
  // F1: all agents (any engine) so uncollected-engine members can be shown as
  // 「不计入」 instead of vanishing from the per-agent table. Best-effort — a
  // failed listAgents only loses the honesty note, never the usage numbers.
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [err, setErr] = useState(false);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [updatedAt, setUpdatedAt] = useState<number | null>(null);
  // Stale-response guard: bump on every load(); a slower in-flight request
  // whose id no longer matches the latest must not clobber fresher state.
  const reqIdRef = useRef(0);
  const refreshingRef = useRef(false);
  const [pricing, setPricing] = useState<UsagePricingResponse | null>(null);
  const [pricingDraft, setPricingDraft] = useState<UsagePricingRule[]>([]);
  const [pricingSaving, setPricingSaving] = useState(false);
  const [pricingError, setPricingError] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<ConfirmActionState | null>(null);
  // 预算刹车:仅当选中单个工作空间时有意义("全部"视图跨空间,不挂预算)。
  const [budget, setBudget] = useState<WorkspaceBudget | null>(null);
  const [budgetDraft, setBudgetDraft] = useState("");
  const [budgetSaving, setBudgetSaving] = useState(false);
  const [budgetError, setBudgetError] = useState<string | null>(null);

  const load = useCallback(
    async (showSpinner = false, interactive = false) => {
      // Manual refresh: ignore re-clicks while one is already in flight.
      if (interactive && refreshingRef.current) return;
      const reqId = ++reqIdRef.current;
      if (showSpinner) setLoading(true);
      if (interactive) {
        refreshingRef.current = true;
        setRefreshing(true);
      }
      try {
        const d = await api.getUsage(wsId || undefined);
        if (reqId !== reqIdRef.current) return; // a newer load() superseded us
        setData(d);
        setErr(false);
        setUpdatedAt(Date.now());
        // 引擎覆盖名单搭同一趟轮询:页面开着时新 spawn 的 zulu 成员也能
        // 及时出现「不计入」行。失败只丢诚实提示,绝不拖垮用量数字。
        api
          .listAgents()
          .then((list) => {
            if (reqId === reqIdRef.current) setAgents(list);
          })
          .catch(() => {});
      } catch (e) {
        if (reqId !== reqIdRef.current) return;
        setErr(true);
        // A manual refresh that fails must be visible — the page may still be
        // showing older numbers, so a silent failure looks like "nothing happened".
        if (interactive) toast.error(t("usage.refreshFailed"), { description: errMsg(e) });
      } finally {
        // Clear loading when WE are the latest request — NOT only if we were the
        // spinner load. Gating on `showSpinner` here stranded the page on a
        // permanent "Loading…" whenever a spinner load got superseded by a
        // non-spinner poll (e.g. opening /usage in a background tab, then
        // foregrounding): the spinner load bailed as stale and the superseding
        // poll never owned the spinner, so nobody cleared it. Whoever finishes
        // last and is still current owns the clear. The interactive flag belongs
        // to THIS click, so always release it.
        if (reqId === reqIdRef.current) setLoading(false);
        if (interactive) {
          refreshingRef.current = false;
          setRefreshing(false);
        }
      }
    },
    [wsId, t],
  );

  // Live-ish: first load (with spinner) on workspace change, then poll so the
  // cost/token numbers don't sit frozen while workers keep burning tokens.
  // Visibility-aware: pause the poll while the tab is hidden (no point hammering
  // the API from a background tab) and refresh immediately on return so you
  // never stare at stale numbers.
  useEffect(() => {
    load(true);
    let id: number | undefined;
    const start = () => {
      if (id == null) id = window.setInterval(() => load(false), 8000);
    };
    const stop = () => {
      if (id != null) {
        window.clearInterval(id);
        id = undefined;
      }
    };
    const onVisibility = () => {
      if (document.hidden) {
        stop();
      } else {
        load(false);
        start();
      }
    };
    if (!document.hidden) start();
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      stop();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [load]);

  const loadPricing = useCallback(async () => {
    try {
      const p = await api.getUsagePricing();
      setPricing(p);
      setPricingDraft(p.rules);
      setPricingError(null);
    } catch (e) {
      setPricingError(errMsg(e));
    }
  }, []);

  useEffect(() => {
    loadPricing();
  }, [loadPricing]);

  // 预算刹车:跟随 wsId 拉取;保存/清除后服务端会广播 budget_changed,
  // 工作空间页的横幅随之刷新,本页用返回值直接更新。
  const budgetDirtyRef = useRef(false);
  const loadBudget = useCallback(async () => {
    if (!wsId) {
      setBudget(null);
      setBudgetError(null);
      budgetDirtyRef.current = false;
      setBudgetDraft("");
      return;
    }
    try {
      const b = await api.getWorkspaceBudget(wsId);
      setBudget(b);
      setBudgetError(null);
      // 不覆盖用户正在编辑的草稿 —— 只在草稿未脏时回填。
      if (!budgetDirtyRef.current) {
        setBudgetDraft(b.budget_usd != null ? String(b.budget_usd) : "");
      }
    } catch (e) {
      setBudgetError(errMsg(e));
    }
  }, [wsId]);

  useEffect(() => {
    budgetDirtyRef.current = false;
    loadBudget();
  }, [loadBudget]);

  const saveBudget = async (raw: string) => {
    if (!wsId) return;
    const trimmed = raw.trim();
    // 空 = 清除上限(unlimited)。其余必须是非负有限数;0 也视为清除
    // (与服务端 NULL/0 = unlimited 的约定一致)。
    let value: number | null = null;
    if (trimmed !== "") {
      const n = Number(trimmed);
      if (!Number.isFinite(n) || n < 0) {
        setBudgetError(
          t("budget.invalidInput", { defaultValue: "请输入非负数字(留空或 0 表示不限)" }),
        );
        return;
      }
      value = n > 0 ? n : null;
    }
    setBudgetSaving(true);
    setBudgetError(null);
    try {
      const b = await api.putWorkspaceBudget(wsId, value);
      setBudget(b);
      budgetDirtyRef.current = false;
      setBudgetDraft(b.budget_usd != null ? String(b.budget_usd) : "");
      toast.success(
        b.budget_usd != null
          ? t("budget.saved", { defaultValue: "预算已保存" })
          : t("budget.cleared", { defaultValue: "已清除预算上限" }),
      );
      // 估算可能随新用量越过新上限 —— 立刻重拉用量数字保持一致。
      await load(false);
    } catch (e) {
      setBudgetError(errMsg(e));
    } finally {
      setBudgetSaving(false);
    }
  };

  const pricingDirty =
    pricing != null && JSON.stringify(pricing.rules) !== JSON.stringify(pricingDraft);

  const savePricing = async () => {
    setPricingSaving(true);
    setPricingError(null);
    try {
      const p = await api.putUsagePricing(pricingDraft);
      setPricing(p);
      setPricingDraft(p.rules);
      await load(false);
    } catch (e) {
      setPricingError(errMsg(e));
    } finally {
      setPricingSaving(false);
    }
  };

  const resetPricing = async () => {
    setPricingSaving(true);
    setPricingError(null);
    try {
      const p = await api.resetUsagePricing();
      setPricing(p);
      setPricingDraft(p.rules);
      await load(false);
    } catch (e) {
      setPricingError(errMsg(e));
    } finally {
      setPricingSaving(false);
    }
  };

  const confirmResetPricing = () =>
    setConfirm({
      title: t("usage.confirmResetTitle"),
      description: t("usage.confirmResetDesc", {
        path: pricing ? foldHome(pricing.path) : "~/.swarmx/pricing.json",
      }),
      confirmLabel: t("usage.pricingReset"),
      variant: "destructive",
      onConfirm: resetPricing,
    });

  const chartData: DayDatum[] = (data?.by_day ?? []).map((d) => ({
    day: d.day,
    tokens: d.input_tokens + d.output_tokens,
    input: d.input_tokens,
    output: d.output_tokens,
  }));
  const workspaceById = useMemo(
    () => new Map(workspaces.map((w) => [w.id, w])),
    [workspaces],
  );
  // F1 诚实性:当前筛选范围内、引擎未被采集(opencode/reasonix/zulu…)的
  // agent。他们永远不会出现在 by_agent 里(后端不 tail 其 transcript),而
  // "整行缺席"读起来像"没花钱" —— 所以挑出来单独渲染:表里补「不计入」行,
  // 费用汇总旁挂小字说明。无 workspace_id 的历史行只在"全部"视图计入。
  const uncollectedAgents = useMemo(
    () =>
      agents.filter(
        (a) => (!wsId || a.workspace_id === wsId) && !usageIsCollected(a.cli),
      ),
    [agents, wsId],
  );
  const uncollectedNames = uncollectedEngines(uncollectedAgents);

  const perAgentRows: UsageTableRow[] = useMemo(() => {
    const rows: UsageTableRow[] = (data?.by_agent ?? []).map((a) => ({
      key: a.agent_id,
      cells: [
        <AgentUsageCell
          key="agent"
          row={a}
          workspaceSlug={
            a.workspace_id ? workspaceById.get(a.workspace_id)?.slug ?? null : null
          }
        />,
        fmtTokens(a.input_tokens),
        fmtTokens(a.output_tokens),
        String(a.events),
      ],
    }));
    // F1: 未采集引擎的 agent 没有 usage 事件,by_agent 里永远没有他们 ——
    // 补合成行,数字格统一渲染「不计入」(不是 0,也不是空白)。
    const listed = new Set((data?.by_agent ?? []).map((a) => a.agent_id));
    for (const a of uncollectedAgents) {
      if (listed.has(a.agent_id)) continue;
      const notCounted = (
        <span
          className="font-caption text-foreground-tertiary"
          title={t("usage.notCountedHint", {
            engine: a.cli,
            defaultValue:
              "{{engine}} 引擎暂不被采集用量:实际花费不显示,也不会触发预算刹车。",
          })}
        >
          {t("usage.notCounted", { defaultValue: "不计入" })}
        </span>
      );
      rows.push({
        key: `uncollected-${a.agent_id}`,
        cells: [
          <AgentUsageCell
            key="agent"
            row={{
              agent_id: a.agent_id,
              role: a.role,
              workspace_id: a.workspace_id,
              thread_id: a.thread_id,
              input_tokens: 0,
              output_tokens: 0,
              cache_read_tokens: 0,
              cache_write_tokens: 0,
              events: 0,
            }}
            cli={a.cli}
            workspaceSlug={
              a.workspace_id ? workspaceById.get(a.workspace_id)?.slug ?? null : null
            }
          />,
          notCounted,
          notCounted,
          notCounted,
        ],
      });
    }
    return rows;
  }, [data, uncollectedAgents, workspaceById, t]);

  return (
    <div className="h-full overflow-y-auto">
      <div className="mx-auto flex max-w-5xl flex-col gap-6 px-6 py-6">
        <header className="flex flex-col items-stretch gap-3 sm:flex-row sm:items-start sm:justify-between">
          <div className="flex flex-col gap-1">
            <h1 className="font-display text-lg text-foreground-primary">{t("usage.title")}</h1>
            <p className="font-caption text-xs text-foreground-tertiary">{t("usage.subtitle")}</p>
          </div>
          <div className="flex flex-wrap items-center justify-between gap-2 sm:shrink-0 sm:justify-end">
            {updatedAt && (
              <span className="font-caption text-[11px] text-foreground-tertiary sm:order-none">
                {t("usage.updatedAt", { time: new Date(updatedAt).toLocaleTimeString() })}
              </span>
            )}
            <button
              type="button"
              onClick={() => load(false, true)}
              disabled={refreshing}
              title={t("common.refresh")}
              aria-label={t("common.refresh")}
              className="inline-flex size-8 items-center justify-center rounded border border-border-subtle text-foreground-tertiary hover:bg-surface-tertiary hover:text-foreground-secondary disabled:opacity-50"
            >
              <RefreshCw className={cn("size-3.5", refreshing && "animate-spin")} />
            </button>
            <WorkspacePicker workspaces={workspaces} value={wsId} onChange={setWsId} allowAll />
          </div>
        </header>

        {/* 预算刹车:只对单个工作空间有意义("全部"是跨空间视图)。金额是
            估算,不等于订阅账单 —— 编辑器内文案保持这个诚实标签。 */}
        {wsId && (
          <BudgetEditor
            budget={budget}
            draft={budgetDraft}
            saving={budgetSaving}
            error={budgetError}
            uncollected={uncollectedNames}
            onDraftChange={(v) => {
              budgetDirtyRef.current = true;
              setBudgetDraft(v);
            }}
            onSave={() => saveBudget(budgetDraft)}
            onClear={() => saveBudget("")}
          />
        )}

        {loading && (
          <div className="font-caption text-sm text-foreground-tertiary">{t("common.loading")}</div>
        )}
        {/* Hard failure with nothing to show → "load failed + retry". */}
        {err && !data && !loading && (
          <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-border-subtle bg-surface-tertiary px-4 py-3 font-caption text-sm text-status-danger">
            <span>{t("usage.loadError")}</span>
            <button
              type="button"
              onClick={() => load(true, true)}
              disabled={refreshing}
              className="inline-flex min-h-7 items-center gap-1.5 rounded border border-border-subtle px-2 py-1 text-xs text-foreground-secondary hover:bg-surface-secondary disabled:opacity-50"
            >
              {refreshing ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <RefreshCw className="size-3.5" />
              )}
              {t("common.retry")}
            </button>
          </div>
        )}
        {/* Poll failed but we still have older numbers → don't hide them, mark stale. */}
        {err && data && (
          <div className="flex flex-wrap items-center gap-1.5 rounded-lg border border-status-warning/40 bg-surface-tertiary px-4 py-2 font-caption text-xs text-status-warning">
            <RefreshCw className="size-3.5" />
            <span>{t("usage.stale")}</span>
          </div>
        )}

        {data && !loading && data.totals.events === 0 && (
          <>
            <EmptyState
              icon={<BarChart3 className="size-8" />}
              title={t("usage.empty")}
              hint={t("usage.emptyHint")}
              primaryAction={{ label: t("usage.emptyAction"), href: "/chat" }}
            />
            {/* F1: 纯未采集引擎(如全 zulu)的空间事件数恒为 0 —— 空态旁必须
                交代"不是没花钱,是这些引擎不统计",否则空态读起来像免费。 */}
            {uncollectedNames.length > 0 && (
              <p className="-mt-4 text-center font-caption text-[11px] text-foreground-tertiary">
                {t("usage.uncollectedNote", {
                  engines: uncollectedNames.join(" / "),
                  defaultValue:
                    "{{engines}} 引擎暂不计入用量统计:这些成员的实际花费不显示在本页,也不会触发预算刹车。",
                })}
              </p>
            )}
          </>
        )}

        {data && !loading && data.totals.events > 0 && (
          // Stale (last poll errored) → dim the whole block so the numbers read
          // as "possibly outdated", in concert with the stale banner above.
          <div className={cn("flex flex-col gap-6", err && "opacity-60")}>
            {/* headline cards */}
            <section className="grid grid-cols-2 gap-3 md:grid-cols-5">
              <StatCard
                label={t("usage.totalCost")}
                // Partially-priced totals undercount (unpriced models contribute
                // tokens but $0), so the real spend is at least this much → "≥".
                value={`${data.totals.priced ? "" : "≥ "}${fmtCost(data.totals.cost_usd)}`}
                hint={data.totals.priced ? t("usage.estimated") : t("usage.costAtLeast")}
              />
              <StatCard
                label={t("usage.input")}
                value={fmtTokens(data.totals.input_tokens)}
                hint={`${t("usage.cacheRead")} ${fmtTokens(data.totals.cache_read_tokens)}`}
              />
              <StatCard label={t("usage.output")} value={fmtTokens(data.totals.output_tokens)} />
              <StatCard
                label={t("usage.cacheHitRate")}
                value={fmtCacheHit(
                  data.totals.input_tokens,
                  data.totals.cache_read_tokens,
                )}
                hint={t("usage.cacheHitHint")}
              />
              <StatCard label={t("usage.events")} value={String(data.totals.events)} />
              {/* F1: 汇总数字只覆盖已采集引擎;范围里有未采集成员时如实标注,
                  免得把"没采集"读成"总花费就这么多"。 */}
              {uncollectedNames.length > 0 && (
                <p className="col-span-full font-caption text-[11px] text-foreground-tertiary">
                  {t("usage.uncollectedNote", {
                    engines: uncollectedNames.join(" / "),
                    defaultValue:
                      "{{engines}} 引擎暂不计入用量统计:这些成员的实际花费不显示在本页,也不会触发预算刹车。",
                  })}
                </p>
              )}
            </section>

            {/* per-day trend (recharts) */}
            {chartData.length > 0 && (
              <section className="flex flex-col gap-2">
                <h2 className="flex flex-wrap items-baseline gap-x-2 font-caption text-xs tracking-wide text-foreground-tertiary">
                  {t("usage.byDay")}
                  {/* Server buckets rows by UTC calendar day; say so so a user in
                      a non-UTC zone doesn't read the day boundary as local. */}
                  <span className="text-[10px] normal-case tracking-normal text-foreground-tertiary/80">
                    {t("usage.byDayUtc")}
                  </span>
                </h2>
                {/* The recharts bars are mouse-hover-only and not keyboard/SR
                    reachable. Give assistive tech an equivalent, lossless data
                    path: a visually-hidden per-day table that mirrors the chart
                    (read without hovering). aria-hidden on the chart container
                    avoids the SVG being announced as noise. */}
                <div
                  className="h-48 rounded-lg border border-border-subtle bg-surface-secondary p-3"
                  aria-hidden="true"
                >
                  <Suspense
                    fallback={
                      <div className="flex h-full items-center justify-center font-caption text-xs text-foreground-tertiary">
                        {t("common.loading")}
                      </div>
                    }
                  >
                    <UsageTrendChart data={chartData} />
                  </Suspense>
                </div>
                <table className="sr-only">
                  <caption>
                    {t("usage.chartTableCaption", {
                      defaultValue: "每日 token 用量（按 UTC 日切分）",
                    })}
                  </caption>
                  <thead>
                    <tr>
                      <th scope="col">{t("usage.day", { defaultValue: "日期" })}</th>
                      <th scope="col">
                        {t("usage.totalTokens", { defaultValue: "总 token" })}
                      </th>
                      <th scope="col">{t("usage.input")}</th>
                      <th scope="col">{t("usage.output")}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {chartData.map((d) => (
                      <tr key={d.day}>
                        <th scope="row">{d.day}</th>
                        <td>{fmtTokens(d.tokens)}</td>
                        <td>{fmtTokens(d.input)}</td>
                        <td>{fmtTokens(d.output)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </section>
            )}

            {/* per-model */}
            <section className="flex flex-col gap-2">
              <h2 className="font-caption text-xs tracking-wide text-foreground-tertiary">
                {t("usage.byModel")}
              </h2>
              <UsageTable
                cols={[t("usage.model"), t("usage.input"), t("usage.output"), t("usage.cacheRead"), t("usage.cacheHitRate"), t("usage.ctxPeak"), t("usage.totalCost")]}
                rows={data.by_model.map((m, i) => ({
                  key: `${m.model ?? "unknown"}-${i}`,
                  cells: [
                    m.model ?? "—",
                    fmtTokens(m.input_tokens),
                    fmtTokens(m.output_tokens),
                    fmtTokens(m.cache_read_tokens),
                    fmtCacheHit(m.input_tokens, m.cache_read_tokens),
                    fmtCtxPeak(m.context_peak, m.context_window),
                    m.priced ? fmtCost(m.cost_usd) : t("usage.tokensOnly"),
                  ],
                }))}
              />
            </section>

            {/* per-agent */}
            <section className="flex flex-col gap-2">
              <h2 className="font-caption text-xs tracking-wide text-foreground-tertiary">
                {t("usage.byAgent")}
              </h2>
              <UsageTable
                cols={[t("usage.agent"), t("usage.input"), t("usage.output"), t("usage.events")]}
                rows={perAgentRows}
              />
            </section>
          </div>
        )}

        {/* 价目编辑是高级项：默认收起，别在空态下面摊一墙数字。 */}
        {pricing && (
          <PricingEditor
            pricing={pricing}
            rules={pricingDraft}
            dirty={pricingDirty}
            saving={pricingSaving}
            error={pricingError}
            onChange={setPricingDraft}
            onSave={savePricing}
            onReset={confirmResetPricing}
          />
        )}
      </div>
      <ConfirmActionDialog action={confirm} onOpenChange={(open) => !open && setConfirm(null)} />
    </div>
  );
}

type UsageTableRow = { key: string; cells: ReactNode[] };

function BudgetEditor({
  budget,
  draft,
  saving,
  error,
  uncollected,
  onDraftChange,
  onSave,
  onClear,
}: {
  budget: WorkspaceBudget | null;
  draft: string;
  saving: boolean;
  error: string | null;
  /** F1: 当前空间内未被用量采集的引擎 id 列表 —— 预算刹车按 usage 事件
   *  触发,覆盖不到这些引擎的成员,必须如实告知。 */
  uncollected: string[];
  onDraftChange: (v: string) => void;
  onSave: () => void;
  onClear: () => void;
}) {
  const { t } = useTranslation();
  const exceeded = budget?.exceeded ?? false;
  return (
    <section
      className={cn(
        "flex flex-col gap-3 rounded-lg border px-4 py-3",
        exceeded
          ? "border-status-warning/45 bg-status-warning-soft/45"
          : "border-border-subtle bg-surface-secondary/60",
      )}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex min-w-0 flex-col gap-0.5">
          <h2 className="font-caption text-xs font-medium uppercase tracking-wide text-foreground-tertiary">
            {t("budget.editorTitle", { defaultValue: "预算刹车(估算)" })}
          </h2>
          <p className="font-caption text-[11px] text-foreground-tertiary">
            {t("budget.editorHint", {
              defaultValue:
                "到达上限时暂停本工作空间全部成员,并拒绝新成员/新回合;调高或清除后自动恢复。金额为估算,不等于订阅账单。",
            })}
          </p>
          {uncollected.length > 0 && (
            <p className="font-caption text-[11px] text-foreground-tertiary">
              {t("budget.uncollectedNote", {
                engines: uncollected.join(" / "),
                defaultValue:
                  "注意:{{engines}} 引擎暂不计入用量统计,预算刹车覆盖不到这些成员(花费不显示,也不触发刹车)。",
              })}
            </p>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <label className="flex items-center gap-1.5 font-caption text-[11px] text-foreground-tertiary">
            {t("budget.capLabel", { defaultValue: "上限 (USD)" })}
            <input
              name="workspace-budget"
              aria-label={t("budget.capLabel", { defaultValue: "上限 (USD)" })}
              type="number"
              min="0"
              step="1"
              placeholder={t("budget.unlimited", { defaultValue: "不限" })}
              value={draft}
              onChange={(e) => onDraftChange(e.target.value)}
              className="h-8 w-28 rounded border border-border-subtle bg-surface-primary px-2 text-right font-mono text-[12px] text-foreground-primary"
            />
          </label>
          <button
            type="button"
            onClick={onSave}
            disabled={saving}
            className="inline-flex min-h-8 items-center gap-1.5 rounded border border-border-subtle bg-surface-primary px-2 py-1 font-caption text-xs text-foreground-primary hover:bg-surface-tertiary disabled:opacity-50"
          >
            {saving ? <Loader2 className="size-3.5 animate-spin" /> : <Save className="size-3.5" />}
            {saving ? t("common.saving") : t("common.save")}
          </button>
          {budget?.budget_usd != null && (
            <button
              type="button"
              onClick={onClear}
              disabled={saving}
              className="inline-flex min-h-8 items-center gap-1.5 rounded border border-border-subtle px-2 py-1 font-caption text-xs text-foreground-secondary hover:bg-surface-tertiary disabled:opacity-50"
            >
              <RotateCcw className="size-3.5" />
              {t("budget.clear", { defaultValue: "清除上限" })}
            </button>
          )}
        </div>
      </div>
      <div className="flex flex-wrap items-baseline gap-x-4 gap-y-1 font-caption text-[11px] text-foreground-tertiary">
        <span>
          {t("budget.currentEstimate", { defaultValue: "当前估算花费" })}{" "}
          <span className="font-mono text-foreground-secondary">
            {budget ? `${budget.priced ? "" : "≥ "}${fmtCost(budget.current_cost_usd)}` : "—"}
          </span>
        </span>
        <span>
          {t("budget.currentCap", { defaultValue: "当前上限" })}{" "}
          <span className="font-mono text-foreground-secondary">
            {budget?.budget_usd != null
              ? fmtCost(budget.budget_usd)
              : t("budget.unlimited", { defaultValue: "不限" })}
          </span>
        </span>
      </div>
      {exceeded && (
        <div className="rounded border border-status-warning/45 bg-status-warning-soft px-3 py-2 font-caption text-xs text-status-warning">
          {t("budget.exceededNote", {
            cost: fmtCost(budget?.trip_cost_usd ?? budget?.current_cost_usd ?? 0),
            defaultValue:
              "刹车已触发(trip 时估算 {{cost}}):成员已暂停,新任务被拒绝。调高预算或清除上限后,成员自动恢复。",
          })}
        </div>
      )}
      {error && (
        <div className="rounded border border-border-subtle bg-surface-tertiary px-3 py-2 font-caption text-xs text-status-danger">
          {error}
        </div>
      )}
    </section>
  );
}

function PricingEditor({
  pricing,
  rules,
  dirty,
  saving,
  error,
  onChange,
  onSave,
  onReset,
}: {
  pricing: UsagePricingResponse;
  rules: UsagePricingRule[];
  dirty: boolean;
  saving: boolean;
  error: string | null;
  onChange: (rules: UsagePricingRule[]) => void;
  onSave: () => void;
  onReset: () => void;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [compact, setCompact] = useState(() =>
    typeof window !== "undefined"
      ? window.matchMedia("(max-width: 767.98px)").matches
      : false,
  );
  useEffect(() => {
    if (typeof window === "undefined") return;
    const mq = window.matchMedia("(max-width: 767.98px)");
    const onChange = () => setCompact(mq.matches);
    onChange();
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);
  const updateRate = (
    index: number,
    key: keyof UsagePricingRule["rates_usd_per_mtok"],
    raw: string,
  ) => {
    const value = Number(raw);
    onChange(
      rules.map((rule, i) =>
        i === index
          ? {
              ...rule,
              rates_usd_per_mtok: {
                ...rule.rates_usd_per_mtok,
                [key]: Number.isFinite(value) ? value : 0,
              },
            }
          : rule,
      ),
    );
  };

  const updateWindow = (index: number, raw: string) => {
    const value = raw.trim() === "" ? null : Number(raw);
    onChange(
      rules.map((rule, i) =>
        i === index
          ? {
              ...rule,
              context_window:
                value == null || !Number.isFinite(value) ? null : Math.max(0, Math.round(value)),
            }
          : rule,
      ),
    );
  };
  const rateLabel = (
    rule: UsagePricingRule,
    key: keyof UsagePricingRule["rates_usd_per_mtok"],
  ) => {
    const labels: Record<keyof UsagePricingRule["rates_usd_per_mtok"], string> = {
      input: t("usage.input"),
      output: t("usage.output"),
      cache_read: t("usage.cacheRead"),
      cache_write: t("usage.cacheWrite"),
    };
    return `${rule.label} ${labels[key]} (${pricing.unit})`;
  };
  const contextLabel = (rule: UsagePricingRule) => `${rule.label} ${t("usage.ctx")}`;

  return (
    <section className="flex flex-col gap-2 rounded-lg border border-border-subtle bg-surface-secondary/60">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="flex w-full items-center gap-2 px-4 py-3 text-left transition-colors hover:bg-surface-secondary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent-primary/40"
      >
        <div className="min-w-0 flex-1">
          <h2 className="font-caption text-xs font-medium uppercase tracking-wide text-foreground-tertiary">
            {t("usage.pricingTitle")}
            <span className="ml-2 font-normal normal-case tracking-normal">
              {t("usage.pricingAdvanced", { defaultValue: "高级 · 改估算单价" })}
            </span>
          </h2>
          {!open && (
            <p className="mt-0.5 truncate font-caption text-[11px] text-foreground-tertiary">
              {t("usage.pricingMeta", {
                unit: pricing.unit,
                source:
                  pricing.source === "user"
                    ? t("usage.pricingUser")
                    : t("usage.pricingDefault"),
                path: foldHome(pricing.path),
              })}
            </p>
          )}
        </div>
        <ChevronDown
          className={cn(
            "size-4 shrink-0 text-foreground-tertiary transition-transform duration-200",
            open && "rotate-180",
          )}
          aria-hidden
        />
      </button>
      {open && (
        <div className="flex flex-col gap-2 border-t border-border-subtle px-4 pb-4 pt-3">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <p className="font-caption text-[11px] text-foreground-tertiary">
            {t("usage.pricingMetaOpen", {
              unit: pricing.unit,
              source:
                pricing.source === "user"
                  ? t("usage.pricingUser")
                  : t("usage.pricingDefault"),
              path: foldHome(pricing.path),
            })}
          </p>
          {pricing.fallback && pricing.fallback.models > 0 && (
            <p className="mt-0.5 font-caption text-[11px] text-foreground-tertiary">
              {t("usage.pricingFallback", {
                count: pricing.fallback.models,
                origin:
                  pricing.fallback.origin === "disk"
                    ? t("usage.pricingFallbackOriginDisk")
                    : pricing.fallback.origin === "refreshed"
                      ? t("usage.pricingFallbackOriginRefreshed")
                      : t("usage.pricingFallbackOriginEmbedded"),
                auto:
                  pricing.fallback.auto_refresh === false
                    ? t("usage.pricingFallbackAutoOff")
                    : t("usage.pricingFallbackAutoOn"),
              })}
            </p>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            onClick={onReset}
            disabled={saving}
            className="inline-flex min-h-8 items-center gap-1.5 rounded border border-border-subtle px-2 py-1 font-caption text-xs text-foreground-secondary hover:bg-surface-tertiary disabled:opacity-50"
          >
            <RotateCcw className="size-3.5" />
            {t("usage.pricingReset")}
          </button>
          <button
            type="button"
            onClick={onSave}
            disabled={saving || !dirty}
            className="inline-flex min-h-8 items-center gap-1.5 rounded border border-border-subtle bg-surface-primary px-2 py-1 font-caption text-xs text-foreground-primary hover:bg-surface-tertiary disabled:opacity-50"
          >
            {saving ? <Loader2 className="size-3.5 animate-spin" /> : <Save className="size-3.5" />}
            {saving ? t("common.saving") : t("common.save")}
          </button>
        </div>
      </div>
      {error && (
        <div className="rounded border border-border-subtle bg-surface-tertiary px-3 py-2 font-caption text-xs text-status-danger">
          {error}
        </div>
      )}
      {compact ? (
        <div className="grid gap-3">
          {rules.map((rule, index) => (
            <div
              key={rule.id}
              className="rounded-md border border-border-subtle bg-surface-primary p-3"
            >
              <div className="flex min-w-0 flex-col gap-0.5">
                <span className="truncate font-mono text-[12px] text-foreground-primary">
                  {rule.label}
                </span>
                <span className="truncate font-mono text-[10px] text-foreground-tertiary">
                  {rule.provider} · {rule.matchers.join(", ")}
                </span>
              </div>
              <div className="mt-3 grid grid-cols-2 gap-2">
                {(["input", "output", "cache_read", "cache_write"] as const).map((key) => (
                  <label key={key} className="flex min-w-0 flex-col gap-1">
                    <span className="font-caption text-[10px] uppercase text-foreground-tertiary">
                      {rateLabel(rule, key)}
                    </span>
                    <input
                      name={`pricing-${rule.id}-${key}`}
                      aria-label={rateLabel(rule, key)}
                      type="number"
                      min="0"
                      step="0.001"
                      value={fmtRate(rule.rates_usd_per_mtok[key])}
                      onChange={(e) => updateRate(index, key, e.target.value)}
                      className="h-9 min-w-0 rounded border border-border-subtle bg-surface-secondary px-2 text-right font-mono text-[12px] text-foreground-primary"
                    />
                  </label>
                ))}
                <label className="col-span-2 flex min-w-0 flex-col gap-1">
                  <span className="font-caption text-[10px] uppercase text-foreground-tertiary">
                    {contextLabel(rule)}
                  </span>
                  <input
                    name={`pricing-${rule.id}-context-window`}
                    aria-label={contextLabel(rule)}
                    type="number"
                    min="0"
                    step="1000"
                    value={rule.context_window ?? ""}
                    onChange={(e) => updateWindow(index, e.target.value)}
                    className="h-9 min-w-0 rounded border border-border-subtle bg-surface-secondary px-2 text-right font-mono text-[12px] text-foreground-primary"
                  />
                </label>
              </div>
            </div>
          ))}
        </div>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[760px] border-collapse font-mono text-[12px]">
          <thead>
            <tr className="border-b border-border-subtle text-foreground-tertiary">
              <th className="px-2 py-2 text-left font-caption text-[11px] uppercase">
                {t("usage.pricingRule")}
              </th>
              <th className="px-2 py-2 text-left font-caption text-[11px] uppercase">
                {t("usage.pricingMatchers")}
              </th>
              <th className="px-2 py-2 text-right font-caption text-[11px] uppercase">
                {t("usage.input")}
              </th>
              <th className="px-2 py-2 text-right font-caption text-[11px] uppercase">
                {t("usage.output")}
              </th>
              <th className="px-2 py-2 text-right font-caption text-[11px] uppercase">
                {t("usage.cacheRead")}
              </th>
              <th className="px-2 py-2 text-right font-caption text-[11px] uppercase">
                {t("usage.cacheWrite")}
              </th>
              <th className="px-2 py-2 text-right font-caption text-[11px] uppercase">
                {t("usage.ctx")}
              </th>
            </tr>
          </thead>
          <tbody>
            {rules.map((rule, index) => (
              <tr key={rule.id} className="border-b border-border-subtle last:border-0">
                <td className="px-2 py-2 align-top">
                  <div className="flex min-w-0 flex-col">
                    <span className="truncate text-foreground-primary">{rule.label}</span>
                    <span className="truncate text-[10px] text-foreground-tertiary">
                      {rule.provider}
                    </span>
                  </div>
                </td>
                <td className="max-w-[220px] px-2 py-2 align-top text-[11px] text-foreground-tertiary">
                  {rule.matchers.join(", ")}
                </td>
                {(["input", "output", "cache_read", "cache_write"] as const).map((key) => (
                  <td key={key} className="px-2 py-2 align-top">
                    <input
                      name={`pricing-${rule.id}-${key}`}
                      aria-label={rateLabel(rule, key)}
                      title={rateLabel(rule, key)}
                      type="number"
                      min="0"
                      step="0.001"
                      value={fmtRate(rule.rates_usd_per_mtok[key])}
                      onChange={(e) => updateRate(index, key, e.target.value)}
                      className="h-8 w-20 rounded border border-border-subtle bg-surface-primary px-2 text-right text-foreground-primary"
                    />
                  </td>
                ))}
                <td className="px-2 py-2 align-top">
                  <input
                    name={`pricing-${rule.id}-context-window`}
                    aria-label={contextLabel(rule)}
                    title={contextLabel(rule)}
                    type="number"
                    min="0"
                    step="1000"
                    value={rule.context_window ?? ""}
                    onChange={(e) => updateWindow(index, e.target.value)}
                    className="h-8 w-24 rounded border border-border-subtle bg-surface-primary px-2 text-right text-foreground-primary"
                  />
                </td>
              </tr>
            ))}
          </tbody>
          </table>
        </div>
      )}
        </div>
      )}
    </section>
  );
}

function AgentUsageCell({
  row,
  workspaceSlug,
  cli,
}: {
  row: UsageAgentRow;
  workspaceSlug: string | null;
  /** F1: 未采集引擎的合成行把引擎名带进第二行,让「不计入」看得出是谁。 */
  cli?: string;
}) {
  const label = row.role ? roleDisplayName(row.role) : row.agent_id.slice(0, 12);
  const body = (
    <span className="flex min-w-0 flex-col">
      <span className="truncate font-mono text-foreground-primary">{label}</span>
      <span className="truncate font-mono text-[10px] text-foreground-tertiary">
        {cli ? `${cli} · ${row.agent_id.slice(0, 12)}` : row.agent_id.slice(0, 12)}
      </span>
    </span>
  );
  if (!workspaceSlug) return body;
  return (
    <Link
      to={`/chat/${workspaceSlug}?agent=${encodeURIComponent(row.agent_id)}`}
      className="inline-flex min-w-0 max-w-full hover:underline"
    >
      {body}
    </Link>
  );
}

function UsageTable({
  cols,
  rows,
}: {
  cols: string[];
  rows: UsageTableRow[];
}) {
  return (
    <div className="overflow-hidden rounded-lg border border-border-subtle">
      <table className="w-full border-collapse font-mono text-[12px]">
        <thead>
          <tr className="bg-surface-tertiary">
            {cols.map((c, i) => (
              <th
                key={c}
                className={cn(
                  "px-3 py-2 font-caption text-[11px] uppercase tracking-wide text-foreground-tertiary",
                  i === 0 ? "text-left" : "text-right",
                )}
              >
                {c}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.key} className="border-t border-border-subtle">
              {r.cells.map((cell, ci) => (
                <td
                  key={ci}
                  className={cn(
                    "px-3 py-1.5 text-foreground-secondary",
                    ci === 0 ? "text-left text-foreground-primary" : "text-right",
                  )}
                >
                  {cell}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
