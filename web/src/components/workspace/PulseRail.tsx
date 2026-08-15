/**
 * PulseRail — the 54px collapsed form of the members panel (P0-12).
 *
 * The full members panel only renders at ≥1536px, so on a 1280–1535px laptop
 * the richest "who's working / who's stuck" signal was completely invisible —
 * the diagnosed "并行即失控 + 健康信号 <1536px 整体蒸发" gap. This rail fills
 * that range: a vertical strip of member avatars, each with an honest state dot
 * (reusing `resolveMemberVisual` — error floats to top, typing pulses, never a
 * fake green), an unread badge, and a click that opens the member's drawer
 * (focus). At ≥1536px the full panel takes over (this rail is CSS-hidden), and
 * below 1280px neither shows (same as before).
 */
import { useTranslation } from "react-i18next";
import { Users } from "lucide-react";
import { cn } from "@/lib/cn";
import { resolveMemberVisual, roleColorClass, roleLabelAmong } from "@/lib/agent";
import type { AgentInfo, AgentLiveState, MessageRecord } from "../../api/types";

// The rail shows dots only, no status text, so the labels are stubbed — only
// `dotClass` / `typing` / `isError` from the visual are used.
const STUB_LABELS = {
  spawning: "",
  ready: "",
  thinking: "",
  idle: "",
  exited: "",
  waiting_dep: "",
  error: "",
  shimExit: "",
  starting: "",
  stalled: "",
  noResponse: "",
} as const;

export function PulseRail({
  members,
  agentStateById,
  recentMessages,
  unreadByFrom,
  onOpenAgent,
}: {
  members: AgentInfo[];
  agentStateById: Record<string, AgentLiveState>;
  recentMessages: MessageRecord[];
  unreadByFrom: Record<string, number>;
  onOpenAgent: (agentId: string) => void;
}) {
  const { t } = useTranslation();

  // Same ranking as the full panel: error first, orchestrator next, rest after.
  const isErr = (a: AgentInfo) =>
    a.killed_at == null &&
    a.shim_exit == null &&
    agentStateById[a.agent_id]?.state === "error";
  const rank = (a: AgentInfo) => (isErr(a) ? 0 : a.role === "orchestrator" ? 1 : 2);
  const sorted = [...members].sort((a, b) => rank(a) - rank(b));

  return (
    <>
      <div
        className="flex h-12 shrink-0 items-center justify-center border-b border-border-subtle"
        title={t("chat.members")}
      >
        <Users className="size-4 text-foreground-tertiary" />
      </div>
      <div className="flex flex-1 flex-col items-center gap-3 overflow-y-auto py-3">
        {sorted.map((a) => {
          const v = resolveMemberVisual(
            a,
            agentStateById[a.agent_id],
            recentMessages,
            STUB_LABELS,
          );
          const unread = unreadByFrom[a.agent_id] ?? 0;
          const isOrchestrator = a.role === "orchestrator";
          return (
            <button
              key={a.agent_id}
              type="button"
              onClick={() => onOpenAgent(a.agent_id)}
              title={roleLabelAmong(a, members)}
              aria-label={roleLabelAmong(a, members)}
              className="relative flex min-h-11 min-w-11 shrink-0 items-center justify-center transition-transform hover:scale-105"
            >
              <span
                className={cn(
                  "flex size-8 items-center justify-center rounded-full text-xs font-medium text-foreground-on-accent shadow-sm",
                  roleColorClass(a.role),
                  isOrchestrator &&
                    "ring-2 ring-accent-primary ring-offset-1 ring-offset-surface-secondary",
                )}
              >
                {a.role.charAt(0).toUpperCase()}
              </span>
              {/* honest state dot — typing pulses, error/idle/etc. coloured, and
                  nothing shown when the visual yields no dot (never a fake green) */}
              {v.typing ? (
                <span className="absolute -bottom-0.5 -right-0.5 size-2.5 animate-pulse rounded-full border border-surface-secondary bg-accent-primary" />
              ) : v.dotClass ? (
                <span
                  className={cn(
                    "absolute -bottom-0.5 -right-0.5 size-2.5 rounded-full border border-surface-secondary",
                    v.dotClass,
                  )}
                />
              ) : null}
              {unread > 0 && (
                <span className="absolute -right-1.5 -top-1.5 inline-flex min-w-[15px] items-center justify-center rounded-full bg-state-danger px-1 text-[9px] font-semibold leading-[14px] text-foreground-on-accent">
                  {unread > 9 ? "9+" : unread}
                </span>
              )}
            </button>
          );
        })}
      </div>
    </>
  );
}
