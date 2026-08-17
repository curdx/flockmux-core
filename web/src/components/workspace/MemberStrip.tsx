/**
 * MemberStrip — <xl 视口的横向紧凑成员条(P0-12 的向下延伸)。
 *
 * 完整成员面板 ≥2xl(1536px)、PulseRail 覆盖 xl(1280–1535px),而 <1280px
 * (半屏并排 / 小窗)成员健康信号整体蒸发 —— 「谁在干活 / 谁卡住」恰恰是小窗
 * 并排干活时最值钱的信息。本组件把 PulseRail 的点逻辑横过来:一排成员头像 +
 * 诚实状态点(reuse `resolveMemberVisual` —— error 红、typing 脉冲、绝不假绿)
 * + 未读 badge,点击经 `onOpenAgent` 打开对应成员抽屉。≥xl 由右侧 rail 接管
 * (父级 `xl:hidden`),此条不占高。
 */
import { useTranslation } from "react-i18next";
import { cn } from "@/lib/cn";
import {
  resolveMemberVisual,
  roleColorClass,
  roleLabelAmong,
} from "@/lib/agent";
import type { AgentInfo, AgentLiveState, MessageRecord } from "../../api/types";

// 与 PulseRail 相同:条上只有点、没有状态文案,labels 全部留空 ——
// 只消费 visual 的 dotClass / typing / isError。
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

export function MemberStrip({
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

  // 与 PulseRail / 完整成员面板同序:error 最前、orchestrator 次之、其余原序
  // (主动 kill 的不算 error,不顶置)。
  const isErr = (a: AgentInfo) =>
    a.killed_at == null &&
    a.shim_exit == null &&
    agentStateById[a.agent_id]?.state === "error";
  const rank = (a: AgentInfo) =>
    isErr(a) ? 0 : a.role === "orchestrator" ? 1 : 2;
  const sorted = [...members].sort((a, b) => rank(a) - rank(b));

  return (
    <div
      role="group"
      aria-label={t("chat.members")}
      className="flex items-center gap-1 overflow-x-auto px-3 py-1.5"
    >
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
            className="relative flex shrink-0 items-center justify-center p-1 transition-transform hover:scale-105"
          >
            <span
              className={cn(
                "flex size-7 items-center justify-center rounded-full text-[11px] font-medium text-foreground-on-accent shadow-sm",
                roleColorClass(a.role),
                isOrchestrator &&
                  "ring-2 ring-accent-primary ring-offset-1 ring-offset-surface-secondary",
              )}
            >
              {a.role.charAt(0).toUpperCase()}
            </span>
            {/* 诚实状态点 —— typing 脉冲、error/idle 等按 visual 着色,无点则不画
                (绝不假绿)。渲染与 PulseRail 完全一致,只是尺寸场景不同。 */}
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
  );
}
