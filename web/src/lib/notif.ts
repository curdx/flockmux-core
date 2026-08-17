/**
 * Shared notification rendering helpers.
 *
 * `humanizeBlackboard` lives here (not in a component) because BOTH the bell
 * popover and the full /notifications page render blackboard events — and they
 * MUST agree. They drifted once: the popover humanized the key while the page
 * printed the raw `write {32-hex-uuid}/{thread_slug}/task.ledger.md` storage
 * key, which is meaningless to a user. One source of truth prevents that.
 */

import type { MessageMeta, Workspace } from "@/api/types";
import { resolveRole } from "@/lib/agent";

/** Shared with the bell badge so popover / center / red-dot never drift. */
export const NOTIF_READ_KEY = "swarmx:notif:read:v1";
export const NOTIF_READ_EVENT = "swarmx:notif-read";

type Tr = (k: string, opts?: Record<string, unknown>) => string;

export function isNoisyBlackboard(path: string): boolean {
  return path.endsWith(".progress.md");
}

export function blackboardWorkspaceId(path: string): string | undefined {
  const segs = path.split("/").filter(Boolean);
  return segs.length >= 3 ? segs[0] : undefined;
}

export function notifyNotifReadChanged(): void {
  try {
    window.dispatchEvent(new Event(NOTIF_READ_EVENT));
  } catch {
    /* ignore */
  }
}

/** Human label for an agent id in a notification: "你"/"系统" for the
 *  user/system pseudo-agents, else the role ("orchestrator", "Backend
 *  Engineer", …) resolved from /api/agent — never the raw `codex-6fc9b645`
 *  short id, which is noise to a user. Shared by BOTH the bell popover and the
 *  full /notifications page so the two surfaces render a sender identically
 *  (they drifted: the popover showed "orchestrator 6fc9b645" via AgentChip
 *  while the page showed a clean "orchestrator"). roleLookup covers exited
 *  agents too (listAgents returns them). */
export function friendlyAgent(
  id: string,
  roleLookup: Map<string, string>,
  t: Tr,
): string {
  const r = resolveRole(id, roleLookup);
  if (r === "user") return t("notifications.fromUser");
  if (r === "system") return t("notifications.fromSystem");
  return r;
}

/** Auto blackboard wakes (and legacy untyped wakes, meta absent) are internal
 *  agent-coordination plumbing — redundant with the BlackboardChanged event the
 *  feed already shows. Both the bell popover and the full /notifications page
 *  hide them; only operator-initiated manual wakes (meta.reason === "manual")
 *  stay, since they record a real intervention. Shared so the two surfaces
 *  filter identically. */
export function isHiddenWake(m: {
  kind: string;
  meta?: MessageMeta | null;
}): boolean {
  return m.kind === "wake" && m.meta?.reason !== "manual";
}

/** Display body for a message-derived notification. Two cleanups, shared so the
 *  bell popover and the full /notifications page preview identically:
 *   - WAKE notifications carry the raw injection prompt ("操作员唤醒——请先查邮
 *     箱里的新消息…") as their body — an instruction TO the agent, noise FOR the
 *     user. The title already says what happened, so drop the body entirely.
 *   - Otherwise collapse fenced code blocks to a short 「[…]」 placeholder so a
 *     preview isn't a wall of raw ```svg```/```mermaid``` source (a chat that
 *     renders to an image/diagram dumps its source verbatim into the feed). */
export function notifBody(
  kind: string,
  body: string,
  t: Tr,
): string | undefined {
  if (kind === "wake") return undefined;
  const collapsed = collapseCodeFences(body, t);
  return collapsed || undefined;
}

/** Replace ```lang …``` fenced blocks with a one-token placeholder; inline
 *  `code` (single backtick) and prose are left untouched. */
function collapseCodeFences(s: string, t: Tr): string {
  return s
    .replace(/```([\w+#-]*)[^\n]*\n[\s\S]*?```/g, (_m, lang: string) =>
      lang
        ? t("notifications.codeBlockLang", { lang })
        : t("notifications.codeBlock"),
    )
    .replace(/[ \t]+\n/g, "\n")
    .trim();
}

/** A blackboard key is `{workspace_id}/{thread_slug}/{file}`. Render it as
 *  human text — a friendly ledger label + the workspace/direction names —
 *  instead of the raw 32-char UUID + slug + content hash the user can't read. */
export function humanizeBlackboard(
  path: string,
  workspaces: Workspace[],
  t: Tr,
): { title: string; context?: string } {
  const segs = path.split("/").filter(Boolean);
  if (segs.length < 3) {
    return { title: segs[segs.length - 1] ?? path };
  }
  const [wsid, slug] = segs;
  const file = segs.slice(2).join("/");
  const leaf = segs[segs.length - 1] ?? "";
  const title =
    file === "task.ledger.md"
      ? t("notifications.bb.taskLedger")
      : file === "progress.ledger.md"
        ? t("notifications.bb.progressLedger")
        : leaf.endsWith(".error") || leaf === "error"
          ? t("notifications.bb.failed")
          : t("notifications.bb.update", { name: leaf });
  // Prefer an exact workspace-id match; fall back to locating the direction by
  // its (workspace-unique) slug so this still resolves if the id scheme drifts.
  const ws =
    workspaces.find((w) => w.id === wsid) ??
    workspaces.find((w) => (w.threads ?? []).some((th) => th.slug === slug));
  const thread = (ws?.threads ?? []).find((th) => th.slug === slug);
  const dirName = thread?.name?.trim()
    ? thread.name.trim()
    : slug === "main"
      ? t("notifications.bb.mainDir")
      : thread
        ? slug
        : undefined;
  const context = [ws?.name, dirName].filter(Boolean).join(" · ");
  return { title, context: context || undefined };
}
