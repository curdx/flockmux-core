/**
 * Workspace chrome: the per-view tab bar (chat + optional advanced views) with
 * LIVE + unread-jump actions, plus the Outlet cross-fade wrapper. Extracted
 * from Shell.tsx so the layout route stays focused on data orchestration.
 *
 * Default UI shows **chat only**; DAG / ledger / fusion / consult / replays
 * sit behind an 「高级」disclosure so strangers aren't hit with a six-tab
 * author console on first open (road-to-100 上手建议).
 *
 * R3 形态收敛:fusion / consult 进一步归「实验室功能」(设置→通用,默认 OFF)。
 * 开关在 buildTabs 里读 —— Shell 的 ⌘1-6 用的也是这份列表,所以隐藏后
 * 快捷键自动跳过它们,深链则由各 view 里的 LabGate 兜底成诚实空态。
 *
 * `buildTabs` is exported because Shell registers the ⌘1-6 global shortcut that
 * navigates to these same tab targets — one definition, two consumers.
 */

import { type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import { NavLink, useLocation, useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import {
  ChevronDown,
  ClipboardList,
  GitBranch,
  GitMerge,
  MessageSquare,
  PanelLeft,
  Play,
  Swords,
  Users,
} from "lucide-react";
import type { WorkspaceSummary } from "./types";
import { directionBase } from "@/lib/thread";
import { MergeDialog } from "@/components/workspace/MergeDialog";
import { Badge } from "@/components/ui/badge";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { cn } from "@/lib/cn";
import { useSwarmFeedStatus } from "@/hooks/useSwarmFeed";
import { formatShortcutChord, getClientPlatformInfo } from "@/lib/platform";
import { loadAppSettings, useLabFeatures } from "@/lib/appSettings";
import { track } from "@/lib/telemetry";

interface TabDef {
  /** 稳定 id —— R9 本机统计按它计数(`tab.<id>`),与 CommandPalette 的
   *  WORKSPACE_VIEWS 保持一致。 */
  id: string;
  to: string;
  labelKey: string;
  icon: typeof MessageSquare;
  /** Hidden behind 「高级」 until the user expands or lands on the route. */
  advanced?: boolean;
  /** 实验室功能:设置→通用的开关(默认 OFF)为关时从 tab 列表剔除。 */
  lab?: boolean;
}

export function buildTabs(wsId: string, threadSlug?: string): TabDef[] {
  // Non-main directions live under `/chat/:wsId/t/:threadSlug/*`; the main
  // direction keeps the bare `/chat/:wsId/*` URLs. Tabs preserve whichever
  // direction is active so switching views never drops you back to main.
  const base = directionBase(wsId, threadSlug);
  const tabs: TabDef[] = [
    { id: "chat", to: `${base}`, labelKey: "chat.tabs.chat", icon: MessageSquare },
    {
      id: "dag",
      to: `${base}/dag`,
      labelKey: "chat.tabs.dag",
      icon: GitBranch,
      advanced: true,
    },
    {
      id: "ledger",
      to: `${base}/ledger`,
      labelKey: "chat.tabs.ledger",
      icon: ClipboardList,
      advanced: true,
    },
    {
      id: "fusion",
      to: `${base}/fusion`,
      labelKey: "chat.tabs.fusion",
      icon: Swords,
      advanced: true,
      lab: true,
    },
    {
      id: "consult",
      to: `${base}/consult`,
      labelKey: "chat.tabs.consult",
      icon: Users,
      advanced: true,
      lab: true,
    },
    {
      id: "replays",
      to: `${base}/replays`,
      labelKey: "chat.tabs.replays",
      icon: Play,
      advanced: true,
    },
  ];
  // R3:实验室关 → 摘掉 lab tab。每次调用都现读(不走缓存),Shell 的 ⌘1-6
  // 在挂载/切方向时重建这份列表,天然跟上开关;⌘ 序号按位置算(见渲染处)。
  if (!loadAppSettings().labFeatures) return tabs.filter((tab) => !tab.lab);
  return tabs;
}

export function WorkspaceToolbar({
  workspace,
  threadSlug,
  agentCount,
  totalUnread,
  onJumpUnread,
  onCleanupThread,
  onOpenWorkspaceNav,
}: {
  workspace: WorkspaceSummary;
  /** Active direction slug; tabs stay within this direction. */
  threadSlug: string;
  agentCount: number;
  totalUnread: number;
  onJumpUnread: () => void;
  /** Clean up a direction after merge (delete worktree+branch+card, nav to main). */
  onCleanupThread: (threadId: string) => void;
  /** Mobile-only entry for opening the workspace / direction rail. */
  onOpenWorkspaceNav?: () => void;
}) {
  const { t } = useTranslation();
  // R3:订阅实验室开关(跨 tab storage 事件)——同窗口内在设置页翻转后,
  // 回到工作区本组件重挂载,buildTabs 现读也是新的;这里主要保跨 tab 同步。
  const labOn = useLabFeatures();
  const tabs = buildTabs(workspace.id, threadSlug);
  const primaryTabs = useMemo(() => tabs.filter((tab) => !tab.advanced), [tabs]);
  const advancedTabs = useMemo(() => tabs.filter((tab) => tab.advanced), [tabs]);
  const platform = getClientPlatformInfo();
  const location = useLocation();
  const navigate = useNavigate();
  // "跳转未读" only does anything on the chat tab (only MessagesPanel scrolls).
  // The chat tab is tabs[0] (the direction base, no `/dag|/ledger|/replays`
  // suffix). On the other tabs the button was dead — clicking it did nothing.
  // Detect we're on chat by an exact path match against the chat tab's target.
  const chatTo = tabs[0].to;
  const onChatTab = location.pathname === chatTo;
  const onAdvancedRoute = advancedTabs.some(
    (tab) => location.pathname === tab.to || location.pathname.startsWith(`${tab.to}/`),
  );
  // Session-only expand — do NOT persist to localStorage. Persisting "1" once
  // left strangers (and returning chat sessions) stuck with the six-tab author
  // console forever (UX-003). Deep links / ⌘2-6 still auto-reveal; leaving
  // those routes collapses again so chat stays a single primary surface.
  const [userExpanded, setUserExpanded] = useState(false);
  const wasOnAdvanced = useRef(onAdvancedRoute);
  useEffect(() => {
    if (wasOnAdvanced.current && !onAdvancedRoute) {
      setUserExpanded(false);
    }
    wasOnAdvanced.current = onAdvancedRoute;
  }, [onAdvancedRoute]);
  const advancedOpen = userExpanded || onAdvancedRoute;
  const visibleTabs = advancedOpen ? tabs : primaryTabs;
  // M4: bind the "LIVE" badge to the REAL socket connection, not just the
  // member count. After the WS drops (network blip / server restart) the REST
  // member snapshot lingers, so a count-only badge kept claiming "LIVE" over a
  // dead feed. Now it dims to an honest "离线" until the feed reconnects.
  const feedConnected = useSwarmFeedStatus() === "open";

  // "合并到主线" only when the direction has commits ahead of main. Showing
  // it for ahead=0+behind>N (already merged / stale) opens a dead-end dialog
  // that says「无需合并」while the sidebar still flashes ↓N — UX-028.
  const [mergeOpen, setMergeOpen] = useState(false);
  const activeThread = workspace.threads.find((th) => th.slug === threadSlug);
  const activeThreadLabel =
    activeThread?.slug === "main"
      ? t("chat.mainDirection")
      : activeThread?.name?.trim() ||
        activeThread?.slug ||
        t("chat.directionUnnamed");
  const ahead = activeThread?.ahead ?? 0;
  const behind = activeThread?.behind ?? 0;
  const canMerge =
    !!activeThread &&
    activeThread.slug !== "main" &&
    activeThread.isolation === "worktree" &&
    activeThread.state === "ready" &&
    ahead > 0;

  return (
    <div className="shrink-0 border-b border-border-subtle">
      <div className="flex min-h-8 items-center gap-2 border-b border-border-subtle/70 px-3 py-1.5 lg:hidden">
        <button
          type="button"
          onClick={onOpenWorkspaceNav}
          className="inline-flex size-8 shrink-0 items-center justify-center rounded-md border border-border-subtle bg-surface-elevated text-foreground-secondary hover:bg-surface-tertiary hover:text-foreground-primary"
          aria-label={t("chat.mobileWorkspaceNav", "打开工作空间列表")}
          title={t("chat.mobileWorkspaceNav", "打开工作空间列表")}
        >
          <PanelLeft className="size-4" />
        </button>
        <div className="min-w-0 flex-1">
          <div className="truncate font-heading text-xs font-semibold text-foreground-primary">
            {workspace.name}
          </div>
          <div className="truncate font-caption text-[10px] text-foreground-tertiary">
            {activeThreadLabel}
          </div>
        </div>
        {totalUnread > 0 && (
          <Badge className="shrink-0 rounded-full px-2 py-0.5 text-[10px]">
            {t("chat.unread", { count: totalUnread })}
          </Badge>
        )}
      </div>
      <nav className="flex h-10 items-center gap-1 px-3">
        {visibleTabs.map((tab, tabIndex) => {
        const Icon = tab.icon;
        return (
          <NavLink
            key={tab.to}
            to={tab.to}
            // index route 必须 end，否则 /chat/:wsId 在 /chat/:wsId/dag 时
            // 也算 active。其他 tab 路径足够独特，end 无所谓但保持一致。
            end
            // R9 本机统计:每个 tab 一个计数器(只记次数,localStorage)。
            onClick={() => track(`tab.${tab.id}`)}
            className={({ isActive }) =>
              cn(
                "relative flex min-h-8 shrink-0 items-center gap-1.5 px-3 py-2 text-xs transition-colors",
                isActive
                  ? "text-foreground-primary after:absolute after:inset-x-0 after:-bottom-px after:h-0.5 after:bg-accent-primary"
                  : "text-foreground-secondary hover:text-foreground-primary",
              )
            }
            // ⌘ 序号按可见位置算,跟 Shell 的 tabs[n-1] 对应 —— 实验室关闭
            // 摘掉两个 tab 后,固定编号会对不上,位置序号永远是对的。
            title={`${t(tab.labelKey)}  ${formatShortcutChord(tabIndex + 1, platform)}`}
          >
            <Icon className="size-3.5 shrink-0" />
            {/* Label collapses to icon-only below lg so the bar never wraps the
                text vertically on a narrow chat column (R2-004); the title attr
                keeps the name discoverable on hover. */}
            <span className="hidden whitespace-nowrap lg:inline">{t(tab.labelKey)}</span>
          </NavLink>
        );
        })}

        <button
          type="button"
          onClick={() => setUserExpanded((v) => !v)}
          aria-expanded={advancedOpen}
          title={
            labOn
              ? t("chat.tabs.advancedHint", {
                  defaultValue: "协作图、工作记录、竞赛、多模对比、录像",
                })
              : t("chat.tabs.advancedHintLabOff", {
                  defaultValue: "协作图、工作记录、录像",
                })
          }
          className={cn(
            "relative flex min-h-8 shrink-0 items-center gap-1 px-2.5 py-2 text-xs transition-colors",
            advancedOpen
              ? "text-foreground-primary"
              : "text-foreground-secondary hover:text-foreground-primary",
          )}
        >
          <span className="hidden whitespace-nowrap lg:inline">
            {t("chat.tabs.advanced", { defaultValue: "高级" })}
          </span>
          <ChevronDown
            className={cn(
              "size-3.5 shrink-0 transition-transform",
              advancedOpen && "rotate-180",
            )}
          />
        </button>

        <span className="flex-1" />

      {/* 合并到主线 — 只在「非 main + 已隔离 worktree 方向」出现。 */}
        {canMerge && activeThread && (
        <>
          <button
            type="button"
            onClick={() => setMergeOpen(true)}
            title={t("merge.button")}
            className="flex h-6 shrink-0 items-center gap-1 rounded-md border border-border-subtle px-2 font-caption text-[11px] text-foreground-secondary transition-colors hover:bg-surface-tertiary hover:text-foreground-primary"
          >
            <GitMerge className="size-3.5" />
            <span className="hidden whitespace-nowrap lg:inline">
              {t("merge.button")}
            </span>
          </button>
          <MergeDialog
            open={mergeOpen}
            onOpenChange={setMergeOpen}
            ahead={ahead}
            behind={behind}
            workspaceId={workspace.workspaceId}
            threadId={activeThread.id}
            threadName={activeThread.name || activeThread.slug}
            onCleanup={onCleanupThread}
          />
        </>
        )}

        {/* workspace actions — 全部 shrink-0 + 小尺寸，跟 tab 行高保持一致 */}
        {agentCount > 0 && (
        <Tooltip>
          <TooltipTrigger asChild>
            <span
              className={cn(
                "flex h-5 shrink-0 items-center gap-1 rounded-full px-2 font-caption text-[10px] font-semibold uppercase tracking-wide",
                feedConnected
                  ? "bg-status-running-soft text-status-running"
                  : "bg-status-warning-soft text-state-warning",
              )}
              title={
                feedConnected
                  ? t("chat.memberCount", { count: agentCount })
                  : t("chat.feedOffline", {
                      defaultValue: "实时连接已断开,下面是最后已知状态",
                    })
              }
            >
              <span
                className={cn(
                  "size-1.5 rounded-full",
                  feedConnected ? "bg-status-running" : "bg-state-warning animate-pulse",
                )}
                aria-hidden
              />
              {feedConnected
                ? t("common.live")
                : t("common.offline", { defaultValue: "离线" })}
            </span>
          </TooltipTrigger>
          <TooltipContent side="bottom">
            {feedConnected
              ? t("chat.memberCount", { count: agentCount })
              : t("chat.feedOffline", {
                  defaultValue: "实时连接已断开,下面是最后已知状态",
                })}
          </TooltipContent>
        </Tooltip>
        )}

        {totalUnread > 0 && (
        <button
          type="button"
          onClick={() => {
            // On dag/ledger/replays there's no message list to scroll, so the
            // jump was a no-op. Switch back to the chat tab first; the bumped
            // tick survives the Outlet swap and MessagesPanel mounts already
            // scrolled to the first unread.
            if (!onChatTab) navigate(chatTo);
            onJumpUnread();
          }}
          title={t("chat.jumpUnread")}
          className="flex shrink-0 cursor-pointer items-center"
        >
          <Badge className="rounded-full px-2 py-0.5 text-[10px] transition-transform hover:scale-105">
            {t("chat.unread", { count: totalUnread })}
          </Badge>
        </button>
        )}
      </nav>
    </div>
  );
}

/** 60-80ms cross-fade on Outlet child swap. Long enough to feel soft, short
 *  enough that quick tab-juggling doesn't stack delays. The `key` ties the
 *  fade to the location, so navigating to the same path doesn't replay. */
export function ViewTransition({ children }: { children: ReactNode }) {
  const location = useLocation();
  return (
    <div
      key={location.pathname}
      className="flex h-full min-h-0 flex-1 flex-col animate-in fade-in duration-75"
    >
      {children}
    </div>
  );
}
