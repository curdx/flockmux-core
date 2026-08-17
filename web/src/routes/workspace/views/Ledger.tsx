/**
 * Ledger view — orchestrator 的双 ledger 主面板,Magentic-One 模式核心 UI。
 *
 * 左右分栏:
 *   - 左: Task Ledger (blackboard key `task.ledger.md`) — facts / guesses /
 *         acceptance / plan
 *   - 右: Progress Ledger (`progress.ledger.md`) — status / current_step /
 *         assignments / blockers
 *
 * 数据来源都是 blackboard 直接读,跟 Context.tsx 复用同一套 api.readBlackboard
 * 接口。每次有 blackboard_changed 事件就 refetch — wake-coordinator 已经在
 * 推这个事件,我们只是把已有信息渲染成对用户友好的形态。
 *
 * 视觉是双卡片 + markdown 渲染 + 顶部 "最后更新 XX 秒前"。没有任何编辑能力 —
 * orchestrator 是唯一 writer,用户是 reader。
 *
 * R8 战报导出:顶栏「导出战报」把这一屏已有的数据(双台账 + 近况 + 成员
 * 名单)加上 getUsage 的费用估算拼成一份 markdown 下载 —— 不取新数据源,
 * 不做图片导出,就是给用户一份能贴给别人的总结。
 */

import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { ClipboardList, RefreshCw, Activity, Radio, Sparkles, Download } from "lucide-react";
import { api } from "../../../api/http";
import type { AgentInfo, BlackboardEntry, BlackboardSnapshot, UsageSummary } from "../../../api/types";
import {
  useLiveBbChanges,
  useSwarmRefresh,
} from "../../../hooks/useSwarmProjection";
import { Button } from "@/components/ui/button";
import { EmptyState } from "@/components/EmptyState";
import { cn } from "@/lib/cn";
import { roleDisplayName } from "@/lib/agent";
import { uncollectedEngines } from "@/lib/usageCoverage";
import { useWorkspaceContext } from "../Shell";
import { MarkdownInput, MarkdownLink } from "@/lib/markdownLinks";
import { downloadTextFile } from "@/lib/download";
import { fmtTokens } from "@/lib/format";
import { track } from "@/lib/telemetry";
import { toast } from "@/lib/toast";

function fmtAgo(at: number | null, nowTick: number, t: TFunction): string {
  if (at == null) return "—";
  const sec = Math.max(0, Math.floor((nowTick - at) / 1000));
  if (sec < 60) return t("ledger.agoSeconds", { n: sec, defaultValue: "{{n}}s 前" });
  const min = Math.floor(sec / 60);
  if (min < 60) return t("ledger.agoMinutes", { n: min, defaultValue: "{{n}}m 前" });
  const hr = Math.floor(min / 60);
  if (hr < 24) return t("ledger.agoHours", { n: hr, defaultValue: "{{n}}h 前" });
  const day = Math.floor(hr / 24);
  return t("ledger.agoDays", { n: day, defaultValue: "{{n}}d 前" });
}

interface LedgerSnap {
  content: string;
  at: number | null;
  error: string | null;
}

function emptySnap(): LedgerSnap {
  return { content: "", at: null, error: null };
}

/** The orchestrator writes the raw ledger with a "Task Ledger" / "Progress
 *  Ledger" heading as its first line; each card already titles it
 *  (任务台账 / 进展状态), so that inner heading is redundant. Drop a leading
 *  line that's just "<Task|Progress> Ledger" (optional markdown #). Everything
 *  else — Facts / Status / Plan … — is left intact. */
