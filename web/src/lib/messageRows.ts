/**
 * Pure message-row helpers extracted from MessagesPanel — grouping + timestamp
 * formatting + role resolution. Dependency-free and side-effect-free so they're
 * trivially unit-testable (see messageRows.test.ts) and reusable. Keeping the
 * grouping heuristic here as the single source of truth guards against it
 * silently drifting if the panel is ever rewritten.
 */
import type { MessageRecord } from "../api/types";
import { dateLocale } from "./dateLocale";

/** Gap between adjacent messages beyond which a time-divider is inserted and
 *  the sender header is re-shown (same 5-min heuristic as Telegram). */
export const GROUP_GAP_MS = 5 * 60_000;

export interface Row {
  msg: MessageRecord;
  /** Render the avatar + name header row? (false = collapsed into the run). */
  showHeader: boolean;
  showDividerBefore: boolean;
}

/**
 * Resolve a role label for a `from_agent` id.
 *
 * Looks up the lookup map first (populated from /api/agent — covers both active
 * and exited agents). Falls back to a string heuristic so the very first paint,
 * before listAgents() resolves, still shows *something*.
 */
export function resolveRole(fromAgent: string, lookup: Map<string, string>): string {
  const hit = lookup.get(fromAgent);
  if (hit) return hit;
  // agent_ids historically follow either `<cli>-<hash>` or `_<role>_<hash>`.
  // Neither prefix is the role we want, but it's better than the full id.
  const seg = fromAgent.replace(/^_+/, "").split(/[-_]/)[0];
  return seg || "agent";
}

export function formatClock(ms: number, lang?: string): string {
  const locale = dateLocale(lang);
  return new Date(ms).toLocaleTimeString(locale, {
    hour: "2-digit",
    minute: "2-digit",
    hour12: locale?.startsWith("zh") ? false : undefined,
  });
}

export function formatFullStamp(ms: number, lang?: string): string {
  const locale = dateLocale(lang);
  return new Date(ms).toLocaleString(locale, {
    hour12: locale?.startsWith("zh") ? false : undefined,
  });
}

export function formatDivider(ms: number, lang?: string): string {
  const locale = dateLocale(lang);
  const hour12 = locale?.startsWith("zh") ? false : undefined;
  const now = new Date();
  const d = new Date(ms);
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  return sameDay
    ? d.toLocaleTimeString(locale, { hour: "2-digit", minute: "2-digit", hour12 })
    : d.toLocaleString(locale, {
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
        hour12,
      });
}

/** Cron prompts and coordination wakes are operator plumbing, not chat. */
export function isOperatorChatNoise(m: Pick<MessageRecord, "from_agent" | "kind">): boolean {
  return m.from_agent === "cron" || m.kind === "wake";
}

/** Collapse consecutive same-role dispatch cards into one (keeps the latest
 *  worker so a click still opens someone). Stress-test / retry spawns used to
 *  paint three identical 「派给 调研」 chips in a row. */
export function collapseChatMessages(items: MessageRecord[]): MessageRecord[] {
  const out: MessageRecord[] = [];
  for (const m of items) {
    if (isOperatorChatNoise(m)) continue;
    const prev = out[out.length - 1];
    if (
      prev &&
      prev.from_agent === "system" &&
      m.from_agent === "system" &&
      prev.meta?.subtype === "dispatch" &&
      m.meta?.subtype === "dispatch" &&
      (prev.meta.child_role ?? "") === (m.meta.child_role ?? "")
    ) {
      const count = (prev.meta.dispatch_count ?? 1) + 1;
      out[out.length - 1] = {
        ...m,
        meta: { ...m.meta, dispatch_count: count },
      };
      continue;
    }
    out.push(m);
  }
  return out;
}

export function formatElapsed(ms: number): string {
  const sec = Math.max(0, Math.floor(ms / 1000));
  if (sec < 60) return `${sec}s`;
  const min = Math.floor(sec / 60);
  const s = sec % 60;
  if (min < 60) return `${min}m ${String(s).padStart(2, "0")}s`;
  const h = Math.floor(min / 60);
  return `${h}h ${String(min % 60).padStart(2, "0")}m`;
}

/**
 * Build render rows. EVERY message shows its own sender header (avatar + name +
 * h:m:s timestamp) — we deliberately do NOT collapse a run of same-sender
 * messages, because when an agent (e.g. the captain) fires many messages, the
 * collapsed follow-ups lost all attribution: the user couldn't tell who sent
 * them without scrolling up to the run's first message. Per-message headers keep
 * "who + when" visible on every bubble. A time-divider is still inserted when
 * the gap between adjacent messages exceeds {@link GROUP_GAP_MS}.
 */
export function buildRows(items: MessageRecord[]): Row[] {
  const rows: Row[] = [];
  let prev: MessageRecord | null = null;
  for (const msg of collapseChatMessages(items)) {
    const gap = prev ? msg.sent_at - prev.sent_at : Infinity;
    const showDividerBefore = prev !== null && gap > GROUP_GAP_MS;
    rows.push({ msg, showHeader: true, showDividerBefore });
    prev = msg;
  }
  return rows;
}
