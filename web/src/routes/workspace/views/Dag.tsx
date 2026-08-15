/**
 * Dag view — collaboration graph inside WorkspaceShell.
 *
 * The original dag.tsx route owned its own header, WorkspaceScopeBar and
 * "back to chat" link. All of that lives in Shell now; this file is just
 * the canvas + left inspector + right detail panel.
 *
 * State persistence: selectedId / roleFilter live in URL search params so
 * a tab-switch + tab-return restores them. Avoids the "click a node →
 * switch to replays → come back → selection is gone" annoyance.
 */

import { useCallback, useEffect, useMemo, useRef } from "react";
import { useSearchParams } from "react-router-dom";
import {
  Background,
  Controls,
  Handle,
  MarkerType,
  MiniMap,
  Position,
  ReactFlow,
  ReactFlowProvider,
  useReactFlow,
  type Edge,
  type Node,
  type NodeProps,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import dagre from "@dagrejs/dagre";
import {
  Activity,
  Layers,
  Maximize2,
  Pause,
  Play,
  X,
  Zap,
} from "lucide-react";
import { Link } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { api } from "../../../api/http";
import type {
  AgentInfo,
  BlackboardEntry,
  SwarmEvent,
} from "../../../api/types";
import { useSwarmFeed } from "../../../hooks/useSwarmFeed";
import { useWorkspaceContext } from "../Shell";
import {
  ConfirmActionDialog,
  type ConfirmActionState,
} from "@/components/ConfirmActionDialog";
import { EmptyState } from "@/components/EmptyState";
import { agentInThread, directionBase } from "@/lib/thread";
import { toast } from "@/lib/toast";
import { cn } from "@/lib/cn";
import {
  roleColorClass as roleColor,
  roleColorHex as roleHex,
  roleDisplayName,
} from "@/lib/agent";
import {
  deriveHandoffEdges,
  deriveSpawnEdges,
} from "@/lib/dagEdgeDerivation";
import { useState } from "react";

const NODE_W = 200;
const NODE_H = 80;

function layout(nodes: Node[], edges: Edge[]) {
  const g = new dagre.graphlib.Graph();
  g.setGraph({ rankdir: "TB", nodesep: 60, ranksep: 80 });
  g.setDefaultEdgeLabel(() => ({}));
  for (const n of nodes) g.setNode(n.id, { width: NODE_W, height: NODE_H });
  for (const e of edges) g.setEdge(e.source, e.target);
  dagre.layout(g);
  return nodes.map((n) => {
    const p = g.node(n.id);
    return {
      ...n,
      position: { x: p.x - NODE_W / 2, y: p.y - NODE_H / 2 },
      sourcePosition: Position.Bottom,
      targetPosition: Position.Top,
    };
  });
}

interface AgentNodeData extends Record<string, unknown> {
  info: AgentInfo;
  selected: boolean;
}

function AgentNode({ data }: NodeProps<Node<AgentNodeData>>) {
  const { t } = useTranslation();
  const a = data.info;
  const role = a.role;
  const live = a.killed_at == null && a.shim_exit == null;
  const paused = !!a.paused;
  // Paused nodes get a dashed border + a pause glyph so operators can
  // spot them at a glance. Dim the whole card slightly so an unpaused
  // sibling visually takes priority.
  return (
    <div
      className={cn(
        "flex w-[200px] !cursor-pointer flex-col gap-1 rounded-lg border-2 bg-surface-elevated px-3 py-2 shadow-sm transition-shadow hover:shadow-md",
        data.selected
          ? "border-accent-primary shadow-lg"
          : "border-border-subtle",
        paused && !data.selected && "border-dashed opacity-75",
        // Exited/reaped workers stay on the canvas (full-roster) but are dimmed
        // so they read as "done, no longer running" vs the bright live nodes —
        // without this an ephemeral swarm that already delivered looks identical
        // to one still working. The status dot already goes idle-gray; this
        // fades the whole card. `selected` keeps full opacity so a clicked dead
        // node is still clearly inspectable.
        !live && !data.selected && "opacity-50",
      )}
    >
      <Handle type="target" position={Position.Top} className="!bg-foreground-tertiary" />
      <div className="flex items-center gap-2">
        <span
          className={cn(
            "flex size-6 items-center justify-center rounded-full text-[10px] font-bold text-foreground-on-accent",
            roleColor(role),
          )}
        >
          {role.slice(0, 1).toUpperCase()}
        </span>
        <span className="flex-1 truncate font-heading text-sm font-semibold text-foreground-primary">
          {roleDisplayName(role)}
        </span>
        {paused && (
          <Pause
            className="size-3 text-foreground-tertiary"
            aria-label="paused"
          />
        )}
        <span
          className={cn(
            "size-2 rounded-full",
            live
              ? a.shim_ready
                ? "bg-state-success"
                : "bg-state-wake"
              : "bg-state-idle",
          )}
        />
      </div>
      <div className="truncate font-mono text-[10px] text-foreground-tertiary">
        {a.cli} · {a.agent_id.slice(-8)}
      </div>
      {a.handoff_signal && (
        <div
          className={cn(
            "truncate font-mono text-[10px]",
            // A failed handoff (worker wrote `<signal>.error`) renders in the
            // danger color with an ✗ so the DAG distinguishes an aborted
            // delivery from a successful one — the key string itself is the
            // same declared success key in both cases.
            a.handoff_failed ? "text-state-danger" : "text-state-success",
          )}
        >
          {a.handoff_failed
            ? t("tasks.failed", { defaultValue: "失败" })
            : t("tasks.delivered", { defaultValue: "已交付" })}
        </div>
      )}
      <Handle type="source" position={Position.Bottom} className="!bg-foreground-tertiary" />
    </div>
  );
}

const NODE_TYPES = { agent: AgentNode };

interface CanvasProps {
  agents: AgentInfo[];
  bbAt: Map<string, number>;
  selectedId: string | null;
  onSelect: (id: string | null) => void;
  showMinimap: boolean;
  // Bumped each time selection originates from the LEFT LIST (not a canvas
  // click). On a bump we pan/zoom the viewport to the selected node so an
  // off-canvas pick doesn't silently select something the user can't see.
  focusNonce: number;
}

function Canvas({ agents, bbAt, selectedId, onSelect, showMinimap, focusNonce }: CanvasProps) {
  const { t } = useTranslation();
  // Draw the FULL roster (live + already-exited), not just live. Workers are
  // ephemeral by design — they deliver a `*.done` and get killed (the
  // Magentic-One / Anthropic orchestrator-worker model). If the canvas only
  // painted live agents, a completed swarm collapsed back to a lone
  // orchestrator the instant its workers finished, erasing the very topology
  // the graph exists to show. Keeping exited nodes (rendered dimmed + a "done"
  // dot by AgentNode) preserves "what team just ran" without keeping any
  // process alive. Edges derive from the full set so a dead parent still
  // anchors its children and a completed handoff still renders.
  const edges = useMemo<Edge[]>(() => {
    const handoff = deriveHandoffEdges(agents, bbAt);
    const spawn = deriveSpawnEdges(agents);
    const handoffEdges: Edge[] = handoff.map((e, i) => ({
      id: `h-${i}-${e.producerId}-${e.dependentId}`,
      source: e.producerId,
      target: e.dependentId,
      label: e.key,
      animated: !e.satisfied,
      style: {
        // Theme vars (not hardcoded hex) so light/dark both stay legible —
        // success/warning have dark-mode overrides in global.css.
        stroke: e.satisfied
          ? "var(--color-state-success)"
          : "var(--color-state-warning)",
        strokeWidth: 1.75,
        strokeDasharray: e.satisfied ? undefined : "6 4",
      },
      labelStyle: {
        fill: e.satisfied
          ? "var(--color-state-success)"
          : "var(--color-state-warning)",
        fontSize: 10,
        fontFamily: "Geist Mono, ui-monospace, monospace",
      },
      // Elevated-surface bg (white in light, slate-800 in dark) so the label
      // chip never sits as a harsh white block on the dark canvas.
      labelBgStyle: { fill: "var(--color-surface-elevated)" },
    }));
    // Spawn edges render BEFORE handoff in the array — ReactFlow paints in
    // array order, so handoff arrows (the live signal) overlay parent
    // lineage (context). Solid slate, no label, light arrowhead. Use a
    // dedicated id namespace ("s-") so a re-render that changes only
    // handoff data doesn't disturb spawn edges.
    const spawnEdges: Edge[] = spawn.map((e, i) => ({
      id: `s-${i}-${e.parentId}-${e.childId}`,
      source: e.parentId,
      target: e.childId,
      // No animation, no label — spawn lineage is static context.
      style: {
        // idle slate, theme var so it tracks light/dark.
        stroke: "var(--color-state-idle)",
        strokeWidth: 1.25,
        opacity: 0.55,
      },
      markerEnd: {
        type: MarkerType.ArrowClosed,
        color: "var(--color-state-idle)",
      },
    }));
    return [...spawnEdges, ...handoffEdges];
  }, [agents, bbAt]);

  const nodes = useMemo<Node[]>(() => {
    // Full roster so exited workers stay on the canvas as dimmed "done"
    // nodes (see the Canvas-level comment). `live` is still derived above for
    // any live-only consumers, but the drawn node set is the whole team.
    const raw: Node[] = agents.map((a) => ({
      id: a.agent_id,
      type: "agent",
      position: { x: 0, y: 0 },
      data: { info: a, selected: a.agent_id === selectedId },
    }));
    return layout(raw, edges);
  }, [agents, edges, selectedId]);

  const flow = useReactFlow();
  useEffect(() => {
    const timer = window.setTimeout(() => {
      try {
        // Cap auto-fit zoom at 1.0 so a workspace with a single agent
        // doesn't blow up the node to fill the entire pane. Multi-agent
        // graphs still fit naturally; users can manually zoom past 1.0
        // via the controls (ReactFlow.maxZoom below still allows up to 2x).
        flow.fitView({ padding: 0.2, duration: 200, maxZoom: 1 });
      } catch {
        /* fitView throws on empty graph */
      }
    }, 50);
    return () => window.clearTimeout(timer);
  }, [flow, nodes.length, edges.length]);

  // Pan the viewport to a node selected from the left list so the selection
  // and what's on screen stay in sync — an off-canvas pick should bring the
  // node into view, not just highlight something the user can't see. Keyed on
  // focusNonce (bumps on every list pick, incl. re-selecting the same node);
  // canvas clicks leave the nonce untouched so they don't re-pan. `nodes`
  // already carries dagre-laid-out positions, so center on the node's middle.
  useEffect(() => {
    if (focusNonce === 0 || !selectedId) return;
    const n = nodes.find((node) => node.id === selectedId);
    if (!n) return;
    try {
      flow.setCenter(n.position.x + NODE_W / 2, n.position.y + NODE_H / 2, {
        duration: 400,
        zoom: Math.max(flow.getZoom(), 1),
      });
    } catch {
      /* setCenter can throw before the flow is mounted */
    }
    // Intentionally NOT depending on `nodes`/`selectedId` — only the nonce
    // drives a re-pan, so a routine refresh (which rebuilds `nodes`) doesn't
    // yank the viewport out from under the user.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusNonce, flow]);

  return (
    <ReactFlow
      nodes={nodes}
      edges={edges}
      nodeTypes={NODE_TYPES}
      onNodeClick={(_, n) => onSelect(n.id)}
      onPaneClick={() => onSelect(null)}
      fitView
      fitViewOptions={{ padding: 0.2, maxZoom: 1 }}
      maxZoom={2}
      proOptions={{ hideAttribution: true }}
      // Layout is dagre-computed every render; there's no onNodesChange to
      // persist a manual drag, so dragging used to just snap back — a broken
      // affordance. The node a11y description already says "节点不可移动",
      // so disable dragging to match intent (selection via click still works).
      nodesDraggable={false}
      nodesConnectable={false}
      ariaLabelConfig={{
        "node.a11yDescription.default": t(
          "dag.a11y.nodeDefault",
          "按 Enter 或空格选中节点，方向键移动，Esc 取消。",
        ),
        "node.a11yDescription.keyboardDisabled": t(
          "dag.a11y.nodeDisabled",
          "节点不可移动。",
        ),
        "node.a11yDescription.ariaLiveMessage": ({ direction, x, y }) =>
          t("dag.a11y.nodeMoved", {
            dir: t(`dag.a11y.dir.${direction}`, direction),
            x: Math.round(x),
            y: Math.round(y),
            defaultValue: "节点已{{dir}}至 {{x}}, {{y}}",
          }),
        "edge.a11yDescription.default": t(
          "dag.a11y.edgeDefault",
          "agent 间的 handoff 依赖连线。",
        ),
        "controls.ariaLabel": t("dag.a11y.controls", "画布缩放控制"),
        "controls.zoomIn.ariaLabel": t("dag.a11y.zoomIn", "放大"),
        "controls.zoomOut.ariaLabel": t("dag.a11y.zoomOut", "缩小"),
        "controls.fitView.ariaLabel": t("dag.a11y.fitView", "适应画面"),
        "controls.interactive.ariaLabel": t("dag.a11y.interactive", "切换交互模式"),
        "minimap.ariaLabel": t("dag.a11y.minimap", "缩略图"),
        "handle.ariaLabel": t("dag.a11y.handle", "连接点"),
      }}
    >
      <Background gap={24} size={1.25} />
      {showMinimap && (
        <MiniMap
          pannable
          zoomable
          nodeColor={(n) => roleHex((n.data as AgentNodeData).info.role)}
          nodeStrokeColor={(n) => roleHex((n.data as AgentNodeData).info.role)}
          nodeStrokeWidth={2}
          nodeBorderRadius={4}
          maskStrokeColor="var(--color-border-strong)"
          maskStrokeWidth={1}
        />
      )}
      <Controls showInteractive={false} />
    </ReactFlow>
  );
}

export default function DagView() {
  const { t } = useTranslation();
  const { workspace, activeThread, mainThread } = useWorkspaceContext();
  // selectedId / roleFilter 用 URL 持久化，切走再回不丢。
  const [searchParams, setSearchParams] = useSearchParams();
  const selectedId = searchParams.get("node");
  const roleFilter = searchParams.get("role") ?? "all";

  const setSelectedId = useCallback(
    (id: string | null) => {
      setSearchParams(
        (prev) => {
          const next = new URLSearchParams(prev);
          if (id) next.set("node", id);
          else next.delete("node");
          return next;
        },
        { replace: true },
      );
    },
    [setSearchParams],
  );
  const setRoleFilter = useCallback(
    (r: string) => {
      setSearchParams(
        (prev) => {
          const next = new URLSearchParams(prev);
          if (r === "all") next.delete("role");
          else next.set("role", r);
          return next;
        },
        { replace: true },
      );
    },
    [setSearchParams],
  );

  // Selecting from the LEFT LIST bumps this nonce so the canvas pans to the
  // (possibly off-screen) node; canvas clicks go through setSelectedId only
  // and leave the nonce alone, so they don't trigger a re-pan. Lives in state
  // (not the URL) — it's an ephemeral "focus now" pulse, not view state worth
  // persisting across tab switches.
  const [focusNonce, setFocusNonce] = useState(0);
  const selectFromList = useCallback(
    (id: string) => {
      setSelectedId(id);
      setFocusNonce((n) => n + 1);
    },
    [setSelectedId],
  );

  // Shell 拿到 allAliveAgents 是 cross-workspace 的，但我们要这个 workspace
  // 范围 + 历史 (含 killed) 的边数据 — 自己 listAgents 一次。同时也独立
  // 拉 blackboard 给 edge satisfied 计算用。Shell 没有这两份数据。
  const [allAgents, setAllAgents] = useState<AgentInfo[]>([]);
  const [bb, setBb] = useState<BlackboardEntry[]>([]);
  const [error, setError] = useState<string | null>(null);

  // F19: refresh runs from an effect AND from swarm-feed callbacks; guard its
  // setState against a refresh that resolves after the view unmounts (tab
  // switch) so we don't poke a dead component.
  const mountedRef = useRef(true);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const refresh = useCallback(async () => {
    try {
      const [a, b] = await Promise.all([api.listAgents(), api.listBlackboard()]);
      if (!mountedRef.current) return;
      setAllAgents(a);
      setBb(b);
      setError(null);
    } catch (e) {
      if (mountedRef.current) setError((e as Error).message);
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  useSwarmFeed({
    onEvent: (ev: SwarmEvent) => {
      if (ev.type === "agent_state" || ev.type === "blackboard_changed") {
        refresh();
      }
    },
    onReconnect: () => refresh(),
  });

  // Scope the DAG to the ACTIVE direction (thread) by filtering DagView's OWN
  // allAgents with the shared predicate (folds `thread_id == null` into main).
  // Filtering our own snapshot — rather than intersecting an id-set from the
  // shell's separately-fetched (debounced) state — avoids transiently dropping
  // a just-spawned node before the shell catches up.
  const agents = useMemo(
    () =>
      allAgents.filter((a) =>
        agentInThread(a, workspace.workspaceId, activeThread, mainThread),
      ),
    [allAgents, workspace.workspaceId, activeThread, mainThread],
  );

  const bbAt = useMemo(() => {
    const m = new Map<string, number>();
    for (const e of bb) m.set(e.path, e.at);
    return m;
  }, [bb]);

  const liveAgents = useMemo(
    () => agents.filter((a) => a.killed_at == null && a.shim_exit == null),
    [agents],
  );

  const roles = useMemo(() => {
    const s = new Set<string>();
    for (const a of liveAgents) s.add(a.role.toLowerCase());
    return ["all", ...Array.from(s).sort()];
  }, [liveAgents]);

  // P2: 过滤器指向的角色消失时(该 role 的 agent 全死)自动重置为 all。
  // 否则 pill 行(roles.length > 2 才渲染)可能整行隐藏,留下一个指向已消失
  // 角色的 roleFilter,filteredAgents 永远空,列表/画布卡死且无控件可重置。
  // 等首批 agent 真正加载完(roles 至少含 "all" 之外的项)再判断,避免
  // 初次拉取前把用户从 URL 带来的合法 filter 误清掉。
  useEffect(() => {
    if (roleFilter !== "all" && roles.length > 1 && !roles.includes(roleFilter)) {
      setRoleFilter("all");
    }
  }, [roleFilter, roles, setRoleFilter]);

  // 成员列表 / role filter 都只关心还活着的 agent — DAG canvas 也只画
  // alive 节点,两边语义一致避免列表里灰着的 agent 让人误以为"画里少了"。
  // 历史死亡 agent 走 Recordings 视图复盘,不该在 live 操作面板里干扰视线。
  const filteredAgents = useMemo(
    () =>
      roleFilter === "all"
        ? liveAgents
        : liveAgents.filter((a) => a.role.toLowerCase() === roleFilter),
    [liveAgents, roleFilter],
  );

  // Canvas roster: thread-scoped + role-filtered like `filteredAgents`, but it
  // KEEPS exited agents (no live-only filter). The left member list stays
  // live-only (operators act on live agents); the graph shows the full team
  // including finished workers as dimmed "done" nodes, so an ephemeral swarm
  // that already delivered doesn't collapse back to a lone orchestrator.
  const canvasAgents = useMemo(
    () =>
      roleFilter === "all"
        ? agents
        : agents.filter((a) => a.role.toLowerCase() === roleFilter),
    [agents, roleFilter],
  );

  const selected = useMemo(
    () => agents.find((a) => a.agent_id === selectedId) ?? null,
    [agents, selectedId],
  );

  // 选中 agent 已死(被 kill 或 shim 退出)时,wake/pause/resume 都无意义 ——
  // 后端会拒绝,过去却让按钮可点 + 静默吞错,看起来像"点了没反应"。死按钮直接禁用。
  const selectedDead =
    !!selected && (selected.killed_at != null || selected.shim_exit != null);

  const pausedCount = useMemo(
    () => liveAgents.filter((a) => a.paused).length,
    [liveAgents],
  );
  const [confirm, setConfirm] = useState<ConfirmActionState | null>(null);

  // workspace-level "pause all live agents". Fire-and-forget — refresh()
  // will pick up the new paused state via the swarm feed (paused agents
  // don't emit a swarm event, but the next agent_state / blackboard
  // event will retrigger refresh; we also nudge it manually here).
  const [busy, setBusy] = useState(false);
  const onInterruptAll = useCallback(async () => {
    // Interrupt EXACTLY the live agents shown in THIS direction — the same set
    // the confirm dialog counts. (Was: a workspace-bulk endpoint that also
    // stopped agents in *other* directions while the dialog only counted this
    // one — P0-2.) Per-agent, mirroring onResumeAll; no cross-direction reach.
    const targets = liveAgents.filter((a) => !a.paused);
    if (busy || targets.length === 0) return;
    setBusy(true);
    try {
      const results = await Promise.allSettled(
        targets.map((a) => api.interruptAgent(a.agent_id)),
      );
      const failed = results.filter((r) => r.status === "rejected").length;
      if (failed > 0) {
        setError(
          t("dag.interruptAllPartialFail", {
            n: failed,
            defaultValue: "{{n}} 个 agent 暂停失败，请重试",
          }),
        );
      }
      await refresh();
    } finally {
      setBusy(false);
    }
  }, [busy, liveAgents, refresh, t]);

  // Resume every paused agent in the active workspace. No batch endpoint —
  // resume is per-agent because each one synthesizes a manual wake; ≤ a
  // handful of agents per spell so the loop is cheap.
  const onResumeAll = useCallback(async () => {
    if (busy) return;
    const paused = liveAgents.filter((a) => a.paused);
    if (paused.length === 0) return;
    setBusy(true);
    try {
      // Per-agent soft-fail so one bad agent doesn't abort the batch — but
      // P2: surface the failures. Was console.warn-only, so a whole-batch
      // failure looked like nothing happened (界面撒谎).
      const results = await Promise.allSettled(
        paused.map((a) => api.resumeAgent(a.agent_id)),
      );
      const failed = results.filter((r) => r.status === "rejected").length;
      if (failed > 0) {
        toast.error(
          t("dag.resumeAllPartialFail", {
            n: failed,
            defaultValue: "{{n}} 个 agent 恢复失败，请重试",
          }),
        );
      }
      await refresh();
    } finally {
      setBusy(false);
    }
  }, [busy, liveAgents, refresh, t]);

  const onTogglePauseSelected = useCallback(async () => {
    if (!selected || busy) return;
    setBusy(true);
    try {
      if (selected.paused) {
        await api.resumeAgent(selected.agent_id);
      } else {
        await api.interruptAgent(selected.agent_id);
      }
      await refresh();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }, [busy, refresh, selected]);

  const requestInterruptAll = useCallback(() => {
    setConfirm({
      title: t("dag.confirm.interruptAll.title", {
        count: liveAgents.length,
        defaultValue: "暂停所有 agent？",
      }),
      description: t(
        "dag.confirm.interruptAll.desc",
        "会向当前方向所有运行中的 agent 发送中断，并停止它们的自动唤醒，直到你恢复。",
      ),
      confirmLabel: t("dag.interruptAll"),
      variant: "destructive",
      onConfirm: onInterruptAll,
    });
  }, [liveAgents.length, onInterruptAll, t]);

  const requestResumeAll = useCallback(() => {
    setConfirm({
      title: t("dag.confirm.resumeAll.title", {
        count: pausedCount,
        defaultValue: "恢复所有暂停 agent？",
      }),
      description: t(
        "dag.confirm.resumeAll.desc",
        "会逐个恢复当前工作空间内暂停的 agent，并让它们继续处理当前工作。",
      ),
      confirmLabel: t("dag.resumeAll"),
      onConfirm: onResumeAll,
    });
  }, [onResumeAll, pausedCount, t]);

  const requestWakeSelected = useCallback(() => {
    if (!selected) return;
    setConfirm({
      title: t("agent.confirm.wake.title", {
        role: selected.role,
        defaultValue: "唤醒 agent？",
      }),
      description: t(
        "agent.confirm.wake.desc",
        "一般不用点——卡住时系统会自动催。只有它还在睡、自动催没用时再用。",
      ),
      confirmLabel: t("agent.wake"),
      onConfirm: async () => {
        try {
          await api.wakeAgent(selected.agent_id);
        } catch (e) {
          toast.error(
            t("agent.wakeFailed", { defaultValue: "催促失败" }),
            { description: (e as Error)?.message },
          );
        }
      },
    });
  }, [selected, t]);

  const requestTogglePauseSelected = useCallback(() => {
    if (!selected) return;
    const paused = !!selected.paused;
    setConfirm({
      title: paused
        ? t("agent.confirm.resume.title", {
            role: selected.role,
            defaultValue: "恢复 agent？",
          })
        : t("agent.confirm.pause.title", {
            role: selected.role,
            defaultValue: "暂停 agent？",
          }),
      description: paused
        ? t(
            "agent.confirm.resume.desc",
            "会恢复自动催促，并让它马上接着干。",
          )
        : t(
            "agent.confirm.pause.desc",
            "会打断它当前这一轮，并暂时不再自动催它，直到你点「继续」。",
          ),
      confirmLabel: paused ? t("agent.resume") : t("agent.pause"),
      variant: paused ? "default" : "destructive",
      onConfirm: onTogglePauseSelected,
    });
  }, [onTogglePauseSelected, selected, t]);

  // 空图只渲染单一 EmptyState（UX-030），不再左栏/右栏复读
  if (liveAgents.length === 0) {
    return (
      <div className="flex min-h-0 flex-1 items-center justify-center p-6">
        <EmptyState
          icon={<Activity className="size-8" />}
          title={t("dag.empty")}
          hint={t("dag.emptyHint")}
          primaryAction={{ label: t("dag.emptyAction"), href: ".." }}
        />
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col lg:flex-row">
      <aside className="flex max-h-52 shrink-0 flex-col gap-4 overflow-y-auto border-b border-border-subtle bg-surface-secondary p-3 lg:max-h-none lg:w-[200px] lg:gap-5 lg:border-b-0 lg:border-r lg:p-4">
        <section>
          <h3 className="mb-2 font-heading text-[11px] font-semibold uppercase tracking-wider text-foreground-tertiary">
            {t("dag.legend")}
          </h3>
          <div className="flex flex-col gap-2 font-caption text-xs">
            <div className="flex items-center gap-2">
              <svg width="36" height="10">
                <line
                  x1="0"
                  y1="5"
                  x2="36"
                  y2="5"
                  stroke="var(--color-state-success)"
                  strokeWidth="1.75"
                />
              </svg>
              <span className="text-foreground-secondary">{t("dag.satisfied")}</span>
            </div>
            <div className="flex items-center gap-2">
              <svg width="36" height="10">
                <line
                  x1="0"
                  y1="5"
                  x2="36"
                  y2="5"
                  stroke="var(--color-state-warning)"
                  strokeWidth="1.75"
                  strokeDasharray="6 4"
                />
              </svg>
              <span className="text-foreground-secondary">{t("dag.waiting")}</span>
            </div>
            <div className="flex items-center gap-2">
              <svg width="36" height="10">
                <line
                  x1="0"
                  y1="5"
                  x2="36"
                  y2="5"
                  stroke="var(--color-state-idle)"
                  strokeWidth="1.25"
                  opacity="0.7"
                />
              </svg>
              <span className="text-foreground-secondary">{t("dag.spawn")}</span>
            </div>
          </div>
        </section>
        <section>
          <h3 className="mb-2 font-heading text-[11px] font-semibold uppercase tracking-wider text-foreground-tertiary">
            {t("dag.members")}
          </h3>
          {roles.length > 2 && (
            <div className="mb-2 flex flex-wrap gap-1">
              {roles.map((r) => (
                <button
                  key={r}
                  onClick={() => setRoleFilter(r)}
                  className={cn(
                    "flex items-center gap-1 rounded-full px-2 py-0.5 text-[10px]",
                    roleFilter === r
                      ? "bg-accent-primary text-foreground-on-accent"
                      : "bg-surface-tertiary text-foreground-secondary hover:bg-surface-primary",
                  )}
                >
                  {r !== "all" && (
                    <span
                      className="size-1.5 rounded-full"
                      style={{ background: roleHex(r) }}
                    />
                  )}
                  <span>{r === "all" ? t("common.all") : roleDisplayName(r)}</span>
                </button>
              ))}
            </div>
          )}
          <ul className="flex flex-col gap-1">
            {filteredAgents.map((a) => (
              <li key={a.agent_id}>
                <button
                  onClick={() => selectFromList(a.agent_id)}
                  className={cn(
                    "flex w-full items-center gap-2 rounded px-2 py-1 text-left",
                    selectedId === a.agent_id
                      ? "bg-accent-primary-soft"
                      : "hover:bg-surface-tertiary",
                  )}
                >
                  <span
                    className={cn(
                      "size-2 rounded-full",
                      a.killed_at == null && a.shim_exit == null
                        ? a.shim_ready
                          ? "bg-state-success"
                          : "bg-state-wake"
                        : "bg-state-idle",
                    )}
                  />
                  <span className="truncate font-heading text-xs text-foreground-primary">
                    {roleDisplayName(a.role)}
                  </span>
                  <span className="ml-auto truncate font-mono text-[10px] text-foreground-tertiary">
                    {a.agent_id.slice(-8)}
                  </span>
                </button>
              </li>
            ))}
            {filteredAgents.length === 0 && (
              <li className="px-2 py-1 font-caption text-[11px] text-foreground-tertiary">
                {t("dag.empty")}
              </li>
            )}
          </ul>
        </section>
      </aside>

      <div className="relative min-h-[420px] min-w-0 flex-1 lg:min-h-0">
        {error && (
          <div className="absolute top-4 left-4 z-10 rounded-md border border-state-danger/40 bg-status-danger-soft px-3 py-2 text-xs text-state-danger">
            {error}
          </div>
        )}
        <div className="absolute top-3 right-3 z-10 flex max-w-[calc(100%-1.5rem)] flex-wrap items-center justify-end gap-2 lg:top-4 lg:right-4">
            {pausedCount > 0 && (
              <span className="rounded-full bg-surface-elevated px-2 py-0.5 font-caption text-[10px] text-foreground-tertiary">
                {t("dag.pausedCount", { count: pausedCount })}
              </span>
            )}
            <button
              type="button"
              onClick={requestInterruptAll}
              disabled={busy || liveAgents.length === pausedCount}
              className={cn(
                "flex h-8 items-center gap-1.5 rounded-md border border-border-subtle bg-surface-elevated px-2.5 text-xs text-foreground-secondary shadow-sm transition-colors",
                "hover:bg-surface-tertiary",
                "disabled:cursor-not-allowed disabled:opacity-50",
              )}
              title={t("dag.interruptAll")}
            >
              <Pause className="size-3.5" />
              {t("dag.interruptAll")}
            </button>
            <button
              type="button"
              onClick={requestResumeAll}
              disabled={busy || pausedCount === 0}
              className={cn(
                "flex h-8 items-center gap-1.5 rounded-md border border-border-subtle bg-surface-elevated px-2.5 text-xs text-foreground-secondary shadow-sm transition-colors",
                "hover:bg-surface-tertiary",
                "disabled:cursor-not-allowed disabled:opacity-50",
              )}
              title={t("dag.resumeAll")}
            >
              <Play className="size-3.5" />
              {t("dag.resumeAll")}
            </button>
          </div>
          <ReactFlowProvider>
            <Canvas
              // Role filter applies to the canvas too (parity with the left
              // member list). Unlike the list, this set KEEPS exited agents so
              // finished ephemeral workers stay visible as dimmed "done" nodes
              // instead of vanishing the moment they deliver.
              agents={canvasAgents}
              bbAt={bbAt}
              selectedId={selectedId}
              onSelect={setSelectedId}
              showMinimap={canvasAgents.length > 4}
              focusNonce={focusNonce}
            />
          </ReactFlowProvider>
      </div>

      <aside className="flex max-h-72 shrink-0 flex-col gap-4 overflow-y-auto border-t border-border-subtle bg-surface-elevated p-4 lg:max-h-none lg:w-[300px] lg:border-t-0 lg:border-l lg:p-5">
        {!selected ? (
          <div className="flex h-full flex-col items-center justify-center gap-2 text-center text-foreground-tertiary">
            <Maximize2 className="size-8 opacity-40" />
            <p className="font-caption text-sm">{t("dag.selectNode")}</p>
          </div>
        ) : (
          <>
            <div className="flex items-center gap-3">
              <span
                className={cn(
                  "flex size-10 items-center justify-center rounded-full text-sm font-bold text-foreground-on-accent",
                  roleColor(selected.role),
                )}
              >
                {selected.role.slice(0, 1).toUpperCase()}
              </span>
              <div className="flex min-w-0 flex-1 flex-col">
                <span className="font-heading text-sm font-bold text-foreground-primary">
                  {roleDisplayName(selected.role)}
                </span>
                <span className="truncate font-mono text-[10px] text-foreground-tertiary">
                  {selected.agent_id}
                </span>
              </div>
              <button
                onClick={() => setSelectedId(null)}
                className="flex size-8 items-center justify-center rounded-md text-foreground-tertiary hover:bg-surface-tertiary"
              >
                <X className="size-4" />
              </button>
            </div>
            <dl className="grid grid-cols-[80px_1fr] gap-y-2 font-caption text-xs">
              <dt className="text-foreground-tertiary">{t("dag.cli")}</dt>
              <dd className="font-mono text-foreground-primary">{selected.cli}</dd>
              <dt className="text-foreground-tertiary">{t("dag.status")}</dt>
              <dd className="text-foreground-primary">
                {/* 优先级:dead > paused > ready > starting。shim_ready
                 *  在 spawn 时被置 true,killed 不会复位,所以不能让
                 *  shim_ready 优先,否则死 agent 永远 READY。 */}
                {selected.killed_at != null || selected.shim_exit != null
                  ? t("dag.killed")
                  : selected.paused
                    ? t("chat.paused", "已暂停")
                    : selected.shim_ready
                      ? t("dag.ready")
                      : t("dag.startingShort")}
              </dd>
              <dt className="text-foreground-tertiary">{t("dag.handoff")}</dt>
              <dd className="break-all font-mono text-state-success">
                {selected.handoff_signal || "—"}
              </dd>
              <dt className="text-foreground-tertiary">{t("dag.dependsOn")}</dt>
              <dd className="flex flex-wrap gap-1">
                {(selected.depends_on ?? []).length === 0 ? (
                  <span className="text-foreground-tertiary">—</span>
                ) : (
                  (selected.depends_on ?? []).map((k) => (
                    <span
                      key={k}
                      className="rounded bg-surface-tertiary px-1.5 py-0.5 font-mono text-foreground-primary"
                    >
                      {k}
                    </span>
                  ))
                )}
              </dd>
            </dl>
            <div className="mt-auto flex flex-col gap-2">
              <Link
                to={`${directionBase(workspace.id, activeThread?.slug)}?agent=${encodeURIComponent(selected.agent_id)}`}
                className="flex h-9 items-center justify-center gap-1.5 rounded-md bg-accent-primary text-xs font-bold text-foreground-on-accent hover:bg-accent-primary-deep"
              >
                {t("dag.openDrawer")}
              </Link>
              <div className="flex gap-2">
                <button
                  onClick={requestWakeSelected}
                  disabled={busy || selectedDead}
                  className={cn(
                    "flex h-9 flex-1 items-center justify-center gap-1.5 rounded-md border border-border-subtle bg-surface-elevated text-xs text-foreground-secondary hover:bg-surface-tertiary",
                    "disabled:cursor-not-allowed disabled:opacity-50",
                  )}
                  title={
                    selectedDead
                      ? t("agent.deadNoAction", {
                          defaultValue: "该 agent 已结束,无法操作",
                        })
                      : t("agent.wake")
                  }
                >
                  <Zap className="size-3.5" />
                  {t("agent.wake")}
                </button>
                <button
                  onClick={requestTogglePauseSelected}
                  disabled={busy || selectedDead}
                  className={cn(
                    "flex h-9 flex-1 items-center justify-center gap-1.5 rounded-md border border-border-subtle bg-surface-elevated text-xs text-foreground-secondary hover:bg-surface-tertiary",
                    "disabled:cursor-not-allowed disabled:opacity-50",
                  )}
                  title={
                    selectedDead
                      ? t("agent.deadNoAction", {
                          defaultValue: "该 agent 已结束,无法操作",
                        })
                      : selected.paused
                        ? t("agent.resume")
                        : t("agent.pause")
                  }
                >
                  {selected.paused ? (
                    <>
                      <Play className="size-3.5" />
                      {t("agent.resume")}
                    </>
                  ) : (
                    <>
                      <Pause className="size-3.5" />
                      {t("agent.pause")}
                    </>
                  )}
                </button>
              </div>
            </div>
          </>
        )}
      </aside>
      <ConfirmActionDialog
        action={confirm}
        onOpenChange={(next) => {
          if (!next) setConfirm(null);
        }}
      />
    </div>
  );
}

void Layers;
