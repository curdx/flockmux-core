/**
 * Goal Mode (`/goals`).
 *
 * Workspace-level objectives with acceptance criteria, status, and optional
 * token budget. This is the durable layer above the task board: tasks show
 * what workers are doing now; goals show what the whole run is supposed to
 * achieve and whether it is still active, blocked, or complete.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Archive,
  CheckCircle2,
  CirclePause,
  Flag,
  Loader2,
  MessageSquarePlus,
  OctagonAlert,
  Plus,
  RefreshCw,
} from "lucide-react";
import { api } from "@/api/http";
import type { GoalEvidenceRecord, GoalRecord, GoalStatus, ThreadInfo } from "@/api/types";
import { fmtTokens } from "@/lib/format";
import { WorkspacePicker } from "@/components/WorkspacePicker";
import { EmptyState } from "@/components/EmptyState";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/cn";
import { relTime } from "@/lib/relTime";
import { toast } from "@/lib/toast";
import { useToolWorkspaces } from "@/lib/useToolWorkspaces";

const STATUSES: Array<{ key: GoalStatus; icon: typeof Flag; tone: string }> = [
  { key: "active", icon: Flag, tone: "text-accent-primary" },
  { key: "paused", icon: CirclePause, tone: "text-foreground-tertiary" },
  { key: "blocked", icon: OctagonAlert, tone: "text-status-danger" },
  { key: "complete", icon: CheckCircle2, tone: "text-status-success" },
  { key: "archived", icon: Archive, tone: "text-foreground-tertiary" },
];
const MAIN_THREAD_VALUE = "__main__";
// Radix Select forbids an empty-string item value, so the "all directions"
// option needs its own sentinel (mirrors WorkspacePicker's __all_workspaces__).
const ALL_DIRECTIONS_VALUE = "__all__";
// Evidence kinds: a small, meaningful enum instead of a free-text box whose
// semantics nobody could guess. `note` is the default neutral entry. Chinese
// fallbacks live here so the dropdown never shows a raw enum token before the
// locale merge lands.
const EVIDENCE_KINDS: Array<{ key: string; zh: string }> = [
  { key: "note", zh: "记录" },
  { key: "proof", zh: "证明" },
  { key: "blocker", zh: "阻塞" },
];
// P2: token budget is an i64 on the backend; a digit string longer than that
// makes serde reject the body with a 500 before our route can validate it, and
// anything above Number.MAX_SAFE_INTEGER loses precision in JS. Cap well below
// both (1 trillion tokens is already absurd for a budget) and refuse to submit.
const MAX_BUDGET_TOKENS = 1_000_000_000_000;

function parseCriteria(input: string): string[] {
  // P2: dedupe so identical lines don't collide as React keys (which silently
  // drops the duplicate rows from the rendered list).
  const seen = new Set<string>();
  const out: string[] = [];
  for (const line of input.split("\n")) {
    const c = line.replace(/^[-*]\s+/, "").trim();
    if (c && !seen.has(c)) {
      seen.add(c);
      out.push(c);
    }
  }
  return out;
}

function formatBudget(n: number | null): string {
  if (!n) return "—";
  return fmtTokens(n);
}

export default function GoalsRoute() {
  const { t } = useTranslation();
  const { workspaces, wsId, setWsId, ready, error: wsError } = useToolWorkspaces();
  const [threadId, setThreadId] = useState<string>("");
  const [threads, setThreads] = useState<ThreadInfo[]>([]);
  const [goals, setGoals] = useState<GoalRecord[]>([]);
  const [objective, setObjective] = useState("");
  const [criteria, setCriteria] = useState("");
  const [budget, setBudget] = useState("");
  const [loading, setLoading] = useState(true);
  const [creating, setCreating] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  // Guard against stale list responses: if the workspace/thread changes (or the
  // button is spammed) while a slower fetch is in flight, only the newest one is
  // allowed to write state.
  const loadReqId = useRef(0);

  const activeWorkspace = useMemo(
    () => workspaces.find((w) => w.id === wsId) ?? null,
    [workspaces, wsId],
  );
  // P2: validate the token budget before it can reach the backend i64.
  const budgetError = useMemo(() => {
    const raw = budget.trim();
    if (!raw) return null;
    // Don't silently strip non-digits in onChange (a user typing "1,000" would
    // watch it become "1000" with no idea why). Keep their input, flag it here.
    if (!/^\d+$/.test(raw)) {
      return t("goals.budgetNotNumeric", {
        defaultValue: "只能填数字（不带逗号、空格或字母）",
      });
    }
    const n = Number(raw);
    if (!Number.isSafeInteger(n) || n > MAX_BUDGET_TOKENS) {
      return t("goals.budgetTooLarge", {
        defaultValue: "预算过大，请输入不超过 1 万亿的整数",
      });
    }
    return null;
  }, [budget, t]);
  // P1-16: backend list failed (wsError) or goal fetch failed (err) — either
  // way an empty list is a CONNECTION problem, not "you have no goals yet".
  const hasError = Boolean(wsError || err);

  useEffect(() => {
    setThreadId("");
    setThreads(activeWorkspace?.threads ?? []);
  }, [activeWorkspace]);

  const load = useCallback(async () => {
    if (!wsId) {
      setGoals([]);
      setLoading(false);
      return;
    }
    const reqId = ++loadReqId.current;
    setLoading(true);
    try {
      const threadFilter =
        threadId === MAIN_THREAD_VALUE ? null : threadId || undefined;
      const res = await api.listGoals(wsId, threadFilter);
      if (reqId !== loadReqId.current) return; // a newer load superseded this one
      setGoals(res.goals);
      setErr(null);
    } catch (e) {
      if (reqId !== loadReqId.current) return;
      // P1-16/17: mark "load failed" so the list area can offer retry instead
      // of pretending you simply have no goals. Keep the raw detail off the
      // headline — surface it in a toast description.
      setErr((e as Error)?.message || "load-failed");
      toast.error(t("goals.loadFailed", { defaultValue: "加载目标失败，请重试" }), {
        description: (e as Error)?.message,
      });
    } finally {
      if (reqId === loadReqId.current) setLoading(false);
    }
  }, [t, threadId, wsId]);

  useEffect(() => {
    if (!ready) return;
    load();
  }, [load, ready]);

  const create = async () => {
    if (!wsId || !objective.trim() || budgetError) return;
    setCreating(true);
    setErr(null);
    try {
      const rawBudget = budget.trim();
      await api.createGoal({
        workspace_id: wsId,
        thread_id: threadId && threadId !== MAIN_THREAD_VALUE ? threadId : null,
        objective: objective.trim(),
        success_criteria: parseCriteria(criteria),
        budget_tokens: rawBudget ? Number(rawBudget) : null,
      });
      setObjective("");
      setCriteria("");
      setBudget("");
      await load();
    } catch (e) {
      // P1-17: don't dump "POST /api/... 500" into the UI as the headline.
      toast.error(t("goals.createFailed", { defaultValue: "创建目标失败，请重试" }), {
        description: (e as Error)?.message,
      });
    } finally {
      setCreating(false);
    }
  };

  const setStatus = async (goal: GoalRecord, status: GoalStatus) => {
    // P1-15: keep the pre-change list so a failed write can be rolled back
    // instead of leaving the card optimistically in a status the backend
    // never recorded.
    const snap = goals;
    setGoals((prev) =>
      prev.map((g) =>
        g.id === goal.id
          ? {
              ...g,
              status,
              updated_at: Date.now(),
              completed_at: status === "complete" || status === "archived" ? Date.now() : null,
            }
          : g,
      ),
    );
    try {
      await api.updateGoalStatus(goal.id, status);
      load();
    } catch (e) {
      // P1-15: a failed write must NOT masquerade as success — roll back to
      // the real state and tell the user, rather than refetching blindly in
      // finally (which itself can fail and strand the optimistic status).
      setGoals(snap);
      toast.error(t("goals.statusFailed", { defaultValue: "状态更新失败，请重试" }), {
        description: (e as Error)?.message,
      });
    }
  };

  return (
    <div className="h-full overflow-y-auto">
      <div className="mx-auto flex max-w-5xl flex-col gap-5 px-6 py-6">
        <header className="flex items-start justify-between gap-3">
          <div className="flex flex-col gap-1">
            <h1 className="font-display text-lg text-foreground-primary">{t("goals.title")}</h1>
            <p className="font-caption text-xs text-foreground-tertiary">{t("goals.subtitle")}</p>
          </div>
          <div className="flex shrink-0 items-center gap-2">
            <button
              type="button"
              onClick={load}
              disabled={loading}
              title={t("common.refresh")}
              aria-label={t("common.refresh")}
              className="rounded border border-border-subtle p-1 text-foreground-tertiary hover:bg-surface-tertiary hover:text-foreground-secondary disabled:cursor-not-allowed disabled:opacity-50"
            >
              <RefreshCw className={cn("size-3.5", loading && "animate-spin")} />
            </button>
            <WorkspacePicker workspaces={workspaces} value={wsId} onChange={setWsId} />
            <Select
              value={threadId || ALL_DIRECTIONS_VALUE}
              onValueChange={(next) =>
                setThreadId(next === ALL_DIRECTIONS_VALUE ? "" : next)
              }
              disabled={!wsId}
            >
              <SelectTrigger aria-label={t("goals.thread")} className="min-w-[140px] text-[13px]">
                <SelectValue placeholder={t("goals.allDirections")} />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={ALL_DIRECTIONS_VALUE}>{t("goals.allDirections")}</SelectItem>
                <SelectItem value={MAIN_THREAD_VALUE}>{t("goals.mainDirection")}</SelectItem>
                {threads.map((th) => (
                  <SelectItem key={th.id} value={th.id}>
                    {th.name || th.slug}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        </header>

        <section className="grid gap-4 lg:grid-cols-[360px_1fr]">
          <div className="flex flex-col gap-3 rounded-lg border border-border-subtle bg-surface-secondary p-4">
            <div className="flex items-center gap-2">
              <Plus className="size-4 text-accent-primary" />
              <h2 className="font-caption text-sm font-medium text-foreground-primary">
                {t("goals.newGoal")}
              </h2>
            </div>
            <label className="flex flex-col gap-1">
              <span className="font-caption text-[11px] text-foreground-tertiary">
                {t("goals.objective")}
              </span>
              <textarea
                id="goal-objective"
                name="objective"
                value={objective}
                onChange={(e) => setObjective(e.target.value)}
                rows={3}
                maxLength={4000}
                className="resize-none rounded border border-border-subtle bg-surface-primary px-2 py-1.5 text-[13px] text-foreground-primary"
                placeholder={t("goals.objectivePlaceholder")}
              />
            </label>
            <label className="flex flex-col gap-1">
              <span className="font-caption text-[11px] text-foreground-tertiary">
                {t("goals.criteria")}
              </span>
              <textarea
                id="goal-criteria"
                name="criteria"
                value={criteria}
                onChange={(e) => setCriteria(e.target.value)}
                rows={5}
                maxLength={4000}
                className="resize-none rounded border border-border-subtle bg-surface-primary px-2 py-1.5 text-[13px] text-foreground-primary"
                placeholder={t("goals.criteriaPlaceholder")}
              />
            </label>
            <label className="flex flex-col gap-1">
              <span className="font-caption text-[11px] text-foreground-tertiary">
                {t("goals.budget")}
              </span>
              <input
                id="goal-budget"
                name="budget"
                value={budget}
                onChange={(e) => setBudget(e.target.value)}
                inputMode="numeric"
                aria-invalid={budgetError ? true : undefined}
                className={cn(
                  "rounded border bg-surface-primary px-2 py-1.5 font-mono text-[13px]",
                  budgetError ? "border-status-danger" : "border-border-subtle",
                )}
                placeholder="200000"
              />
              {budgetError && (
                <span className="font-caption text-[11px] text-status-danger">{budgetError}</span>
              )}
            </label>
            <button
              type="button"
              onClick={create}
              disabled={creating || !wsId || !objective.trim() || Boolean(budgetError)}
              className="inline-flex items-center justify-center gap-1.5 rounded-md bg-accent-primary px-3 py-1.5 text-[13px] text-foreground-on-accent disabled:opacity-50"
            >
              {creating && <Loader2 className="size-3.5 animate-spin" />}
              {creating ? t("goals.creating") : t("goals.create")}
            </button>
          </div>

          <div className="flex min-h-[360px] flex-col gap-3">
            {/* P1-17: a transient refetch failure while a list is showing — a
                friendly banner + retry, never the raw "GET /api/... 500". */}
            {hasError && goals.length > 0 && (
              <div className="flex items-center justify-between gap-3 rounded-lg border border-border-subtle bg-surface-secondary px-4 py-3 font-caption text-sm text-status-danger">
                <span>{t("goals.loadFailed", { defaultValue: "加载目标失败，请重试" })}</span>
                <button
                  type="button"
                  onClick={load}
                  className="inline-flex shrink-0 items-center gap-1 rounded border border-border-subtle px-2 py-1 text-foreground-secondary hover:bg-surface-tertiary hover:text-foreground-primary"
                >
                  <RefreshCw className="size-3" /> {t("common.retry", { defaultValue: "重试" })}
                </button>
              </div>
            )}
            {loading ? (
              <div className="flex items-center gap-2 rounded-lg border border-border-subtle bg-surface-secondary px-4 py-6 font-caption text-xs text-foreground-tertiary">
                <Loader2 className="size-3.5 animate-spin" /> {t("common.loading")}
              </div>
            ) : goals.length === 0 && hasError ? (
              /* P1-16: an empty list after a FAILED fetch is NOT "no goals" —
                 say the backend couldn't be reached and offer retry. */
              <div className="flex flex-1 flex-col items-center justify-center gap-3 rounded-lg border border-border-subtle bg-surface-secondary px-4 py-10 text-center font-caption text-sm text-status-danger">
                <span>{t("goals.loadFailed", { defaultValue: "连不上后端，加载目标失败" })}</span>
                <button
                  type="button"
                  onClick={load}
                  className="inline-flex items-center gap-1 rounded border border-border-subtle px-2.5 py-1 text-foreground-secondary hover:bg-surface-tertiary hover:text-foreground-primary"
                >
                  <RefreshCw className="size-3" /> {t("common.retry", { defaultValue: "重试" })}
                </button>
              </div>
            ) : goals.length === 0 ? (
              <EmptyState
                icon={<Flag className="size-8" />}
                title={t("goals.empty")}
                hint={t("goals.emptyHint")}
              />
            ) : (
              goals.map((goal) => (
                <GoalCard
                  key={goal.id}
                  goal={goal}
                  threads={threads}
                  onStatus={setStatus}
                  onEvidenceAdded={load}
                  t={t}
                />
              ))
            )}
          </div>
        </section>
      </div>
    </div>
  );
}

