/**
 * AgentDrawer — Pencil frame t1JPL (540×920 right-side drawer).
 *
 * Anatomy:
 *   Header           HeaderTopRow (avatar + role + close)
 *                    ActionRow    (focus injector / wake / pause / restart / more)
 *   TabBar           终端 · 录像 · 消息 · 工具 · 上下文
 *   MainContent      one of the five tab panels
 *   InjectBar        prompt input + Send  (writes to /api/message as system→agent)
 *   StatBar          SPAWN · TURN · TOKEN · TOOLS · PTY · HOOK
 *
 * Tab caveats:
 *   - Terminal      : XtermPane via the existing WebGL pool; `visible` follows
 *                     the active tab so other panes can claim a GL context.
 *   - Recordings    : list of casts for this agent; cards navigate to /replays/:id
 *                     (never embedded — 540px is too narrow, see commit 005defc).
 *   - Messages      : mini message list filtered to from/to = this agent. Composer
 *                     intentionally NOT duplicated here — use InjectBar.
 *   - Tools         : placeholder. SwarmEvent has no tool_call type yet; needs a
 *                     shim-level emit. Hidden behind a soft "WIP" plate.
 *   - Context       : blackboard entries this agent wrote; click jumps to /context.
 *
 * Lifecycle:
 *   - Mount when ?agent=<id> appears in the route URL, unmount when removed.
 *   - We do NOT keep XtermPane mounted across opens — sessionStorage-backed
 *     `lastSeq` lets it reconnect with a gap-replay, so reopening is cheap
 *     enough. Keeping it mounted would hold a WebGL slot indefinitely.
 *
 * Inject semantics:
 *   The Pencil mock labels this "注入 prompt", but PTY input is owned exclusively
 *   by XtermPane. Sending the text as a swarm message (kind=note, from=system,
 *   to=agent_id) wakes the agent the same way an in-band human comment would —
 *   close enough to the design's intent without forking a new PTY-write path.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import {
  Activity,
  ChevronDown,
  ChevronRight,
  Code2,
  Download,
  FileText,
  Loader2,
  MessageSquare,
  Pause,
  Play,
  Terminal as TerminalIcon,
  Zap,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { api, ApiError } from "../../api/http";
import { downloadRecordingCast } from "@/lib/download";
import type {
  AgentActivity,
  AgentInfo,
  BlackboardEntry,
  MessageRecord,
  RecordingInfo,
  SwarmEvent,
  Workspace,
} from "../../api/types";
import { XtermPane } from "../XtermPane";
import { AgentActivityLog } from "./AgentActivityLog";
import { useSwarmFeed } from "../../hooks/useSwarmFeed";
import { Button } from "@/components/ui/button";
import {
  ConfirmActionDialog,
  type ConfirmActionState,
} from "@/components/ConfirmActionDialog";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/cn";
import { toast } from "@/lib/toast";
import { roleColorClass as roleColor, roleDisplayName } from "@/lib/agent";
import { directionBase, directionSlugFromKey } from "@/lib/thread";

type TabId = "terminal" | "activity" | "recordings" | "messages" | "context";

// The "工具/Tools" placeholder tab is now the real "活动/Activity" tab —
// `agent_activity` swarm events (server tails the CLI session JSONL) give it a
// live step-level data source. It streams the current round's tool/system
// steps and folds finished rounds into a one-line summary.
const TABS: { id: TabId; labelKey: string; icon: typeof TerminalIcon }[] = [
  { id: "activity", labelKey: "agent.tabs.activity", icon: Activity },
  { id: "terminal", labelKey: "agent.tabs.terminal", icon: TerminalIcon },
  { id: "messages", labelKey: "agent.tabs.messages", icon: MessageSquare },
];
// 低频深查 tab(录像/上下文)收进「更多」菜单 — 80% 的打开是为了「它现在
// 怎么样了 / 我该回什么」(活动/消息/终端),低频项不再平权占决策成本。
// tab 内容本身不受影响,「更多」里选中后照常全宽渲染。
const OVERFLOW_TABS: { id: TabId; labelKey: string; icon: typeof TerminalIcon }[] = [
  { id: "recordings", labelKey: "agent.tabs.recordings", icon: Play },
  { id: "context", labelKey: "agent.tabs.context", icon: FileText },
];
const ALL_TABS = [...TABS, ...OVERFLOW_TABS];

function formatDelta(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  const rem = s - m * 60;
  if (m < 60) return `${m}m${rem}s`;
  const h = Math.floor(m / 60);
  return `${h}h${m - h * 60}m`;
}

function formatTime(ms: number): string {
  return new Date(ms).toLocaleTimeString();
}

interface Props {
  agentId: string;
  /** Persistent per-agent activity stream (from useWorkspaceShellData) so the
   *  Activity tab survives reopen/remount instead of re-subscribing fresh. */
  activities: AgentActivity[];
  onClose: () => void;
}

