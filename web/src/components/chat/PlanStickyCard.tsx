/**
 * PlanStickyCard — 规划计划，钉在对话上方。
 *
 * 视觉原则：默认是一条进度条 + 未完成项，不把已勾完的步骤摊满半屏。
 * 点开才展开全表。handoff 未交付时禁止绿条撒谎。
 */
import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  ChevronDown,
  Circle,
  CircleCheck,
  CircleDot,
  ListTodo,
  TriangleAlert,
  X,
} from "lucide-react";
import i18n from "@/i18n";
import { cn } from "@/lib/cn";
import { roleColorClass as roleColor } from "@/lib/agent";
import type { ParsedPlan, PlanStatus } from "@/lib/parsePlan";

function ownerLabel(
  owner: string | undefined,
): { name: string; isCaptain: boolean } | null {
  if (!owner) return null;
  const o = owner.trim().toLowerCase();
  if (["self", "orchestrator", "captain", "me", "队长", "管家", "组长", "规划"].includes(o)) {
    return {
      name: i18n.t("chat.role.captain", { defaultValue: "规划" }),
      isCaptain: true,
    };
  }
  return { name: owner, isCaptain: false };
}

function StatusGlyph({ status }: { status: PlanStatus }) {
  if (status === "done") {
    return <CircleCheck className="size-3.5 shrink-0 text-status-success" aria-label="done" />;
  }
  if (status === "doing") {
    return <CircleDot className="size-3.5 shrink-0 text-accent-primary" aria-label="in progress" />;
  }
  if (status === "blocked") {
    return <TriangleAlert className="size-3.5 shrink-0 text-state-warning" aria-label="blocked" />;
  }
  return <Circle className="size-3.5 shrink-0 text-foreground-tertiary" aria-label="todo" />;
}

function StepRow({
  task,
  status,
  owner,
}: {
  task: string;
  status: PlanStatus;
  owner: string | undefined;
}) {
  const label = ownerLabel(owner);
  return (
    <li className="flex items-center gap-2 py-0.5 text-[13px] leading-snug">
      <StatusGlyph status={status} />
      <span
        className={cn(
          "min-w-0 flex-1 truncate",
          status === "done"
            ? "text-foreground-tertiary line-through"
            : "text-foreground-primary",
        )}
        title={task}
      >
        {task}
      </span>
      {label && (
        <span
          className={cn(
            "inline-flex shrink-0 items-center gap-1 rounded px-1.5 py-px font-caption text-[10px]",
            label.isCaptain
              ? "text-accent-primary"
              : "text-foreground-secondary",
          )}
        >
          {!label.isCaptain && (
            <span className={cn("size-1.5 rounded-full", roleColor(label.name))} />
          )}
          {label.name}
        </span>
      )}
    </li>
  );
}

export function PlanStickyCard({
  plan,
  undeliveredHandoffs = 0,
  dismissKey,
}: {
  plan: ParsedPlan;
  undeliveredHandoffs?: number;
  /** workspace+thread id — session-dismiss completed plan strip */
  dismissKey?: string;
}) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const total = plan.steps.length;
  const done = plan.steps.filter((s) => s.status === "done").length;
  const allDone = total > 0 && done === total;
  const handoffsPending = undeliveredHandoffs > 0;
  const openSteps = plan.steps.filter((s) => s.status !== "done");
  const pct = total > 0 ? Math.round((done / total) * 100) : 0;
  const storageKey = useMemo(() => {
    if (!dismissKey) return null;
    return `swarmx:plan-done-dismiss:${dismissKey}`;
  }, [dismissKey]);
  const [doneDismissed, setDoneDismissed] = useState(() => {
    if (!storageKey || typeof sessionStorage === "undefined") return false;
    try {
      return sessionStorage.getItem(storageKey) === "1";
    } catch {
      return false;
    }
  });

  if (allDone && handoffsPending) {
    return (
      <div className="shrink-0 border-b border-border-subtle px-4 py-1.5">
        <div className="mx-auto flex max-w-[1040px] items-center gap-2 text-[13px]">
          <TriangleAlert className="size-3.5 shrink-0 text-status-warning" aria-hidden />
          <span className="truncate font-medium text-foreground-secondary">
            {t("chat.plan.doneButHandoffMissing", {
              total,
              count: undeliveredHandoffs,
              defaultValue: "计划勾了完成 · 仍有 {{count}} 个成员没交结果",
            })}
          </span>
        </div>
      </div>
    );
  }

  if (allDone) {
    if (doneDismissed) return null;
    return (
      <div className="shrink-0 border-b border-border-subtle px-4 py-1.5">
        <div className="mx-auto flex max-w-[1040px] items-center gap-2 text-[13px]">
          <CircleCheck className="size-3.5 shrink-0 text-status-success" aria-hidden />
          <span className="min-w-0 flex-1 truncate font-medium text-foreground-secondary">
            {t("chat.plan.allDone", {
              total,
              defaultValue: "计划完成 · 全部 {{total}} 步已交付",
            })}
          </span>
          <button
            type="button"
            className="inline-flex size-6 shrink-0 items-center justify-center rounded text-foreground-tertiary hover:bg-surface-secondary hover:text-foreground-secondary"
            aria-label={t("chat.plan.dismissDone", { defaultValue: "收起" })}
            title={t("chat.plan.dismissDone", { defaultValue: "收起" })}
            onClick={() => {
              setDoneDismissed(true);
              if (storageKey) {
                try {
                  sessionStorage.setItem(storageKey, "1");
                } catch {
                  /* ignore quota */
                }
              }
            }}
          >
            <X className="size-3.5" aria-hidden />
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="shrink-0 border-b border-border-subtle px-4 py-1.5">
      <div className="mx-auto max-w-[1040px]">
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          aria-expanded={expanded}
          className="group flex w-full items-center gap-2.5 rounded-md py-1 text-left transition-colors hover:bg-surface-secondary/80 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent-primary/40"
        >
          <ListTodo className="size-3.5 shrink-0 text-foreground-tertiary" aria-hidden />
          <span className="shrink-0 font-heading text-xs font-medium text-foreground-secondary">
            {t("chat.plan.titleCompact", {
              done,
              total,
              defaultValue: "计划 {{done}}/{{total}}",
            })}
          </span>
          <span
            className="h-1 min-w-[48px] max-w-[120px] flex-1 overflow-hidden rounded-full bg-surface-tertiary"
            aria-hidden
          >
            <span
              className="block h-full rounded-full bg-accent-primary transition-[width] duration-300"
              style={{ width: `${pct}%` }}
            />
          </span>
          {!expanded && openSteps[0] && (
            <span className="min-w-0 flex-1 truncate font-body text-[12px] text-foreground-tertiary">
              {openSteps[0].task}
            </span>
          )}
          {expanded && <span className="min-w-0 flex-1" />}
          <ChevronDown
            className={cn(
              "size-3.5 shrink-0 text-foreground-tertiary transition-transform duration-200",
              expanded && "rotate-180",
            )}
            aria-hidden
          />
        </button>

        {/* 默认一行；展开才摊全表 —— 别再用蓝卡片把聊天顶出视口 */}
        {expanded && (
          <ul className="mt-1 max-h-[28vh] space-y-0.5 overflow-y-auto border-l border-border-subtle pl-3 ml-1.5 pb-1">
            {plan.steps.map((s, i) => (
              <StepRow
                key={s.seq ?? i}
                task={s.task}
                status={s.status}
                owner={s.owner}
              />
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}
