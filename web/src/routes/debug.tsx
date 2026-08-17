/**
 * Legacy M2 dashboard, now hosted at /debug.
 *
 * Layout invariants (intentional):
 *   - panes that are minimized or hidden behind a maximize use `display:none`
 *     and stay mounted, so the WS+PTY stays alive without reconnect cost.
 *     XtermPane's ResizeObserver no-ops while host is 0x0, then refits when
 *     visibility returns. Reconnecting would lose terminal scrollback and
 *     re-trigger the CLI's "Welcome" sequence — both jarring.
 *   - cols cap at 6: ~Math.ceil(sqrt(40))=7 already gives <250px panes on a
 *     1500px-wide window; beyond 6 the terminal grid becomes unreadable.
 *
 * Logic is identical to the pre-router App.tsx. The wrapper carries
 * `legacy-dashboard` so the dark scheme defined in global.css applies only
 * here, not to the new product surfaces.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Loader2 } from "lucide-react";
import { api, ApiError } from "../api/http";
import type { CliPluginInfo, SpawnAgentResponse } from "../api/types";
import { XtermPane } from "../components/XtermPane";
import { SwarmPanel } from "../components/SwarmPanel";
import { SpellsLauncher } from "../components/SpellsLauncher";
import { useSwarmRefresh } from "../hooks/useSwarmProjection";
import { toast } from "@/lib/toast";
import { notifySpawnFallbacks } from "../lib/engineFallback";
import {
  isTauriWindow,
  TAURI_DRAG_REGION,
  tauriLeftSafePadding,
} from "@/lib/tauriWindowChrome";

const MAX_COLS = 6;
const SWARM_PANEL_KEY = "swarmx:swarmPanelOpen";

// Module-level single-flight: two quick spawns (or a remount mid-create) must
// not race and create duplicate "debug-scratch" workspaces. The first caller's
// promise is shared; if it rejects we clear the cache so a later spawn retries.
let debugWorkspacePromise: Promise<string> | null = null;

// Step 3 of the workspace refactor makes spawnAgent require a workspace_id.
// The legacy /debug dashboard isn't workspace-aware, so we lazily ensure a
// "debug-scratch" workspace exists and pin every spawn to it. Production users
// go through the main UI's CreateWizard.
//
// cwd is fixed to "/tmp": the old `window.__SWARMX_HOME` was never injected,
// so this always resolved to "/tmp" anyway, and the backend requires cwd to be
// an existing directory ("/tmp" reliably is on macOS/Linux).
async function ensureDebugWorkspace(): Promise<string> {
  if (debugWorkspacePromise) return debugWorkspacePromise;
  debugWorkspacePromise = (async () => {
    const all = await api.listWorkspaces();
    const found = all.find((w) => w.name === "debug-scratch");
    if (found) return found.id;
    const created = await api.createWorkspace({
      name: "debug-scratch",
      cwd: "/tmp",
      accent: "peach",
    });
    return created.id;
  })();
  debugWorkspacePromise.catch(() => {
    // Allow a later spawn to retry after a transient failure.
    debugWorkspacePromise = null;
  });
  return debugWorkspacePromise;
}

export default function DebugRoute() {
  const { t } = useTranslation();
  const isTauri = isTauriWindow();
  const [plugins, setPlugins] = useState<CliPluginInfo[]>([]);
  const [pluginsError, setPluginsError] = useState<string | null>(null);
  const [agents, setAgents] = useState<SpawnAgentResponse[]>([]);
  const [spawning, setSpawning] = useState(false);
  const [maximized, setMaximized] = useState<string | null>(null);
  const [minimized, setMinimized] = useState<Set<string>>(new Set());
  const [killing, setKilling] = useState<Set<string>>(new Set());
  const [swarmOpen, setSwarmOpen] = useState<boolean>(() => {
    try {
      return window.localStorage.getItem(SWARM_PANEL_KEY) === "1";
    } catch {
      return false;
    }
  });

  useEffect(() => {
    try {
      window.localStorage.setItem(SWARM_PANEL_KEY, swarmOpen ? "1" : "0");
    } catch {
      // ignore
    }
  }, [swarmOpen]);

  const refreshAgents = async () => {
    try {
      const items = await api.listAgents();
      const live = items.filter(
        (a) => a.killed_at == null && a.shim_exit == null,
      );
      setAgents(
        live.map((a) => ({
          agent_id: a.agent_id,
          cli: a.cli,
          role: a.role,
          workspace: a.workspace,
        })),
      );
    } catch (err) {
      // eslint-disable-next-line no-console
      console.warn("listAgents failed", err);
    }
  };

  useEffect(() => {
    api
      .listPlugins()
      .then(setPlugins)
      .catch((err: Error) => setPluginsError(err.message));
    refreshAgents();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const refreshTimerRef = useRef<number | null>(null);
  const scheduleRefresh = useCallback(() => {
    if (refreshTimerRef.current != null) {
      window.clearTimeout(refreshTimerRef.current);
    }
    refreshTimerRef.current = window.setTimeout(() => {
      refreshTimerRef.current = null;
      refreshAgents();
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, 200);
  }, []);
  useSwarmRefresh(
    (s) => `${s.rosterGen}:${s.reconnectGen}`,
    scheduleRefresh,
  );

  const spawn = async (cli: string) => {
    setSpawning(true);
    try {
      const workspace_id = await ensureDebugWorkspace();
      const agent = await api.spawnAgent({ cli, workspace_id });
      // Engine fallback is never silent (billing red line) — see CreateWizard.
      notifySpawnFallbacks(t, [agent]);
      // De-dup against the WS-driven refreshAgents that may have already
      // inserted this agent_id (agent_state event lands before this resolves).
      setAgents((prev) =>
        prev.some((a) => a.agent_id === agent.agent_id)
          ? prev
          : [...prev, agent],
      );
    } catch (err) {
      toast.error(
        t("debug.spawnFailed", { defaultValue: "启动失败" }),
        {
          description:
            err instanceof ApiError ? err.detail : (err as Error).message,
        },
      );
    } finally {
      setSpawning(false);
    }
  };

  const kill = async (agentId: string) => {
    // In-flight guard: ignore repeat clicks while the kill is pending.
    if (killing.has(agentId)) return;
    setKilling((prev) => new Set(prev).add(agentId));
    try {
      // Remove from the UI only AFTER the backend confirms the kill. Removing
      // optimistically can "lie" (the kill may have failed) and the agent would
      // get resurrected by the next WS-driven refreshAgents.
      await api.killAgent(agentId);
      setAgents((prev) => prev.filter((a) => a.agent_id !== agentId));
      setMinimized((prev) => {
        if (!prev.has(agentId)) return prev;
        const next = new Set(prev);
        next.delete(agentId);
        return next;
      });
      setMaximized((cur) => (cur === agentId ? null : cur));
    } catch (err) {
      toast.error(t("debug.killFailed", { defaultValue: "终止失败" }), {
        description:
          err instanceof ApiError ? err.detail : (err as Error).message,
      });
    } finally {
      setKilling((prev) => {
        if (!prev.has(agentId)) return prev;
        const next = new Set(prev);
        next.delete(agentId);
        return next;
      });
    }
  };

  const wakeAgent = async (agentId: string) => {
    try {
      await api.wakeAgent(agentId);
      toast.success(t("debug.wakeSent", { defaultValue: "已发送唤醒" }), {
        description: agentId,
      });
    } catch (err) {
      toast.error(t("debug.wakeFailed", { defaultValue: "唤醒失败" }), {
        description: (err as Error)?.message,
      });
    }
  };

  const toggleMinimize = (agentId: string) => {
    setMinimized((prev) => {
      const next = new Set(prev);
      if (next.has(agentId)) next.delete(agentId);
      else next.add(agentId);
      return next;
    });
    setMaximized((cur) => (cur === agentId ? null : cur));
  };

  const toggleMaximize = (agentId: string) => {
    setMaximized((cur) => (cur === agentId ? null : agentId));
    setMinimized((prev) => {
      if (!prev.has(agentId)) return prev;
      const next = new Set(prev);
      next.delete(agentId);
      return next;
    });
  };

  const visibleAgents = useMemo(() => {
    if (maximized) return agents.filter((a) => a.agent_id === maximized);
    return agents.filter((a) => !minimized.has(a.agent_id));
  }, [agents, maximized, minimized]);

  const cols = useMemo(() => {
    if (maximized) return 1;
    if (visibleAgents.length === 0) return 1;
    return Math.min(MAX_COLS, Math.ceil(Math.sqrt(visibleAgents.length)));
  }, [maximized, visibleAgents.length]);

  const dockAgents = agents.filter((a) => minimized.has(a.agent_id));

  return (
    <div
      className="legacy-dashboard"
      style={{
        display: "flex",
        flexDirection: "column",
        height: "100%",
        background: "#0d0d0d",
        color: "#f0f0f0",
      }}
    >
      <header
        {...TAURI_DRAG_REGION}
        style={{
          background: "#111827",
          borderBottom: "1px solid #374151",
          display: "flex",
          flexDirection: "column",
          gap: 6,
          padding: isTauri ? tauriLeftSafePadding(8) : "8px 12px",
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 12,
            flexWrap: "wrap",
          }}
        >
          <strong style={{ fontSize: 14 }} {...TAURI_DRAG_REGION}>
            swarmx M2 (debug)
          </strong>
          <span style={{ color: "#64748b", fontSize: 12 }} {...TAURI_DRAG_REGION}>
            {t("debug.localSingleUser", {
              defaultValue: "本地单用户 · 仅回环",
            })}
          </span>
          <span style={{ color: "#64748b", fontSize: 12 }} {...TAURI_DRAG_REGION}>
            {t("debug.agentCount", {
              defaultValue: "{{total}} 个 agent · 显示 {{visible}}",
              total: agents.length,
              visible: visibleAgents.length,
            })}
          </span>
          <div style={{ flex: 1, alignSelf: "stretch" }} {...TAURI_DRAG_REGION} />
          {pluginsError && (
            <span style={{ color: "#ef4444", fontSize: 12 }}>
              {t("debug.pluginsLoadFailed", {
                defaultValue: "插件加载失败：{{error}}",
                error: pluginsError,
              })}
            </span>
          )}
          {plugins.map((p) => {
            const notInstalled = p.installed === false;
            return (
              <button
                key={p.id}
                onClick={() => spawn(p.id)}
                disabled={spawning || notInstalled}
                title={
                  notInstalled
                    ? t("debug.cliNotDetected", {
                        defaultValue: "未检测到该 CLI",
                      })
                    : t("debug.launchBinary", {
                        defaultValue: "启动 {{binary}}",
                        binary: p.binary,
                      })
                }
              >
                + {p.display_name}
              </button>
            );
          })}
          <button
            onClick={() => setSwarmOpen((v) => !v)}
            title={t("debug.toggleSwarmPanel", {
              defaultValue: "切换协作面板",
            })}
            style={{
              background: swarmOpen ? "#1e3a8a" : "#1f2937",
              color: "#e2e8f0",
              border: "1px solid #374151",
              borderRadius: 4,
              padding: "2px 8px",
              fontSize: 12,
            }}
          >
            {swarmOpen
              ? t("debug.hidePanel", { defaultValue: "隐藏面板" })
              : t("debug.showPanel", { defaultValue: "显示面板" })}
          </button>
        </div>
        <SpellsLauncher onSpellLaunched={refreshAgents} />
      </header>

      <div
        style={{
          flex: 1,
          minHeight: 0,
          display: "flex",
          flexDirection: "row",
        }}
      >
      <main
        style={{
          flex: 1,
          minHeight: 0,
          padding: 8,
          display: "grid",
          gap: 8,
          gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`,
          gridAutoRows: "minmax(0, 1fr)",
        }}
      >
        {agents.length === 0 && (
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              color: "#64748b",
              border: "1px dashed #374151",
              borderRadius: 6,
            }}
          >
            {t("debug.noAgentsHint", {
              defaultValue:
                "还没有 agent — 在上方输入任务点 ✨ Auto，或单独启动一个 CLI",
            })}
          </div>
        )}
        {agents.map((agent) => {
          const isMinimized = minimized.has(agent.agent_id);
          const isMaximized = maximized === agent.agent_id;
          const hidden =
            (maximized !== null && !isMaximized) || (!maximized && isMinimized);
          return (
            <div
              key={agent.agent_id}
              style={{
                display: hidden ? "none" : "flex",
                flexDirection: "column",
                border: "1px solid #374151",
                borderRadius: 6,
                minHeight: 0,
                overflow: "hidden",
              }}
            >
              <div
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  alignItems: "center",
                  padding: "4px 8px",
                  background: "#1f2937",
                  fontSize: 12,
                  gap: 8,
                }}
              >
                <span style={{ overflow: "hidden", textOverflow: "ellipsis" }}>
                  <strong>{agent.role}</strong>
                  <span style={{ color: "#94a3b8", marginLeft: 8 }}>
                    {agent.cli}
                  </span>
                </span>
                <span style={{ display: "flex", gap: 4 }}>
                  <button
                    onClick={() => wakeAgent(agent.agent_id)}
                    aria-label={t("debug.wakeAgent", {
                      defaultValue: "手动唤醒",
                    })}
                    title={t("debug.wakeAgentHint", {
                      defaultValue:
                        "手动唤醒（agent 卡住时点这个 — 给它发一条系统消息让它继续）",
                    })}
                  >
                    ⚡
                  </button>
                  <button
                    onClick={() => toggleMinimize(agent.agent_id)}
                    aria-label={t("debug.minimize", { defaultValue: "最小化" })}
                    title={t("debug.minimize", { defaultValue: "最小化" })}
                  >
                    _
                  </button>
                  <button
                    onClick={() => toggleMaximize(agent.agent_id)}
                    aria-label={
                      isMaximized
                        ? t("debug.restore", { defaultValue: "还原" })
                        : t("debug.maximize", { defaultValue: "最大化" })
                    }
                    title={
                      isMaximized
                        ? t("debug.restore", { defaultValue: "还原" })
                        : t("debug.maximize", { defaultValue: "最大化" })
                    }
                  >
                    {isMaximized ? "❐" : "□"}
                  </button>
                  <button
                    onClick={() => kill(agent.agent_id)}
                    disabled={killing.has(agent.agent_id)}
                    aria-label={t("debug.killAgent", {
                      defaultValue: "终止 agent",
                    })}
                    title={t("debug.kill", { defaultValue: "终止" })}
                  >
                    {killing.has(agent.agent_id) ? (
                      <Loader2 size={11} className="animate-spin" />
                    ) : (
                      "×"
                    )}
                  </button>
                </span>
              </div>
              <div style={{ flex: 1, minHeight: 0 }}>
                <XtermPane agentId={agent.agent_id} visible={!hidden} />
              </div>
            </div>
          );
        })}
      </main>
        {swarmOpen && <SwarmPanel />}
      </div>

      {dockAgents.length > 0 && (
        <footer
          style={{
            display: "flex",
            gap: 6,
            flexWrap: "wrap",
            padding: "6px 8px",
            background: "#111827",
            borderTop: "1px solid #374151",
            fontSize: 12,
          }}
        >
          <span style={{ color: "#64748b", alignSelf: "center" }}>
            {t("debug.minimizedLabel", { defaultValue: "已最小化：" })}
          </span>
          {dockAgents.map((a) => (
            <button
              key={a.agent_id}
              onClick={() => toggleMinimize(a.agent_id)}
              title={t("debug.restoreAgent", {
                defaultValue: "还原 {{id}}",
                id: a.agent_id,
              })}
              style={{
                background: "#1f2937",
                color: "#cbd5f5",
                border: "1px solid #374151",
                borderRadius: 4,
                padding: "2px 8px",
              }}
            >
              {a.role}{" "}
              <span style={{ color: "#64748b" }}>({a.agent_id.slice(-8)})</span>
            </button>
          ))}
        </footer>
      )}
    </div>
  );
}
