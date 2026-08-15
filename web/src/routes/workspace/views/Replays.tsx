/**
 * Replays view — recording library inside WorkspaceShell.
 *
 * Strips the previous /replays route's own header / WorkspaceScopeBar
 * (Shell owns them now). Keeps the tag bar + card grid + search box;
 * tag and search live in URL state so navigating away and back preserves
 * filters.
 *
 * Cards link to /chat/:wsId/replays/:recId for the fullscreen player —
 * that route is OUTSIDE the Shell (mounted directly under App routes)
 * because the player wants a fully dark, chrome-less surface.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { useTranslation } from "react-i18next";
import {
  Clock,
  Download,
  Monitor,
  Play,
  RefreshCw,
  Search,
} from "lucide-react";
import { api, ApiError } from "../../../api/http";
import { downloadRecordingCast } from "@/lib/download";
import { getCachedCastPreview, loadCastPreview } from "@/lib/castPreview";
import type { AgentInfo, RecordingInfo } from "../../../api/types";
import { useSwarmFeed } from "../../../hooks/useSwarmFeed";
import { Button } from "@/components/ui/button";
import { AgentChip } from "@/components/agent/AgentChip";
import { EmptyState } from "@/components/EmptyState";
import { resolveRole, roleDisplayName } from "@/lib/agent";
import { cn } from "@/lib/cn";
import { useWorkspaceContext } from "../Shell";

function formatTime(ms: number): string {
  return new Date(ms).toLocaleString();
}

function formatShortTime(ms: number): string {
  const d = new Date(ms);
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  return sameDay
    ? d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
    : d.toLocaleString([], {
        month: "numeric",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      });
}

function formatDuration(ms: number | null): string {
  if (ms == null) return "—";
  if (ms < 1000) return `${ms}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(1)}s`;
  const m = Math.floor(s / 60);
  const rem = (s - m * 60).toFixed(0);
  return `${m}m${rem}s`;
}

export default function ReplaysView() {
  const { t } = useTranslation();
  const { workspace } = useWorkspaceContext();
  const [searchParams, setSearchParams] = useSearchParams();
  const activeTag = searchParams.get("tag") ?? "all";
  const filter = searchParams.get("q") ?? "";

  const setActiveTag = (tag: string) => {
    setSearchParams(
      (prev) => {
        const next = new URLSearchParams(prev);
        if (tag === "all") next.delete("tag");
        else next.set("tag", tag);
        return next;
      },
      { replace: true },
    );
  };
  const setFilter = (q: string) => {
    setSearchParams(
      (prev) => {
        const next = new URLSearchParams(prev);
        if (!q) next.delete("q");
        else next.set("q", q);
        return next;
      },
      { replace: true },
    );
  };

  const [items, setItems] = useState<RecordingInfo[]>([]);
  // 录像 cross-ref：agent_id → role (给 AgentChip) + agent_id → workspace_id
  // (给 ws 过滤)。Shell 已有 allAliveAgents 但不含 killed agents 的历史
  // workspace_id，录像可能属于已退出的 agent，所以这里独立 listAgents 一次。
  const [agentWsIdById, setAgentWsIdById] = useState<Map<string, string>>(
    () => new Map(),
  );
  const [roleLookup, setRoleLookup] = useState<Map<string, string>>(
    () => new Map(),
  );
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  // 防重：refresh 在飞时不再发第二次请求（手动连点 / swarm 事件叠加）。
  const inFlightRef = useRef(false);
  // 陈旧响应守卫：每次 refresh 自增，setState 前校验仍是最新一次。
  const reqIdRef = useRef(0);

  const refresh = useCallback(async () => {
    if (inFlightRef.current) return;
    inFlightRef.current = true;
    const reqId = ++reqIdRef.current;
    setRefreshing(true);
    try {
      const [rows, agents] = await Promise.all([
        api.listRecordings(),
        api.listAgents(),
      ]);
      if (reqId !== reqIdRef.current) return;
      setItems(rows);
      const wsIdM = new Map<string, string>();
      const roleM = new Map<string, string>();
      for (const a of agents as AgentInfo[]) {
        if (a.workspace_id) wsIdM.set(a.agent_id, a.workspace_id);
        roleM.set(a.agent_id, a.role);
      }
      setAgentWsIdById(wsIdM);
      setRoleLookup(roleM);
      setError(null);
    } catch (e) {
      if (reqId !== reqIdRef.current) return;
      setError(e instanceof ApiError ? e.detail : (e as Error).message);
    } finally {
      if (reqId === reqIdRef.current) {
        setLoading(false);
        setRefreshing(false);
      }
      inFlightRef.current = false;
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // swarm exited 事件触发的 refresh 去抖：一次 orchestrator 结束往往连发多个
  // agent_state=exited，去抖合并成一次请求，避免 N 次重复拉列表。
  const debounceRef = useRef<number | null>(null);
  const debouncedRefresh = useCallback(() => {
    if (debounceRef.current !== null) window.clearTimeout(debounceRef.current);
    debounceRef.current = window.setTimeout(() => {
      debounceRef.current = null;
      refresh();
    }, 250);
  }, [refresh]);
  useEffect(
    () => () => {
      if (debounceRef.current !== null) window.clearTimeout(debounceRef.current);
    },
    [],
  );

  useSwarmFeed({
    onEvent: (ev) => {
      if (ev.type === "agent_state" && ev.state === "exited") debouncedRefresh();
    },
    onReconnect: () => refresh(),
  });

  const scopedToWorkspace = useMemo(() => {
    return items.filter((r) => {
      const wsId = agentWsIdById.get(r.agent_id);
      // P2-1：放宽过滤。属于本 workspace 的录像照常显示；agent 没有 ws 归属
      // (workspace_id 为 NULL，例如 MCP swarm_run_spell 直接拉起的 agent，或
      // agent 行已不在 listAgents 里) 的录像归到「未归属」，否则它们会从所有
      // workspace 视图里彻底消失，用户再也找不到。
      return wsId === workspace.workspaceId || wsId === undefined;
    });
  }, [items, workspace.workspaceId, agentWsIdById]);

  // 未归属录像（agent 无 workspace_id）单独成组，避免和本 ws 录像混淆。
  const unscopedIds = useMemo(() => {
    const s = new Set<string>();
    for (const r of items) {
      if (agentWsIdById.get(r.agent_id) === undefined) s.add(r.id);
    }
    return s;
  }, [items, agentWsIdById]);

  const tags = useMemo(() => {
    const set = new Set<string>();
    for (const r of scopedToWorkspace) set.add(resolveRole(r.agent_id, roleLookup));
    return ["all", ...Array.from(set).sort()];
  }, [scopedToWorkspace, roleLookup]);

  const filtered = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return scopedToWorkspace.filter((r) => {
      if (activeTag !== "all" && resolveRole(r.agent_id, roleLookup) !== activeTag)
        return false;
      if (
        q &&
        !r.agent_id.toLowerCase().includes(q) &&
        !r.id.toLowerCase().includes(q)
      ) {
        return false;
      }
      return true;
    });
  }, [scopedToWorkspace, filter, activeTag, roleLookup]);

  return (
    <div className="flex min-h-0 flex-1 flex-col bg-surface-primary">
      {/* sub-header: search + refresh + tag bar — Shell-level channel
          header already shows the workspace name / path / unread; this
          row is view-internal toolbar only. */}
      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-border-subtle bg-surface-secondary px-4 py-2 md:h-11 md:flex-nowrap md:px-5 md:py-0">
        <div className="flex h-8 min-w-0 flex-1 items-center gap-2 rounded-md bg-surface-primary px-3 transition-shadow focus-within:ring-2 focus-within:ring-ring/50 md:w-60 md:flex-none">
          <Search className="size-3.5 text-foreground-tertiary" />
          <input
            name="replay-search"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder={t("replays.search")}
            aria-label={t("replays.search")}
            className="min-h-8 min-w-0 flex-1 bg-transparent text-xs text-foreground-primary placeholder:text-foreground-tertiary focus:outline-none"
          />
        </div>
        <Button
          variant="ghost"
          size="icon"
          onClick={refresh}
          disabled={refreshing}
          title={t("common.refresh")}
          aria-label={t("common.refresh")}
          className="size-8 shrink-0"
        >
          <RefreshCw className={cn("size-3.5", refreshing && "animate-spin")} />
        </Button>
        <span className="hidden flex-1 md:block" />
        <div className="-mx-1 flex w-full items-center gap-1 overflow-x-auto px-1 pb-1 md:mx-0 md:w-auto md:overflow-visible md:px-0 md:pb-0">
          {tags.map((tag) => {
            const active = tag === activeTag;
            return (
              <Button
                key={tag}
                variant={active ? "default" : "outline"}
                size="sm"
                onClick={() => setActiveTag(tag)}
                className="h-8 shrink-0 rounded-full"
              >
                {tag === "all" ? t("replays.tagAll") : roleDisplayName(tag)}
              </Button>
            );
          })}
        </div>
        <span className="ml-auto w-full text-right font-caption text-xs text-foreground-tertiary md:ml-2 md:w-auto">
          {t("replays.ratio", {
            shown: filtered.length,
            total: scopedToWorkspace.length,
          })}
        </span>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto p-5">
        {/* P2-4：三态分流。loading / error+重试 / 真空，互斥渲染——
            error 时只显示「加载失败 + 重试」，绝不再叠「暂无录像」自相矛盾。 */}
        {loading && items.length === 0 ? (
          <div className="flex h-full flex-col items-center justify-center gap-2 text-foreground-tertiary">
            <RefreshCw className="size-6 animate-spin opacity-50" />
            <p className="font-caption text-sm">{t("common.loading")}</p>
          </div>
        ) : error ? (
          <div className="flex h-full flex-col items-center justify-center gap-3 text-foreground-tertiary">
            <p className="max-w-md text-center font-caption text-sm text-state-danger">
              {t("replays.loadFailed", { defaultValue: "加载录像失败" })}
            </p>
            <p className="max-w-md break-words text-center font-caption text-xs text-foreground-tertiary">
              {error}
            </p>
            <Button
              variant="outline"
              size="sm"
              onClick={refresh}
              disabled={refreshing}
              className="h-8"
            >
              <RefreshCw
                className={cn("mr-1.5 size-3.5", refreshing && "animate-spin")}
              />
              {t("common.retry", { defaultValue: "重试" })}
            </Button>
          </div>
        ) : filtered.length === 0 ? (
          scopedToWorkspace.length === 0 ? (
            <EmptyState
              icon={<Play className="size-8" />}
              title={t("replays.noRecordings")}
              hint={t("replays.noRecordingsHint")}
              primaryAction={{ label: t("replays.noRecordingsAction"), href: ".." }}
            />
          ) : (
            <EmptyState
              icon={<Search className="size-8" />}
              title={t("replays.empty")}
              hint={t("replays.emptyHint")}
            />
          )
        ) : (
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-3">
            {filtered.map((r) => {
              const live = r.finalized_at == null;
              return (
                <article
                  key={r.id}
                  className="group overflow-hidden rounded-lg border border-border-subtle bg-surface-elevated transition-shadow hover:shadow-lg"
                >
                  <div className="flex items-center gap-2 px-3 py-2">
                    <AgentChip
                      agentId={r.agent_id}
                      roleLookup={roleLookup}
                      size="sm"
                      showId={false}
                      title={roleDisplayName(resolveRole(r.agent_id, roleLookup))}
                      className="min-w-0 flex-1"
                    />
                    {unscopedIds.has(r.id) && (
                      <span
                        className="rounded-full bg-surface-tertiary px-2 py-0.5 font-caption text-[10px] text-foreground-tertiary"
                        title={t("replays.unscopedHint")}
                      >
                        {t("replays.unscoped", { defaultValue: "未归属" })}
                      </span>
                    )}
                    <span
                      className={cn(
                        "rounded-full px-2 py-0.5 font-caption text-[10px]",
                        live
                          ? "bg-status-running-soft text-status-running"
                          : "bg-surface-tertiary text-foreground-tertiary",
                      )}
                      title={
                        live
                          ? t("replays.live", { defaultValue: "● 实时" })
                          : t("replays.completed", { defaultValue: "○ 已完成" })
                      }
                    >
                      {live
                        ? t("replays.live", { defaultValue: "● 实时" })
                        : t("replays.completed", { defaultValue: "○ 已完成" })}
                    </span>
                  </div>

                  <Link
                    to={`/chat/${workspace.id}/replays/${encodeURIComponent(r.id)}`}
                    aria-label={`${t("replays.play")} · ${roleDisplayName(resolveRole(r.agent_id, roleLookup))}`}
                    className="group/thumb relative block h-32 overflow-hidden border-y border-[#1F1F1F] bg-term-bg"
                  >
                    <CastThumb recording={r} live={live} t={t} />
                    <div className="absolute inset-0 flex items-center justify-center bg-black/40 opacity-0 transition-opacity group-hover:opacity-100 group-focus-within/thumb:opacity-100">
                      <span className="flex items-center gap-1.5 rounded-full bg-accent-primary px-4 py-2 text-xs font-medium text-foreground-on-accent">
                        <Play className="size-3.5" />
                        {t("replays.play")}
                      </span>
                    </div>
                  </Link>

                  <div className="flex items-center gap-2 px-3 py-2 text-foreground-secondary">
                    <span
                      className="font-caption text-[11px]"
                      title={`${formatTime(r.started_at)} · ${r.id}`}
                    >
                      {formatShortTime(r.started_at)}
                      {r.duration_ms != null && (
                        <span className="ml-1.5 text-foreground-tertiary">
                          · {formatDuration(r.duration_ms)}
                        </span>
                      )}
                    </span>
                    <span className="flex-1" />
                    <Link
                      to={`/chat/${workspace.id}/replays/${encodeURIComponent(r.id)}`}
                      className="flex h-8 items-center gap-1 rounded-md bg-accent-primary px-2.5 text-xs text-foreground-on-accent hover:bg-accent-primary-deep"
                    >
                      <Play className="size-3" />
                      {t("replays.play")}
                    </Link>
                    <button
                      type="button"
                      onClick={() => downloadRecordingCast(r.id)}
                      className="flex h-8 items-center gap-1 rounded-md border border-border-subtle bg-surface-elevated px-2.5 text-xs hover:bg-surface-tertiary"
                      title={t("replays.download")}
                    >
                      <Download className="size-3" />
                      {t("replays.download")}
                    </button>
                  </div>
                </article>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

function CastThumb({
  recording: r,
  live,
  t,
}: {
  recording: RecordingInfo;
  live: boolean;
  t: (k: string, opts?: Record<string, unknown>) => string;
}) {
  // P1-11: render the recording's real first frames as the thumbnail (was: a
  // static Play icon — castPreview.ts existed but was never wired). Streams ~16
  // KB of the .cast, ANSI-strips it, caches by id. Falls back to the icon view
  // while loading / when empty (e.g. a live cast with nothing written yet).
  const [lines, setLines] = useState<string[] | null>(
    () => getCachedCastPreview(r.id) ?? null,
  );
  useEffect(() => {
    let alive = true;
    loadCastPreview(api.recordingCastUrl(r.id), r.id)
      .then((p) => {
        if (alive && p.length > 0) setLines(p);
      })
      .catch(() => {
        /* leave the icon fallback in place */
      });
    return () => {
      alive = false;
    };
  }, [r.id]);

  if (lines && lines.length > 0) {
    return (
      <div className="relative h-full overflow-hidden bg-[#0d0d0d]">
        <pre className="h-full overflow-hidden whitespace-pre-wrap break-all px-3 py-2 font-mono text-[8px] leading-[1.4] text-term-dim">
          {lines.join("\n")}
        </pre>
        <div className="absolute inset-0 flex items-center justify-center bg-gradient-to-t from-[#0d0d0d] via-transparent">
          <span
            className={cn(
              "flex size-10 items-center justify-center rounded-full backdrop-blur-sm",
              live
                ? "bg-status-running-soft/80 text-status-running"
                : "bg-black/55 text-white",
            )}
          >
            <Play className="size-5" />
          </span>
        </div>
        <div className="absolute bottom-1 right-2 flex items-center gap-1 font-mono text-[9px] text-term-dim">
          <Monitor className="size-2.5" />
          <span>{r.cols}×{r.rows}</span>
          {r.duration_ms != null && <span>· {formatDuration(r.duration_ms)}</span>}
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 px-4 py-3 text-term-fg">
      <div
        className={cn(
          "flex size-12 items-center justify-center rounded-full",
          live
            ? "bg-status-running-soft text-status-running"
            : "bg-[#1F1F1F] text-term-dim",
        )}
      >
        <Play className="size-6" />
      </div>
      <div className="flex items-center gap-2 font-mono text-[10px] text-term-dim">
        <Monitor className="size-3" />
        <span>{r.cols}×{r.rows}</span>
        {r.duration_ms != null && (
          <>
            <Clock className="size-3" />
            <span>{formatDuration(r.duration_ms)}</span>
          </>
        )}
      </div>
      <div className="font-caption text-[10px] uppercase tracking-wider text-term-dim">
        {live
          ? t("replays.recording", { defaultValue: "▶ 录制中…" })
          : t("replays.ready", { defaultValue: "✓ 可回放" })}
      </div>
    </div>
  );
}
