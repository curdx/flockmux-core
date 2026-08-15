/**
 * SpellsLauncher — small header control that lists `/api/spells` in a
 * dropdown and lets the user fire one off with a task description.
 *
 * Why this component is intentionally small:
 *   - Spells are spawn-orchestrators; once launched the user interacts
 *     with the individual agents through the existing pane UI. So this
 *     component owns no per-agent state — it just kicks the API and
 *     forgets.
 *   - We list spells on mount only; new spells require editing files on
 *     disk and restarting the server (no hot-reload), so the dropdown is
 *     stable for the session's lifetime.
 *   - Failure modes are user-facing: a /api/spells 500 is logged inline
 *     so people don't think the dropdown is just empty.
 *
 * Two run paths:
 *   - "✨ Auto" — primary CTA. Hardcodes `name: "auto-dispatch"`; the
 *     planner agent reads the task, picks a downstream spell, and
 *     launches it. Workspace input is ignored (planner picks one).
 *     Hidden if the auto-dispatch spell isn't loaded server-side.
 *   - "run" — manual. Uses whatever the user picked in the dropdown
 *     + the explicit workspace dir. Always available; useful when
 *     the user already knows which spell they want or wants to feed
 *     a specific workspace path.
 */

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, ApiError } from "../api/http";
import type { SpellInfo } from "../api/types";
import { notifySpawnFallbacks } from "../lib/engineFallback";

interface Props {
  /** Notify the parent that new agents popped into existence so it can
   *  refresh its agent list. The parent already polls /ws/swarm for
   *  `agent_state=spawning`, so this is belt-and-braces. */
  onSpellLaunched?: () => void;
  /** Workspace context for the run. The server now REQUIRES a workspace_id
   *  (or caller_agent_id) on /api/spells/run — without it every launch 400s
   *  with "spell requires workspace context". A launcher mounted outside any
   *  workspace (e.g. the legacy /debug dashboard) has none, so we disable the
   *  run buttons there instead of firing a doomed request. */
  workspaceId?: string | null;
  /** Direction (thread) to run the spell in. Optional even with a workspace —
   *  the server falls back to the workspace's main direction. */
  threadId?: string | null;
}

