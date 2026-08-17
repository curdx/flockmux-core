/**
 * Cron page (`/cron`).
 *
 * Schedule a prompt to be delivered to a workspace's orchestrator on a 5-field
 * cron. Times are interpreted in a fixed offset (the browser's local offset for
 * new jobs; the job's own stored offset when editing), which the server uses to
 * evaluate the expression — the UI never shows raw UTC. A live cronstrue
 * description + server-computed next-run preview catch a typo before save; each
 * row shows when it next fires, can be edited in place, and "Run now" fires
 * immediately via /api/cron/:id/run.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import { Play, Trash2, Loader2, Check, X, Pencil, FolderPlus, Clock } from "lucide-react";
import { api, ApiError } from "@/api/http";
import type { CronJob, Workspace } from "@/api/types";
import { cn } from "@/lib/cn";
import { EmptyState } from "@/components/EmptyState";
import { toast } from "@/lib/toast";
import {
  describeCron,
  fmtDate,
  fmtTime,
  localOffsetMinutes,
  relativeFromNow,
  tzOffsetLabel,
  wallClock,
} from "@/lib/cron";

const PRESETS: { key: string; expr: string }[] = [
  { key: "hourly", expr: "0 * * * *" },
  { key: "daily9", expr: "0 9 * * *" },
  { key: "weekdays9", expr: "0 9 * * 1-5" },
  { key: "monday9", expr: "0 9 * * 1" },
  { key: "monthly1", expr: "0 9 1 * *" },
];

type FormValues = {
  workspace_id: string;
  name: string;
  cron_expr: string;
  prompt: string;
  tz_offset_minutes: number;
};

type TFn = ReturnType<typeof useTranslation>["t"];

/** Unwrap an error to user-facing copy: ApiError carries the server's friendly
 *  `{ error }` detail (not the raw "POST /api/cron → 400:" dev string). */
function errMsg(e: unknown): string {
  return e instanceof ApiError ? e.detail : (e as Error).message;
}

/**
 * A `Date.now()` snapshot that re-renders every `intervalMs` so relative-time
 * copy ("约 3 小时后") stays fresh instead of freezing at first paint. The timer
 * is cleared on unmount.
 */
function useNowTick(intervalMs = 30_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(id);
  }, [intervalMs]);
  return now;
}

/** "今天 09:00 · 约 3 小时后" — a UTC instant rendered in a fixed offset. */
function formatRun(utcMs: number, offsetMin: number, now: number, t: TFn, lang: string): string {
  const w = wallClock(utcMs, offsetMin, now);
  const day =
    w.dayDiff === 0
      ? t("cron.today")
      : w.dayDiff === 1
        ? t("cron.tomorrow")
        : w.dayDiff === -1
          ? t("cron.yesterday")
          : fmtDate(w);
  return `${day} ${fmtTime(w)} · ${relativeFromNow(utcMs, now, lang)}`;
}

/** Server preview verdict, or `null` when we have none, or `"error"` when the
 *  preview request itself failed (distinct from a server "invalid" verdict). */
type PreviewState = { valid: boolean; next_run: number | null } | null | "error";

/**
 * Shared create/edit form. `tzOffset` is the offset the expression is authored
 * in — the browser's local offset for a new job, the job's own offset when
 * editing (so an edit never silently shifts the schedule's timezone).
 */
