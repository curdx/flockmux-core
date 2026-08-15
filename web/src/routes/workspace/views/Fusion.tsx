/**
 * Fusion view — multi-model competition inside WorkspaceShell.
 *
 * Flow (mirrors the backend fusion lifecycle):
 *   1. Create a competition: one `need` fans out to 2..4 isolated contestant
 *      directions (one per label / model name). status → "running".
 *   2. Judge stage: trigger judge → backend spawns a judge direction and
 *      returns each contestant's diff bundle (files changed vs base).
 *      status → "judging".
 *   3. Decide: pick ONE winning contestant; the batch flips to "done" and
 *      (unless merge=false) the winner's branch is merged back into base.
 *      We surface the merge result (merged / resolving / nothing_to_merge).
 *
 * All data comes from the fusion REST endpoints (api.listFusion / createFusion
 * / judgeFusion / decideFusion). We refetch the batch list on swarm activity
 * so a running competition's status updates without a manual reload.
 *
 * Naming rule: a contestant's human-readable name uses its `name` field first,
 * falling back to `slug` (never a raw role string — those go through
 * roleDisplayName elsewhere; fusion contestants carry their own label).
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Bot,
  Check,
  FileText,
  Gavel,
  Sparkles,
  Plus,
  RefreshCw,
  Swords,
  Trophy,
  X,
} from "lucide-react";
import { api, ApiError } from "../../../api/http";
import type {
  FusionBatch,
  FusionContestantDiff,
  FusionDecideResponse,
  FusionJudgeResponse,
  SwarmEvent,
} from "../../../api/types";
import { useSwarmFeed } from "../../../hooks/useSwarmFeed";
import { useWorkspaceContext } from "../Shell";
import { Button } from "@/components/ui/button";
import { EmptyState } from "@/components/EmptyState";
import { cn } from "@/lib/cn";
import { toast } from "@/lib/toast";

/** Contestant display name: prefer the backend-sent `name`, fall back to the
 *  slug. Never a raw role slug — fusion contestants carry their own label. */
function contestantLabel(c: { name: string | null; slug: string }): string {
  return c.name?.trim() || c.slug;
}

/** Status pill color + label key for a fusion batch. */
function statusTone(status: string): string {
  switch (status) {
    case "running":
      return "bg-status-running-soft text-status-running";
    case "judging":
      return "bg-state-warning/15 text-state-warning";
    case "needs_decision":
      return "bg-state-danger/15 text-state-danger";
    case "done":
      return "bg-status-success-soft text-status-success";
    case "failed":
      return "bg-state-danger/10 text-state-danger";
    default:
      return "bg-surface-tertiary text-foreground-secondary";
  }
}

function FileList({ files }: { files: string[] }) {
  if (files.length === 0) return null;
  return (
    <ul className="max-h-40 overflow-y-auto rounded-md border border-border-subtle bg-surface-secondary">
      {files.map((f) => (
        <li
          key={f}
          className="flex items-center gap-2 px-2.5 py-1.5 font-mono text-[11px] text-foreground-secondary"
        >
          <FileText className="size-3 shrink-0 text-foreground-tertiary" />
          <span className="truncate" title={f}>
            {f}
          </span>
        </li>
      ))}
    </ul>
  );
}

// ── Create form ───────────────────────────────────────────────────────────