export function AgentDrawer({ agentId, activities, onClose }: Props) {
  const { t } = useTranslation();
  const [info, setInfo] = useState<AgentInfo | null>(null);
  const [infoResolved, setInfoResolved] = useState(false);
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [confirm, setConfirm] = useState<ConfirmActionState | null>(null);
  // Tab lives in the URL (?tab=…) so it survives a drawer remount (e.g. a
  // background agents refresh) instead of snapping back to "terminal", and so
  // it deep-links. Defaults to terminal when the param is absent/unknown.
  const [searchParams, setSearchParams] = useSearchParams();
  const liveAgent =
    infoResolved && !!info && info.killed_at == null && info.shim_exit == null;
  const explicitTab: TabId | null = ALL_TABS.some((x) => x.id === searchParams.get("tab"))
    ? (searchParams.get("tab") as TabId)
    : null;
  // F6: a killed agent's terminal is a dead PTY (the WS connect just errors), so
  // clicking its avatar in chat used to open an empty/error drawer. Default a
  // dead agent to its recordings — the richest "what did this agent do" view —
  // instead. An explicit ?tab= always wins (deep-link / user pick).
  //
  // reasonix runs `reasonix serve` (headless HTTP+SSE) — it has NO TUI, so its
  // PTY only carries serve's startup banner and there's nothing useful in the
  // terminal/recordings. Default reasonix (live OR dead) to the Activity view,
  // which is fed from its SSE tool/usage events.
  //
  // 2026-07 UX(A4): live agents default to ACTIVITY, not terminal — 80% of
  // drawer opens are "它现在怎么样了 / 我该回什么",and the structured tool
  // timeline answers that faster than a raw TUI. Terminal stays one tab away.
  const defaultTab: TabId =
    !infoResolved
      ? "activity"
      : info?.cli === "reasonix"
        ? "activity"
        : liveAgent
          ? "activity"
          : "recordings";
  const tab: TabId = explicitTab ?? defaultTab;
  // Drop the param only for the section's natural default so the URL stays
  // clean there — and crucially so clicking a NON-default tab (e.g. 终端 on a
  // dead agent) actually sticks instead of bouncing back to the default.
  const setTab = (next: TabId) =>
    setSearchParams(
      (prev) => {
        const p = new URLSearchParams(prev);
        if (next === defaultTab) p.delete("tab");
        else p.set("tab", next);
        return p;
      },
      { replace: true },
    );
  const [now, setNow] = useState(Date.now());
  // Cold-start backfill for the Activity tab. The live `activities` prop only
  // holds steps that streamed in AFTER the WS subscription/this shell mount —
  // so opening the drawer on an agent that already worked showed "暂无活动"
  // even though it has history (visible in its terminal). Pull the tailer's
  // ring once per agent; it shares the live stream's `seq` space, so we merge.
  const [backfill, setBackfill] = useState<AgentActivity[]>([]);
  useEffect(() => {
    let alive = true;
    setBackfill([]);
    api
      .getAgentActivity(agentId)
      .then((rows) => {
        if (alive) setBackfill(rows);
      })
      // L2: don't swallow silently — a failed backfill makes the activity tab
      // look like history only started when the drawer opened. At least log it.
      .catch((e) => {
        // eslint-disable-next-line no-console
        console.warn(`[swarmx] agent activity backfill failed (${agentId})`, e);
      });
    return () => {
      alive = false;
    };
  }, [agentId]);
  // Merge backfill + live by `seq` (live wins — it's the freshest phase for an
  // in-flight step). Sorted ascending: AgentActivityLog renders newest at the
  // bottom and auto-scrolls there.
  const mergedActivities = useMemo(() => {
    const bySeq = new Map<number, AgentActivity>();
    for (const a of backfill) bySeq.set(a.seq, a);
    for (const a of activities) bySeq.set(a.seq, a);
    return [...bySeq.values()].sort((x, y) => x.seq - y.seq);
  }, [backfill, activities]);

  const refreshInfo = async () => {
    try {
      const rows = await api.listAgents();
      const found = rows.find((a) => a.agent_id === agentId) ?? null;
      setInfo(found);
    } catch {
      /* best-effort */
    } finally {
      setInfoResolved(true);
    }
  };

  useEffect(() => {
    setInfo(null);
    setInfoResolved(false);
    refreshInfo();
    // Canonical workspace list — needed to map the agent's workspace_id (a
    // full FK id) to the workspace's URL slug for Recordings/Context links.
    api.listWorkspaces().then(setWorkspaces).catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentId]);

  // Tick once a second so SPAWN ago + uptime feel alive without re-rendering
  // the children every event.
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);

  // Esc 关闭由 Sheet 自身处理（Radix Dialog primitive 内部监听）。

  useSwarmFeed({
    onEvent: (ev: SwarmEvent) => {
      if (ev.type === "agent_state" && ev.agent_id === agentId) {
        refreshInfo();
      }
    },
  });

  const wake = () =>
    setConfirm({
      title: t("agent.confirm.wake.title", {
        role: info?.role ?? agentId.slice(0, 8),
        defaultValue: "唤醒 agent？",
      }),
      description: t("agent.confirm.wake.desc", {
        defaultValue:
          "一般不用点——卡住时系统会自动催。只有它还在睡、自动催没用时再用。",
      }),
      confirmLabel: t("agent.wake"),
      onConfirm: async () => {
        try {
          await api.wakeAgent(agentId);
          toast.success(
            t("agent.wakeOk", {
              role: info?.role ?? agentId.slice(0, 8),
              defaultValue: "已催促 {{role}}",
            }),
          );
        } catch (e) {
          toast.error(
            t("agent.wakeFailed", { defaultValue: "催促失败" }),
            { description: e instanceof ApiError ? e.detail : (e as Error)?.message },
          );
        }
      },
    });
  // Pause toggles between interrupt (active → paused) and resume
  // (paused → active). Both refresh the local AgentInfo so the button
  // label flips immediately without waiting for the swarm feed.
  const togglePause = async () => {
    const wasPaused = !!info?.paused;
    try {
      if (wasPaused) {
        await api.resumeAgent(agentId);
      } else {
        await api.interruptAgent(agentId);
      }
      await refreshInfo();
    } catch (e) {
      toast.error(
        wasPaused
          ? t("agent.resumeFailed", { defaultValue: "无法继续" })
          : t("agent.pauseFailed", { defaultValue: "无法暂停" }),
        { description: e instanceof ApiError ? e.detail : (e as Error)?.message },
      );
    }
  };
  const requestTogglePause = () => {
    const paused = !!info?.paused;
    setConfirm({
      title: paused
        ? t("agent.confirm.resume.title", {
            role: info?.role ?? agentId.slice(0, 8),
            defaultValue: "让 {{role}} 继续？",
          })
        : t("agent.confirm.pause.title", {
            role: info?.role ?? agentId.slice(0, 8),
            defaultValue: "暂停 {{role}}？",
          }),
      description: paused
        ? t("agent.confirm.resume.desc", {
            defaultValue: "会恢复自动催促，并让它马上接着干。",
          })
        : t("agent.confirm.pause.desc", {
            defaultValue:
              "会打断它当前这一轮，并暂时不再自动催它，直到你点「继续」。",
          }),
      confirmLabel: paused ? t("agent.resume") : t("agent.pause"),
      variant: paused ? "default" : "destructive",
      onConfirm: togglePause,
    });
  };

  // The URL slug (≠ id, and NOT the id's first 8 chars — slug is generated
  // independently) is what /chat/:wsId routing expects. Map the agent's
  // workspace_id FK to its workspace's slug via the canonical workspace list,
  // so Recordings/Context links land on the right workspace instead of a
  // garbled slice of the id/path.
  const wsSlug = info?.workspace_id
    ? (workspaces.find((w) => w.id === info.workspace_id)?.slug ?? null)
    : null;

  return (
    <Sheet
      open
      modal={false}
      onOpenChange={(next) => {
        if (!next) onClose();
      }}
    >
      <SheetContent
        side="right"
        // Radix SheetContent ships `data-[side=right]:sm:max-w-sm` (384px) — a
        // data-attribute variant that out-specifies a plain `sm:max-w-[…]`, so
        // a wider override MUST carry the same `data-[side=right]:` prefix to
        // win. 880px fits claude's 120-col TUI (the PTY/recording width) so the
        // terminal stops wrapping into garbage in the drawer.
        className="flex w-[880px] max-w-[94vw] flex-col gap-0 border-l border-border-subtle bg-surface-elevated p-0 shadow-xl data-[side=right]:w-[880px] data-[side=right]:sm:max-w-[880px]"
        // modal=false 时 Radix 不渲染 overlay，但 SheetContent 默认 onInteractOutside
        // 会关 sheet — 我们希望用户点 chat 列表切 agent 不关 drawer，drawer
        // 跟着切新 agent_id 重新 mount 就行。
        onInteractOutside={(e) => e.preventDefault()}
        onPointerDownOutside={(e) => e.preventDefault()}
      >
        <SheetHeader className="sr-only">
          <SheetTitle>{`Agent drawer · ${info?.role ?? agentId}`}</SheetTitle>
          <SheetDescription>
            {t("agent.sheetDesc", {
              id: agentId,
              defaultValue: "PTY 终端 / 录像 / 消息 / 上下文 for agent {{id}}",
            })}
          </SheetDescription>
        </SheetHeader>
        <Header
          info={info}
          agentId={agentId}
          now={now}
          onWake={wake}
          onTogglePause={requestTogglePause}
        />
        <TabBar tab={tab} onChange={setTab} />
        <div className="min-h-0 flex-1 overflow-hidden">
          {/* Mount all tabs concurrently is tempting (keeps terminal alive
              across tab switches), but xterm holds a WebGL slot and a WS
              so we'd burn budget on idle panes. Switch-unmount instead. */}
          {tab === "terminal" && (
            <TerminalTab agentId={agentId} live={liveAgent} loading={!infoResolved} />
          )}
          {tab === "activity" && (
            <AgentActivityLog activities={mergedActivities} />
          )}
          {tab === "recordings" && (
            <RecordingsTab agentId={agentId} wsId={wsSlug} />
          )}
          {tab === "messages" && <MessagesTab agentId={agentId} />}
          {tab === "context" && (
            <ContextTab agentId={agentId} wsId={wsSlug} />
          )}
        </div>
        <StatBar info={info} now={now} />
        <ConfirmActionDialog
          action={confirm}
          onOpenChange={(next) => {
            if (!next) setConfirm(null);
          }}
        />
      </SheetContent>
    </Sheet>
  );
}

