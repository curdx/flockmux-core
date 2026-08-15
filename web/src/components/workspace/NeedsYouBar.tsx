/**
 * NeedsYouBar — 「需要你」收件箱：单行、紧凑、不抢戏。
 *
 * error → 开抽屉。handoff（已死）→ 聚焦输入框；× 本会话收起。
 * stalled 不进栏（自动催）。
 */

import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, PackageX, X } from "lucide-react";
import { cn } from "@/lib/cn";
import { deriveNeedsYou, type NeedsYouItem, type NeedsYouKind } from "@/lib/needsYou";
import { roleLabelAmong } from "@/lib/agent";
import { toast } from "@/lib/toast";
import type { AgentInfo, AgentLiveState, MessageRecord } from "@/api/types";

export const FOCUS_COMPOSER_EVENT = "swarmx:focus-composer";

const DISMISS_KEY = "swarmx:needsYou:dismissed";

function readDismissed(): Set<string> {
  try {
    const raw = sessionStorage.getItem(DISMISS_KEY);
    if (!raw) return new Set();
    const arr = JSON.parse(raw) as unknown;
    return Array.isArray(arr) ? new Set(arr.filter((x) => typeof x === "string")) : new Set();
  } catch {
    return new Set();
  }
}

function writeDismissed(ids: Set<string>) {
  try {
    sessionStorage.setItem(DISMISS_KEY, JSON.stringify([...ids]));
  } catch {
    /* ignore */
  }
}

function agentEnded(a: AgentInfo): boolean {
  return a.killed_at != null || a.shim_exit != null;
}

const KIND_META: Record<
  Exclude<NeedsYouKind, "stalled">,
  { icon: typeof AlertTriangle; tone: string; key: string }
> = {
  error: {
    icon: AlertTriangle,
    tone: "text-status-danger bg-status-danger-soft/80 hover:bg-status-danger-soft",
    key: "needsYou.kind.error",
  },
  handoff: {
    icon: PackageX,
    tone: "text-accent-primary-deep bg-accent-primary-soft/70 hover:bg-accent-primary-soft",
    key: "needsYou.kind.handoff",
  },
};

interface Props {
  members: AgentInfo[];
  liveById: Record<string, AgentLiveState | undefined>;
  messages: MessageRecord[];
  onOpenAgent: (agentId: string) => void;
}

export function NeedsYouBar({
  members,
  liveById,
  messages,
  onOpenAgent,
}: Props) {
  const { t } = useTranslation();
  const [now, setNow] = useState(Date.now());
  const [dismissed, setDismissed] = useState<Set<string>>(() => readDismissed());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 5000);
    return () => window.clearInterval(id);
  }, []);

  const items = useMemo(
    () =>
      deriveNeedsYou(members, liveById, messages, now).filter(
        (i): i is NeedsYouItem & { kind: "error" | "handoff" } =>
          (i.kind === "error" || i.kind === "handoff") &&
          !dismissed.has(i.agent.agent_id),
      ),
    [members, liveById, messages, now, dismissed],
  );

  if (items.length === 0) return null;

  const shown = items.slice(0, 4);
  const extra = items.length - shown.length;

  const dismiss = (agentId: string) => {
    setDismissed((prev) => {
      const next = new Set(prev);
      next.add(agentId);
      writeDismissed(next);
      return next;
    });
  };

  const dismissAll = () => {
    setDismissed((prev) => {
      const next = new Set(prev);
      for (const i of items) next.add(i.agent.agent_id);
      writeDismissed(next);
      return next;
    });
  };

  const askCaptain = () => {
    window.dispatchEvent(new CustomEvent(FOCUS_COMPOSER_EVENT));
    toast.message(
      t("needsYou.askCaptainToast", {
        defaultValue: "在下面跟规划说一声就行",
      }),
    );
  };

  return (
    <div
      role="status"
      aria-live="polite"
      className="flex h-9 shrink-0 items-center gap-2 border-b border-border-subtle bg-surface-secondary/60 px-3"
    >
      <span className="shrink-0 font-caption text-[11px] font-medium text-foreground-tertiary">
        {t("needsYou.titleShort", {
          count: items.length,
          defaultValue: "需要你 · {{count}}",
        })}
      </span>
      <div className="flex min-w-0 flex-1 items-center gap-1.5 overflow-x-auto">
        {shown.map((item) => (
          <NeedsYouChip
            key={item.agent.agent_id}
            item={item}
            peers={members}
            onOpen={onOpenAgent}
            onAskCaptain={askCaptain}
            onDismiss={dismiss}
          />
        ))}
        {extra > 0 && (
          <span className="shrink-0 font-caption text-[11px] text-foreground-tertiary">
            {t("needsYou.more", { count: extra })}
          </span>
        )}
      </div>
      <button
        type="button"
        onClick={dismissAll}
        className="shrink-0 rounded px-1.5 py-1 font-caption text-[11px] text-foreground-tertiary transition-colors hover:bg-surface-tertiary hover:text-foreground-secondary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent-primary/40"
      >
        {t("needsYou.dismissAll", { defaultValue: "知道了" })}
      </button>
    </div>
  );
}

function NeedsYouChip({
  item,
  peers,
  onOpen,
  onAskCaptain,
  onDismiss,
}: {
  item: NeedsYouItem & { kind: "error" | "handoff" };
  peers: AgentInfo[];
  onOpen: (agentId: string) => void;
  onAskCaptain: () => void;
  onDismiss: (agentId: string) => void;
}) {
  const { t } = useTranslation();
  const meta = KIND_META[item.kind];
  const Icon = meta.icon;
  const role = roleLabelAmong(item.agent, peers);
  const ended = agentEnded(item.agent);
  const primary = () => {
    if (item.kind === "handoff" && ended) {
      onAskCaptain();
      return;
    }
    onOpen(item.agent.agent_id);
  };
  const aria =
    item.kind === "handoff" && ended
      ? t("needsYou.askCaptain", { role, defaultValue: "跟规划说 · {{role}}" })
      : t("needsYou.openAgent", { role });

  return (
    <div
      className={cn(
        "group relative flex h-7 shrink-0 items-center rounded-md",
        meta.tone,
      )}
    >
      <button
        type="button"
        onClick={primary}
        aria-label={aria}
        title={aria}
        className="flex h-7 max-w-[200px] items-center gap-1 rounded-md pl-2 pr-6 text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent-primary/40"
      >
        <Icon className="size-3 shrink-0 opacity-80" aria-hidden />
        <span className="truncate font-body text-[12px] font-medium text-foreground-primary">
          {role}
        </span>
        <span className="shrink-0 font-caption text-[10px] opacity-70">
          {t(meta.key)}
        </span>
      </button>
      <button
        type="button"
        onClick={() => onDismiss(item.agent.agent_id)}
        aria-label={t("needsYou.dismiss", { role, defaultValue: "收起 {{role}}" })}
        title={t("needsYou.dismiss", { role, defaultValue: "收起 {{role}}" })}
        className="absolute right-0 top-0 flex h-7 w-6 items-center justify-center rounded-r-md text-foreground-tertiary opacity-60 transition-opacity hover:bg-black/5 hover:opacity-100 focus-visible:opacity-100 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent-primary/40"
      >
        <X className="size-3" aria-hidden />
      </button>
    </div>
  );
}
