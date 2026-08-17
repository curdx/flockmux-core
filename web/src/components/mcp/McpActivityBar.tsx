/**
 * McpActivityBar — 最外层应用导航菜单，常驻在 AppShell 内容区最左。横排菜单项
 * (图标 + 文字)，可展开/收起成纯图标窄条(状态存 localStorage)：
 *   - 顶部组 → 对话 / 用量 常驻;文件 / 终端 / MCP / 定时 / 目标 / 任务 收进「更多」
 *   - 底部分组 → 设置
 *   - 最底     → 菜单展开/收起开关
 *
 * 「对话」打头：它是产品主入口(工作空间聊天)，其余都是围绕它的工具页；以前主入口
 * 在这条栏里没有锚点(只能点 logo)，且在 /chat 时全栏无 active 高亮。
 *
 * 这些页面以前只能 ⌘K 或直接敲 URL 到达(发现性差)；放进常驻左栏作为 app-level
 * 入口。图标/标签与 CommandPalette 的 NAV 保持一致。
 *
 * R3 形态收敛:栏里只钉 对话 / 用量(设置固定在底部),其余六个目的地收进
 * 「更多」展开区 —— 每个页面仍然可达,这是 declutter,不是删除。落在被收起的
 * 页面上时「更多」自动展开(跟 WorkspaceToolbar 的「高级」一个思路)。
 */

import { cloneElement, useEffect, useState, type ComponentType, type ReactElement } from "react";
import { NavLink, useLocation } from "react-router-dom";
import { useTranslation } from "react-i18next";
import {
  BarChart3,
  Blocks,
  ChevronDown,
  ChevronsLeft,
  ChevronsRight,
  ClipboardList,
  Clock,
  Flag,
  FolderTree,
  MessageSquare,
  Monitor,
  Moon,
  Settings,
  Sun,
  Terminal,
} from "lucide-react";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { cn } from "@/lib/cn";
import { useAvailableUpdate } from "@/lib/updater";
import { useThemePreference } from "@/lib/useThemePreference";
import type { ThemeMode } from "@/lib/theme";

const MENU_KEY = "swarmx:mcp:menuCollapsed:v1";

interface NavEntry {
  href: string;
  labelKey: string;
  fallback: string;
  icon: ComponentType<{ className?: string }>;
}

/** Top nav group — app-level destinations (each a standalone route). Keep
 *  labels/icons in sync with NAV in components/CommandPalette.tsx. */
const NAV: ReadonlyArray<NavEntry> = [
  // Chat (the workspace surface) leads — it's the product's primary feature and
  // every other page is a tool around it. Without this anchor the only way back
  // to a conversation from a tool page was the logo, and no rail item was ever
  // active on /chat. `startsWith("/chat")` keeps it lit across the Flow/Ledger/
  // Replays sub-tabs too.
  { href: "/chat", labelKey: "nav.chat", fallback: "对话", icon: MessageSquare },
  { href: "/usage", labelKey: "nav.usage", fallback: "用量", icon: BarChart3 },
];

/** R3「更多」区:次要工具页默认收起,点开或落在这些路由上时展开。 */
const NAV_MORE: ReadonlyArray<NavEntry> = [
  { href: "/files", labelKey: "nav.files", fallback: "文件", icon: FolderTree },
  { href: "/terminal", labelKey: "nav.terminal", fallback: "终端", icon: Terminal },
  { href: "/mcp", labelKey: "mcp.title", fallback: "MCP", icon: Blocks },
  { href: "/cron", labelKey: "nav.cron", fallback: "定时", icon: Clock },
  { href: "/goals", labelKey: "nav.goals", fallback: "目标", icon: Flag },
  { href: "/tasks", labelKey: "nav.tasks", fallback: "任务", icon: ClipboardList },
];

function readFlag(key: string): boolean {
  try {
    return window.localStorage.getItem(key) === "1";
  } catch {
    return false;
  }
}

const itemBase =
  "flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-[13px] font-medium transition-colors";
const itemActive = "bg-accent-primary-soft text-accent-primary";
const itemIdle =
  "text-foreground-secondary hover:bg-surface-tertiary hover:text-foreground-primary";