// ── Header ───────────────────────────────────────────────────────────────

function Header({
  info,
  agentId,
  now,
  onWake,
  onTogglePause,
}: {
  info: AgentInfo | null;
  agentId: string;
  now: number;
  onWake: () => void;
  onTogglePause: () => void;
}) {
  const { t } = useTranslation();
  const role = info?.role ?? "—";
  const cli = info?.cli ?? "—";
  const initial = role.slice(0, 1).toUpperCase();
  const spawnedAt = info?.spawned_at ?? null;
  const live = info && info.killed_at == null && info.shim_exit == null;
  // M3: "alive" (PTY up) is NOT "working". An agent flagged by the server's
  // HealthScanner / first-response watchdog (last_error set) is alive but
  // wedged — paint it red "出错", not a green "Ready" with a working spinner.
  const errored = !!info?.last_error;
  const dotColor = !info
    ? "bg-state-idle"
    : !live
      ? "bg-state-idle"
      : errored
        ? "bg-status-danger"
        : info.shim_ready
          ? "bg-state-success"
          : "bg-state-wake";

  return (
    <header className="flex shrink-0 flex-col border-b border-border-subtle">
      {/* HeaderTopRow */}
      <div className="flex items-center gap-3 px-5 py-4">
        <div className="relative size-10 shrink-0">
          <div
            className={cn(
              "flex size-10 items-center justify-center rounded-full font-heading text-base font-bold text-foreground-on-accent",
              roleColor(role),
            )}
          >
            {initial}
          </div>
          <span
            className={cn(
              "absolute right-0 bottom-0 size-3 rounded-full ring-2 ring-surface-elevated",
              dotColor,
            )}
          />
        </div>
        <div className="flex min-w-0 flex-1 flex-col gap-1">
          <div className="flex items-center gap-2">
            <span className="font-heading text-base font-bold text-foreground-primary">
              {roleDisplayName(role)}
            </span>
            <span className="rounded bg-surface-cool-tint px-1.5 py-0.5 font-mono text-[10px] text-foreground-secondary">
              {cli}
            </span>
            <span className="truncate font-mono text-[10px] text-foreground-tertiary">
              {agentId}
            </span>
          </div>
          <div className="flex items-center gap-1.5 font-caption text-[11px] text-foreground-secondary">
            {live && !errored && (
              <Loader2 className="size-3 animate-spin text-accent-primary" />
            )}
            <span>
              {!live
                ? t("agent.status.exited")
                : errored
                  ? t("agent.status.error", { defaultValue: "出错·无法工作" })
                  : info?.shim_ready
                    ? t("agent.status.ready")
                    : t("agent.status.starting")}
              {/* 死后别用 now-spawned 继续涨——小白会以为它还在跑 */}
              {spawnedAt && live && (
                <> · {t("agent.status.uptime", { delta: formatDelta(now - spawnedAt) })}</>
              )}
              {spawnedAt && !live && info?.killed_at != null && (
                <>
                  {" "}
                  ·{" "}
                  {t("agent.status.lived", {
                    delta: formatDelta(info.killed_at - spawnedAt),
                    defaultValue: "存活 {{delta}}",
                  })}
                </>
              )}
            </span>
          </div>
        </div>
      </div>

      {/* ActionRow — 之前有「发送消息」按钮指向 drawer 自带的 InjectBar，
          但 chat 主 composer 上的 recipient picker 已经能选这个 agent
          发消息（同一 mailbox 路径，行为完全等价），两套 prompt-inject UI
          是重复，drawer 这边删了。Wake / Pause / Restart 保留，是 agent
          独有的操作。 */}
      <div className="flex items-center gap-2 px-5 pt-1 pb-4">
        {/* Wake is an escape hatch — auto-nudge handles stalls. Ghost + soft
            label so the drawer doesn't look like an ops console. Hidden when
            the agent is already exited: 催一个死人只会让小白更懵. */}
        {live && (
          <Button
            size="sm"
            variant="ghost"
            className="text-foreground-tertiary"
            onClick={onWake}
            title={t("agent.confirm.wake.desc")}
          >
            <Zap className="size-3" />
            {t("agent.wake")}
          </Button>
        )}
        {live && (
          <Button
            size="sm"
            variant="outline"
            onClick={onTogglePause}
            title={info?.paused ? t("agent.resume") : t("agent.pause")}
          >
            {info?.paused ? (
              <>
                <Play className="size-3" />
                {t("agent.resume")}
              </>
            ) : (
              <>
                <Pause className="size-3" />
                {t("agent.pause")}
              </>
            )}
          </Button>
        )}
        {/* "重启" 按钮删了 —— 它一直 disabled 标"暂未实现"，给用户露出一个
            按不动的死按钮。等真支持重启再加回来。 */}
      </div>
    </header>
  );
}

