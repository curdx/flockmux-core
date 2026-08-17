/**
 * AppShell — top-level chrome shared by every product route.
 *
 * Layout: thin top bar (brand left + actions right) + outlet fills below.
 *
 * The previous design had a 192px / 56px collapsible left nav with
 * 4 items (Chat / Notifications / Settings / Debug). After the workspace
 * Shell refactor, "Chat" was the only first-class entry; the other 3
 * are account-level / dev-tool surfaces that don't deserve always-on
 * sidebar real estate. We follow the Slack / Linear / Notion playbook
 * now: brand top-left, app menu top-right (notification bell + ⌘K +
 * dropdown for settings / theme / debug). The Outlet gets the entire
 * horizontal width — important because the workspace Shell already
 * owns a 264px sidebar of its own.
 *
 * Sources: Slack's "Meet your simpler, more streamlined sidebar"
 * blog + Linear "How we redesigned the Linear UI" — both moved
 * notifications/settings out of the primary nav and into the user
 * menu / top right.
 *
 * /debug renders without AppShell (it owns its own dark chrome).
 */

import { Link, Outlet, useLocation, useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { lazy, Suspense, useEffect, useState } from "react";
import { Command, Search } from "lucide-react";
import { BrandMark } from "@/components/Brand";
import { CommandPalette } from "@/components/CommandPalette";
import { BackendDownBanner } from "@/components/BackendDownBanner";
import { McpActivityBar } from "@/components/mcp/McpActivityBar";
import { ErrorBoundary } from "@/components/ErrorBoundary";
import { NotificationPopover } from "@/components/NotificationPopover";
import { useNotificationBadge } from "@/hooks/useNotificationBadge";
import { useAppSettingsBehaviors } from "@/hooks/useAppSettingsBehaviors";
import { api } from "@/api/http";
import { primeInputPolicies } from "@/lib/cliInputPolicy";
import { formatShortcutChord, getClientPlatformInfo } from "@/lib/platform";
import {
  isTauriOverlayWindow,
  TAURI_DRAG_REGION,
  TAURI_TITLEBAR_LEFT_SAFE_PX,
} from "@/lib/tauriWindowChrome";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";

import { ensureSwarmProjection } from "@/lib/swarmProjectionStore";

const CreateWizard = lazy(() =>
  import("@/components/workspace/CreateWizard").then((m) => ({
    default: m.CreateWizard,
  })),
);

export function AppShell() {
  // Start the immortal /ws/swarm reduction before any workspace view mounts
  // so Chat / Dag / Ledger share one snapshot instead of each re-reducing.
  ensureSwarmProjection();
  const { t } = useTranslation();
  const location = useLocation();
  const navigate = useNavigate();
  const { unseenCount } = useNotificationBadge();

  // Global fallback host for ⌘K → 新建工作空间. Home / WorkspaceShell claim
  // the event first (their listeners register before this one); on pages with
  // no host (/settings, /usage, /mcp…) this opens the wizard instead of the
  // event silently dying. Navigating after create is race-free here: no
  // Shell is mounted (it would have claimed the event), so the destination
  // Shell fetches a fresh workspace list that already contains the new slug.
  const [wizardOpen, setWizardOpen] = useState(false);
  useEffect(() => {
    const onOpen = (e: Event) => {
      if ((e as CustomEvent<{ handled: boolean }>).detail?.handled) return;
      setWizardOpen(true);
    };
    window.addEventListener("swarmx:open-wizard", onOpen as EventListener);
    return () =>
      window.removeEventListener("swarmx:open-wizard", onOpen as EventListener);
  }, []);

  // Wire the three behavioural Settings toggles (launch show/hide, desktop
  // notifications, kill-others-on-fail) to real runtime effects. Mounted here
  // because AppShell is the cross-route always-on host; behaviours read the
  // latest persisted preference on each use, so no re-mount on a toggle change.
  useAppSettingsBehaviors();

  // Prime per-CLI input policies from the backend plugin manifest once at
  // startup, so the terminal's keystroke-settle timing is data-driven (no
  // hardcoded per-CLI branch). Best-effort: a fetch failure just leaves the
  // permissive 0ms default. Runs long before any agent terminal mounts.
  useEffect(() => {
    api
      .listPlugins()
      .then(primeInputPolicies)
      .catch(() => {});
  }, []);

  const platform = getClientPlatformInfo();
  const isTauri = isTauriOverlayWindow();
  const commandPaletteShortcut = formatShortcutChord("K", platform);
  const commandPaletteHint = t("shell.cmdkHint", {
    shortcut: commandPaletteShortcut,
  });

  return (
    <TooltipProvider delayDuration={300}>
      <div className="flex h-full flex-col bg-surface-primary text-foreground-primary">
        {/* Honest backend-down strip — top of the shell, full width, above the
            header so it's the first thing the user sees if the sidecar dies.
            No-op in browser dev builds and while the backend is healthy. */}
        <BackendDownBanner />
        <header
          className="flex h-11 shrink-0 items-center gap-3 border-b border-border-subtle bg-surface-secondary px-3"
          {...TAURI_DRAG_REGION}
          // Keep the brand clearly to the right of macOS traffic lights when
          // using Tauri's overlay title bar. The extra gap is intentional:
          // "not touching" feels better than "barely not overlapping".
          style={isTauri ? { paddingLeft: TAURI_TITLEBAR_LEFT_SAFE_PX } : undefined}
        >
          {isTauri && (
            <div
              aria-hidden
              className="pointer-events-none shrink-0"
              style={{ width: 8 }}
            />
          )}
          {/* Brand — top-left in browser, right after OS traffic lights in Tauri. */}
          <Link
            to="/chat"
            className="group flex items-center gap-2 rounded-md px-1.5 py-1 transition-colors hover:bg-surface-tertiary"
            title={t("nav.chat")}
          >
            <BrandMark size={28} className="drop-shadow-sm" />
            <span className="font-mono text-[15px] font-semibold tracking-tight text-foreground-primary">
              swarmx
            </span>
          </Link>

          <span className="flex-1 self-stretch" {...TAURI_DRAG_REGION} />

          {/* ⌘K trigger — 用 Search icon + 旁边小 kbd hint，跟 🔔 / ⚙
              统一成 icon button 风格。之前用纯 kbd + border 像"输入框
              提示"，视觉权重比旁边的 icon button 重，三个并排不统一。
              click 仍触发同个 keyboard event，键盘党 / 鼠标党都覆盖。
              Notion / Slack / Linear 顶栏 search 全是这模式。 */}
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                onClick={() => {
                  const ev = new KeyboardEvent("keydown", {
                    key: "k",
                    code: "KeyK",
                    metaKey: platform.isApple,
                    ctrlKey: !platform.isApple,
                    bubbles: true,
                  });
                  window.dispatchEvent(ev);
                }}
                className="flex h-9 items-center gap-1.5 rounded-md px-2 text-foreground-tertiary transition-colors hover:bg-surface-tertiary hover:text-foreground-primary"
                aria-label={commandPaletteHint}
              >
                <Search className="size-4" />
                {/* ⌘ as a crisp lucide SVG (not the U+2318 font glyph, which
                    renders thin/small next to a cap "K"). Non-mac falls back
                    to a "Ctrl" label. Borderless to match the bell / search
                    icon-button weight. */}
                <span className="hidden items-center gap-0.5 font-mono text-[11px] leading-none text-foreground-tertiary sm:inline-flex">
                  {platform.isApple ? (
                    <Command className="size-3" strokeWidth={2.25} />
                  ) : (
                    <span>Ctrl</span>
                  )}
                  K
                </span>
              </button>
            </TooltipTrigger>
            <TooltipContent side="bottom">{commandPaletteHint}</TooltipContent>
          </Tooltip>

          {/* Notification bell — Popover quick preview。点 bell 弹 360px
              panel 显示最近 12 条事件，每条 click 跳对应 workspace；底
              部"查看全部"跳 /notifications 完整页。GitHub / Discord /
              Notion 同款做法 — 通知是"瞄一眼"动作，不该跳走当前 view。
              unseenCount 显示数字徽章；瞄一眼弹层不算已读。 */}
          <NotificationPopover unseenCount={unseenCount} />

          {/* 设置已挪到最左导航菜单条(McpActivityBar)底部 — 顶栏只留
              ⌘K + 通知,跟 VS Code 把设置放活动栏底一致。 */}
        </header>
        <main className="flex min-h-0 flex-1 overflow-hidden">
          {/* 最外层窄活动栏(VS Code 式)，常驻所有 AppShell 路由的内容区最左。
              MCP 图标点开左侧 Sheet。全屏录像 / debug 在 AppShell 之外，不受影响。 */}
          <McpActivityBar />
          {/* Contain a render throw to the content area — the header (and its
              ⌘K / bell / settings) stays usable, and the route key resets the
              boundary so navigating away clears a stuck error. */}
          <div className="flex min-w-0 flex-1 flex-col">
            <ErrorBoundary resetKey={location.pathname}>
              <Outlet />
            </ErrorBoundary>
          </div>
        </main>
        <CommandPalette />
        {wizardOpen && (
          <Suspense fallback={null}>
            <CreateWizard
              open={wizardOpen}
              onClose={() => setWizardOpen(false)}
              onCreated={(ws) => {
                if (ws) navigate(`/chat/${ws.slug}`);
              }}
            />
          </Suspense>
        )}
      </div>
    </TooltipProvider>
  );
}