function CronJobForm({
  workspaces,
  tzOffset,
  lang,
  initial,
  submitLabel,
  submittingLabel,
  onSubmit,
  onDone,
  onCancel,
  resetAfterSubmit,
}: {
  workspaces: Workspace[];
  tzOffset: number;
  lang: string;
  initial: { wsId: string; name: string; expr: string; prompt: string };
  submitLabel: string;
  submittingLabel: string;
  onSubmit: (v: FormValues) => Promise<{ ok: boolean; error?: string }>;
  onDone: (describe: string | null) => void;
  onCancel?: () => void;
  resetAfterSubmit?: boolean;
}) {
  const { t } = useTranslation();
  const tzLabel = tzOffsetLabel(tzOffset);
  const [wsId, setWsId] = useState(initial.wsId);
  const [name, setName] = useState(initial.name);
  const [expr, setExpr] = useState(initial.expr);
  const [prompt, setPrompt] = useState(initial.prompt);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  // `null` = not yet previewed (empty expr); `"error"` = the preview request
  // failed (distinct from a server verdict of invalid). Keeping them apart lets
  // canSubmit refuse to gate on a verdict we never actually got.
  const [exprPreview, setExprPreview] = useState<PreviewState>(null);
  const describe = useMemo(() => describeCron(expr, lang), [expr, lang]);

  // Fill an empty workspace once the list arrives (create mode mounts before
  // workspaces load). Never overrides a value already set (edit mode).
  useEffect(() => {
    if (!wsId && workspaces.length) setWsId(workspaces[0].id);
  }, [workspaces, wsId]);

  useEffect(() => {
    const e = expr.trim();
    if (!e) {
      setExprPreview(null);
      return;
    }
    // Guard against out-of-order responses: a slow earlier request must not
    // overwrite a newer one. clearTimeout covers the not-yet-fired case; the
    // flag covers a request already in flight when the input changes.
    let cancelled = false;
    const id = setTimeout(() => {
      api
        .cronPreview(e, tzOffset)
        .then((r) => {
          if (!cancelled) setExprPreview(r);
        })
        .catch(() => {
          // A failed preview is NOT "no verdict yet" — mark it distinctly so
          // canSubmit won't wave the expr through as if it had been validated.
          if (!cancelled) setExprPreview("error");
        });
    }, 300);
    return () => {
      cancelled = true;
      clearTimeout(id);
    };
  }, [expr, tzOffset]);

  // Ticking clock so the preview's "下次 … · 约 3 小时后" doesn't freeze.
  const now = useNowTick();
  // Resolve the union into plain values the JSX can branch on without
  // re-narrowing inline: `verdict` is the server's actual response (or null when
  // we have none / it failed), `previewFailed` flags the failed probe.
  const previewFailed = exprPreview === "error";
  const verdict = exprPreview === "error" ? null : exprPreview;
  // A still-pending / empty preview (`null`) is NOT a green light — only an
  // affirmative `valid === true` is. A failed preview leaves the server as the
  // final arbiter (don't dead-lock the form on a flaky probe), surfaced below.
  const previewOk = verdict !== null && verdict.valid;
  const previewInvalid = verdict !== null && !verdict.valid;
  const canSubmit =
    !busy && !!wsId && !!expr.trim() && !!prompt.trim() && (previewOk || previewFailed);

  const submit = async () => {
    setBusy(true);
    setErr(null);
    try {
      const res = await onSubmit({
        workspace_id: wsId,
        name,
        cron_expr: expr,
        prompt,
        tz_offset_minutes: tzOffset,
      });
      if (!res.ok) {
        setErr(res.error ?? t("cron.submitFailed", { defaultValue: "保存失败，请重试" }));
      } else {
        if (resetAfterSubmit) {
          setName("");
          setPrompt("");
        }
        onDone(describe);
      }
    } catch (e) {
      // ApiError → the server's friendly `{ error }` detail, not the raw
      // "POST /api/cron → 400:" dev string.
      setErr(errMsg(e));
    } finally {
      setBusy(false);
    }
  };

  // No workspaces yet → the form can't target anything (the dropdown would only
  // show a dead "—"). Guide the user to create a workspace first instead of
  // presenting an unusable form. Edit mode always has a workspace (initial.wsId),
  // so this only ever shows in the create area.
  if (workspaces.length === 0 && !initial.wsId) {
    return (
      <div className="flex flex-col items-center gap-3 px-4 py-6 text-center">
        <FolderPlus className="size-6 text-foreground-tertiary" />
        <p className="font-caption text-[13px] text-foreground-secondary">
          {t("cron.noWorkspaces", {
            defaultValue: "还没有工作空间。定时任务需要先有一个工作空间来接收提示词。",
          })}
        </p>
        <Link
          to="/chat"
          className="inline-flex min-h-8 items-center gap-1.5 rounded-md bg-accent-primary px-3 py-1.5 text-[13px] text-foreground-on-accent hover:opacity-90"
        >
          <FolderPlus className="size-3.5" />
          {t("cron.createWorkspace", { defaultValue: "新建工作空间" })}
        </Link>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-2">
      <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
        <label className="flex flex-col gap-1">
          <span className="font-caption text-[11px] text-foreground-tertiary">{t("cron.workspace")}</span>
          <select
            name="cron-workspace"
            value={wsId}
            onChange={(e) => setWsId(e.target.value)}
            className="min-h-8 rounded border border-border-subtle bg-surface-primary px-2 py-1 text-[13px]"
          >
            {workspaces.length === 0 && <option value="">—</option>}
            {/* an edited job may point at a deleted workspace not in the list */}
            {wsId && !workspaces.some((w) => w.id === wsId) && <option value={wsId}>{wsId.slice(0, 8)}</option>}
            {workspaces.map((w) => (
              <option key={w.id} value={w.id}>
                {w.name}
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1">
          <span className="font-caption text-[11px] text-foreground-tertiary">
            {t("cron.expr")} · {tzLabel}
          </span>
          <input
            name="cron-expression"
            value={expr}
            onChange={(e) => setExpr(e.target.value)}
            placeholder="0 9 * * 1-5"
            spellCheck={false}
            aria-invalid={previewInvalid}
            className={cn(
              "min-h-8 rounded border bg-surface-primary px-2 py-1 font-mono text-[13px]",
              previewInvalid ? "border-status-danger" : "border-border-subtle",
            )}
          />
        </label>
      </div>

      {/* preset chips */}
      <div className="flex flex-wrap items-center gap-1.5">
        <span className="font-caption text-[11px] text-foreground-tertiary">{t("cron.presetsLabel")}</span>
        {PRESETS.map((p) => (
          <button
            key={p.key}
            type="button"
            onClick={() => setExpr(p.expr)}
            aria-pressed={expr.trim() === p.expr}
            className={cn(
              "rounded-full border px-2 py-0.5 font-caption text-[11px] transition-colors",
              expr.trim() === p.expr
                ? "border-accent-primary bg-accent-primary/10 text-accent-primary"
                : "border-border-subtle text-foreground-secondary hover:bg-surface-tertiary",
            )}
          >
            {t(`cron.preset.${p.key}`)}
          </button>
        ))}
      </div>

      {/* human-readable description + validity / next-run */}
      <div className="min-h-[16px] font-caption text-[11px]">
        {previewFailed ? (
          // Preview couldn't be reached — say so honestly instead of silently
          // showing the expr as if it had been validated. Submit stays allowed
          // (server is the final arbiter), so this is a warning, not a block.
          <span className="text-state-warning">
            {describe ? `${describe} · ` : ""}
            {t("cron.previewFailed", { defaultValue: "无法校验表达式（预览接口未响应），可仍尝试保存。" })}
          </span>
        ) : previewInvalid ? (
          <span className="text-status-danger">{t("cron.invalidExpr")}</span>
        ) : describe ? (
          <span className="text-foreground-secondary">
            {describe}
            {previewOk && verdict && verdict.next_run !== null
              ? ` · ${t("cron.nextLabel")} ${formatRun(verdict.next_run, tzOffset, now, t, lang)}`
              : previewOk && verdict && verdict.next_run === null
                ? ` · ${t("cron.noUpcoming")}`
                : ""}
          </span>
        ) : (
          <span className="text-foreground-tertiary">{t("cron.exprHint")}</span>
        )}
      </div>

      <label className="flex flex-col gap-1">
        <span className="font-caption text-[11px] text-foreground-tertiary">{t("cron.name")}</span>
        <input
          name="cron-name"
          value={name}
          onChange={(e) => setName(e.target.value)}
          maxLength={120}
          placeholder={t("cron.namePlaceholder")}
          className="min-h-8 rounded border border-border-subtle bg-surface-primary px-2 py-1 text-[13px]"
        />
      </label>
      <label className="flex flex-col gap-1">
        <span className="font-caption text-[11px] text-foreground-tertiary">{t("cron.prompt")}</span>
        <textarea
          name="cron-prompt"
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
          rows={2}
          maxLength={8000}
          placeholder={t("cron.promptPlaceholder")}
          className="resize-none rounded border border-border-subtle bg-surface-primary px-2 py-1 text-[13px]"
        />
      </label>

      {err && <div className="font-caption text-[11px] text-status-danger">{err}</div>}

      <div className="flex justify-end gap-2">
        {onCancel && (
          <button
            type="button"
            onClick={onCancel}
            className="min-h-8 rounded-md border border-border-subtle px-3 py-1.5 text-[13px] text-foreground-secondary hover:bg-surface-tertiary"
          >
            {t("cron.cancel")}
          </button>
        )}
        <button
          type="button"
          disabled={!canSubmit}
          onClick={submit}
          className="min-h-8 rounded-md bg-accent-primary px-3 py-1.5 text-[13px] text-foreground-on-accent disabled:opacity-50"
        >
          {busy ? submittingLabel : submitLabel}
        </button>
      </div>
    </div>
  );
}

export default function CronRoute() {
  const { t, i18n } = useTranslation();
  const lang = i18n.language ?? "zh";
  const tzOffset = useMemo(() => localOffsetMinutes(), []);
  const tzLabel = tzOffsetLabel(tzOffset);

  const [jobs, setJobs] = useState<CronJob[]>([]);
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const [notice, setNotice] = useState<string | null>(null);
  const [runResult, setRunResult] = useState<{ id: string; ok: boolean; msg: string } | null>(null);
  const [runningId, setRunningId] = useState<string | null>(null);
  const [togglingId, setTogglingId] = useState<string | null>(null);
  const [confirmDel, setConfirmDel] = useState<string | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  // In-flight guard for the enable/disable switch: blocks a second flip while
  // the first request is still resolving (a fast double-click could otherwise
  // fire two opposite toggles and land on the wrong state).
  const togglingRef = useRef<string | null>(null);

  useEffect(() => {
    if (!notice) return;
    const id = setTimeout(() => setNotice(null), 4000);
    return () => clearTimeout(id);
  }, [notice]);
  useEffect(() => {
    if (!runResult) return;
    const id = setTimeout(() => setRunResult(null), 4000);
    return () => clearTimeout(id);
  }, [runResult]);
  useEffect(() => {
    if (!confirmDel) return;
    // Give the user a comfortable window to land the second (confirm) click
    // before the button reverts to its idle "Delete" label.
    const id = setTimeout(() => setConfirmDel(null), 6000);
    return () => clearTimeout(id);
  }, [confirmDel]);

  const load = useCallback(async () => {
    try {
      const [c, ws] = await Promise.all([api.listCron(), api.listWorkspaces().catch(() => [])]);
      setJobs(c.jobs);
      setWorkspaces(ws);
      setErr(null);
    } catch (e) {
      setErr(errMsg(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const toggle = async (id: string) => {
    // 防重:同一行的切换在途时丢弃后续点击,避免两个相反的请求竞态。
    if (togglingRef.current === id) return;
    // Read the CURRENT enabled from the latest list (not a stale captured prop),
    // so the target is always the negation of what's actually on screen.
    const snap = jobs; // P1-21: keep the pre-toggle list so we can revert
    const cur = snap.find((x) => x.id === id);
    if (!cur) return;
    const target = !cur.enabled;
    togglingRef.current = id;
    setTogglingId(id);
    // Optimistic: flip immediately, reconcile on the response.
    setJobs((prev) => prev.map((x) => (x.id === id ? { ...x, enabled: target } : x)));
    try {
      await api.toggleCron(id, target);
      load();
    } catch (e) {
      // A failed toggle must NOT leave the switch optimistically flipped while the
      // backend never recorded it. Roll the switch back and surface the error.
      setJobs(snap);
      toast.error(t("cron.toggleFailed", { defaultValue: "切换启用状态失败，请重试" }), {
        description: errMsg(e),
      });
    } finally {
      togglingRef.current = null;
      setTogglingId(null);
    }
  };

  const runNow = async (j: CronJob) => {
    setRunningId(j.id);
    setRunResult(null);
    try {
      const r = await api.runCron(j.id);
      if (!r.ok && r.skipped) setRunResult({ id: j.id, ok: false, msg: r.skipped });
      else setRunResult({ id: j.id, ok: true, msg: t("cron.ranOk") });
    } catch (e) {
      setRunResult({ id: j.id, ok: false, msg: errMsg(e) });
    } finally {
      setRunningId(null);
      load();
    }
  };

  const remove = async (id: string) => {
    setConfirmDel(null);
    const snap = jobs; // P1-22: keep the pre-delete list so we can revert
    setJobs((prev) => prev.filter((x) => x.id !== id)); // optimistic
    try {
      await api.deleteCron(id);
      load();
    } catch (e) {
      // A failed delete must NOT make the row silently vanish as if it succeeded.
      // Pull it back (snapshot first, then reload for ground truth) and tell the user.
      setJobs(snap);
      load();
      toast.error(t("cron.deleteFailed", { defaultValue: "删除任务失败，请重试" }), {
        description: errMsg(e),
      });
    }
  };

  const wsName = (id: string) => workspaces.find((w) => w.id === id)?.name ?? id.slice(0, 8);
  // Ticking clock so each row's "下次 … · 约 3 小时后" stays accurate the longer
  // the page sits open, instead of freezing at the first-render snapshot.
  const now = useNowTick();

  return (
    <div className="h-full overflow-y-auto">
      <div className="mx-auto flex max-w-3xl flex-col gap-6 px-6 py-6">
        <header className="flex flex-col gap-1">
          <h1 className="font-display text-lg text-foreground-primary">{t("cron.title")}</h1>
          <p className="font-caption text-xs text-foreground-tertiary">
            {t("cron.subtitle")} · {t("cron.localTimeNote", { tz: tzLabel })}
          </p>
        </header>

        {/* create form */}
        <section className="rounded-lg border border-border-subtle bg-surface-secondary p-4">
          <CronJobForm
            workspaces={workspaces}
            tzOffset={tzOffset}
            lang={lang}
            initial={{ wsId: "", name: "", expr: "0 9 * * *", prompt: "" }}
            submitLabel={t("cron.create")}
            submittingLabel={t("cron.creating")}
            resetAfterSubmit
            onSubmit={(v) => api.createCron(v)}
            onDone={(describe) => {
              setNotice(describe ? t("cron.createdOk", { desc: describe }) : t("cron.createdPlain"));
              load();
            }}
          />
        </section>

        {notice && <div className="font-caption text-sm text-status-success">{notice}</div>}

        {/* job list */}
        {loading ? (
          <div className="flex items-center gap-2 font-caption text-xs text-foreground-tertiary">
            <Loader2 className="size-3.5 animate-spin" /> {t("common.loading")}
          </div>
        ) : err ? (
          // A failed fetch is NOT "no jobs yet" — show the failure + a retry so an
          // empty list can't masquerade as a real empty state.
          <div className="flex flex-col items-center gap-2 rounded-lg border border-status-danger/30 bg-surface-secondary px-4 py-6 text-center">
            <span className="font-caption text-sm text-status-danger">
              {t("cron.loadFailed", { defaultValue: "定时任务加载失败" })}
            </span>
            <span className="font-caption text-[11px] text-foreground-tertiary">{err}</span>
            <button
              type="button"
              onClick={() => {
                setLoading(true);
                load();
              }}
              className="mt-1 min-h-8 rounded-md border border-border-subtle px-3 py-1.5 font-caption text-[12px] text-foreground-secondary hover:bg-surface-tertiary"
            >
              {t("common.retry", { defaultValue: "重试" })}
            </button>
          </div>
        ) : jobs.length === 0 ? (
          <EmptyState
            icon={<Clock className="size-8" />}
            title={t("cron.empty")}
            hint={t("cron.emptyHint")}
          />
        ) : (
          <>
            <div className="font-caption text-[11px] text-foreground-tertiary">{t("cron.count", { n: jobs.length })}</div>
            <ul className="flex flex-col gap-2">
              {jobs.map((j) => {
                if (editingId === j.id) {
                  return (
                    <li
                      key={j.id}
                      className="rounded-lg border border-accent-primary/40 bg-surface-secondary px-3 py-3"
                    >
                      <CronJobForm
                        workspaces={workspaces}
                        // Edit in the job's OWN offset so changing name/expr/prompt
                        // never silently shifts the schedule's timezone.
                        tzOffset={j.tz_offset_minutes}
                        lang={lang}
                        initial={{ wsId: j.workspace_id, name: j.name, expr: j.cron_expr, prompt: j.prompt }}
                        submitLabel={t("cron.save")}
                        submittingLabel={t("cron.saving")}
                        onCancel={() => setEditingId(null)}
                        onSubmit={(v) => api.updateCron(j.id, v)}
                        onDone={() => {
                          setEditingId(null);
                          setNotice(t("cron.savedOk"));
                          load();
                        }}
                      />
                    </li>
                  );
                }
                const desc = describeCron(j.cron_expr, lang);
                const orphaned = workspaces.length > 0 && !workspaces.some((w) => w.id === j.workspace_id);
                return (
                  <li
                    key={j.id}
                    className="flex flex-col gap-1.5 rounded-lg border border-border-subtle bg-surface-secondary px-3 py-2"
                  >
                    <div className="flex items-center gap-3">
                      <button
                        type="button"
                        role="switch"
                        aria-checked={j.enabled}
                        aria-label={j.enabled ? t("cron.enabledState") : t("cron.paused")}
                        disabled={togglingId === j.id}
                        onClick={() => toggle(j.id)}
                        title={j.enabled ? t("cron.disable") : t("cron.enable")}
                        className={cn(
                          "size-8 shrink-0 rounded-full border border-transparent p-0 hover:border-border-subtle hover:bg-surface-tertiary after:mx-auto after:block after:size-2.5 after:rounded-full disabled:opacity-60",
                          j.enabled ? "after:bg-status-success" : "after:bg-foreground-tertiary",
                        )}
                      />
                      <div className={cn("flex min-w-0 flex-1 flex-col", !j.enabled && "opacity-55")}>
                        <span className="flex items-center gap-1.5 truncate text-[13px] text-foreground-primary">
                          <span className="truncate">{j.name}</span>
                          {!j.enabled && (
                            <span className="shrink-0 rounded bg-surface-tertiary px-1 py-px font-caption text-[10px] text-foreground-tertiary">
                              {t("cron.paused")}
                            </span>
                          )}
                          {orphaned && (
                            <span className="shrink-0 rounded bg-status-danger/10 px-1 py-px font-caption text-[10px] text-status-danger">
                              {t("cron.orphaned")}
                            </span>
                          )}
                        </span>
                        <span className="truncate font-caption text-[11px] text-foreground-tertiary" title={j.cron_expr}>
                          {desc ?? j.cron_expr} · {wsName(j.workspace_id)}
                          {/* Disclose the authoring zone when it differs from the
                              viewer's, so the global "your local zone" note can't
                              silently mislabel a job written in another offset
                              (incl. legacy UTC rows that predate this column). */}
                          {j.tz_offset_minutes !== tzOffset ? ` · ${tzOffsetLabel(j.tz_offset_minutes)}` : ""}
                        </span>
                        <span className="truncate font-caption text-[11px] text-foreground-tertiary">
                          {j.enabled && j.next_run
                            ? `${t("cron.nextLabel")} ${formatRun(j.next_run, j.tz_offset_minutes, now, t, lang)}`
                            : j.enabled
                              ? t("cron.noUpcoming")
                              : ""}
                          {j.last_run_at
                            ? `${j.enabled && j.next_run ? " · " : ""}${t("cron.lastRun")} ${relativeFromNow(j.last_run_at, now, lang)}`
                            : ""}
                        </span>
                      </div>
                      <button
                        type="button"
                        onClick={() => {
                          setConfirmDel(null);
                          setEditingId(j.id);
                        }}
                        aria-label={t("cron.edit")}
                        title={t("cron.edit")}
                        className="flex size-8 shrink-0 items-center justify-center rounded text-foreground-tertiary hover:bg-surface-tertiary hover:text-foreground-primary"
                      >
                        <Pencil className="size-3.5" />
                      </button>
                      <button
                        type="button"
                        disabled={runningId === j.id}
                        onClick={() => runNow(j)}
                        aria-label={runningId === j.id ? t("cron.running") : t("cron.runNow")}
                        title={runningId === j.id ? t("cron.running") : t("cron.runNow")}
                        className="flex size-8 shrink-0 items-center justify-center rounded text-foreground-tertiary hover:bg-surface-tertiary hover:text-foreground-primary disabled:opacity-60"
                      >
                        {runningId === j.id ? <Loader2 className="size-3.5 animate-spin" /> : <Play className="size-3.5" />}
                      </button>
                      {confirmDel === j.id ? (
                        <div className="flex shrink-0 items-center gap-1">
                          <button
                            type="button"
                            onClick={() => remove(j.id)}
                            aria-label={t("cron.confirmDelete")}
                            title={t("cron.confirmDelete")}
                            className="flex items-center gap-1 rounded bg-status-danger/10 px-2 py-1 font-caption text-[11px] text-status-danger hover:bg-status-danger/20"
                          >
                            <Check className="size-3" />
                            {t("cron.confirmDelete")}
                          </button>
                          <button
                            type="button"
                            onClick={() => setConfirmDel(null)}
                            aria-label={t("cron.cancel")}
                            title={t("cron.cancel")}
                            className="flex size-7 items-center justify-center rounded text-foreground-tertiary hover:bg-surface-tertiary"
                          >
                            <X className="size-3.5" />
                          </button>
                        </div>
                      ) : (
                        <button
                          type="button"
                          onClick={() => setConfirmDel(j.id)}
                          aria-label={t("cron.delete")}
                          title={t("cron.delete")}
                          className="flex size-8 shrink-0 items-center justify-center rounded text-foreground-tertiary hover:bg-surface-tertiary hover:text-status-danger"
                        >
                          <Trash2 className="size-3.5" />
                        </button>
                      )}
                    </div>
                    {runResult?.id === j.id && (
                      <div
                        className={cn(
                          "pl-11 font-caption text-[11px]",
                          runResult.ok ? "text-status-success" : "text-status-danger",
                        )}
                      >
                        {runResult.msg}
                      </div>
                    )}
                  </li>
                );
              })}
            </ul>
          </>
        )}
      </div>
    </div>
  );
}