export function SpellsLauncher({
  onSpellLaunched,
  workspaceId = null,
  threadId = null,
}: Props) {
  const { t } = useTranslation();
  const [spells, setSpells] = useState<SpellInfo[]>([]);
  const [selected, setSelected] = useState<string>("");
  const [task, setTask] = useState("");
  // 可选的共享 workspace 路径。仅当法术 manifest 声明 `shared_workspace = true`
  // 时有用（目前是 fullstack-feature 系列）。其他法术（critic-loop 等）服务端
  // 直接忽略此字段。默认折叠：99% 的运行不需要它，露出来只是噪音 + 把 task
  // 输入框挤窄。
  const [workspaceDir, setWorkspaceDir] = useState("");
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastRun, setLastRun] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .listSpells()
      .then((rows) => {
        if (cancelled) return;
        setSpells(rows);
        if (rows.length > 0 && !selected) setSelected(rows[0].name);
      })
      .catch((e) => {
        if (cancelled) return;
        setError(
          t("spells.loadFailed", {
            defaultValue: "法术列表加载失败：{{msg}}",
            msg: (e as Error).message,
          }),
        );
      });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  if (spells.length === 0 && !error) {
    // Nothing to show — keep the header tidy.
    return null;
  }

  // Shared launch path: PUT-and-forget the spell run, render the spawn
  // summary, clear the task input on success. `name` is the only thing
  // that differs between the manual and auto buttons — auto hardcodes
  // "auto-dispatch", manual uses whatever the dropdown selected.
  const launch = async (name: string, includeWorkspace: boolean) => {
    if (!name || !task.trim()) return;
    // Guard: the server rejects a context-less spell run with a 400. Don't fire
    // a request we know will fail — block at the button and tell the user why.
    if (!workspaceId) {
      setError(
        t("spells.needWorkspaceToRun", {
          defaultValue: "请在工作区内使用：法术需要一个工作区上下文才能运行",
        }),
      );
      return;
    }
    setBusy(true);
    setError(null);
    setLastRun(null);
    try {
      const wd = includeWorkspace ? workspaceDir.trim() : "";
      const resp = await api.runSpell({
        name,
        task: task.trim(),
        workspace_id: workspaceId,
        ...(threadId ? { thread_id: threadId } : {}),
        ...(wd ? { workspace_dir: wd } : {}),
      });
      // Engine fallback is never silent (billing red line) — see CreateWizard.
      notifySpawnFallbacks(t, resp.agents);
      setLastRun(
        t("spells.launchedSummary", {
          defaultValue: "已通过 {{name}} 启动 {{count}} 个 agent：{{agents}}",
          name,
          count: resp.agents.length,
          agents: resp.agents
            .map((a) => `${a.role}=${a.agent_id}`)
            .join(
              t("spells.agentSeparator", { defaultValue: "、" }),
            ),
        }),
      );
      setTask("");
      onSpellLaunched?.();
    } catch (e) {
      const msg = e instanceof ApiError ? e.detail : (e as Error).message;
      setError(
        t("spells.runFailed", {
          defaultValue: "运行失败：{{msg}}",
          msg,
        }),
      );
    } finally {
      setBusy(false);
    }
  };

  const run = () => launch(selected, true);
  const runAuto = () => launch("auto-dispatch", false);

  // No workspace context → every run 400s. Disable the run buttons and surface
  // a single inline hint, rather than letting users click into a guaranteed error.
  const noWorkspace = !workspaceId;

  const hasAutoDispatch = spells.some((s) => s.name === "auto-dispatch");
  const current = spells.find((s) => s.name === selected);

  return (
    <div style={wrap}>
      <input
        name="spell-task"
        type="text"
        value={task}
        onChange={(e) => setTask(e.target.value)}
        onKeyDown={(e) => {
          // task 输入框上按 Enter 触发主动作：有 auto-dispatch 就走 Auto，
          // 否则走 run。想用快捷键执行手动模式的话，可以聚焦下拉框或
          // workspace 输入框后按 Enter（那两个都转发到 run()）。
          if (e.key === "Enter" && !noWorkspace) {
            if (hasAutoDispatch) runAuto();
            else run();
          }
        }}
        placeholder={t("spells.taskPlaceholder", {
          defaultValue: "想做什么？例如：做个简单的密码强度检测",
        })}
        style={input}
        disabled={busy}
      />
      {hasAutoDispatch && (
        <button
          onClick={runAuto}
          disabled={busy || !task.trim() || noWorkspace}
          style={autoButton}
          title={
            noWorkspace
              ? t("spells.needWorkspaceShort", {
                  defaultValue: "请在工作区内使用：法术需要一个工作区上下文",
                })
              : t("spells.autoTitle", {
                  defaultValue:
                    "自动：planner 看你的任务自动挑法术。下方共享 workspace 配置会被忽略 — planner 自己挑一个。",
                })
          }
        >
          {busy
            ? t("spells.thinking", { defaultValue: "思考中…" })
            : t("spells.auto", { defaultValue: "✨ Auto" })}
        </button>
      )}
      <span style={divider}>{t("spells.or", { defaultValue: "或" })}</span>
      <select
        value={selected}
        onChange={(e) => setSelected(e.target.value)}
        style={select}
        disabled={busy}
        title={current?.description}
      >
        {spells.map((s) => (
          <option key={s.name} value={s.name}>
            {s.name} ({s.agents.map((a) => `${a.role}:${a.cli}`).join(" → ")})
          </option>
        ))}
      </select>
      <button
        onClick={run}
        disabled={busy || !task.trim() || noWorkspace}
        title={
          noWorkspace
            ? t("spells.needWorkspaceShort", {
                defaultValue: "请在工作区内使用：法术需要一个工作区上下文",
              })
            : t("spells.runTitle", {
                defaultValue: "按下拉框里选中的法术运行",
              })
        }
      >
        {t("spells.run", { defaultValue: "运行" })}
      </button>
      <button
        onClick={() => setShowAdvanced((v) => !v)}
        title={t("spells.advancedTitle", {
          defaultValue: "展开/收起共享 workspace 配置（fullstack 类法术才需要）",
        })}
        style={advancedToggle}
      >
        {showAdvanced
          ? t("spells.advancedExpanded", { defaultValue: "高级 ▾" })
          : t("spells.advancedCollapsed", { defaultValue: "高级 ▸" })}
      </button>
      {showAdvanced && (
        <input
          name="spell-workspace-dir"
          type="text"
          value={workspaceDir}
          onChange={(e) => setWorkspaceDir(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !noWorkspace) run();
          }}
          placeholder={t("spells.workspaceDirPlaceholder", {
            defaultValue: "共享 workspace 绝对路径（仅 fullstack 类法术）",
          })}
          style={workspaceInput}
          disabled={busy}
          title={t("spells.workspaceDirTitle", {
            defaultValue:
              "绝对路径。仅当法术 manifest 声明 shared_workspace=true 时才会生效。留空则由服务端自动分配。",
          })}
        />
      )}
      {/* Persistent hint when there's no workspace to run in — the run buttons
          are disabled, so explain why rather than leaving them mysteriously dead. */}
      {noWorkspace && !error && (
        <span style={hintStyle}>
          {t("spells.needWorkspaceShort", {
            defaultValue: "请在工作区内使用：法术需要一个工作区上下文",
          })}
        </span>
      )}
      {error && <span style={errStyle}>{error}</span>}
      {lastRun && <span style={okStyle}>{lastRun}</span>}
    </div>
  );
}