// ── TabBar ───────────────────────────────────────────────────────────────

function TabBar({ tab, onChange }: { tab: TabId; onChange: (t: TabId) => void }) {
  const { t: tr } = useTranslation();
  const overflowActive = OVERFLOW_TABS.find((x) => x.id === tab);
  return (
    <Tabs
      value={tab}
      onValueChange={(v) => onChange(v as TabId)}
      className="shrink-0"
    >
      <TabsList className="h-auto w-full justify-start overflow-x-auto rounded-none border-b border-border-subtle bg-transparent p-0 px-5">
        {TABS.map((item) => {
          const Icon = item.icon;
          return (
            <TabsTrigger
              key={item.id}
              value={item.id}
              className="relative gap-1.5 rounded-none border-0 bg-transparent px-4 py-3 text-xs text-foreground-secondary shadow-none hover:text-foreground-primary data-[state=active]:bg-transparent data-[state=active]:text-foreground-primary data-[state=active]:shadow-none data-[state=active]:after:absolute data-[state=active]:after:inset-x-0 data-[state=active]:after:-bottom-px data-[state=active]:after:h-0.5 data-[state=active]:after:bg-accent-primary"
            >
              <Icon className="size-3.5" />
              {tr(item.labelKey)}
            </TabsTrigger>
          );
        })}
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <button
              type="button"
              className={cn(
                "relative flex items-center gap-1.5 rounded-none border-0 bg-transparent px-4 py-3 text-xs shadow-none hover:text-foreground-primary",
                overflowActive
                  ? "text-foreground-primary after:absolute after:inset-x-0 after:-bottom-px after:h-0.5 after:bg-accent-primary"
                  : "text-foreground-secondary",
              )}
            >
              {overflowActive ? (
                <>
                  <overflowActive.icon className="size-3.5" />
                  {tr(overflowActive.labelKey)}
                </>
              ) : (
                tr("agent.tabs.more")
              )}
              <ChevronDown className="size-3 opacity-60" />
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start" sideOffset={4}>
            {OVERFLOW_TABS.map((item) => {
              const Icon = item.icon;
              return (
                <DropdownMenuItem key={item.id} onSelect={() => onChange(item.id)}>
                  <Icon className="size-3.5" />
                  {tr(item.labelKey)}
                </DropdownMenuItem>
              );
            })}
          </DropdownMenuContent>
        </DropdownMenu>
      </TabsList>
    </Tabs>
  );
}