export function McpActivityBar() {
  const { t } = useTranslation();
  const { pathname } = useLocation();
  const [collapsedPref, setCollapsedPref] = useState(() => readFlag(MENU_KEY));
  // Below `lg` the workspace column + chat need the width, so force the rail to
  // its icon-only state regardless of the saved preference (R2-004); widening
  // back restores the user's choice. CSS can't do this alone — the labels are
  // rendered conditionally on `collapsed`, not just hidden.
  const [narrow, setNarrow] = useState(false);
  useEffect(() => {
    const mq = window.matchMedia("(max-width: 1023.98px)");
    const sync = () => setNarrow(mq.matches);
    sync();
    mq.addEventListener("change", sync);
    return () => mq.removeEventListener("change", sync);
  }, []);
  const collapsed = collapsedPref || narrow;
  // Non-intrusive "有新版本" badge: the silent startup check stashes any update;
  // we dot the settings icon so the user notices without a popup.
  const hasUpdate = useAvailableUpdate() != null;

  // NOTE: NavLink's function-style className ({isActive}) => ... does NOT
  // survive Radix `asChild`/Slot. In the collapsed state each NavLink is
  // wrapped by <TooltipTrigger asChild>, and Slot only merges *string*
  // classNames — a function gets String()'d into the DOM `class` attribute
  // verbatim, so itemBase/justify-center never apply and the icons sit
  // unaligned. Compute active from the location ourselves and pass a plain
  // string so it merges correctly in both collapsed and expanded states.
  const linkClass = (active: boolean) =>
    cn(itemBase, collapsed && "justify-center px-0", active ? itemActive : itemIdle);

  const setCollapsedPersist = (v: boolean) => {
    setCollapsedPref(v);
    try {
      window.localStorage.setItem(MENU_KEY, v ? "1" : "0");
    } catch {
      /* ignore */
    }
  };

  // 收起态下菜单项只剩图标 → 用 tooltip 补回标签。
  const withTip = (label: string, node: ReactElement, key?: string) =>
    collapsed ? (
      <Tooltip key={key}>
        <TooltipTrigger asChild>{node}</TooltipTrigger>
        <TooltipContent side="right">{label}</TooltipContent>
      </Tooltip>
    ) : (
      cloneElement(node, { key })
    );

  // R3「更多」区:session-only 展开状态(不落 localStorage —— 跟工具栏「高级」
  // 同理,持久化的展开会把陌生人永远埋在完整列表里);当前路由在收起组里时
  // 自动展开,保证 active 高亮永远可见。
  const onMoreRoute = NAV_MORE.some((e) => pathname.startsWith(e.href));
  const [moreExpanded, setMoreExpanded] = useState(false);
  const moreOpen = moreExpanded || onMoreRoute;

  const renderNavItem = ({ href, labelKey, fallback, icon: Icon }: NavEntry) => {
    const label = t(labelKey, fallback);
    return withTip(
      label,
      <NavLink
        to={href}
        aria-label={label}
        className={linkClass(pathname.startsWith(href))}
      >
        <Icon className="size-[18px] shrink-0" />
        {!collapsed && <span className="font-heading">{label}</span>}
      </NavLink>,
      href,
    );
  };

  return (
    <aside
      className={cn(
        "flex shrink-0 flex-col gap-1 border-r border-border-subtle bg-surface-secondary py-2.5",
        collapsed ? "w-14 px-1.5" : "w-[184px] px-2",
      )}
    >
      <div className="flex flex-col gap-1">
        {/* 顶部常驻:对话 / 用量 */}
        {NAV.map(renderNavItem)}

        {/* 「更多」:文件 / 终端 / MCP / 定时 / 目标 / 任务(默认收起) */}
        {withTip(
          t("nav.more", { defaultValue: "更多" }),
          <button
            type="button"
            onClick={() => setMoreExpanded((v) => !v)}
            aria-expanded={moreOpen}
            aria-label={t("nav.more", { defaultValue: "更多" })}
            className={cn(itemBase, collapsed && "justify-center px-0", itemIdle)}
          >
            <ChevronDown
              className={cn(
                "size-[18px] shrink-0 transition-transform",
                moreOpen && "rotate-180",
              )}
            />
            {!collapsed && (
              <span className="font-heading">
                {t("nav.more", { defaultValue: "更多" })}
              </span>
            )}
          </button>,
          "more-toggle",
        )}
        {moreOpen && NAV_MORE.map(renderNavItem)}
      </div>

      {/* 底部分组：设置固定可见，不再和折叠按钮互相挤压。 */}
      <div className="mt-auto flex flex-col gap-1 border-t border-border-subtle/80 pt-2">
        {withTip(
          t("nav.settings", "设置"),
          <NavLink
            to="/settings"
            aria-label={t("nav.settings", "设置")}
            className={linkClass(pathname.startsWith("/settings"))}
          >
            <span className="relative shrink-0">
              <Settings className="size-[18px]" />
              {hasUpdate && (
                <span
                  className="absolute -right-1 -top-1 size-2 rounded-full bg-destructive ring-2 ring-surface-secondary"
                  aria-label={t("settings.about.update.badge", "有新版本")}
                />
              )}
            </span>
            {!collapsed && (
              <span className="font-heading">{t("nav.settings", "设置")}</span>
            )}
          </NavLink>,
          "settings",
        )}

        {/* 主题快捷切换(浅→深→跟随系统):Settings→外观 的同一份数据,
            但切换主题不该要进设置页两步。 */}
        <ThemeCycleButton collapsed={collapsed} withTip={withTip} itemBase={itemBase} itemIdle={itemIdle} />

        {/* 底部：菜单展开/收起开关 */}
        {withTip(
          collapsed ? t("mcp.menuExpand", "展开菜单") : t("mcp.menuCollapse", "收起菜单"),
          <button
            type="button"
            onClick={() => setCollapsedPersist(!collapsed)}
            aria-label={collapsed ? t("mcp.menuExpand", "展开菜单") : t("mcp.menuCollapse", "收起菜单")}
            className={cn(itemBase, collapsed && "justify-center px-0", itemIdle)}
          >
            {collapsed ? (
              <ChevronsRight className="size-[18px] shrink-0" />
            ) : (
              <>
                <ChevronsLeft className="size-[18px] shrink-0" />
                <span className="font-heading">{t("mcp.menuCollapse", "收起")}</span>
              </>
            )}
          </button>,
          "collapse-toggle",
        )}
      </div>
    </aside>
  );
}