const wrap: React.CSSProperties = {
  display: "flex",
  gap: 6,
  alignItems: "center",
  flexWrap: "wrap",
};

const select: React.CSSProperties = {
  background: "#0b1220",
  color: "#e2e8f0",
  border: "1px solid #374151",
  borderRadius: 4,
  padding: "2px 6px",
  fontSize: 12,
  fontFamily: "inherit",
};

// task 输入框抢占剩余空间 — header 第二行整行就剩它和按钮，所以
// minWidth 给一个底线避免被法术下拉挤到没法读，flex:1 吃掉余量。
const input: React.CSSProperties = {
  background: "#0b1220",
  color: "#e2e8f0",
  border: "1px solid #374151",
  borderRadius: 4,
  padding: "4px 8px",
  fontSize: 13,
  fontFamily: "inherit",
  minWidth: 240,
  flex: 1,
};

const workspaceInput: React.CSSProperties = {
  background: "#0b1220",
  color: "#cbd5e1",
  border: "1px solid #374151",
  borderRadius: 4,
  padding: "2px 6px",
  fontSize: 11,
  fontFamily: "inherit",
  minWidth: 200,
  flex: 1,
};

const advancedToggle: React.CSSProperties = {
  background: "transparent",
  color: "#94a3b8",
  border: "1px solid #374151",
  borderRadius: 4,
  padding: "2px 8px",
  fontSize: 11,
  fontFamily: "inherit",
  cursor: "pointer",
};

// Primary CTA — visually louder than the manual "run" button so a new
// user's eye lands on it first. Purple gradient picks up the ✨ vibe.
const autoButton: React.CSSProperties = {
  background: "linear-gradient(135deg, #7c3aed 0%, #c026d3 100%)",
  color: "#fff",
  border: "1px solid #9333ea",
  borderRadius: 4,
  padding: "3px 12px",
  fontSize: 12,
  fontWeight: 600,
  cursor: "pointer",
  fontFamily: "inherit",
};

const divider: React.CSSProperties = {
  color: "#6b7280",
  fontSize: 11,
  padding: "0 4px",
  fontStyle: "italic",
};

const errStyle: React.CSSProperties = {
  color: "#ef4444",
  fontSize: 11,
};

const hintStyle: React.CSSProperties = {
  color: "#94a3b8",
  fontSize: 11,
  fontStyle: "italic",
};

const okStyle: React.CSSProperties = {
  color: "#86efac",
  fontSize: 11,
};