function CreateFusionForm({
  workspaceId,
  onCreated,
}: {
  workspaceId: string;
  onCreated: () => void;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [need, setNeed] = useState("");
  // Full-auto by default: the novice path. Server picks the panel, implements,
  // judges (synthesize), and merges with zero further clicks.
  const [autopilot, setAutopilot] = useState(true);
  const [checkCmd, setCheckCmd] = useState("");
  // Start with two contestant labels (the minimum the backend allows).
  const [labels, setLabels] = useState<string[]>(["claude", "codex"]);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reset = () => {
    setNeed("");
    setAutopilot(true);
    setCheckCmd("");
    setLabels(["claude", "codex"]);
    setError(null);
  };

  const setLabel = (i: number, v: string) =>
    setLabels((prev) => prev.map((l, idx) => (idx === i ? v : l)));
  const addLabel = () =>
    setLabels((prev) => (prev.length >= 4 ? prev : [...prev, ""]));
  const removeLabel = (i: number) =>
    setLabels((prev) => (prev.length <= 2 ? prev : prev.filter((_, idx) => idx !== i)));

  const cleanLabels = labels.map((l) => l.trim()).filter(Boolean);
  const canSubmit =
    need.trim().length > 0 &&
    (autopilot || (cleanLabels.length >= 2 && cleanLabels.length <= 4)) &&
    !submitting;

  const submit = async () => {
    if (!canSubmit) return;
    setSubmitting(true);
    setError(null);
    try {
      const check = checkCmd.trim();
      await api.createFusion(workspaceId, {
        need: need.trim(),
        labels: autopilot ? [] : cleanLabels,
        autopilot,
        ...(check ? { check_cmd: check } : {}),
      });
      reset();
      setOpen(false);
      onCreated();
    } catch (e) {
      setError(e instanceof ApiError ? e.detail : (e as Error).message);
    } finally {
      setSubmitting(false);
    }
  };

  if (!open) {
    return (
      <Button onClick={() => setOpen(true)} size="sm">
        <Plus className="size-3.5" />
        {t("fusion.newButton", { defaultValue: "发起竞赛" })}
      </Button>
    );
  }

  return (
    <div className="space-y-3 rounded-lg border border-border-subtle bg-surface-elevated p-4">
      <div className="flex items-center justify-between">
        <h3 className="flex items-center gap-1.5 font-heading text-sm font-semibold text-foreground-primary">
          <Swords className="size-4 text-accent-primary" />
          {t("fusion.newTitle", { defaultValue: "发起一场竞赛" })}
        </h3>
        <button
          type="button"
          onClick={() => {
            reset();
            setOpen(false);
          }}
          className="text-foreground-tertiary hover:text-foreground-primary"
          aria-label={t("common.cancel")}
        >
          <X className="size-4" />
        </button>
      </div>

      <div className="space-y-1.5">
        <label className="font-caption text-[11px] tracking-wide text-foreground-tertiary">
          {t("fusion.needLabel")}
        </label>
        <textarea
          value={need}
          onChange={(e) => setNeed(e.target.value)}
          rows={3}
          placeholder={t("fusion.needPlaceholder", {
            defaultValue: "描述你要让各模型并行实现的同一个任务…",
          })}
          className="w-full resize-y rounded-md border border-border-subtle bg-surface-secondary px-2.5 py-2 font-body text-sm text-foreground-primary outline-none focus:border-accent-primary"
        />
      </div>

      {/* full-auto toggle — the novice one-click path */}
      <label className="flex cursor-pointer items-center gap-2 rounded-md border border-border-subtle bg-surface-secondary px-2.5 py-2">
        <input
          type="checkbox"
          checked={autopilot}
          onChange={(e) => setAutopilot(e.target.checked)}
          className="size-3.5 accent-accent-primary"
        />
        <Sparkles className="size-3.5 text-accent-primary" />
        <span className="font-body text-[13px] text-foreground-primary">
          {t("fusion.autopilot", { defaultValue: "全自动（自动选模型 · 并行实现 · 评审综合 · 合并）" })}
        </span>
      </label>

      {autopilot ? (
        <div className="space-y-1.5">
          <p className="rounded-md bg-surface-secondary px-2.5 py-2 font-body text-[12px] text-foreground-secondary">
            {t("fusion.autopilotHint", {
              defaultValue:
                "只填需求即可。系统会自动挑选可用模型并行实现、跑客观检查、综合出最优版并合并到主线，全程零手动。",
            })}
          </p>
          <p className="rounded-md border border-status-warning/30 bg-status-warning-soft/40 px-2.5 py-2 font-body text-[12px] text-foreground-secondary">
            {t("fusion.costWarning", {
              defaultValue:
                "会同时拉起多个模型跑同一任务，可能较快消耗订阅额度。没把握先用聊天让规划派一个人。",
            })}
          </p>
        </div>
      ) : (
        <div className="space-y-1.5">
          <label className="font-caption text-[11px] tracking-wide text-foreground-tertiary">
            {t("fusion.labelsLabel")}
          </label>
          <div className="space-y-2">
            {labels.map((l, i) => (
              <div key={i} className="flex items-center gap-2">
                <input
                  value={l}
                  onChange={(e) => setLabel(i, e.target.value)}
                  placeholder={t("fusion.labelPlaceholder", {
                    defaultValue: "选手名，如 claude / codex / deepseek",
                  })}
                  className="flex-1 rounded-md border border-border-subtle bg-surface-secondary px-2.5 py-1.5 font-mono text-[13px] text-foreground-primary outline-none focus:border-accent-primary"
                />
                <button
                  type="button"
                  onClick={() => removeLabel(i)}
                  disabled={labels.length <= 2}
                  className="text-foreground-tertiary hover:text-state-danger disabled:opacity-30"
                  aria-label={t("fusion.removeLabel", { defaultValue: "删除选手" })}
                >
                  <X className="size-4" />
                </button>
              </div>
            ))}
          </div>
          {labels.length < 4 && (
            <button
              type="button"
              onClick={addLabel}
              className="flex items-center gap-1 font-caption text-[11px] text-accent-primary hover:underline"
            >
              <Plus className="size-3" />
              {t("fusion.addLabel", { defaultValue: "加一位选手" })}
            </button>
          )}
        </div>
      )}

      {/* optional objective check (both modes) */}
      <div className="space-y-1.5">
        <label className="font-caption text-[11px] tracking-wide text-foreground-tertiary">
          {t("fusion.checkCmdLabel")}
        </label>
        <input
          value={checkCmd}
          onChange={(e) => setCheckCmd(e.target.value)}
          placeholder="python3 check.py"
          className="w-full rounded-md border border-border-subtle bg-surface-secondary px-2.5 py-1.5 font-mono text-[13px] text-foreground-primary outline-none focus:border-accent-primary"
        />
      </div>

      {/* 非 git 目录的副作用明示(同 CreateWizard):发起竞赛 = N 个选手并行
          隔离,后端会在项目目录 git init + 全量提交一次。 */}
      <p className="font-caption text-[11px] leading-relaxed text-foreground-tertiary">
        {t("fusion.gitInitNotice")}
      </p>

      {error && (
        <p className="rounded-md bg-state-danger/10 px-2.5 py-2 font-caption text-[11px] text-state-danger">
          {error}
        </p>
      )}

      <div className="flex justify-end gap-2">
        <Button
          variant="outline"
          size="sm"
          onClick={() => {
            reset();
            setOpen(false);
          }}
        >
          {t("common.cancel")}
        </Button>
        <Button size="sm" onClick={submit} disabled={!canSubmit}>
          <Swords className="size-3.5" />
          {submitting
            ? t("fusion.creating", { defaultValue: "发起中…" })
            : t("fusion.create", { defaultValue: "发起竞赛" })}
        </Button>
      </div>
    </div>
  );
}

// ── One batch card ──────────────────────────────────────────────────────────

function BatchCard({
  workspaceId,
  batch,
  onChanged,
}: {
  workspaceId: string;
  batch: FusionBatch;
  onChanged: () => void;
}) {
  const { t } = useTranslation();
  // Judge response (contestant diffs) — cached per batch once the judge stage
  // runs. Persisted in component state so re-fetches of the list don't wipe it.
  const [judge, setJudge] = useState<FusionJudgeResponse | null>(null);
  const [judging, setJudging] = useState(false);
  const [decision, setDecision] = useState<FusionDecideResponse | null>(null);
  const [deciding, setDeciding] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const runJudge = async (auto?: boolean, synthesize?: boolean) => {
    setJudging(true);
    setError(null);
    try {
      const resp = await api.judgeFusion(workspaceId, batch.id, auto, synthesize);
      setJudge(resp);
      if (auto && resp.judge_agent_id) {
        toast.success(
          synthesize
            ? t("fusion.synthStarted", { defaultValue: "已派出综合者，博采众长合成一版并合并" })
            : t("fusion.autoJudgeStarted"),
        );
      }
      onChanged();
    } catch (e) {
      setError(e instanceof ApiError ? e.detail : (e as Error).message);
      toast.error(
        t("fusion.judgeFailed", { defaultValue: "进入评判阶段失败" }),
        { description: e instanceof ApiError ? e.detail : (e as Error).message },
      );
    } finally {
      setJudging(false);
    }
  };

  const decide = async (winnerThreadId: string) => {
    setDeciding(winnerThreadId);
    setError(null);
    try {
      const resp = await api.decideFusion(workspaceId, batch.id, {
        winner_thread_id: winnerThreadId,
        merge: true,
      });
      setDecision(resp);
      onChanged();
    } catch (e) {
      setError(e instanceof ApiError ? e.detail : (e as Error).message);
      toast.error(
        t("fusion.decideFailed", { defaultValue: "选定赢家失败" }),
        { description: e instanceof ApiError ? e.detail : (e as Error).message },
      );
    } finally {
      setDeciding(null);
    }
  };

  // Contestants to render: prefer the rich judge diffs once we have them,
  // otherwise a minimal list derived from the batch's thread ids.
  const contestants: FusionContestantDiff[] = useMemo(() => {
    if (judge) return judge.contestants;
    return batch.contestant_thread_ids.map((id) => ({
      thread_id: id,
      slug: id.slice(0, 8),
      name: null,
      branch: null,
      files: [],
      degraded: false,
    }));
  }, [judge, batch.contestant_thread_ids]);

  const winnerId = decision?.winner_thread_id ?? batch.winner_thread_id ?? null;
  const hasDiffs = !!judge;
  const isDone = batch.status === "done" || !!decision;
  const needsDecision = batch.status === "needs_decision";
  // Only offer the judge buttons on a fresh batch. A `judging` batch already has
  // a judge (+ its watchdog) running — re-showing them on reload would spawn a
  // SECOND judge.
  const canJudge = batch.status === "running";
  // Decide is available with judge diffs, OR when the watchdog fell back to
  // needs_decision (server-side auto-judge couldn't pick — the human picks now).
  const canDecide = (hasDiffs || needsDecision) && !isDone;

  return (
    <div className="space-y-3 rounded-lg border border-border-subtle bg-surface-elevated p-4">
      {/* header */}
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <Swords className="size-4 shrink-0 text-accent-primary" />
            <span className="truncate font-mono text-[13px] font-semibold text-foreground-primary">
              {batch.slug}
            </span>
            <span
              className={cn(
                "shrink-0 rounded-full px-2 py-0.5 font-caption text-[10px] font-semibold tracking-wide",
                statusTone(batch.status),
              )}
            >
              {t(`fusion.status.${batch.status}`, { defaultValue: batch.status })}
            </span>
          </div>
          <p className="mt-1 line-clamp-2 font-body text-[13px] text-foreground-secondary" title={batch.need}>
            {batch.need}
          </p>
        </div>
        {canJudge && (
          <div className="flex shrink-0 items-center gap-2">
            <Button size="sm" variant="secondary" onClick={() => runJudge(false)} disabled={judging}>
              <Gavel className="size-3.5" />
              {judging
                ? t("fusion.judging", { defaultValue: "评判中…" })
                : t("fusion.judge", { defaultValue: "进入评判" })}
            </Button>
            <Button size="sm" onClick={() => runJudge(true)} disabled={judging}
              title={t("fusion.autoJudgeHint")}>
              <Gavel className="size-3.5" />
              {t("fusion.autoJudge", { defaultValue: "自动评判" })}
            </Button>
            <Button size="sm" variant="secondary" onClick={() => runJudge(true, true)} disabled={judging}
              title={t("fusion.synthHint", { defaultValue: "派一个 agent 博采众长、综合出一版最优实现并合并（不是挑一个赢家）" })}>
              <Sparkles className="size-3.5" />
              {t("fusion.synthesize", { defaultValue: "综合最优" })}
            </Button>
          </div>
        )}
      </div>

      {/* batch-level status banner (survives reload; judge diffs are session-only) */}
      {batch.status === "judging" && !hasDiffs && (
        <p className="flex items-center gap-1.5 rounded-md bg-state-warning/10 px-2.5 py-1.5 font-body text-[12px] text-state-warning">
          <Gavel className="size-3.5 shrink-0" />
          {t("fusion.autoJudgeRunning", {
            defaultValue: "评审正在读 diff 并选出结果，完成后会自动写入。",
          })}
        </p>
      )}
      {needsDecision && (
        <p className="flex items-center gap-1.5 rounded-md bg-state-danger/10 px-2.5 py-1.5 font-body text-[12px] text-state-danger">
          <Gavel className="size-3.5 shrink-0" />
          {t("fusion.needsDecision", {
            defaultValue: "自动裁决未能自动完成,请在下方手动选择赢家。",
          })}
        </p>
      )}

      {/* contestants */}
      <div className="grid gap-3 sm:grid-cols-2">
        {contestants.map((c) => {
          const isWinner = winnerId === c.thread_id;
          return (
            <div
              key={c.thread_id}
              className={cn(
                "flex flex-col gap-2 rounded-md border p-3",
                isWinner
                  ? "border-status-success bg-status-success-soft/30"
                  : "border-border-subtle bg-surface-secondary",
              )}
            >
              <div className="flex items-center justify-between gap-2">
                <span className="flex min-w-0 items-center gap-1.5">
                  {isWinner && (
                    <Trophy className="size-3.5 shrink-0 text-status-success" />
                  )}
                  <span className="truncate font-heading text-[13px] font-semibold text-foreground-primary">
                    {contestantLabel(c)}
                  </span>
                </span>
                <span className="flex shrink-0 items-center gap-1">
                  {/* Objective gate result — run for BOTH auto and manual
                      judging now; previously a manual 进入评判 silently
                      ignored the batch's check_cmd. */}
                  {c.check_passed != null && (
                    <span
                      className={cn(
                        "rounded-full px-1.5 py-0.5 font-caption text-[10px]",
                        c.check_passed
                          ? "bg-status-success-soft text-status-success"
                          : "bg-state-danger/15 text-state-danger",
                      )}
                      title={c.check_output ?? undefined}
                    >
                      {c.check_passed
                        ? t("fusion.checkPassed", { defaultValue: "客观检查通过" })
                        : t("fusion.checkFailed", { defaultValue: "客观检查未过" })}
                    </span>
                  )}
                  {c.degraded && (
                    <span
                      className="rounded-full bg-state-warning/15 px-1.5 py-0.5 font-caption text-[10px] text-state-warning"
                      title={t("fusion.degradedHint", {
                        defaultValue: "未获得独立工作树，改动无法单独比较",
                      })}
                    >
                      {t("fusion.degraded", { defaultValue: "降级" })}
                    </span>
                  )}
                </span>
              </div>

              {c.branch && (
                <div className="truncate font-mono text-[10px] text-foreground-tertiary" title={c.branch}>
                  {c.branch}
                </div>
              )}

              {hasDiffs && (
                <>
                  <div className="font-caption text-[10px] tracking-wide text-foreground-tertiary">
                    {t("fusion.filesChanged", {
                      count: c.files.length,
                      defaultValue: "改动 {{count}} 个文件",
                    })}
                  </div>
                  <FileList files={c.files} />
                </>
              )}

              {canDecide && (
                <Button
                  size="xs"
                  variant={isWinner ? "default" : "outline"}
                  onClick={() => decide(c.thread_id)}
                  disabled={deciding != null}
                  className="mt-auto"
                >
                  <Trophy className="size-3" />
                  {deciding === c.thread_id
                    ? t("fusion.deciding", { defaultValue: "选定中…" })
                    : t("fusion.pickWinner", { defaultValue: "选为赢家" })}
                </Button>
              )}
            </div>
          );
        })}
      </div>

      {/* judge thread reference */}
      {hasDiffs && judge?.judge_thread_id && (
        <p className="flex items-center gap-1.5 font-caption text-[11px] text-foreground-tertiary">
          <Gavel className="size-3" />
          {judge?.judge_agent_id
            ? t("fusion.autoJudgeRunning", {
                defaultValue: "评审正在读 diff 并选出结果，完成后会自动写入。",
              })
            : t("fusion.judgeThread", {
                defaultValue: "评审方向已创建，可在聊天中查看其判决推理。",
              })}
        </p>
      )}

      {/* decision result */}
      {decision && (
        <div className="space-y-2 rounded-md border border-status-success/40 bg-status-success-soft/20 p-3">
          <p className="flex items-center gap-1.5 font-heading text-[13px] font-semibold text-status-success">
            <Check className="size-4" />
            {t("fusion.decided", { defaultValue: "已选定" })}
          </p>
          {decision.merge_status === "merged" && (
            <p className="font-body text-[12px] text-foreground-secondary">
              {t("fusion.merged", {
                count: decision.files.length,
                base: decision.base ?? "main",
                defaultValue: "已把赢家的 {{count}} 个文件合并到 {{base}}",
              })}
            </p>
          )}
          {decision.merge_status === "resolving" && (
            <>
              <p className="flex items-center gap-1.5 font-body text-[12px] text-state-warning">
                <Bot className="size-3.5" />
                {t("fusion.resolving", {
                  count: decision.files.length,
                  defaultValue: "合并有冲突,AI 正在协调 {{count}} 个文件——回聊天看进度",
                })}
              </p>
              <FileList files={decision.files} />
            </>
          )}
          {decision.merge_status === "nothing_to_merge" && (
            <p className="font-body text-[12px] text-foreground-secondary">
              {t("fusion.nothingToMerge", {
                defaultValue: "赢家没有可合并的改动",
              })}
            </p>
          )}
          {decision.merge_status == null && (
            <p className="font-body text-[12px] text-foreground-secondary">
              {t("fusion.recordedNoMerge", {
                defaultValue: "已记录赢家(未执行合并)",
              })}
            </p>
          )}
        </div>
      )}

      {error && (
        <p className="rounded-md bg-state-danger/10 px-2.5 py-2 font-caption text-[11px] text-state-danger">
          {error}
        </p>
      )}
    </div>
  );
}

// ── View ────────────────────────────────────────────────────────────────────

export default function FusionView() {
  const { t } = useTranslation();
  const { workspace } = useWorkspaceContext();
  const workspaceId = workspace.workspaceId;
  const [batches, setBatches] = useState<FusionBatch[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const mountedRef = useRef(true);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const refresh = useCallback(async () => {
    try {
      const list = await api.listFusion(workspaceId);
      if (!mountedRef.current) return;
      setBatches(list);
      setError(null);
    } catch (e) {
      if (mountedRef.current) {
        setError(e instanceof ApiError ? e.detail : (e as Error).message);
      }
    } finally {
      if (mountedRef.current) setLoading(false);
    }
  }, [workspaceId]);

  useEffect(() => {
    setLoading(true);
    refresh();
  }, [refresh]);

  // Refetch on swarm activity so a running competition's status updates live.
  // `thread_changed` covers the decide/merge moment (the judge's curl or the
  // watchdog) — without it the card sticks on 'judging' until a manual refresh.
  useSwarmFeed({
    onEvent: (ev: SwarmEvent) => {
      if (
        ev.type === "agent_state" ||
        ev.type === "blackboard_changed" ||
        ev.type === "thread_changed"
      ) {
        refresh();
      }
    },
    onReconnect: () => refresh(),
  });

  return (
    <div className="flex h-full min-h-0 flex-1 flex-col overflow-y-auto">
      <div className="flex items-center justify-between gap-3 border-b border-border-subtle px-4 py-3">
        <div className="flex items-center gap-2">
          <Swords className="size-4 text-accent-primary" />
          <h2 className="font-heading text-sm font-semibold text-foreground-primary">
            {t("fusion.title", { defaultValue: "模型竞赛" })}
          </h2>
        </div>
        <button
          type="button"
          onClick={() => refresh()}
          className="text-foreground-tertiary hover:text-foreground-primary"
          title={t("common.refresh", { defaultValue: "刷新" })}
          aria-label={t("common.refresh", { defaultValue: "刷新" })}
        >
          <RefreshCw className="size-4" />
        </button>
      </div>

      <div className="flex flex-col gap-4 p-4">
        <CreateFusionForm workspaceId={workspaceId} onCreated={refresh} />

        {error && (
          <p className="rounded-md bg-state-danger/10 px-2.5 py-2 font-caption text-[12px] text-state-danger">
            {error}
          </p>
        )}

        {loading ? (
          <p className="font-caption text-sm text-foreground-tertiary">
            {t("common.loading", { defaultValue: "加载中…" })}
          </p>
        ) : batches.length === 0 ? (
          <EmptyState
            icon={<Swords className="size-8" />}
            title={t("fusion.empty")}
            hint={t("fusion.emptyHint")}
          />
        ) : (
          batches.map((b) => (
            <BatchCard
              key={b.id}
              workspaceId={workspaceId}
              batch={b}
              onChanged={refresh}
            />
          ))
        )}
      </div>
    </div>
  );
}
