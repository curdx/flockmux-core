/**
 * Notification Center — Pencil frame COJDW.
 *
 * Read-only feed assembled from the events swarmx-server already emits
 * via /ws/swarm: messages, blackboard writes, agent state transitions.
 *
 * No new backend endpoint is needed today — when /api/events lands we'll
 * swap the in-memory accumulator for a since-cursor pull, but the surface
 * stays the same.
 *
 * Persistence: only the read-id set lives in localStorage; notification
 * payload itself is ephemeral (lost on refresh; the initial mount pulls a
 * batch of recent messages + blackboard entries to seed the list).
 */

import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { TFunction } from "i18next";
import i18n from "@/i18n";
import { dateLocale } from "@/lib/dateLocale";
import {
  AlertTriangle,
  Bell,
  CheckCircle2,
  CircleAlert,
  Inbox,
  MessageSquare,
  RefreshCw,
  Settings as SettingsIcon,
  X,
} from "lucide-react";
import { api } from "../api/http";
import type {
  AgentInfo,
  BlackboardEntry,
  MessageRecord,
  SwarmEvent,
  Workspace,
} from "../api/types";
import { useSwarmFeed } from "../hooks/useSwarmFeed";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { EmptyState } from "@/components/EmptyState";
import {
  ConfirmActionDialog,
  type ConfirmActionState,
} from "@/components/ConfirmActionDialog";
import { cn } from "@/lib/cn";
import {
  friendlyAgent,
  humanizeBlackboard,
  isHiddenWake,
  isNoisyBlackboard,
  notifBody,
  NOTIF_READ_KEY,
  notifyNotifReadChanged,
} from "@/lib/notif";
import { buildRoleLookup, resolveRole } from "@/lib/agent";

type NotifKind = "message" | "blackboard" | "state" | "error" | "completed";

interface Notif {
  id: string;
  kind: NotifKind;
  agent: string;
  title: string;
  body?: string;
  at: number;
  /** Workspace this notif belongs to (FK into the workspaces table), resolved
   *  when the notif is built so a card click can deep-link `/chat/:slug`,
   *  mirroring the bell popover. Absent when no workspace resolves (e.g. a
   *  system message or an agent that spawned after the last refresh). */
  workspaceId?: string;
}

// Hoisted out of render: these were rebuilt for every <li> on every render.
// Static maps from notif kind → badge classes / lucide icon.
const KIND_BG: Record<NotifKind, string> = {
  message: "bg-state-info/15 text-state-info",
  blackboard: "bg-accent-primary-soft text-accent-primary-deep",
  state: "bg-state-wake/20 text-state-wake",
  error: "bg-status-danger-soft text-status-danger",
  completed: "bg-status-success-soft text-status-success",
};
const KIND_ICON: Record<NotifKind, typeof Bell> = {
  message: MessageSquare,
  blackboard: Inbox,
  state: SettingsIcon,
  error: CircleAlert,
  completed: CheckCircle2,
};


function loadRead(): Set<string> {
  try {
    const raw = window.localStorage.getItem(NOTIF_READ_KEY);
    if (!raw) return new Set();
    return new Set(JSON.parse(raw));
  } catch {
    return new Set();
  }
}

function saveRead(s: Set<string>) {
  try {
    const arr = Array.from(s).slice(-500);
    window.localStorage.setItem(NOTIF_READ_KEY, JSON.stringify(arr));
    notifyNotifReadChanged();
  } catch {
    /* ignore */
  }
}

function fmtTime(ms: number): string {
  const delta = Date.now() - ms;
  if (delta < 60_000) return "_now_";
  if (delta < 3_600_000) return `_min_${Math.floor(delta / 60_000)}`;
  if (delta < 86_400_000) return `_hour_${Math.floor(delta / 3_600_000)}`;
  const locale = dateLocale(i18n.language);
  return new Date(ms).toLocaleString(locale, {
    hour12: locale?.startsWith("zh") ? false : undefined,
  });
}

// Resolve the sentinel strings returned by fmtTime against the active locale.
function resolveTime(s: string, t: TFunction): string {
  if (s === "_now_") return t("notifications.time.now");
  if (s.startsWith("_min_")) return t("notifications.time.minAgo", { n: s.slice(5) });
  if (s.startsWith("_hour_")) return t("notifications.time.hourAgo", { n: s.slice(6) });
  return s;
}