// ── Tab: Terminal ────────────────────────────────────────────────────────

function TerminalTab({
  agentId,
  live,
  loading,
}: {
  agentId: string;
  live: boolean;
  loading: boolean;
}) {
  const { t } = useTranslation();
  if (loading) {
    return (
      <div className="flex h-full items-center justify-center bg-surface-inverse p-6">
        <p className="max-w-xs text-center font-caption text-sm text-foreground-inverse-secondary">
          {t("agent.terminalLoading")}
        </p>
      </div>
    );
  }
  // A killed agent has no PTY — connecting the xterm WS would just error. Show a
  // plain note pointing at the historical tabs instead of a dead/error terminal.
  if (!live) {
    return (
      <div className="flex h-full items-center justify-center bg-surface-inverse p-6">
        <p className="max-w-xs text-center font-caption text-sm text-foreground-inverse-secondary">
          {t("agent.terminalExited")}
        </p>
      </div>
    );
  }
  return (
    <div className="h-full bg-surface-inverse">
      <XtermPane agentId={agentId} visible />
    </div>
  );
}

// ── Tab: Recordings ──────────────────────────────────────────────────────

function RecordingsTab({
  agentId,
  wsId,
}: {
  agentId: string;
  wsId: string | null;
}) {
  const { t } = useTranslation();
  const [items, setItems] = useState<RecordingInfo[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .listRecordings(agentId)
      .then((rows) => {
        if (!cancelled) setItems(rows);
      })
      .catch((e) => {
        if (!cancelled) setError((e as Error).message);
      });
    return () => {
      cancelled = true;
    };
  }, [agentId]);

  if (error) {
    return (
      <div className="p-5 text-xs text-state-danger">{error}</div>
    );
  }
  if (items.length === 0) {
    return (
      <div className="flex h-full items-center justify-center text-foreground-tertiary">
        <span className="font-caption text-sm">{t("agent.noRecordings")}</span>
      </div>
    );
  }
  return (
    <div className="flex h-full flex-col gap-2 overflow-y-auto p-4">
      {items.map((r) => {
        const live = r.finalized_at == null;
        return (
          <div
            key={r.id}
            className="group flex items-center gap-2 rounded-md border border-border-subtle bg-surface-primary p-2.5 hover:border-accent-primary sm:gap-3 sm:p-3"
          >
            <Link
              // 没拿到 wsId（agent 无 workspace 或还在加载）就退回 chat 主页 —
              // 录像本身没法在 Shell 外播放，给个能登陆的入口比 404 强。
              to={
                wsId
                  ? `/chat/${wsId}/replays/${encodeURIComponent(r.id)}`
                  : "/chat"
              }
              className="flex min-w-0 flex-1 items-center gap-2 rounded-md focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent-primary sm:gap-3"
              title={r.id}
            >
              <span className="hidden size-9 shrink-0 items-center justify-center rounded-md bg-term-bg text-term-green sm:flex">
                <Play className="size-4" />
              </span>
              <div className="min-w-0 flex-1">
                <div className="truncate font-mono text-xs text-foreground-primary">
                  {r.id}
                </div>
                <div className="font-caption text-[11px] text-foreground-tertiary">
                  {r.cols}×{r.rows}
                  {r.duration_ms != null && (
                    <> · {formatDelta(r.duration_ms)}</>
                  )}{" "}
                  · {formatTime(r.started_at)}
                </div>
              </div>
              <span
                className={cn(
                  "hidden rounded-full px-2 py-0.5 font-caption text-[10px] sm:inline-flex",
                  live
                    ? "bg-status-running-soft text-status-running"
                    : "bg-surface-tertiary text-foreground-tertiary",
                )}
              >
                {live ? t("replays.live") : t("replays.completed")}
              </span>
              <ChevronRight className="hidden size-4 shrink-0 text-foreground-tertiary sm:block" />
            </Link>
            <button
              type="button"
              onClick={() => downloadRecordingCast(r.id)}
              className="flex size-7 shrink-0 items-center justify-center rounded-md text-foreground-tertiary hover:bg-surface-tertiary"
              aria-label={t("agent.downloadCastNamed", {
                id: r.id,
                defaultValue: "下载 {{id}} .cast",
              })}
              title={t("agent.downloadCast")}
            >
              <Download className="size-3.5" />
            </button>
          </div>
        );
      })}
    </div>
  );
}

