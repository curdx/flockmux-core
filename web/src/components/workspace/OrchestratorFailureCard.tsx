/**
 * OrchestratorFailureCard — the honest replacement for "暂无消息" when the
 * workspace's orchestrator is alive but can't actually work.
 *
 * Phase-1 honesty fix (see .ux-review/final-redesign.md §4). The backend now
 * flips an agent to `AgentState::Error` + a system `AgentActivity(phase=error)`
 * when its CLI prints "Not logged in" (HealthScanner) or it produces no sign of
 * life within 90s of becoming ready (first-response watchdog), and persists the
 * reason to `AgentInfo.last_error`. This card consumes that — in the MAIN chat
 * view, not just the ≥1536px member rail — so the failure is impossible to
 * miss and immediately actionable, instead of a fake green dot sitting forever
 * over an empty room.
 */
import { useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Check,
  Copy,
  LogIn,
  RotateCw,
  SquareTerminal,
  TriangleAlert,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/cn";

export interface OrchestratorFailureCardProps {
  /** Human-facing failure reason (e.g. "Claude Code 未登录，请在终端运行 …"). */
  reason: string;
  /** Coarse class (auth | rate_limit | watchdog | fatal) steering the copy
   *  + which remedy the card leads with. */
  kind?: string | null;
  /** Login command for the failed CLI (from `/api/plugins`), shown as a
   *  copyable line for auth failures. */
  loginCommand?: string | null;
  /** Open the orchestrator's live terminal — where the user runs `/login` or
   *  reads the raw CLI error. */
  onOpenTerminal: () => void;
  /** Re-spawn the orchestrator once the user has fixed the cause. */
  onRetry: () => void;
  retrying?: boolean;
}

export function OrchestratorFailureCard({
  reason,
  kind,
  loginCommand,
  onOpenTerminal,
  onRetry,
  retrying,
}: OrchestratorFailureCardProps) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  // Treat anything that smells like a login problem as auth, even if the
  // backend didn't tag `kind` (e.g. an older row, or the watchdog firing on a
  // never-logged-in CLI) — the login terminal is the right fix either way.
  const isAuth =
    kind === "auth" || /未登录|not logged in|\/login/i.test(reason);
  // Kind-specific honest title (P0-7). Order: escalated (retry exhausted) >
  // auth (login is the fix even when the watchdog surfaced it) > rate-limit >
  // crash > timeout > generic. The reason text below is shown in full,
  // never truncated — this card is the single source of truth for the failure.
  const isRate =
    kind === "rate_limit" || /限流|rate.?limit|429|quota|too many requests/i.test(reason);
  const isCrash =
    kind === "crash" ||
    kind === "fatal" ||
    /退出码|exit code|crash|panic|abort|segfault/i.test(reason);
  const isTimeout =
    kind === "watchdog" ||
    kind === "timeout" ||
    /看门狗|watchdog|timed? ?out|无响应|no response/i.test(reason);
  const title =
    kind === "escalated"
      ? t("chat.orchestratorFailure.titleEscalated", "多次重试仍未成功，需要你处理")
      : isAuth
        ? t("chat.orchestratorFailure.titleAuth", "规划还没法开始")
        : isRate
          ? t("chat.orchestratorFailure.titleRate", "规划暂时被限流")
          : isCrash
            ? t("chat.orchestratorFailure.titleCrash", "规划意外退出")
            : isTimeout
              ? t("chat.orchestratorFailure.titleTimeout", "规划启动后没有响应")
              : t("chat.orchestratorFailure.title", "规划还不能开始工作");

  const copy = async () => {
    if (!loginCommand) return;
    try {
      await navigator.clipboard.writeText(loginCommand);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard blocked (insecure context) — the command is still visible */
    }
  };

  return (
    <div className="mx-auto mt-10 flex w-full max-w-[460px] flex-col gap-3 rounded-2xl border border-status-danger/30 bg-status-danger/5 px-5 py-4">
      <div className="flex items-start gap-2.5">
        <TriangleAlert className="mt-0.5 size-4 shrink-0 text-status-danger" />
        <div className="flex flex-col gap-0.5">
          <p className="font-heading text-sm font-semibold text-foreground">
            {title}
          </p>
          <p className="font-caption text-xs leading-5 text-foreground-secondary">
            {reason}
          </p>
        </div>
      </div>

      {isAuth && loginCommand ? (
        <div className="flex items-center gap-2 rounded-lg border border-border-subtle bg-surface px-3 py-2">
          <span className="shrink-0 font-caption text-[11px] text-foreground-tertiary">
            {t("chat.orchestratorFailure.runInTerminal", "在终端运行")}
          </span>
          <code className="flex-1 truncate font-mono text-xs text-foreground">
            {loginCommand}
          </code>
          <button
            type="button"
            onClick={copy}
            className="flex shrink-0 items-center gap-1 font-caption text-xs text-foreground-tertiary transition-colors hover:text-foreground"
          >
            {copied ? (
              <Check className="size-3.5" />
            ) : (
              <Copy className="size-3.5" />
            )}
            {copied ? t("common.copied", "已复制") : t("common.copy", "复制")}
          </button>
        </div>
      ) : null}

      <div className="flex flex-wrap gap-2">
        <Button size="sm" onClick={onOpenTerminal} className="h-8 gap-1.5">
          {isAuth ? (
            <LogIn className="size-3.5" />
          ) : (
            <SquareTerminal className="size-3.5" />
          )}
          {isAuth
            ? t("chat.orchestratorFailure.openTerminalLogin", "打开终端登录")
            : t("chat.orchestratorFailure.openTerminal", "打开终端查看")}
        </Button>
        <Button
          size="sm"
          variant="outline"
          onClick={onRetry}
          disabled={retrying}
          className="h-8 gap-1.5"
        >
          <RotateCw className={cn("size-3.5", retrying && "animate-spin")} />
          {retrying
            ? t("chat.orchestratorFailure.retrying", "重试中…")
            : t("chat.orchestratorFailure.retry", "重试")}
        </Button>
      </div>
    </div>
  );
}