const TABS = [
  { id: "all", labelKey: "notifications.tabs.all", icon: Bell },
  { id: "message", labelKey: "notifications.tabs.message", icon: MessageSquare },
  { id: "blackboard", labelKey: "notifications.tabs.blackboard", icon: Inbox },
  { id: "state", labelKey: "notifications.tabs.state", icon: SettingsIcon },
  { id: "error", labelKey: "notifications.tabs.error", icon: AlertTriangle },
  { id: "completed", labelKey: "notifications.tabs.completed", icon: CheckCircle2 },
] as const;

type TabId = (typeof TABS)[number]["id"];

// Bucket a message into error / completed / state / message.
//
// The old version did a bare `body.includes("error" | "failed")`, so any
// success summary that merely MENTIONED the word — "0 errors", "error
// handling done", "build passed, no errors" — got filed under 异常/Errors
// with a red alarm icon. That false positive was the worst kind: it makes a
// finished task look broken. We now grade the signal:
//   1. a hard failure glyph (❌ ✗ panic traceback) always wins;
//   2. otherwise an explicit success marker (✅ passed 完成 通过 …) means
//      completed — even if the body also says "error" somewhere;
//   3. only a *soft* failure word with NO success marker counts as an error.
function classifyMessage(
  m: MessageRecord,
  t: TFunction,
  roleLookup: Map<string, string>,
): { kind: NotifKind; title: string } {
  // Structured first: server-stamped `meta.subtype` is GROUND TRUTH. Any
  // message the server itself generated carries a subtype, and we classify
  // those purely from it — the brittle prose-keyword heuristic further down is
  // a *fallback for agent free-text only* (meta absent). This stops, e.g., a
  // `cron`/`dispatch` system message whose body happens to contain "失败"/
  // "完成" from being mis-bucketed as 异常/完成 by the regex.
  const subtype = m.meta?.subtype;
  if (subtype === "completion") {
    return {
      kind: "completed",
      title: t("notifications.kinds.completedTitle"),
    };
  }
  // `wake` is the one subtype that historically keyed off `m.kind === "wake"`;
  // accept either signal (meta.subtype or the legacy kind) so older rows still
  // classify correctly.
  if (subtype === "wake" || m.kind === "wake") {
    return {
      kind: "state",
      title: t("notifications.kinds.wakeTitle", {
        from: friendlyAgent(m.from_agent, roleLookup, t),
        to: friendlyAgent(m.to_agent, roleLookup, t),
      }),
    };
  }
  // Any OTHER server subtype (cron / dispatch / model_changed / …) is a
  // routine system notice, not an agent work-report — file it as a plain
  // message and skip the error/completed prose heuristic entirely. New server
  // subtypes land here safely instead of getting regex-graded.
  if (subtype) {
    return {
      kind: "message",
      title: `${friendlyAgent(m.from_agent, roleLookup, t)} → ${friendlyAgent(m.to_agent, roleLookup, t)}`,
    };
  }
  // Free-text the USER wrote is a request, not an agent work-report — never
  // grade it as 异常/完成 by prose keywords. A user message like "这个测试没
  // 通过，帮我修" or "看下这张报错截图" describes a problem; filing it under
  // Errors with a red alarm is noise (the user authored it). Only agent
  // summaries get the error/completed heuristic below.
  if (resolveRole(m.from_agent, roleLookup) === "user") {
    return {
      kind: "message",
      title: `${friendlyAgent(m.from_agent, roleLookup, t)} → ${friendlyAgent(m.to_agent, roleLookup, t)}`,
    };
  }
  const body = m.body.toLowerCase();
  const error = () => ({
    kind: "error" as NotifKind,
    title: t("notifications.kinds.errorTitle"),
  });
  const completed = () => ({
    kind: "completed" as NotifKind,
    title: t("notifications.kinds.completedTitle"),
  });

  // Classifying free-form agent summaries by keyword is a tar pit — every
  // naive substring trips on its own negation/noun form. So we grade in
  // precedence order, each rule written to dodge the trap that bit the last:
  //
  // 1. UNAMBIGUOUS failure glyphs — SYMBOLS a success/neutral message never
  //    contains (nobody types "no ❌"). The WORDS panic / traceback / 崩溃 /
  //    报错 were demoted OUT of this top tier: they routinely appear NEGATED or
  //    in meta-discussion ("没有红色 traceback", "没有报错", "全是好消息"), and
  //    as a hard, completion-beating signal they flagged green reports as 异常
  //    (the worst false positive — a finished task looks broken). They now live
  //    in rule 4, BELOW completion, so a report that says "构建通过 … 没有
  //    traceback" lands in 完成. A real crash carries no completion word, so it
  //    still falls through to rule 4 and registers as an error.
  if (/❌|✗/.test(body)) return error();

  // 2. NEGATED completion = a real failure verdict (未通过 / 没做好 / not
  //    passed). The negative-lookahead excludes the NOUN form 未完成数 /
  //    未完成项 — "remaining incomplete count" is a feature label, not a
  //    failed task (this is what mis-flagged a working todo-app summary).
  if (
    /(?:未|不|没|未能|没能)\s*(?:通过|完成|做好|做完|跑通|搞定|交付)(?!\s*(?:数|项|度|数量|个|列表|待办))/.test(
      body,
    ) ||
    /not\s+passed|did\s*n'?t\s+pass/.test(body)
  ) {
    return error();
  }

  // 3. Clear completion. Success WINS over a soft failure mention here, so a
  //    summary that merely says "0 errors" or CI config "失败时 upload report"
  //    (on-failure, not an actual failure) stays green instead of alarming.
  if (
    /✅|✔|全绿|搞定|完成|通过|做好|做完|全齐|跑通|已交付|交付完|passed|ready/.test(
      body,
    )
  ) {
    return completed();
  }

  // 4. Soft failure words that survived completion = an actual failure —
  //    including the crash words demoted from rule 1. Excludes the conditional
  //    "失败时/失败后…" (describing failure handling) and bare "error" (0 errors
  //    / error handling), which are not failures themselves. A negated mention
  //    with NO completion word (e.g. a bare "没有 traceback") can still slip
  //    through as a false error here, but that's far rarer than the green
  //    reports rule 1 used to alarm on, and proximity-negation in free prose is
  //    its own tar pit ("没有红色 traceback" puts a word between the two).
  if (
    /失败(?!\s*(?:时|后|则|会|就|重试|的话|时候))|failed|failure|exception|panic|traceback|stack\s*trace|崩溃/.test(
      body,
    )
  ) {
    return error();
  }

  return {
    kind: "message",
    title: `${friendlyAgent(m.from_agent, roleLookup, t)} → ${friendlyAgent(m.to_agent, roleLookup, t)}`,
  };
}