/** 主题循环按钮(浅色 → 深色 → 跟随系统):设置页「外观」的快捷版,挂在
 *  活动栏底部常驻可见 —— 切主题不该要进设置页翻两层。 */
function ThemeCycleButton({
  collapsed,
  withTip,
  itemBase,
  itemIdle,
}: {
  collapsed: boolean;
  withTip: (label: string, node: ReactElement, key?: string) => ReactElement;
  itemBase: string;
  itemIdle: string;
}) {
  const { t } = useTranslation();
  const [mode, setMode] = useThemePreference();
  const ORDER: ThemeMode[] = ["light", "dark", "system"];
  const next = ORDER[(ORDER.indexOf(mode) + 1) % ORDER.length];
  const META: Record<ThemeMode, { icon: ComponentType<{ className?: string }>; key: string; fb: string }> = {
    light: { icon: Sun, key: "nav.theme.light", fb: "浅色" },
    dark: { icon: Moon, key: "nav.theme.dark", fb: "深色" },
    system: { icon: Monitor, key: "nav.theme.system", fb: "跟随系统" },
  };
  const cur = META[mode];
  const Icon = cur.icon;
  const tip = t("nav.theme.toggleHint", {
    current: t(cur.key, cur.fb),
    next: t(META[next].key, META[next].fb),
    defaultValue: `主题:${t(cur.key, cur.fb)} · 点击切换到${t(META[next].key, META[next].fb)}`,
  });
  return withTip(
    tip,
    <button
      type="button"
      onClick={() => setMode(next)}
      aria-label={tip}
      className={cn(itemBase, collapsed && "justify-center px-0", itemIdle)}
    >
      <Icon className="size-[18px] shrink-0" />
      {!collapsed && <span className="font-heading">{t(cur.key, cur.fb)}</span>}
    </button>,
    "theme-cycle",
  );
}
