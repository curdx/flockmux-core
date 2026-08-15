/**
 * CommandPalette — ⌘K (or Ctrl+K) global launcher.
 *
 * Sections:
 *   - 导航: jump to /chat /dag /replays /context /notifications /settings
 *   - 工作空间: switch active workspace (from live /api/agent group-by)
 *   - 唤醒 agent: wake any alive agent by role + id
 *   - 主题: light / dark / system (calls lib/theme.setTheme directly)
 *   - 操作: 新建工作空间(打开 Wizard via custom event) / 全部标已读(notif) ...
 *
 * Built on shadcn `CommandDialog` (which wraps cmdk + Radix Dialog):
 * 自带 portal / focus trap / ESC / ↑↓ / fuzzy filter / 分组渲染.
 * 主题切换 token 跟 popover 一套，跟其他 shadcn 组件视觉一致。
 *
 * Mounted once in AppShell, listens for window keydown to toggle open.
 * Workspace + agent data refreshed lazily on open (one /api/agent call);
 * cheap, avoids a long-lived subscription just for the palette.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import {
  Activity,
  AlertTriangle,
  BarChart3,
  Bell,
  Boxes,
  Bug,
  ClipboardList,
  Clock,
  Flag,
  FolderTree,
  GitBranch,
  MessageSquare,
  Moon,
  Play,
  Plus,
  RefreshCw,
  Settings as SettingsIcon,
  Sun,
  SunMoon,
  Swords,
  Terminal as TerminalIcon,
  Users,
  Zap,
} from "lucide-react";
import { api, ApiError } from "../api/http";
import type { AgentInfo, Workspace } from "../api/types";
import {
  ConfirmActionDialog,
  type ConfirmActionState,
} from "@/components/ConfirmActionDialog";
import { setTheme, type ThemeMode } from "@/lib/theme";
import { toast } from "@/lib/toast";
import { cn } from "@/lib/cn";
import { directionBase } from "@/lib/thread";
import { DEBUG_ENABLED } from "@/lib/debug";
import { formatShortcutChord, getClientPlatformInfo } from "@/lib/platform";
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandShortcut,
} from "@/components/ui/command";

// 主导航 — Shell 化后 dag/replays/context 都是 workspace-scoped child
// route，不存在"全局视图"了。CommandPalette 改成：先列对话/通知/设置等
// app-level 入口，再用 WORKSPACE_VIEWS 给当前 workspace 加 4 个内部 view
// 跳转项（只在用户已经选了 workspace 时显示）。
const BASE_NAV = [
  { labelKey: "nav.chat", href: "/chat", icon: MessageSquare, hintKey: "cmdk.navHint.chat" },
  { labelKey: "nav.notifications", href: "/notifications", icon: Bell, hintKey: "cmdk.navHint.notifications" },
  // MCP is a primary left-rail destination (McpActivityBar) but was missing
  // from ⌘K — typing "mcp" returned no match. Mirror the rail's Boxes icon and
  // sit it next to 设置, the other config surface.
  { labelKey: "nav.mcp", href: "/mcp", icon: Boxes, hintKey: "cmdk.navHint.mcp" },
  { labelKey: "nav.goals", href: "/goals", icon: Flag, hintKey: "cmdk.navHint.goals" },
  { labelKey: "nav.tasks", href: "/tasks", icon: ClipboardList, hintKey: "cmdk.navHint.tasks" },
  { labelKey: "nav.files", href: "/files", icon: FolderTree, hintKey: "cmdk.navHint.files" },
  { labelKey: "nav.terminal", href: "/terminal", icon: TerminalIcon, hintKey: "cmdk.navHint.terminal" },
  { labelKey: "nav.cron", href: "/cron", icon: Clock, hintKey: "cmdk.navHint.cron" },
  { labelKey: "nav.usage", href: "/usage", icon: BarChart3, hintKey: "cmdk.navHint.usage" },
  { labelKey: "nav.settings", href: "/settings", icon: SettingsIcon, hintKey: "cmdk.navHint.settings" },
] as const;

const DEBUG_NAV = [
  { labelKey: "nav.debug", href: "/debug", icon: Bug, hintKey: "cmdk.navHint.debug" },
] as const;

const NAV = DEBUG_ENABLED ? [...BASE_NAV, ...DEBUG_NAV] : BASE_NAV;

// Keep in sync with buildTabs() in WorkspaceToolbar — same order / ⌘1-6.
const WORKSPACE_VIEWS = [
  { id: "chat", labelKey: "chat.tabs.chat", icon: MessageSquare, suffix: "" },
  { id: "dag", labelKey: "chat.tabs.dag", icon: GitBranch, suffix: "/dag" },
  { id: "ledger", labelKey: "chat.tabs.ledger", icon: ClipboardList, suffix: "/ledger" },
  { id: "fusion", labelKey: "chat.tabs.fusion", icon: Swords, suffix: "/fusion" },
  { id: "consult", labelKey: "chat.tabs.consult", icon: Users, suffix: "/consult" },
  { id: "replays", labelKey: "chat.tabs.replays", icon: Play, suffix: "/replays" },
] as const;

// Keep in sync with SECTIONS in routes/settings.tsx; surfaced here so
// ⌘K can jump to any settings tab without leaving the keyboard.
const SETTINGS_SECTIONS = [
  { id: "general", labelKey: "settings.sections.general" },
  { id: "appearance", labelKey: "settings.sections.appearance" },
  { id: "shortcuts", labelKey: "settings.sections.shortcuts" },
  { id: "models", labelKey: "settings.sections.models" },
  { id: "plugins", labelKey: "settings.sections.plugins" },
  { id: "privacy", labelKey: "settings.sections.privacy" },
  { id: "about", labelKey: "settings.sections.about" },
] as const;

export function CommandPalette() {
  const { t } = useTranslation();
  const platform = getClientPlatformInfo();
  const commandPaletteShortcut = formatShortcutChord("K", platform);
  const [open, setOpen] = useState(false);
  // 进入 open 时把当前 URL 的 wsId 冻结到一个 ref —— 不用 useLocation 是
  // 因为 react-router 的 location 变化会触发 CommandPalette 全量 re-render，
  // 让里头 cmdk 的内部 store 在 dialog 还 mount 时被重新初始化，触发
  // "Cannot read properties of undefined (reading 'subscribe')" 整个 root
  // 白屏。CommandPalette 只在用户主动开 ⌘K 时关心 URL，所以读一次就够。
  const currentWsIdRef = useRef<string | null>(null);
  const [currentWsId, setCurrentWsId] = useState<string | null>(null);
  // Active direction slug (captured alongside wsId) so view-jumps stay in the
  // direction the user is looking at, matching the tab bar / ⌘1-4.
  const [currentThreadSlug, setCurrentThreadSlug] = useState<string | null>(null);
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [confirm, setConfirm] = useState<ConfirmActionState | null>(null);
  // Lazy-load state for the agent + workspace lists. Previously both fetches
  // swallowed errors with `.catch(() => {})`, so a backend 500 / dropped
  // connection silently emptied the workspace + wake-agent groups — the palette
  // looked like the user simply had nothing, with no way to tell or retry.
  const [loadError, setLoadError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  // Bumped each time we (re)load so a slow earlier fetch can't clobber a newer
  // one (e.g. close + reopen quickly, or hit retry mid-flight).
  const loadReqRef = useRef(0);
  const navigate = useNavigate();

  // Global ⌘K / Ctrl+K opens, Esc closes (handled by Dialog).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const isCmdK =
        e.key.toLowerCase() === "k" && (e.metaKey || e.ctrlKey) && !e.shiftKey;
      if (isCmdK) {
        e.preventDefault();
        setOpen((o) => !o);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // Load the agent + workspace lists. Both fetches run in parallel; if EITHER
  // fails we flag `loadError` and surface a retry banner instead of silently
  // dropping the corresponding group. Canonical workspace list (server filters
  // soft-deleted) — same source the sidebar uses, so the two never diverge.
  const loadData = useCallback(async () => {
    const reqId = ++loadReqRef.current;
    setLoading(true);
    setLoadError(null);
    const [agentsRes, wsRes] = await Promise.allSettled([
      api.listAgents(),
      api.listWorkspaces(),
    ]);
    if (reqId !== loadReqRef.current) return; // superseded by a newer load
    if (agentsRes.status === "fulfilled") setAgents(agentsRes.value);
    if (wsRes.status === "fulfilled") setWorkspaces(wsRes.value);
    const failed = [agentsRes, wsRes].find((r) => r.status === "rejected") as
      | PromiseRejectedResult
      | undefined;
    if (failed) {
      const e = failed.reason;
      setLoadError(e instanceof ApiError ? e.detail : (e as Error)?.message ?? "");
    }
    setLoading(false);
  }, []);

  // 读取当前 URL 的 wsId + 拉列表 — 都只在 dialog 打开瞬间发生，不订阅
  // router state，避免 nav 流量轰炸 CommandPalette。
  useEffect(() => {
    if (!open) return;
    const m = window.location.pathname.match(/^\/chat\/([^/]+)(?:\/t\/([^/]+))?/);
    const ws = m ? m[1] : null;
    currentWsIdRef.current = ws;
    setCurrentWsId(ws);
    setCurrentThreadSlug(m && m[2] ? m[2] : null);
    loadData();
  }, [open, loadData]);

  const close = useCallback(() => setOpen(false), []);
  const go = useCallback(
    (href: string) => {
      navigate(href);
      close();
    },
    [navigate, close],
  );
  const openWizard = useCallback(() => {
    window.dispatchEvent(new CustomEvent("swarmx:open-wizard"));
    close();
  }, [close]);

  const wsIds = new Set(workspaces.map((w) => w.id));
  // Live agents, scoped to those whose workspace still exists. Drops orphan
  // agents whose workspace was deleted (they'd otherwise appear as phantom
  // wake targets the sidebar no longer lists). Don't over-filter before the
  // workspace list has loaded (wsIds empty).
  const liveAgents = agents.filter(
    (a) =>
      a.killed_at == null &&
      a.shim_exit == null &&
      (wsIds.size === 0 || a.workspace_id == null || wsIds.has(a.workspace_id)),
  );

  return (
    <>
      <CommandDialog
        open={open}
        onOpenChange={setOpen}
        title={t("cmdk.placeholder")}
        description={t("cmdk.kbd.openHint", {
          shortcut: commandPaletteShortcut,
        })}
      >
        <CommandInput placeholder={t("cmdk.placeholder")} />
        <CommandList>
          <CommandEmpty>{t("common.noMatch")}</CommandEmpty>

          {/* List-load failure banner. The agent + workspace groups come from a
              live fetch; on a 500 / dropped connection they'd otherwise just be
              absent (indistinguishable from "you have none"). Tell the user and
              offer a retry. Rendered outside cmdk's filtered item list so it
              never gets hidden by a search query. */}
          {loadError && (
            <div className="flex items-center gap-2 px-3 py-2 text-xs text-status-danger">
              <AlertTriangle className="size-3.5 shrink-0" />
              <span className="flex-1">
                {t("cmdk.listLoadFailed")}
              </span>
              <button
                type="button"
                onClick={() => loadData()}
                disabled={loading}
                className="flex items-center gap-1 rounded px-1.5 py-0.5 text-foreground-secondary transition-colors hover:bg-surface-tertiary disabled:opacity-60"
              >
                <RefreshCw className={cn("size-3", loading && "animate-spin")} />
                {t("common.retry", { defaultValue: "重试" })}
              </button>
            </div>
          )}

          <CommandGroup heading={t("cmdk.groups.frequent", "常用")}>
            <CommandItem
              value={`quick new workspace create ${t("cmdk.newWorkspace")} ${t("cmdk.openWizard")}`}
              onSelect={openWizard}
            >
              <Plus />
              <span>{t("cmdk.newWorkspace")}</span>
              <CommandShortcut>{t("cmdk.openWizard")}</CommandShortcut>
            </CommandItem>
            <CommandItem
              value={`quick chat workspace ${t("nav.chat")} ${t("cmdk.navHint.chat")}`}
              onSelect={() => go(currentWsId ? `/chat/${currentWsId}` : "/chat")}
            >
              <MessageSquare />
              <span>{t("nav.chat")}</span>
              <CommandShortcut>{t("cmdk.navHint.chat")}</CommandShortcut>
            </CommandItem>
            <CommandItem
              value={`quick mcp tools settings ${t("nav.mcp")} ${t("cmdk.navHint.mcp")}`}
              onSelect={() => go("/mcp")}
            >
              <Boxes />
              <span>{t("nav.mcp")}</span>
              <CommandShortcut>{t("cmdk.navHint.mcp")}</CommandShortcut>
            </CommandItem>
          </CommandGroup>

          <CommandGroup heading={t("cmdk.groups.nav")}>
          {NAV.map((n) => {
            const Icon = n.icon;
            return (
              <CommandItem
                key={n.href}
                value={`${n.href} ${t(n.labelKey)} ${t(n.hintKey)}`}
                onSelect={() => go(n.href)}
              >
                <Icon />
                <span>{t(n.labelKey)}</span>
                <CommandShortcut>{t(n.hintKey)}</CommandShortcut>
              </CommandItem>
            );
          })}
          </CommandGroup>

          {currentWsId && (
            <CommandGroup heading={t("cmdk.groups.currentWsView")}>
            {WORKSPACE_VIEWS.map((v, i) => {
              const Icon = v.icon;
              const href = `${directionBase(currentWsId, currentThreadSlug)}${v.suffix}`;
              return (
                <CommandItem
                  key={v.id}
                  value={`view ${v.id} ${t(v.labelKey)}`}
                  onSelect={() => go(href)}
                >
                  <Icon />
                  <span>{t(v.labelKey)}</span>
                  <CommandShortcut>
                    {formatShortcutChord(i + 1, platform)}
                  </CommandShortcut>
                </CommandItem>
              );
            })}
            </CommandGroup>
          )}

          {workspaces.length > 0 && (
            <CommandGroup heading={t("cmdk.groups.workspaces")}>
            {workspaces.map((ws) => (
              <CommandItem
                key={ws.id}
                value={`ws ${ws.name} ${ws.cwd}`}
                onSelect={() => go(`/chat/${ws.slug}`)}
              >
                <Activity />
                <span>{ws.name}</span>
                <CommandShortcut>{t("cmdk.switchWs")}</CommandShortcut>
              </CommandItem>
            ))}
            </CommandGroup>
          )}

          {liveAgents.length > 0 && (
            <CommandGroup heading={t("cmdk.groups.wakeAgent")}>
            {liveAgents.map((a) => (
              <CommandItem
                key={a.agent_id}
                value={`wake ${t("cmdk.groups.wakeAgent")} ${t("cmdk.wake", { role: a.role })} ${a.role} ${a.agent_id}`}
                onSelect={() => {
                  close();
                  setConfirm({
                    title: t("cmdk.confirmWakeTitle", {
                      role: a.role,
                      defaultValue: "手动催一下 {{role}}？",
                    }),
                    description: t("cmdk.confirmWakeDesc", {
                      id: a.agent_id,
                      defaultValue:
                        "一般不用点——卡住时系统会自动催。只有它还在睡、自动催没用时再用。",
                    }),
                    confirmLabel: t("cmdk.confirmWake", "催一下"),
                    onConfirm: async () => {
                      try {
                        await api.wakeAgent(a.agent_id);
                        toast.success(
                          t("cmdk.wakeOk", {
                            role: a.role,
                            defaultValue: "已催促 {{role}}",
                          }),
                        );
                      } catch (e) {
                        toast.error(
                          t("cmdk.wakeFailed", {
                            defaultValue: "催促失败",
                          }),
                          { description: (e as Error)?.message },
                        );
                      }
                    },
                  });
                }}
              >
                <Zap className="text-state-wake" />
                <span>{t("cmdk.wake", { role: a.role })}</span>
                <CommandShortcut>{a.agent_id}</CommandShortcut>
              </CommandItem>
            ))}
            </CommandGroup>
          )}

          <CommandGroup heading={t("cmdk.groups.theme")}>
          <CommandItem
            value={`theme light ${t("cmdk.switchToLight")}`}
            onSelect={() => {
              persistTheme("light");
              close();
            }}
          >
            <Sun />
            <span>{t("cmdk.switchToLight")}</span>
            <CommandShortcut>light</CommandShortcut>
          </CommandItem>
          <CommandItem
            value={`theme dark ${t("cmdk.switchToDark")}`}
            onSelect={() => {
              persistTheme("dark");
              close();
            }}
          >
            <Moon />
            <span>{t("cmdk.switchToDark")}</span>
            <CommandShortcut>dark</CommandShortcut>
          </CommandItem>
          <CommandItem
            value={`theme system ${t("cmdk.followSystem")}`}
            onSelect={() => {
              persistTheme("system");
              close();
            }}
          >
            <SunMoon />
            <span>{t("cmdk.followSystem")}</span>
            <CommandShortcut>system</CommandShortcut>
          </CommandItem>
          </CommandGroup>

          <CommandGroup heading={t("cmdk.groups.settings")}>
          {SETTINGS_SECTIONS.map((s, i) => {
            const label = t(s.labelKey);
            // cmdk 只看 `value` 过滤，把英文 id + 翻译后的 label 都塞进
            // value，这样输 "隐私" 或 "privacy" 都能命中。
            return (
              <CommandItem
                key={s.id}
                value={`settings ${s.id} ${label}`}
                onSelect={() => go(`/settings/${s.id}`)}
              >
                <SettingsIcon />
                <span>{label}</span>
                <CommandShortcut>
                  {formatShortcutChord(i + 1, platform)}
                </CommandShortcut>
              </CommandItem>
            );
          })}
          </CommandGroup>
        </CommandList>
      </CommandDialog>
      <ConfirmActionDialog
        action={confirm}
        onOpenChange={(next) => {
          if (!next) setConfirm(null);
        }}
      />
    </>
  );
}

// 同 Settings 持久化主题的写入路径，避免 palette 和 Settings UI 漂移。
function persistTheme(mode: ThemeMode) {
  const KEY = "swarmx:settings:v1";
  try {
    const raw = window.localStorage.getItem(KEY);
    const obj = raw ? JSON.parse(raw) : {};
    obj.theme = mode;
    window.localStorage.setItem(KEY, JSON.stringify(obj));
  } catch {
    /* ignore */
  }
  setTheme(mode);
}