/** Extract the numeric message id from a `msg-<N>` notification id, or null
 *  for non-message notifs (blackboard / state). Used to sync notification
 *  read-state down to the server's message.read_at so the chat unread badge
 *  clears too — notification "read" used to be localStorage-only, leaving
 *  /chat showing the same messages as unread after a reload. */
function msgIdOf(notifId: string): number | null {
  if (!notifId.startsWith("msg-")) return null;
  const n = Number(notifId.slice(4));
  return Number.isFinite(n) ? n : null;
}

// notif kind → localized type word, for the card's screen-reader label.
const KIND_ARIA_KEY: Record<NotifKind, string> = {
  message: "notifications.typeMessage",
  blackboard: "notifications.typeBlackboard",
  state: "notifications.typeState",
  error: "notifications.typeError",
  completed: "notifications.typeCompleted",
};

/** One notification card. Memoized (the list re-renders on every live event /
 *  read-state change; a stable card row skips re-rendering when its own props
 *  didn't change). The whole card is a focusable, Enter/Space-activatable
 *  button-role element that jumps to the originating workspace — previously
 *  only the X button was reachable by keyboard. The X stays a nested control
 *  (stops propagation so dismissing doesn't also navigate). */
const NotifCard = memo(function NotifCard({
  n,
  isRead,
  t,
  onActivate,
  onMarkRead,
}: {
  n: Notif;
  isRead: boolean;
  t: TFunction;
  onActivate: (n: Notif) => void;
  onMarkRead: (id: string) => void;
}) {
  const Icon = KIND_ICON[n.kind];
  const timeLabel = resolveTime(fmtTime(n.at), t);
  const ariaLabel = t("notifications.cardAria", {
    type: t(KIND_ARIA_KEY[n.kind]),
    time: timeLabel,
    unread: isRead ? "" : t("notifications.cardAriaUnread"),
    defaultValue: "{{type}}通知 · {{time}}{{unread}}",
  });
  return (
    <li
      className={cn(
        "flex items-start rounded-lg border border-border-subtle bg-surface-elevated transition-colors",
        // unread 之前用整张 bg-surface-accent-tint — light 下还
        // 行，dark 下解析成 blue-900 整片蓝海，视觉太重。改成
        // Slack / Mail / Notion 同款 "左侧 accent bar" 风：卡片
        // 主背景保持 surface-elevated（跟 read 一致），unread 只
        // 用左边 2px 蓝条标识 + 右上 dot，subtle 但能扫到。
        !isRead && "border-l-2 border-l-accent-primary",
      )}
    >
      {/* The card body itself is the focusable / Enter|Space-activatable
          control that jumps to the workspace; the X dismiss is a SIBLING (not a
          descendant) so we don't nest a button inside a button. */}
      <div
        role="button"
        tabIndex={0}
        aria-label={ariaLabel}
        onClick={() => onActivate(n)}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onActivate(n);
          }
        }}
        className="flex min-w-0 flex-1 cursor-pointer items-start gap-3 rounded-lg px-3 py-2.5 text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent-primary hover:bg-surface-tertiary"
      >
        <span
          className={cn(
            "flex size-7 shrink-0 items-center justify-center rounded-md",
            KIND_BG[n.kind],
          )}
        >
          <Icon className="size-3.5" />
        </span>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate font-heading text-sm font-semibold text-foreground-primary">
              {n.title}
            </span>
            <span className="ml-auto font-caption text-[10px] text-foreground-tertiary">
              {timeLabel}
            </span>
          </div>
          {n.body && (
            <p className="mt-1 line-clamp-2 font-caption text-[11px] text-foreground-secondary">
              {n.body}
            </p>
          )}
          <div className="mt-1 flex items-center gap-2 font-caption text-[10px] text-foreground-tertiary">
            <span className="font-mono">{n.agent}</span>
            {!isRead && (
              <span className="size-1.5 rounded-full bg-accent-primary" />
            )}
          </div>
        </div>
      </div>
      {!isRead && (
        <Button
          variant="ghost"
          size="icon"
          onClick={() => onMarkRead(n.id)}
          aria-label={t("notifications.markRead")}
          title={t("notifications.markRead")}
          className="mt-2 mr-1 size-8 shrink-0 text-foreground-tertiary hover:text-foreground-primary"
        >
          <X className="size-3" />
        </Button>
      )}
    </li>
  );
});

