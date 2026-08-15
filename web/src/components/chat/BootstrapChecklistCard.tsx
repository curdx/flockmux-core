/**
 * BootstrapChecklistCard — honest "规划正在准备…" placeholder shown in an empty
 * room while the orchestrator is starting up, replacing the bare "暂无消息".
 *
 * P0-5/P0-6 of the chat redesign (事实律): the diagnosed P0 bug was an empty
 * room sitting behind a fake green dot during the 20-40s the engine boots. This
 * card gives proof-of-life from the FIRST second — a checklist that consumes the
 * real agent timestamps (spawned_at / shim_ready) — and its badge is cyan
 * `◐ 启动中`, NEVER green: green only appears after a real first response.
 *
 * It deliberately does NOT self-time the 90s watchdog. When the backend's
 * first-response watchdog fires it flips the orchestrator to AgentState::Error,
 * the parent's failure derivation lights up, and the failure card replaces this
 * card in place (原地翻转). Past 60s we only add a soft "响应较慢" hint — still
 * ◐, never a self-invented failure.
 */
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Circle, CircleCheck, GitBranch, Loader2 } from "lucide-react";
import { cn } from "@/lib/cn";

export interface BootstrapChecklistCardProps {
  /** Branch the session was isolated to, e.g. "main ↘ 退款流程" or just a branch
   *  name. Omitted ⇒ the isolation row is hidden (main session / unknown). */
  branchLabel?: string | null;
  /** Engine display name, e.g. "Claude Code". */
  engineName?: string | null;
  /** Whether the orchestrator's PTY is up. Drives which row is the active ◐. */
  shimReady: boolean;
  /** unix-ms the orchestrator was spawned — drives the elapsed timer + the
   *  >60s "响应较慢" soft hint. */
  spawnedAt?: number | null;
}

type RowState = "done" | "active" | "pending";

function Row({ state, children }: { state: RowState; children: React.ReactNode }) {
  return (
    <div
      className={cn(
        "flex items-center gap-2",
        state === "pending" ? "text-foreground-tertiary" : "text-foreground-secondary",
      )}
    >
      {state === "done" ? (
        <CircleCheck className="size-4 shrink-0 text-status-success" />
      ) : state === "active" ? (
        <Loader2 className="size-4 shrink-0 animate-spin text-state-wake" />
      ) : (
        <Circle className="size-4 shrink-0 text-foreground-tertiary" />
      )}
      <span className="min-w-0">{children}</span>
    </div>
  );
}

export function BootstrapChecklistCard({
  branchLabel,
  engineName,
  shimReady,
  spawnedAt,
}: BootstrapChecklistCardProps) {
  const { t } = useTranslation();
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, []);
  const elapsedSec =
    spawnedAt != null ? Math.max(0, Math.floor((now - spawnedAt) / 1000)) : null;
  const slow = elapsedSec != null && elapsedSec > 60;

  // Active row = the first not-yet-done step. !shim_ready → engine launching;
  // shim_ready (room still empty) → awaiting the first response.
  const engineState: RowState = shimReady ? "done" : "active";
  const awaitingState: RowState = shimReady ? "active" : "pending";
  const timerLabel =
    elapsedSec == null
      ? null
      : elapsedSec < 60
        ? `${elapsedSec}s`
        : `${Math.floor(elapsedSec / 60)}:${String(elapsedSec % 60).padStart(2, "0")}`;

  return (
    <div className="mx-auto mt-10 flex w-full max-w-[460px] flex-col gap-3 rounded-2xl border border-accent-primary/30 bg-accent-primary-soft px-5 py-4">
      <div className="flex items-center gap-2">
        <Loader2 className="size-4 shrink-0 animate-spin text-state-wake" />
        <p className="font-heading text-sm font-semibold text-foreground">
          {t("chat.bootstrap.cardTitle", "规划正在准备…")}
        </p>
        <span className="ml-auto rounded-full bg-surface-elevated px-2 py-0.5 font-caption text-[10px] font-semibold text-state-wake">
          {t("chat.bootstrap.badge", "启动中")}
        </span>
      </div>

      <div className="flex flex-col gap-2 font-caption text-xs leading-5">
        {branchLabel && (
          <Row state="done">
            <span className="inline-flex items-center gap-1.5">
              <GitBranch className="size-3 shrink-0 text-foreground-tertiary" />
              {t("chat.bootstrap.stepIsolateDone", "已隔离到独立分支")}
              <span className="truncate font-mono text-[11px] text-foreground-tertiary">
                {branchLabel}
              </span>
            </span>
          </Row>
        )}
        <Row state={engineState}>
          {engineName
            ? t("chat.bootstrap.stepEngineNamed", {
                name: engineName,
                defaultValue: `正在启动 {{name}} 规划引擎…`,
              })
            : t("chat.bootstrap.stepEngine", "正在启动规划引擎…")}
          {engineState === "active" && timerLabel && (
            <span className="ml-1.5 font-mono text-[11px] text-foreground-tertiary">
              {timerLabel}
            </span>
          )}
        </Row>
        <Row state={awaitingState}>
          {t("chat.bootstrap.stepFirstResponse", "等待规划第一次响应")}
          {awaitingState === "active" && timerLabel && (
            <span className="ml-1.5 font-mono text-[11px] text-foreground-tertiary">
              {timerLabel}
            </span>
          )}
        </Row>
      </div>

      {slow && (
        <p className="font-caption text-[11px] leading-5 text-foreground-tertiary">
          {t("chat.bootstrap.slowHint", "响应较慢，再等等…")}
        </p>
      )}
    </div>
  );
}
