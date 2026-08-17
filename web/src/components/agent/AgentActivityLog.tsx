/**
 * AgentActivityLog — 单 worker 的步骤级活动流（drawer「活动」tab）。
 *
 * 纯展示组件：活动数据由 SwarmProjection 累积（agentActivityById，有界），
 * Shell 把该 agent 的活动列表作为 props 传进来。
 * 这样切 tab / 关开抽屉 / 组件 remount 都不丢历史——数据不住在本组件里，
 * 不再 ephemeral（之前组件级订阅，未提前打开就看不到 worker 已经干过的事）。
 *
 * 每条 = 一次工具/步骤：running 转圈 → 同 seq 的 ok ✓ / error ✕ 就地替换 +
 * 耗时。按 seq 顺序（最新在底）实时滚动。
 */
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Loader2 } from "lucide-react";
import type { AgentActivity } from "../../api/types";
import { cn } from "@/lib/cn";

/** 步骤时长文案：<1s 用 ms，否则用 s（一位小数）。 */
function fmtStepDuration(ms: number | undefined): string {
  if (ms == null) return "";
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

export function AgentActivityLog({
  activities,
}: {
  activities: AgentActivity[];
}) {
  const { t } = useTranslation();
  // 每秒 tick，让运行中步骤的 elapsed 自然增长。
  const [now, setNow] = useState(Date.now());
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, []);

  // 有新步骤时滚到底。
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [activities]);

  if (activities.length === 0) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 px-6 text-center text-foreground-tertiary">
        <span className="font-caption text-sm">{t("agent.activity.empty")}</span>
        <span className="max-w-[44ch] font-caption text-[11px] leading-relaxed">
          {t("agent.activity.emptyHint")}
        </span>
      </div>
    );
  }

  return (
    <div ref={scrollRef} className="h-full overflow-y-auto p-2">
      <ul className="flex flex-col">
        {activities.map((s, j) => (
          <li
            key={`${s.seq}-${j}`}
            className="flex items-center gap-2 rounded px-2 py-1.5 hover:bg-surface-tertiary"
          >
            <StepGlyph phase={s.phase} />
            <span
              className={cn(
                "min-w-0 flex-1 truncate font-mono text-[11px]",
                s.phase === "error"
                  ? "text-status-danger"
                  : "text-foreground-primary",
              )}
              title={s.label}
            >
              {s.label}
            </span>
            <span className="shrink-0 font-mono text-[10px] text-foreground-tertiary">
              {s.phase === "running"
                ? fmtStepDuration(Math.max(0, now - s.at))
                : fmtStepDuration(s.duration_ms)}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function StepGlyph({ phase }: { phase: AgentActivity["phase"] }) {
  if (phase === "running") {
    return (
      <Loader2 className="size-3.5 shrink-0 animate-spin text-accent-primary" />
    );
  }
  if (phase === "ok") {
    return (
      <span className="flex size-3.5 shrink-0 items-center justify-center font-mono text-[11px] text-status-success">
        ✓
      </span>
    );
  }
  return (
    <span className="flex size-3.5 shrink-0 items-center justify-center font-mono text-[11px] text-status-danger">
      ✕
    </span>
  );
}