function stripLedgerHeading(content: string): string {
  return content.replace(/^\s*#{0,6}\s*(?:task|progress)\s+ledger\s*\r?\n+/i, "");
}

// ── R8 战报导出 ─────────────────────────────────────────────────────────

/** 战报里的成员状态一行 —— AgentInfo 字段直译(跟侧栏/DAG 同一套事实),
 *  不造新状态机。 */
function memberStatusLine(a: AgentInfo, t: TFunction): string {
  if (a.killed_at != null)
    return t("ledger.report.statusKilled", { defaultValue: "已终止" });
  if (a.shim_exit != null)
    return t("ledger.report.statusExited", {
      code: a.shim_exit,
      defaultValue: "已退出（码 {{code}}）",
    });
  if (a.shim_ready)
    return t("ledger.report.statusRunning", { defaultValue: "运行中" });
  return t("ledger.report.statusStarting", { defaultValue: "启动中" });
}

/** 把 Ledger 页已有的数据(双台账 + 近况 + 成员名单)加 getUsage 的费用估算
 *  拼成 markdown 战报。除用量外没有任何新数据源 —— 导出的就是用户屏幕上
 *  看到的东西,不是另一份"更全"的隐藏数据。 */
function buildBattleReport(opts: {
  t: TFunction;
  lang: string;
  workspaceName: string;
  threadSlug: string;
  members: AgentInfo[];
  task: LedgerSnap;
  progress: LedgerSnap;
  breadcrumbs: { role: string; content: string; at: number }[];
  usage: UsageSummary | null;
  now: Date;
}): string {
  const { t, lang, workspaceName, threadSlug, members, task, progress, breadcrumbs, usage, now } = opts;
  const L: string[] = [];
  const empty = `_${t("ledger.report.emptySection", { defaultValue: "（暂无内容）" })}_`;
  L.push(
    `# ${workspaceName} · ${t("ledger.report.title", { defaultValue: "战报" })}`,
  );
  L.push("");
  L.push(
    `- ${t("ledger.report.generatedAt", { defaultValue: "生成时间" })}：${now.toLocaleString(lang)}`,
  );
  L.push(
    `- ${t("ledger.report.direction", { defaultValue: "方向" })}：${threadSlug}`,
  );
  // 费用是后端按定价规则算出来的估算 —— 有模型没定价时只有 token 数,
  // 实际花费不低于这个数,所以标 "≥" 而不是假装精确。
  if (usage) {
    const cost = `$${usage.totals.cost_usd.toFixed(2)}`;
    L.push(
      `- ${t("ledger.report.cost", { defaultValue: "费用（估算）" })}：` +
        `${usage.totals.priced ? cost : `≥ ${cost}`} · ` +
        `${t("ledger.report.tokens", { defaultValue: "tokens（入/出）" })} ` +
        `${fmtTokens(usage.totals.input_tokens)} / ${fmtTokens(usage.totals.output_tokens)} · ` +
        `${t("ledger.report.events", { defaultValue: "事件" })} ${usage.totals.events}`,
    );
    // F1 诚实性:未采集引擎(opencode/reasonix/zulu…)的成员没有任何 usage
    // 记录,上面的费用不含他们的花费 —— 战报必须交代,否则读的人以为
    // 全员花费都在这一行里。
    const missing = uncollectedEngines(members);
    if (missing.length > 0) {
      L.push(
        `- ${t("ledger.report.uncollectedCost", {
          engines: missing.join(" / "),
          defaultValue: "不含 {{engines}} 引擎的花费（暂不采集）",
        })}`,
      );
    }
  } else {
    L.push(
      `- ${t("ledger.report.cost", { defaultValue: "费用（估算）" })}：` +
        t("ledger.report.usageUnavailable", { defaultValue: "读取失败" }),
    );
  }
  L.push("");
  L.push(
    `## ${t("ledger.report.members", { defaultValue: "成员" })}（${members.length}）`,
  );
  L.push("");
  if (members.length === 0) {
    L.push(empty);
  } else {
    for (const m of members) {
      L.push(
        `- ${roleDisplayName(m.role)} · ${m.cli} — ${memberStatusLine(m, t)}`,
      );
    }
  }
  L.push("");
  L.push(`## ${t("ledger.taskTitle", { defaultValue: "任务记录" })}`);
  L.push("");
  L.push(task.content.trim() || empty);
  L.push("");
  L.push(`## ${t("ledger.progressTitle", { defaultValue: "进展状态" })}`);
  L.push("");
  L.push(progress.content.trim() || empty);
  L.push("");
  L.push(`## ${t("ledger.breadcrumbsTitle", { defaultValue: "近况" })}`);
  L.push("");
  if (breadcrumbs.length === 0) {
    L.push(empty);
  } else {
    for (const b of breadcrumbs) {
      L.push(
        `- **${roleDisplayName(b.role)}**：${b.content}（${fmtAgo(b.at, now.getTime(), t)}）`,
      );
    }
  }
  L.push("");
  return L.join("\n");
}

export default function LedgerView() {
  const { t, i18n } = useTranslation();
  const { workspace, threadSlug } = useWorkspaceContext();
  // Direction-scoped blackboard paths. All workspaces + directions share one
  // blackboard tree, so the orchestrator writes (and we read) the ledger under
  // `<workspace_id>/<thread_slug>/...` — main direction's slug is `main`.
  const keyPrefix = `${workspace.workspaceId}/${threadSlug}/`;
  const taskKey = `${keyPrefix}task.ledger.md`;
  const progressKey = `${keyPrefix}progress.ledger.md`;
  const [task, setTask] = useState<LedgerSnap>(emptySnap());
  const [progress, setProgress] = useState<LedgerSnap>(emptySnap());
  // 近况 (worker breadcrumbs) — { role_label: { content, at } } keyed by
  // the part of the blackboard path before `.progress.md`. orchestrator
  // tells each worker to overwrite `<workspace_id>/<role>.progress.md`
  // at every milestone (deps installed, build passing, etc.) so this
  // pane lights up while npm install / cargo build / etc. are running.
  const [breadcrumbs, setBreadcrumbs] = useState<
    { role: string; content: string; at: number }[]
  >([]);
  const [refreshing, setRefreshing] = useState(false);
  // tick 用来让"XX 秒前"动起来,1s 一次刷新
  const [nowTick, setNowTick] = useState(Date.now());
  useEffect(() => {
    const i = window.setInterval(() => setNowTick(Date.now()), 1000);
    return () => window.clearInterval(i);
  }, []);

  // F19: guard setState against a load that resolves after the view unmounts
  // (tab switch). refresh/loadOne/loadBreadcrumbs all run from effects + swarm
  // callbacks, so a mounted-ref gate keeps them from poking a dead component.
  const mountedRef = useRef(true);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const loadOne = useCallback(
    async (
      key: string,
      entries: BlackboardEntry[],
      setter: (s: LedgerSnap) => void,
    ) => {
      // Drop the result if we've since unmounted.
      const set = (s: LedgerSnap) => {
        if (mountedRef.current) setter(s);
      };
      if (!entries.some((e) => e.path === key)) {
        set({ content: "", at: null, error: null });
        return;
      }
      try {
        const snap = (await api.readBlackboard(key)) as BlackboardSnapshot | null;
        if (snap) {
          set({ content: snap.content, at: snap.at, error: null });
        } else {
          set({ content: "", at: null, error: null });
        }
      } catch (e) {
        set({ content: "", at: null, error: (e as Error).message });
      }
    },
    [],
  );

  const loadBreadcrumbs = useCallback(async (entries?: BlackboardEntry[]) => {
    try {
      const all = entries ?? ((await api.listBlackboard()) as BlackboardEntry[]);
      const prefix = keyPrefix;
      const suffix = ".progress.md";
      const candidates = all.filter(
        (e) => e.path.startsWith(prefix) && e.path.endsWith(suffix),
      );
      const snaps = await Promise.all(
        candidates.map(async (e) => {
          try {
            const snap = (await api.readBlackboard(e.path)) as BlackboardSnapshot | null;
            if (!snap) return null;
            const role = e.path.slice(prefix.length, -suffix.length);
            return { role, content: snap.content.trim(), at: snap.at };
          } catch {
            return null;
          }
        }),
      );
      const rows = snaps.filter(
        (s): s is { role: string; content: string; at: number } => s !== null,
      );
      // newest first so the most recent worker activity is at the top
      rows.sort((a, b) => b.at - a.at);
      if (mountedRef.current) setBreadcrumbs(rows);
    } catch {
      if (mountedRef.current) setBreadcrumbs([]);
    }
  }, [keyPrefix]);

  // Incremental single-key reload, used by the event path so a worker
  // heartbeat doesn't trigger a full N+1 re-fetch of every breadcrumb. The
  // `blackboard_changed` event carries the exact path + op, so we read JUST
  // that one key and upsert/remove it. `op === "delete"` drops the row.
  const suffix = ".progress.md";
  const applyBreadcrumb = useCallback(
    async (path: string, op: string) => {
      const role = path.slice(keyPrefix.length, -suffix.length);
      if (op === "delete") {
        if (mountedRef.current) {
          setBreadcrumbs((prev) => prev.filter((b) => b.role !== role));
        }
        return;
      }
      try {
        const snap = (await api.readBlackboard(path)) as BlackboardSnapshot | null;
        if (!snap || !mountedRef.current) return;
        const row = { role, content: snap.content.trim(), at: snap.at };
        setBreadcrumbs((prev) => {
          const next = prev.filter((b) => b.role !== role);
          next.push(row);
          next.sort((a, b) => b.at - a.at); // newest first
          return next;
        });
      } catch {
        // a transient read failure just leaves the prior row in place
      }
    },
    [keyPrefix],
  );

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const entries = (await api.listBlackboard()) as BlackboardEntry[];
      await Promise.all([
        loadOne(taskKey, entries, setTask),
        loadOne(progressKey, entries, setProgress),
        loadBreadcrumbs(entries),
      ]);
    } catch (e) {
      if (mountedRef.current) {
        const msg = (e as Error).message;
        setTask({ content: "", at: null, error: msg });
        setProgress({ content: "", at: null, error: msg });
        setBreadcrumbs([]);
      }
    }
    if (mountedRef.current) setRefreshing(false);
  }, [loadOne, taskKey, progressKey, loadBreadcrumbs]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // Context compaction: summarize the (unbounded-growth) ledgers in place via a
  // headless small-model pass. Non-destructive — the blackboard op-log keeps the
  // pre-compaction version. A swarmx-shaped "context compression": the PTY
  // CLIs self-manage their own window; what grows here is the ledger state.
  const [compacting, setCompacting] = useState(false);
  const [compactNote, setCompactNote] = useState<string | null>(null);
  const [compactErr, setCompactErr] = useState(false);
  const compact = useCallback(async () => {
    setCompacting(true);
    setCompactNote(null);
    setCompactErr(false);
    try {
      // P0-7: don't swallow failures into a fake "已是最简，无需压缩". The backend
      // can return 402 (paid transport off), 503 (no claude plugin), 5xx — those
      // mean it never ran, not "nothing to do". Surface the real reason so the
      // user isn't told it succeeded when it didn't.
      const results = await Promise.allSettled([
        api.compactBlackboard(taskKey),
        api.compactBlackboard(progressKey),
      ]);
      if (!mountedRef.current) return;
      const ok = results.flatMap((r) => (r.status === "fulfilled" ? [r.value] : []));
      const errs = results.flatMap((r) => (r.status === "rejected" ? [r.reason] : []));
      if (ok.length === 0 && errs.length > 0) {
        const e = errs[0] as { detail?: string; message?: string };
        const why = e?.detail || e?.message || String(errs[0]);
        setCompactErr(true);
        setCompactNote(t("ledger.compactFailed", { msg: why, defaultValue: "压缩失败：{{msg}}" }));
      } else {
        const saved = ok
          .filter((r) => r && r.changed)
          .reduce((acc, r) => acc + (r.before_tokens - r.after_tokens), 0);
        setCompactNote(
          saved > 0
            ? t("ledger.compactSaved", { n: saved, defaultValue: "已压缩，省约 {{n}} tokens" })
            : t("ledger.compactNoop", { defaultValue: "已是最简，无需压缩" }),
        );
      }
      window.setTimeout(() => {
        if (!mountedRef.current) return;
        setCompactNote(null);
        setCompactErr(false);
      }, 6000);
      await refresh();
    } finally {
      if (mountedRef.current) setCompacting(false);
    }
  }, [taskKey, progressKey, refresh, t]);

  // R8 战报导出:页面已有数据 + getUsage 估算 → markdown 下载。用量接口
  // 挂了不该让整个导出失败 —— 报告里如实写「读取失败」就行。
  const [exporting, setExporting] = useState(false);
  const exportReport = useCallback(async () => {
    if (exporting) return;
    setExporting(true);
    try {
      const usage = await api
        .getUsage(workspace.workspaceId)
        .catch(() => null);
      const now = new Date();
      const md = buildBattleReport({
        t,
        lang: i18n.language,
        workspaceName: workspace.name,
        threadSlug,
        members: workspace.members,
        task,
        progress,
        breadcrumbs,
        usage,
        now,
      });
      // 文件名:<workspace>-战报-<date>.md。空格/路径分隔符折叠成 "-",
      // 各平台文件系统都安全(中文部分 macOS/Windows/Linux 都没问题)。
      const safe =
        workspace.name.trim().replace(/[\\/:*?"<>|\s]+/g, "-") || "workspace";
      downloadTextFile(
        `${safe}-战报-${now.toISOString().slice(0, 10)}.md`,
        md,
        "text/markdown",
      );
      track("report.export");
    } catch (e) {
      toast.error(
        t("ledger.report.failed", { defaultValue: "导出战报失败" }),
        { description: (e as Error)?.message },
      );
    } finally {
      if (mountedRef.current) setExporting(false);
    }
  }, [exporting, workspace, threadSlug, task, progress, breadcrumbs, t, i18n.language]);

  // 监听 blackboard_changed —— orchestrator 写 ledger 时立即重拉,
  // 别等用户手动 refresh。
  //
  // Incremental dispatch (was: full N+1 refresh on EVERY event):
  //   - a ledger key change reloads JUST that one ledger snapshot;
  //   - a single breadcrumb change upserts/removes JUST that row;
  // so a worker heartbeat costs 1 read, not `1 + 2 + N`. A trailing debounce
  // (80ms) coalesces the burst the orchestrator emits when it writes
  // task + progress + several assignments in one turn. The id-equality guard
  // still drops exact-duplicate redeliveries.
  const pendingRef = useRef<{ ledgers: Set<string>; crumbs: Map<string, string> }>(
    { ledgers: new Set(), crumbs: new Map() },
  );
  const debounceRef = useRef<number | null>(null);
  const flushPending = useCallback(() => {
    const { ledgers, crumbs } = pendingRef.current;
    pendingRef.current = { ledgers: new Set(), crumbs: new Map() };
    for (const key of ledgers) {
      const setter = key === taskKey ? setTask : setProgress;
      // Reuse loadOne's single-key read by faking a one-entry "present" list.
      void loadOne(key, [{ path: key } as BlackboardEntry], setter);
    }
    for (const [path, op] of crumbs) {
      void applyBreadcrumb(path, op);
    }
  }, [taskKey, loadOne, applyBreadcrumb]);
  const scheduleFlush = useCallback(() => {
    if (debounceRef.current != null) window.clearTimeout(debounceRef.current);
    debounceRef.current = window.setTimeout(() => {
      debounceRef.current = null;
      flushPending();
    }, 80);
  }, [flushPending]);
  useEffect(() => {
    return () => {
      if (debounceRef.current != null) window.clearTimeout(debounceRef.current);
    };
  }, []);
  useSwarmRefresh((s) => s.reconnectGen, refresh);
  useLiveBbChanges((ev) => {
    const isLedger = ev.path === taskKey || ev.path === progressKey;
    const isBreadcrumb =
      ev.path.startsWith(keyPrefix) && ev.path.endsWith(".progress.md");
    if (!isLedger && !isBreadcrumb) return;
    if (isLedger) pendingRef.current.ledgers.add(ev.path);
    else pendingRef.current.crumbs.set(ev.path, ev.op);
    scheduleFlush();
  });

  return (
    <div className="flex min-h-0 flex-1 flex-col bg-surface-primary">
      {/* 顶栏:刷新 + 简短说明 */}
      <div className="flex shrink-0 items-center justify-between border-b border-border-subtle px-5 py-3">
        <div className="flex flex-col">
          <h2 className="font-heading text-sm font-semibold text-foreground-primary">
            {t("ledger.title", "工作记录")}
          </h2>
          <p className="font-caption text-[11px] text-foreground-tertiary">
            {t("ledger.subtitle", "规划的思考过程都在这里。左侧是任务记录（目标 + 计划），右侧是进展。")}
          </p>
        </div>
        <div className="flex items-center gap-2">
          {compactNote && (
            <span
              className={cn(
                "font-caption text-[11px]",
                compactErr ? "text-state-danger" : "text-foreground-tertiary",
              )}
            >
              {compactNote}
            </span>
          )}
          <Button
            variant="outline"
            size="sm"
            onClick={exportReport}
            disabled={exporting}
            title={t("ledger.report.hint", {
              defaultValue: "把任务记录、进展、成员和费用估算导出成 Markdown",
            })}
            className="gap-1.5"
          >
            <Download className={cn("size-3.5", exporting && "animate-pulse")} />
            {exporting
              ? t("ledger.report.exporting", { defaultValue: "导出中…" })
              : t("ledger.report.button", { defaultValue: "导出战报" })}
          </Button>
          <Button
            variant="outline"
            size="sm"
            onClick={compact}
            disabled={compacting || refreshing}
            title={t("ledger.compactHint", "缩短记录篇幅，关键信息不丢；旧内容可从历史找回")}
            className="gap-1.5"
          >
            <Sparkles className={cn("size-3.5", compacting && "animate-pulse")} />
            {compacting ? t("ledger.compacting", "压缩中…") : t("ledger.compact", "压缩")}
          </Button>
          <Button
            variant="outline"
            size="sm"
            onClick={refresh}
            disabled={refreshing || compacting}
            className="gap-1.5"
          >
            <RefreshCw className={cn("size-3.5", refreshing && "animate-spin")} />
            {t("ledger.refresh", "刷新")}
          </Button>
        </div>
      </div>

      {/* 主体:上半 = 任务 + 进展(双卡),下半 = worker 近况(通栏) */}
      <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-hidden p-5">
        <div className="flex min-h-0 flex-1 gap-4 overflow-hidden">
          <LedgerCard
            icon={<ClipboardList className="size-4 text-accent-primary" />}
            title={t("ledger.taskTitle", "任务记录")}
            subtitle={t("ledger.taskSubtitle", "目标 · 假设 · 计划(DAG)")}
            at={task.at}
            nowTick={nowTick}
            snap={task}
            // When the run is terminally done (progress ledger says all_done),
            // force any leftover `- [ ]` plan boxes to render checked. The
            // orchestrator is *told* to check them off (orchestrator.md), but
            // it's an LLM and sometimes forgets — without this guard a finished
            // plan shows an unchecked step next to an "all done" status, which
            // reads as "not actually finished". Belt to the prompt's suspenders.
            forceAllChecked={/(^|\n)\s*(?:[-*]\s+)?Status:\s*all_done\b/i.test(
              progress.content ?? "",
            )}
            emptyHint={t(
              "ledger.taskEmpty",
              "还没写。orchestrator 第一次 wake 时会自动建立。",
            )}
          />
          <LedgerCard
            icon={<Activity className="size-4 text-state-success" />}
            title={t("ledger.progressTitle", "进展状态")}
            subtitle={t(
              "ledger.progressSubtitle",
              "当前步骤 · 派出去的活 · 卡点",
            )}
            at={progress.at}
            nowTick={nowTick}
            snap={progress}
            emptyHint={t(
              "ledger.progressEmpty",
              "还没写。orchestrator 派活时会实时更新。",
            )}
          />
        </div>
        <BreadcrumbsCard rows={breadcrumbs} nowTick={nowTick} />
      </div>
    </div>
  );
}

/** Worker 近况通栏 —— 把每个 worker 写到 `<role>.progress.md` 的最新一行
 *  当成一条心跳显示。Magentic-One 论文里没有这玩意,是 swarmx 的补丁:
 *  Bash / npm install 这种秒不出动静的任务期间,只有"派活了…然后呢?"对用户
 *  来说是个黑盒。orchestrator 在 worker prompt 里要求每个里程碑都覆写这个
 *  文件,这里就把所有 workers 的最新心跳铺出来,newest first。 */
function BreadcrumbsCard({
  rows,
  nowTick,
}: {
  rows: { role: string; content: string; at: number }[];
  nowTick: number;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex shrink-0 flex-col overflow-hidden rounded-lg border border-border-subtle bg-surface-elevated">
      <div className="flex shrink-0 items-center gap-2 border-b border-border-subtle px-4 py-3">
        <Radio className="size-4 text-accent-primary" />
        <div className="flex min-w-0 flex-1 flex-col">
          <span className="font-heading text-sm font-semibold text-foreground-primary">
            {t("ledger.breadcrumbsTitle", "近况")}
          </span>
          <span className="font-caption text-[11px] text-foreground-tertiary">
            {t(
              "ledger.breadcrumbsSubtitle",
              "成员最近做到哪了（每完成一步会自动更新）",
            )}
          </span>
        </div>
        <span className="shrink-0 font-caption text-[10px] text-foreground-tertiary">
          {t("ledger.breadcrumbsCount", {
            n: rows.length,
            defaultValue: "{{n}} 位成员",
          })}
        </span>
      </div>
      <div className="max-h-[40vh] overflow-auto px-4 py-3">
        {rows.length === 0 ? (
          <EmptyState
            icon={<Radio className="size-8" />}
            title={t("ledger.breadcrumbsEmpty")}
            hint={t("ledger.breadcrumbsEmptyHint")}
          />
        ) : (
          <ul className="flex flex-col gap-2">
            {rows.map((r) => (
              <li
                key={r.role}
                className="flex items-baseline gap-3 rounded-md bg-surface-tertiary px-3 py-2"
              >
                <span className="shrink-0 font-mono text-[11px] font-semibold text-accent-primary">
                  {roleDisplayName(r.role)}
                </span>
                <span className="min-w-0 flex-1 truncate font-body text-[13px] text-foreground-primary">
                  {r.content}
                </span>
                <span className="shrink-0 font-caption text-[10px] text-foreground-tertiary">
                  {fmtAgo(r.at, nowTick, t)}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

/** Tiny isolated "XX 秒前" ticker. The whole page re-renders every second to
 *  advance `nowTick`; keeping the tick consumer down here (instead of feeding a
 *  per-second `ago` string into LedgerCard) means the memoized card body — and
 *  its expensive ReactMarkdown re-parse — doesn't churn once a second. */
function LedgerAgo({ at, nowTick }: { at: number | null; nowTick: number }) {
  const { t } = useTranslation();
  return (
    <span className="shrink-0 font-caption text-[10px] text-foreground-tertiary">
      {fmtAgo(at, nowTick, t)}
    </span>
  );
}

const LedgerCard = memo(function LedgerCard({
  icon,
  title,
  subtitle,
  at,
  nowTick,
  snap,
  emptyHint,
  forceAllChecked = false,
}: {
  icon: React.ReactNode;
  title: string;
  subtitle: string;
  at: number | null;
  nowTick: number;
  snap: LedgerSnap;
  emptyHint: string;
  forceAllChecked?: boolean;
}) {
  const { t } = useTranslation();
  // Memo the markdown parse on the raw content. Without this the per-second
  // page tick (nowTick) re-renders ReactMarkdown, which re-parses the entire
  // (potentially large) ledger every second.
  const body = useMemo(() => {
    if (!snap.content) return null;
    // all_done guard: flip leftover GFM task boxes to checked so a terminally
    // finished plan never shows an unchecked step (see the forceAllChecked
    // comment at the call site). Only touches `- [ ]` markers, nothing else.
    const content = forceAllChecked
      ? snap.content.replace(/^(\s*[-*]\s+)\[ \]/gm, "$1[x]")
      : snap.content;
    return (
      <article className="prose prose-sm max-w-none font-body text-[13px] text-foreground-primary">
        <ReactMarkdown
          remarkPlugins={[remarkGfm]}
          components={{ a: MarkdownLink, input: MarkdownInput }}
        >
          {stripLedgerHeading(content)}
        </ReactMarkdown>
      </article>
    );
  }, [snap.content, forceAllChecked]);
  return (
    <div className="flex min-w-0 flex-1 flex-col overflow-hidden rounded-lg border border-border-subtle bg-surface-elevated">
      <div className="flex shrink-0 items-center gap-2 border-b border-border-subtle px-4 py-3">
        {icon}
        <div className="flex min-w-0 flex-1 flex-col">
          <span className="truncate font-heading text-sm font-semibold text-foreground-primary">
            {title}
          </span>
          <span className="truncate font-caption text-[11px] text-foreground-tertiary">
            {subtitle}
          </span>
        </div>
        <LedgerAgo at={at} nowTick={nowTick} />
      </div>
      <div className="min-h-0 flex-1 overflow-auto px-4 py-3">
        {snap.error ? (
          <p className="font-caption text-xs text-state-danger">
            {t("ledger.readFailed", { msg: snap.error, defaultValue: "读取失败: {{msg}}" })}
          </p>
        ) : body ? (
          body
        ) : (
          <p className="font-caption text-xs text-foreground-tertiary">
            {emptyHint}
          </p>
        )}
      </div>
    </div>
  );
});