// ── Tab: Messages ────────────────────────────────────────────────────────

function MessagesTab({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const [items, setItems] = useState<MessageRecord[]>([]);
  const [error, setError] = useState<string | null>(null);
  // Stale-response guard: each refresh captures the req id live at call time,
  // and only the latest one is allowed to setState. Without it, a slow pull for
  // a previous agent could land after a fast pull for the current one and show
  // the wrong agent's messages when the user clicks between agents quickly.
  const reqIdRef = useRef(0);
  // Debounce timer for swarm-triggered refreshes. Each inbound `message` event
  // would otherwise fire TWO list requests (from + to) immediately — a burst of
  // chatter to/from this agent meant a request storm. Coalesce into one refresh
  // per quiet window.
  const debounceRef = useRef<number | null>(null);

  const refresh = useCallback(async () => {
    const reqId = ++reqIdRef.current;
    try {
      // Two pulls, then merge in client. The server's listMessages doesn't
      // support "from=X OR to=X" in one query, so we cheat with two calls.
      // Volume is small per-agent; if this becomes hot, add a server filter.
      const [from, to] = await Promise.all([
        api.listMessages({ from: agentId, limit: 100 }),
        api.listMessages({ to: agentId, limit: 100 }),
      ]);
      if (reqId !== reqIdRef.current) return; // a newer refresh superseded us
      const merged = [...from, ...to];
      const seen = new Set<number>();
      const dedup: MessageRecord[] = [];
      for (const m of merged) {
        if (seen.has(m.id)) continue;
        seen.add(m.id);
        dedup.push(m);
      }
      dedup.sort((a, b) => a.sent_at - b.sent_at);
      setItems(dedup);
      setError(null);
    } catch (e) {
      if (reqId !== reqIdRef.current) return;
      setError(e instanceof ApiError ? e.detail : (e as Error).message);
    }
  }, [agentId]);

  useEffect(() => {
    refresh();
    // Cancel any pending debounced refresh when the agent changes/unmounts so a
    // stale timer doesn't fire a pull for the previous agent.
    return () => {
      if (debounceRef.current != null) {
        window.clearTimeout(debounceRef.current);
        debounceRef.current = null;
      }
    };
  }, [agentId, refresh]);

  useSwarmFeed({
    onEvent: (ev: SwarmEvent) => {
      if (ev.type !== "message") return;
      if (ev.from_agent !== agentId && ev.to_agent !== agentId) return;
      // 200ms de-bounce: collapse a burst of message events into one refresh.
      if (debounceRef.current != null) window.clearTimeout(debounceRef.current);
      debounceRef.current = window.setTimeout(() => {
        debounceRef.current = null;
        refresh();
      }, 200);
    },
  });

  if (error) {
    return <div className="p-5 text-xs text-state-danger">{error}</div>;
  }
  if (items.length === 0) {
    return (
      <div className="flex h-full items-center justify-center text-foreground-tertiary">
        <span className="font-caption text-sm">{t("agent.noMessages")}</span>
      </div>
    );
  }
  return (
    <div className="flex h-full flex-col gap-2 overflow-y-auto p-4">
      {items.map((m) => {
        const outgoing = m.from_agent === agentId;
        return (
          <div
            key={m.id}
            className={cn(
              "rounded-md border p-3 text-xs",
              outgoing
                ? "border-accent-primary-soft bg-surface-accent-tint"
                : "border-border-subtle bg-surface-primary",
            )}
          >
            <div className="mb-1 flex items-center gap-2 font-caption text-[10px] text-foreground-tertiary">
              <span className="font-mono">#{m.id}</span>
              <span className="font-mono">{m.from_agent}</span>
              <ChevronRight className="size-3" />
              <span className="font-mono">{m.to_agent}</span>
              <span className="ml-auto">{formatTime(m.sent_at)}</span>
            </div>
            <div className="whitespace-pre-wrap font-mono text-foreground-primary">
              {m.body}
            </div>
          </div>
        );
      })}
    </div>
  );
}

// ── Tab: Context ─────────────────────────────────────────────────────────

function ContextTab({
  agentId,
  wsId,
}: {
  agentId: string;
  wsId: string | null;
}) {
  const { t } = useTranslation();
  const [history, setHistory] = useState<
    { path: string; at: number; op: string }[]
  >([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const entries: BlackboardEntry[] = await api.listBlackboard();
        // Fan out history for each key, filter by this agent. Cheap on a
        // small board; if the board grows past hundreds of keys we'd want
        // an indexed-by-agent server endpoint.
        const histories = await Promise.all(
          entries.map(async (e) => {
            try {
              const rows = await api.listBlackboardHistory(e.path, 50, false);
              return rows
                .filter((h) => h.agent_id === agentId)
                .map((h) => ({ path: e.path, at: h.at, op: h.op }));
            } catch {
              return [];
            }
          }),
        );
        if (cancelled) return;
        const flat = histories.flat().sort((a, b) => b.at - a.at);
        setHistory(flat);
      } catch (e) {
        if (!cancelled) setError((e as Error).message);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [agentId]);

  if (error) {
    return <div className="p-5 text-xs text-state-danger">{error}</div>;
  }
  if (history.length === 0) {
    return (
      <div className="flex h-full items-center justify-center text-foreground-tertiary">
        <span className="font-caption text-sm">{t("agent.noBlackboard")}</span>
      </div>
    );
  }
  return (
    <div className="flex h-full flex-col gap-1 overflow-y-auto p-4">
      {history.map((h, i) => (
        <Link
          key={`${h.path}-${h.at}-${i}`}
          // 跳到这个 key 所属【方向】的 context view 并选中它 —— 方向 slug 从
          // key 自身 `{ws}/{slug}/…` 解析,所以 ledger 跳转落在正确方向而非 main;
          // wsId 缺失时回退到 chat 主页 (跳 /context 旧路径已死)。
          to={
            wsId
              ? `${directionBase(wsId, directionSlugFromKey(h.path))}/context?key=${encodeURIComponent(h.path)}`
              : "/chat"
          }
          className="flex items-center gap-2 rounded-md px-3 py-2 hover:bg-surface-tertiary"
        >
          <FileText className="size-4 shrink-0 text-foreground-tertiary" />
          <span className="flex-1 truncate font-mono text-xs text-foreground-primary">
            {h.path}
          </span>
          <span className="font-caption text-[10px] text-foreground-tertiary">
            {h.op} · {formatTime(h.at)}
          </span>
        </Link>
      ))}
    </div>
  );
}


// ── StatBar ──────────────────────────────────────────────────────────────

function StatBar({ info, now }: { info: AgentInfo | null; now: number }) {
  const { t } = useTranslation();
  const spawnedAt = info?.spawned_at ?? null;
  const live = info && info.killed_at == null && info.shim_exit == null;

  const stats = useMemo(
    () =>
      [
        {
          label: t("agent.stat.spawn"),
          value: spawnedAt
            ? t("agent.stat.ago", { delta: formatDelta(now - spawnedAt) })
            : "—",
        },
        // TURN / TOKEN / TOOLS slots were dropped — the server exposes no such
        // metrics yet, so they rendered a permanent "—" that read as broken.
        // Re-add once the shim emits per-turn telemetry.
        {
          label: t("agent.stat.pty"),
          value: live ? t("agent.stat.live") : t("agent.stat.off"),
          color: live ? "text-state-success" : "text-foreground-tertiary",
        },
        {
          label: t("agent.stat.hook"),
          // shim_ready 在死后仍可能为 true（退出前已就绪）——死人不能再显示「忙碌」
          value: !live
            ? t("agent.stat.off")
            : info?.shim_ready
              ? t("agent.stat.active")
              : info
                ? t("agent.stat.wait")
                : "—",
          color:
            live && info?.shim_ready
              ? "text-state-success"
              : "text-foreground-tertiary",
        },
      ] as const,
    [info, live, spawnedAt, now, t],
  );

  return (
    <footer className="flex shrink-0 items-center gap-5 border-t border-border-subtle bg-surface-secondary px-5 py-3">
      {stats.map((s) => (
        <div key={s.label} className="flex flex-col gap-0.5">
          <span className="font-caption text-[9px] tracking-[0.08em] text-foreground-tertiary">
            {s.label}
          </span>
          <span
            className={cn(
              "font-mono text-xs font-bold",
              "color" in s ? s.color : "text-foreground-primary",
            )}
          >
            {s.value}
          </span>
        </div>
      ))}
      <span className="ml-auto">
        <Code2 className="size-4 text-foreground-tertiary" />
      </span>
    </footer>
  );
}
