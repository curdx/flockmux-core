/**
 * SystemCard — renders a persisted `kind=system` message as a structured event
 * card, dispatched by `meta.subtype` (P1).
 *
 * The first subtype is "dispatch": when the orchestrator spawns a worker, the
 * backend now persists a system card so the delegation is visible IN the thread
 * (治诊断1「多 agent 协作在流里不可见」) instead of an opaque new member just
 * appearing in the roster. Clicking it opens that worker's drawer (focus).
 *
 * Unknown / metaless subtypes degrade to the plain centered note the system
 * message used to render as — so older rows and future subtypes never break.
 * This is the seam future P1 cards (delivery, model_changed, …) hang off.
 */
import { useTranslation } from "react-i18next";
import { CircleCheck, ChevronRight, Cpu, Split } from "lucide-react";
import { cn } from "@/lib/cn";
import { roleColorClass as roleColor, roleDisplayName } from "@/lib/agent";
import { fallbackNotice } from "@/lib/engineFallback";
import type { MessageRecord } from "../../api/types";

export function SystemCard({
  message,
  fromRole,
  onOpenAgent,
}: {
  message: MessageRecord;
  /** Resolved role of `message.from_agent` — used by the "completion" (delivery)
   *  card, whose worker isn't named in meta. */
  fromRole?: string;
  onOpenAgent?: (agentId: string) => void;
}) {
  const { t } = useTranslation();
  const subtype = message.meta?.subtype;

  // ── delivery card: a worker finished its task (farewell / completion) ──
  if (subtype === "completion") {
    const rawRole = fromRole ?? t("chat.role.member", { defaultValue: "成员" });
    const role = roleDisplayName(rawRole);
    const agent = message.from_agent;
    const clickable = onOpenAgent != null;
    const body = message.body?.trim();
    return (
      <div className="flex max-w-[min(82vw,560px)] flex-col gap-1.5 rounded-lg border border-status-success/30 bg-status-success-soft/40 px-3 py-2">
        <button
          type="button"
          disabled={!clickable}
          onClick={() => clickable && onOpenAgent?.(agent)}
          title={
            clickable
              ? t("chat.dispatch.open", { role, defaultValue: "查看 {{role}}" })
              : undefined
          }
          className={cn(
            "group flex items-center gap-2 text-left transition-colors",
            clickable && "hover:opacity-90",
          )}
        >
          <CircleCheck className="size-4 shrink-0 text-status-success" />
          <span className="min-w-0 flex-1 font-heading text-xs font-medium text-foreground-primary">
            {t("chat.delivery.title", { role, defaultValue: "{{role}} 交付完成" })}
          </span>
          {clickable && (
            <ChevronRight className="size-3 shrink-0 text-foreground-tertiary transition-colors group-hover:text-status-success" />
          )}
        </button>
        {body && (
          <p className="whitespace-pre-wrap break-words font-body text-[12px] leading-snug text-foreground-secondary">
            {body}
          </p>
        )}
      </div>
    );
  }

  // ── model switched: the captain restarted on a new model (入流律) ──
  if (subtype === "model_changed") {
    return (
      <span className="inline-flex max-w-full items-center gap-1.5 rounded-full border border-border-subtle bg-surface-secondary px-3 py-1 font-caption text-[11px] text-foreground-secondary">
        <Cpu className="size-3 shrink-0 text-foreground-tertiary" />
        <span className="min-w-0 truncate">{message.body}</span>
      </span>
    );
  }

  if (subtype === "dispatch") {
    const childRole = message.meta?.child_role ?? t("chat.role.member", { defaultValue: "成员" });
    const childRoleLabel = roleDisplayName(childRole);
    const childAgent = message.meta?.child_agent ?? null;
    const clickable = childAgent != null && onOpenAgent != null;
    // Billing red line: the worker's requested engine wasn't installed and the
    // server spawned a fallback — possibly API-billed. The card must say so;
    // a paid engine must never run in disguise.
    const fallbackFrom = message.meta?.fallback_from ?? null;
    const card = (
      <button
        type="button"
        disabled={!clickable}
        onClick={() => childAgent && onOpenAgent?.(childAgent)}
        title={
          clickable
            ? t("chat.dispatch.open", { role: childRoleLabel, defaultValue: "查看 {{role}}" })
            : undefined
        }
        className={cn(
          "group inline-flex max-w-full items-center gap-2 rounded-lg border border-border-subtle bg-surface-secondary px-3 py-1.5 transition-colors",
          clickable && "hover:border-accent-primary/40 hover:bg-accent-primary-soft/40",
        )}
      >
        <span className="flex size-5 shrink-0 items-center justify-center rounded-md bg-accent-primary-soft text-accent-primary">
          <Split className="size-3.5" />
        </span>
        <span className="min-w-0 truncate font-body text-xs text-foreground-primary">
          {t("chat.dispatch.title", { role: childRoleLabel, defaultValue: "派给 {{role}}" })}
        </span>
        {/* role-color tick so the worker's identity reads consistently */}
        <span
          className={cn("size-1.5 shrink-0 rounded-full", roleColor(childRole))}
          aria-hidden
        />
        {clickable && (
          <ChevronRight className="size-3 shrink-0 text-foreground-tertiary transition-colors group-hover:text-accent-primary" />
        )}
      </button>
    );
    if (!fallbackFrom) return card;
    return (
      <span className="inline-flex max-w-full flex-col items-start gap-1">
        {card}
        <span className="inline-flex max-w-full items-center gap-1 rounded-full border border-state-warning/40 bg-state-warning/10 px-2.5 py-0.5 font-caption text-[10px] text-state-warning">
          {fallbackNotice(t, {
            cli: message.meta?.fallback_cli ?? "?",
            fallback_from: fallbackFrom,
            billing_surface: message.meta?.billing_surface,
          })}
        </span>
      </span>
    );
  }

  // Unknown subtype / plain system note: the original centered hairline pill.
  return (
    <span className="selectable rounded-full bg-surface-tertiary px-3 py-0.5 font-caption text-[10px] text-foreground-tertiary">
      {message.body}
    </span>
  );
}