function GoalCard({
  goal,
  threads,
  onStatus,
  onEvidenceAdded,
  t,
}: {
  goal: GoalRecord;
  threads: ThreadInfo[];
  onStatus: (goal: GoalRecord, status: GoalStatus) => void;
  onEvidenceAdded: () => void;
  t: (k: string, o?: Record<string, unknown>) => string;
}) {
  const [evidence, setEvidence] = useState<GoalEvidenceRecord[]>([]);
  const [evidenceOpen, setEvidenceOpen] = useState(false);
  const [evidenceSummary, setEvidenceSummary] = useState("");
  const [evidenceKind, setEvidenceKind] = useState("note");
  const [evidenceLoading, setEvidenceLoading] = useState(false);
  const [evidenceErr, setEvidenceErr] = useState<string | null>(null);
  const metaThread =
    goal.thread_id == null
      ? t("goals.mainDirection")
      : threads.find((th) => th.id === goal.thread_id)?.name ||
        threads.find((th) => th.id === goal.thread_id)?.slug ||
        goal.thread_id.slice(0, 8);
  const current = STATUSES.find((s) => s.key === goal.status) ?? STATUSES[0];
  const CurrentIcon = current.icon;
  const loadEvidence = useCallback(async () => {
    if (!evidenceOpen) return;
    setEvidenceLoading(true);
    try {
      const res = await api.listGoalEvidence(goal.id, 20);
      setEvidence(res.evidence);
      setEvidenceErr(null);
    } catch (e) {
      // P1-17: friendly inline message; raw detail goes to the toast.
      setEvidenceErr(t("goals.evidenceLoadFailed", { defaultValue: "加载证据失败，请重试" }));
      toast.error(t("goals.evidenceLoadFailed", { defaultValue: "加载证据失败，请重试" }), {
        description: (e as Error)?.message,
      });
    } finally {
      setEvidenceLoading(false);
    }
  }, [evidenceOpen, goal.id, t]);

  useEffect(() => {
    loadEvidence();
  }, [loadEvidence]);

  const addEvidence = async () => {
    if (!evidenceSummary.trim()) return;
    setEvidenceLoading(true);
    try {
      await api.addGoalEvidence(goal.id, {
        kind: evidenceKind.trim() || "note",
        summary: evidenceSummary.trim(),
      });
      setEvidenceSummary("");
      setEvidenceOpen(true);
      await loadEvidence();
      onEvidenceAdded();
    } catch (e) {
      // P1-17: friendly inline message; raw "POST /api/... 500" goes to toast.
      setEvidenceErr(t("goals.evidenceAddFailed", { defaultValue: "添加证据失败，请重试" }));
      toast.error(t("goals.evidenceAddFailed", { defaultValue: "添加证据失败，请重试" }), {
        description: (e as Error)?.message,
      });
    } finally {
      setEvidenceLoading(false);
    }
  };

  return (
    <article className="rounded-lg border border-border-subtle bg-surface-secondary p-4">
      <div className="flex items-start justify-between gap-3">
        <div className="flex min-w-0 flex-col gap-1">
          <div className="flex items-center gap-2">
            <CurrentIcon className={cn("size-4 shrink-0", current.tone)} />
            <h2 className="text-sm font-medium leading-snug text-foreground-primary">
              {goal.objective}
            </h2>
          </div>
          <div className="flex flex-wrap gap-x-3 gap-y-1 font-caption text-[11px] text-foreground-tertiary">
            <span>{metaThread}</span>
            <span>{t("goals.updated", { time: relTime(goal.updated_at, t) })}</span>
            <span>{t("goals.budgetShort", { budget: formatBudget(goal.budget_tokens) })}</span>
          </div>
        </div>
        <span className="shrink-0 rounded bg-surface-tertiary px-2 py-0.5 font-caption text-[11px] text-foreground-secondary">
          {t(`goals.status.${goal.status}`)}
        </span>
      </div>

      {goal.success_criteria.length > 0 && (
        <ul className="mt-3 flex flex-col gap-1">
          {goal.success_criteria.map((c, i) => (
            // P2: index-prefixed key — identical criteria text would otherwise
            // collide and silently drop the duplicate rows from the list.
            <li key={`${i}-${c}`} className="flex gap-2 text-[12px] text-foreground-secondary">
              <span className="mt-1 size-1.5 shrink-0 rounded-full bg-accent-primary" />
              <span>{c}</span>
            </li>
          ))}
        </ul>
      )}

      <div className="mt-3 flex flex-wrap gap-1.5">
        {STATUSES.map(({ key, icon: Icon }) => (
          <button
            key={key}
            type="button"
            disabled={goal.status === key}
            onClick={() => onStatus(goal, key)}
            className={cn(
              "inline-flex items-center gap-1 rounded border px-2 py-1 font-caption text-[11px] transition-colors",
              goal.status === key
                ? "border-accent-primary bg-accent-primary-soft text-accent-primary"
                : "border-border-subtle text-foreground-secondary hover:bg-surface-tertiary hover:text-foreground-primary",
            )}
          >
            <Icon className="size-3" />
            {t(`goals.status.${key}`)}
          </button>
        ))}
        <button
          type="button"
          onClick={() => setEvidenceOpen((v) => !v)}
          className="inline-flex items-center gap-1 rounded border border-border-subtle px-2 py-1 font-caption text-[11px] text-foreground-secondary hover:bg-surface-tertiary hover:text-foreground-primary"
        >
          <MessageSquarePlus className="size-3" />
          {t("goals.evidence")}
        </button>
      </div>

      {evidenceOpen && (
        <div className="mt-3 rounded-md border border-border-subtle bg-surface-primary p-3">
          <div className="grid gap-2 sm:grid-cols-[120px_1fr_auto]">
            <Select value={evidenceKind} onValueChange={setEvidenceKind}>
              <SelectTrigger
                aria-label={t("goals.evidenceKind")}
                className="w-full text-[12px]"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {EVIDENCE_KINDS.map((kind) => (
                  <SelectItem key={kind.key} value={kind.key}>
                    {t(`goals.evidenceKindLabel.${kind.key}`, {
                      defaultValue: kind.zh,
                    })}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <input
              value={evidenceSummary}
              onChange={(e) => setEvidenceSummary(e.target.value)}
              className="rounded border border-border-subtle bg-surface-secondary px-2 py-1 text-[12px]"
              placeholder={t("goals.evidencePlaceholder")}
              aria-label={t("goals.evidenceSummary")}
            />
            <button
              type="button"
              disabled={evidenceLoading || !evidenceSummary.trim()}
              onClick={addEvidence}
              className="inline-flex items-center justify-center gap-1 rounded bg-accent-primary px-2 py-1 font-caption text-[11px] text-foreground-on-accent disabled:opacity-50"
            >
              {evidenceLoading && <Loader2 className="size-3 animate-spin" />}
              {t("goals.addEvidence")}
            </button>
          </div>
          <p className="mt-1.5 font-caption text-[10px] text-foreground-tertiary">
            {t("goals.evidenceKindHint")}
          </p>
          {evidenceErr && (
            <p className="mt-2 font-caption text-[11px] text-status-danger">{evidenceErr}</p>
          )}
          <div className="mt-3 flex flex-col gap-2">
            {evidenceLoading && evidence.length === 0 ? (
              <p className="font-caption text-[11px] text-foreground-tertiary">
                {t("common.loading")}
              </p>
            ) : evidence.length === 0 ? (
              <p className="font-caption text-[11px] text-foreground-tertiary">
                {t("goals.noEvidence")}
              </p>
            ) : (
              evidence.map((ev) => (
                <div key={ev.id} className="flex gap-2 text-[12px] text-foreground-secondary">
                  <span className="shrink-0 rounded bg-surface-tertiary px-1.5 py-0.5 font-mono text-[10px] text-foreground-tertiary">
                    {ev.kind}
                  </span>
                  <span className="min-w-0 flex-1">{ev.summary}</span>
                  <span className="shrink-0 font-caption text-[10px] text-foreground-tertiary">
                    {relTime(ev.created_at, t)}
                  </span>
                </div>
              ))
            )}
          </div>
        </div>
      )}
    </article>
  );
}
