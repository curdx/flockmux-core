import type { MessageMeta } from "../api/types";

/** Canonical predicate — the SINGLE SOURCE OF TRUTH for "does this message bump
 *  the USER's unread badge?". A message counts only if it's a real agent→user
 *  reply, NOT:
 *    - coordination noise (`kind === "wake"`),
 *    - cron prompts (`from === "cron"`),
 *    - a system event card (`from === "system"`),
 *    - a worker's delivery card (`meta.subtype === "completion"`, which renders
 *      as a status card via SystemCard, not an unread "message").
 *
 *  Both unread surfaces import THIS function:
 *    - the top-bar tally + per-sender badge (useWorkspaceShellData), and
 *    - the in-list "N 条新消息" divider / count (MessagesPanel.firstUnreadId).
 *  They used to keep separate hand-copied predicates that drifted: the divider
 *  forgot the `completion` exclusion, so an UNREAD completion card made the
 *  badge say "0 unread" while the divider said "1" — the honesty red line. One
 *  function, imported by both, makes that drift impossible.
 *
 *  NOTE: this is the message-CLASSIFICATION half only. Callers still apply the
 *  context checks they own — `to_agent === "user"` and `read_at === null` — at
 *  the call site (the tally tracks counted ids, the divider walks scoped rows). */
export function countsAsUserUnread(
  fromAgent: string,
  kind: string,
  meta: MessageMeta | null | undefined,
): boolean {
  return (
    fromAgent !== "system" &&
    fromAgent !== "cron" &&
    kind !== "wake" &&
    meta?.subtype !== "completion"
  );
}