export default function NotificationsRoute() {
  const { t } = useTranslation();
  const [items, setItems] = useState<Notif[]>([]);
  const [read, setRead] = useState<Set<string>>(loadRead);
  const [tab, setTab] = useState<TabId>("all");
  // 后端断连 / 拉取 500 与「真没通知」不能渲染成同一个空态——否则界面会
  // 在加载失败时假装「该分类暂无通知」。seed() 失败时存住错误,渲染时
  // error 优先显示「加载失败 + 重试」。
  const [error, setError] = useState<Error | null>(null);
  // 手动刷新按钮的 loading/防重:mutation 期间按钮 disabled + 图标 spin,
  // 同时一个 in-flight ref 兜住「state 还没刷新就被连点」的竞态(setState
  // 异步,光看 refreshing 挡不住同一 tick 的二次点击)。
  const [refreshing, setRefreshing] = useState(false);
  const refreshingRef = useRef(false);
  const [confirm, setConfirm] = useState<ConfirmActionState | null>(null);
  // Workspaces + their directions resolve a blackboard key's `{ws-id}/{slug}`
  // prefix into "{workspace} · {direction}" instead of a raw 32-hex UUID. Held
  // in a ref (not state) so the live SwarmFeed callback — a stable closure —
  // sees the latest list without re-subscribing; nothing renders off it
  // directly (titles are baked into each Notif when it's built).
  const wsRef = useRef<Workspace[]>([]);
  // agent_id → role label, for friendly "{role} → {role}" titles + source
  // chips (covers exited agents too — /api/agent returns them). Same ref
  // pattern as wsRef so the live SwarmFeed closure sees the latest map.
  const roleRef = useRef<Map<string, string>>(new Map());
  // agent_id → workspace_id (FK), so a card click can deep-link `/chat/:slug`
  // for the originating agent — same resolution the bell popover does. Held in
  // a ref so the live SwarmFeed closure sees the latest map without resubscribe.
  const agentWsRef = useRef<Map<string, string>>(new Map());
  const navigate = useNavigate();

  const seed = useCallback(async () => {
    // In-flight guard: drop overlapping seeds (rapid clicks, or a reconnect
    // racing the manual button) so two pulls can't clobber each other.
    if (refreshingRef.current) return;
    refreshingRef.current = true;
    setRefreshing(true);
    try {
      const [msgs, bb, wss, agents] = await Promise.all([
        api.listMessages({ limit: 50 }),
        api.listBlackboard(),
        api.listWorkspaces().catch(() => [] as Workspace[]),
        api.listAgents().catch(() => [] as AgentInfo[]),
      ]);
      wsRef.current = wss;
      roleRef.current = buildRoleLookup(agents);
      const agentWs = new Map<string, string>();
      for (const a of agents as AgentInfo[]) {
        if (a.workspace_id) agentWs.set(a.agent_id, a.workspace_id);
      }
      agentWsRef.current = agentWs;
      const fromMsg: Notif[] = (msgs as MessageRecord[])
        .filter((m) => !isHiddenWake(m) && m.from_agent !== "cron")
        .map((m) => {
          const c = classifyMessage(m, t, roleRef.current);
          return {
            id: `msg-${m.id}`,
            kind: c.kind,
            agent: friendlyAgent(m.from_agent, roleRef.current, t),
            title: c.title,
            body: notifBody(m.kind, m.body, t),
            at: m.sent_at,
            workspaceId: agentWs.get(m.from_agent),
          };
        });
      const fromBb: Notif[] = (bb as BlackboardEntry[])
        .filter((e) => !isNoisyBlackboard(e.path))
        .map((e) => {
          const h = humanizeBlackboard(e.path, wss, t);
          return {
            // Stable id = the blackboard PATH alone. The `at` timestamp used to
            // be baked into the id, so every rewrite of the same key minted a
            // fresh id and the read-state (keyed by id in localStorage) was lost
            // on refresh — a notif you'd dismissed came back unread. The path is
            // the durable identity of a blackboard entry; the latest `at`/`body`
            // ride on top of it (see the live handler, which updates in place).
            id: `bb-${e.path}`,
            kind: "blackboard" as const,
            agent: t("notifications.tabs.blackboard"),
            title: h.title,
            body: h.context ?? `sha256 ${e.sha256.slice(0, 8)}`,
            at: e.at,
            // The blackboard key is `{ws-id}/{slug}/{file}` — the leading
            // segment IS the workspace id, so a card click can jump to it.
            workspaceId: e.path.split("/")[0] || undefined,
          };
        });
      const all = [...fromMsg, ...fromBb].sort((a, b) => b.at - a.at);
      setItems(all);
      setError(null);
    } catch (e) {
      // 别吞错:后端 500 / 断网时记录真实错误,渲染层据此显示「加载失败 +
      // 重试」而不是谎报空态。
      setError(e as Error);
    } finally {
      refreshingRef.current = false;
      setRefreshing(false);
    }
  }, []);

  useEffect(() => {
    let alive = true;
    seed().finally(() => {
      if (alive) setRead(loadRead());
    });
    return () => {
      alive = false;
    };
  }, [seed]);

  useSwarmFeed({
    onEvent: (ev: SwarmEvent) => {
      setItems((prev) => {
        let next: Notif | null = null;
        if (ev.type === "message") {
          if (isHiddenWake(ev) || ev.from_agent === "cron") return prev;
          const rec: MessageRecord = {
            id: ev.id,
            from_agent: ev.from_agent,
            to_agent: ev.to_agent,
            kind: ev.kind,
            body: ev.body,
            sent_at: ev.sent_at,
            delivered_at: null,
            read_at: null,
            in_reply_to: ev.in_reply_to ?? null,
            thread_id: ev.thread_id ?? null,
            meta: ev.meta,
            thought_trace: ev.thought_trace ?? null,
          };
          const c = classifyMessage(rec, t, roleRef.current);
          next = {
            id: `msg-${ev.id}`,
            kind: c.kind,
            agent: friendlyAgent(ev.from_agent, roleRef.current, t),
            title: c.title,
            body: notifBody(ev.kind, ev.body, t),
            at: ev.sent_at,
            workspaceId: agentWsRef.current.get(ev.from_agent),
          };
        } else if (ev.type === "blackboard_changed") {
          if (isNoisyBlackboard(ev.path)) return prev;
          const h = humanizeBlackboard(ev.path, wsRef.current, t);
          // Stable id = path (matches seed). A rewrite of the same key is the
          // SAME notification with a fresher timestamp/body, not a new one — so
          // we update it in place (refreshing `at`/`body` and re-sorting to the
          // top) instead of either dropping the update (stale) or minting a new
          // id (would resurrect a dismissed notif as unread).
          const updated: Notif = {
            id: `bb-${ev.path}`,
            kind: "blackboard",
            agent: ev.agent_id ?? t("notifications.tabs.blackboard"),
            title: h.title,
            body: h.context ?? `sha256 ${ev.sha256.slice(0, 8)}`,
            at: ev.at,
            workspaceId: ev.path.split("/")[0] || undefined,
          };
          const without = prev.filter((p) => p.id !== updated.id);
          return [updated, ...without].slice(0, 200);
        } else if (ev.type === "agent_state") {
          next = {
            id: `state-${ev.agent_id}-${ev.state}-${Date.now()}`,
            kind: ev.state === "exited" ? "completed" : "state",
            agent: friendlyAgent(ev.agent_id, roleRef.current, t),
            title: `${friendlyAgent(ev.agent_id, roleRef.current, t)} → ${t(
              "notifications.state." + ev.state,
              { defaultValue: ev.state },
            )}`,
            at: Date.now(),
            workspaceId: agentWsRef.current.get(ev.agent_id),
          };
        }
        if (!next) return prev;
        // Avoid duplicates by id (initial seed could race with live).
        if (prev.some((p) => p.id === next!.id)) return prev;
        return [next, ...prev].slice(0, 200);
      });
    },
    onReconnect: () => seed(),
  });

  const markRead = useCallback((id: string) => {
    setRead((prev) => {
      const next = new Set(prev);
      next.add(id);
      saveRead(next);
      return next;
    });
    // Also clear the server-side message read state for message notifs so the
    // chat unread badge stays in sync (to="user" only marks agent→user rows —
    // the ones chat counts; other ids no-op server-side). Best-effort.
    const mid = msgIdOf(id);
    if (mid != null) api.markMessagesRead("user", [mid]).catch(() => {});
  }, []);

  // Clicking a card jumps to the originating workspace's chat (mirrors the bell
  // popover) and marks the notif read in the same gesture. Falls back to a
  // no-op jump when no workspace resolves — the card is still keyboard-read via
  // the X button, so a missing target degrades to "just mark read".
  const handleCardClick = useCallback(
    (n: Notif) => {
      markRead(n.id);
      const ws = n.workspaceId
        ? wsRef.current.find((w) => w.id === n.workspaceId)
        : undefined;
      if (ws) {
        if (n.kind === "blackboard") {
          navigate(`/chat/${ws.slug}/ledger`);
        } else {
          navigate(`/chat/${ws.slug}`);
        }
      }
    },
    [markRead, navigate],
  );
  const doMarkAllRead = () => {
    setRead((prev) => {
      const next = new Set(prev);
      for (const n of items) next.add(n.id);
      saveRead(next);
      return next;
    });
    // Sync server message read state in one batch (see markRead) so reloading
    // /chat no longer re-surfaces these as unread.
    const mids = items
      .map((n) => msgIdOf(n.id))
      .filter((x): x is number => x != null);
    if (mids.length > 0) api.markMessagesRead("user", mids).catch(() => {});
  };
  const markAllRead = () => {
    // 防误触：100 条 unread 一下全 mark read 是不可逆操作（read 状态
    // 只存 localStorage，没法 undo）。当 unread 数 > 5 时弹一个确认框；
    // 少量 (≤5) 直接执行省点击。
    const unreadCount = items.filter((n) => !read.has(n.id)).length;
    if (unreadCount > 5) {
      setConfirm({
        title: t("notifications.markAllRead"),
        description: t("notifications.markAllReadConfirm", { count: unreadCount }),
        confirmLabel: t("notifications.markAllRead"),
        onConfirm: doMarkAllRead,
      });
      return;
    }
    doMarkAllRead();
  };

  const filtered = useMemo(
    () => (tab === "all" ? items : items.filter((n) => n.kind === tab)),
    [items, tab],
  );

  const totalUnread = items.filter((n) => !read.has(n.id)).length;
  const countBy = (k: TabId) =>
    items.filter((n) => k === "all" || n.kind === k).filter((n) => !read.has(n.id)).length;

  return (
    <div className="flex h-full flex-col bg-surface-primary">
      {/* Head */}
      <header className="flex shrink-0 flex-wrap items-center gap-3 border-b border-border-subtle bg-surface-elevated px-4 py-3 sm:h-14 sm:flex-nowrap sm:px-5 sm:py-0">
        <span className="flex size-8 items-center justify-center rounded-md bg-accent-primary-soft">
          <Bell className="size-4 text-accent-primary-deep" />
        </span>
        <div className="flex flex-col">
          <h1 className="font-heading text-sm font-semibold text-foreground-primary">
            {t("notifications.title")}
          </h1>
          <span className="font-caption text-[10px] text-foreground-tertiary">
            {t("notifications.subtitleUnread", {
              count: totalUnread,
              defaultValue: "{{count}} 条未读",
            })}
          </span>
        </div>
        <span className="hidden flex-1 sm:block" />
        <Button
          variant="outline"
          size="sm"
          onClick={markAllRead}
          className="ml-auto h-8 sm:ml-0"
        >
          {t("notifications.markAllRead")}
        </Button>
        <Button
          variant="ghost"
          size="icon"
          onClick={seed}
          disabled={refreshing}
          title={t("common.refresh")}
          className="size-8"
        >
          <RefreshCw className={cn("size-4", refreshing && "animate-spin")} />
        </Button>
      </header>

      {/* Tab bar */}
      <div className="flex shrink-0 flex-wrap items-center gap-1.5 border-b border-border-subtle bg-surface-secondary px-4 py-2 sm:min-h-11 sm:px-5">
        {TABS.map((tabItem) => {
          const Icon = tabItem.icon;
          const active = tabItem.id === tab;
          const c = countBy(tabItem.id);
          return (
            <Button
              key={tabItem.id}
              variant={active ? "default" : "outline"}
              size="sm"
              onClick={() => setTab(tabItem.id)}
              className="h-8 rounded-full"
            >
              <Icon className="size-3" />
              {t(tabItem.labelKey)}
              {c > 0 && (
                <Badge
                  variant={active ? "secondary" : "outline"}
                  className={cn(
                    "rounded-full px-1.5 py-0.5 text-[9px]",
                    active
                      ? "bg-foreground-on-accent text-accent-primary"
                      : "bg-surface-tertiary text-foreground-tertiary",
                  )}
                >
                  {c}
                </Badge>
              )}
            </Button>
          );
        })}
      </div>

      {/* List */}
      <div className="min-h-0 flex-1 overflow-y-auto px-3 py-3">
        {error && filtered.length === 0 ? (
          // 加载失败优先于空态:连不上后端 / 拉取 500 时显示「加载失败 +
          // 重试」,绝不渲染成「该分类暂无通知」假装一切正常。只有在没有任何
          // 可展示项时才接管——避免盖掉已经到达的实时通知。
          <div className="flex h-full flex-col items-center justify-center gap-3 px-6 text-center text-foreground-tertiary">
            <AlertTriangle className="size-10 text-status-danger opacity-60" />
            <p className="font-caption text-sm text-foreground-secondary">
              {t("notifications.loadError", { defaultValue: "加载通知失败" })}
            </p>
            {error.message && (
              <p className="max-w-md break-words font-caption text-[11px] text-foreground-tertiary">
                {error.message}
              </p>
            )}
            <Button
              variant="outline"
              size="sm"
              onClick={seed}
              disabled={refreshing}
              className="h-8"
            >
              <RefreshCw
                className={cn("size-3.5", refreshing && "animate-spin")}
              />
              {t("common.retry", { defaultValue: "重试" })}
            </Button>
          </div>
        ) : filtered.length === 0 ? (
          <EmptyState
            icon={<Bell className="size-8" />}
            title={t("notifications.empty")}
            hint={t("notifications.emptyHint")}
          />
        ) : (
          <ul className="flex flex-col gap-1.5">
            {filtered.map((n) => (
              <NotifCard
                key={n.id}
                n={n}
                isRead={read.has(n.id)}
                t={t}
                onActivate={handleCardClick}
                onMarkRead={markRead}
              />
            ))}
          </ul>
        )}
      </div>
      <ConfirmActionDialog
        action={confirm}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
      />
    </div>
  );
}
