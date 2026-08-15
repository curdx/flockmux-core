/**
 * RecordingsPanel — lists asciicast v2 files captured by swarmx-recorder.
 *
 * Each row exposes a "▶ play" toggle that expands an inline asciinema-player
 * (WASM-backed terminal session player from the official npm package). The
 * "raw .cast" / "download" links remain as escape hatches for offline
 * inspection. Only one player is mounted per row at a time; closing the row
 * disposes the player's WASM/canvas resources via the wrapper's cleanup.
 */

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, ApiError } from "../api/http";
import { downloadRecordingCast } from "@/lib/download";
import type { RecordingInfo } from "../api/types";
import { AsciicastPlayer } from "./AsciicastPlayer";

interface Props {
  /** Bump this whenever the parent sees a swarm event that may affect the
   *  recording list (e.g. `agent_state=exited` ⇒ a recording may have just
   *  been finalized). */
  refreshTick: number;
}

export function RecordingsPanel({ refreshTick }: Props) {
  const { t } = useTranslation();
  const [items, setItems] = useState<RecordingInfo[]>([]);
  const [filter, setFilter] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  // M6e: 录像现在弹全屏 modal 播放（而不是在 360px 侧栏里展开成
  // 蚂蚁字大小）。playingId = 当前要全屏播放的录像 id；null = 关闭。
  // 单实例：每个 player 拿一个 WASM context，开多个浪费资源。
  const [playingId, setPlayingId] = useState<string | null>(null);
  // 防重 + 陈旧响应守卫：refresh 在飞时不再发第二次；setState 前校验仍是最新。
  const inFlightRef = useRef(false);
  const reqIdRef = useRef(0);
  // 最新 filter 给 refresh 闭包读（手动「↻」要用当前过滤词，但不想把 filter
  // 进 refresh 的依赖否则每次输入都重建函数）。
  const filterRef = useRef(filter);
  filterRef.current = filter;

  const refresh = async () => {
    if (inFlightRef.current) return;
    inFlightRef.current = true;
    const reqId = ++reqIdRef.current;
    setRefreshing(true);
    try {
      const q = filterRef.current.trim();
      const rows = await api.listRecordings(q ? q : undefined);
      if (reqId !== reqIdRef.current) return;
      setItems(rows);
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
  };

  // refreshTick 来自父组件：每个 agent_state=exited 都 +1，一次 orchestrator
  // 收尾连发多条会把 tick 连跳几次。这里去抖 250ms 合并成一次拉取，避免重复
  // 打 /api/recordings。
  useEffect(() => {
    const h = window.setTimeout(() => {
      refresh();
    }, 250);
    return () => window.clearTimeout(h);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [refreshTick]);

  // M6e: Esc 关闭全屏播放器
  useEffect(() => {
    if (!playingId) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setPlayingId(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [playingId]);

  const playing = playingId ? items.find((r) => r.id === playingId) : null;

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div style={headerRow}>
        <input
          name="recording-agent-filter"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder={t("recordings.filterPlaceholder")}
          style={{ ...input, flex: 1 }}
          onKeyDown={(e) => {
            if (e.key === "Enter") refresh();
          }}
        />
        <button
          onClick={refresh}
          title={t("recordings.refresh", { defaultValue: "刷新" })}
          disabled={refreshing}
          style={refreshing ? { ...refreshBtn, ...refreshBtnBusy } : refreshBtn}
        >
          <span
            className={refreshing ? "animate-spin" : undefined}
            style={{ display: "inline-block" }}
          >
            ↻
          </span>
        </button>
      </div>
      {/* 三态分流：error 时只显示「加载失败 + 重试」，不再叠「暂无录像」自相矛盾。*/}
      <div style={listStyle}>
        {loading && items.length === 0 ? (
          <div style={emptyHint}>
            {t("recordings.loading", { defaultValue: "加载中…" })}
          </div>
        ) : error ? (
          <div style={errorBlock}>
            <div style={errorTitle}>
              {t("recordings.loadFailed", { defaultValue: "加载录像失败" })}
            </div>
            <div style={errorDetail}>{error}</div>
            <button onClick={refresh} disabled={refreshing} style={retryBtn}>
              {refreshing
                ? t("recordings.retrying", { defaultValue: "重试中…" })
                : t("recordings.retry", { defaultValue: "重试" })}
            </button>
          </div>
        ) : items.length === 0 ? (
          <div style={emptyHint}>
            {t("recordings.empty", { defaultValue: "暂无录像" })}
          </div>
        ) : (
          items.map((r) => {
          const live = r.finalized_at == null;
          return (
            <div key={r.id} style={row}>
              <div style={{ display: "flex", justifyContent: "space-between" }}>
                <span style={{ color: "#cbd5f5", fontSize: 12 }}>
                  <strong>{r.id}</strong>{" "}
                  <span style={{ color: "#64748b" }}>· {r.agent_id}</span>
                </span>
                <span
                  style={{
                    fontSize: 10,
                    color: live ? "#fbbf24" : "#86efac",
                  }}
                >
                  {live
                    ? t("recordings.badgeLive", { defaultValue: "● 实时" })
                    : t("recordings.badgeDone", { defaultValue: "○ 已完结" })}
                </span>
              </div>
              <div style={{ fontSize: 10, color: "#94a3b8" }}>
                {t("recordings.startedAt", {
                  defaultValue: "开始于 {{time}}",
                  time: formatTime(r.started_at),
                })}{" "}
                · {r.cols}×{r.rows}
                {r.duration_ms != null && (
                  <> · {formatDuration(r.duration_ms)}</>
                )}
                {r.last_seq != null && <> · {r.last_seq} B</>}
              </div>
              <div style={{ display: "flex", gap: 4, marginTop: 4 }}>
                <button
                  onClick={() => setPlayingId(r.id)}
                  style={linkButton}
                  title={
                    live
                      ? t("recordings.playTitleLive", {
                          defaultValue:
                            "实时录像可以回放，但只能播到已写入的字节为止",
                        })
                      : t("recordings.playTitleDone", {
                          defaultValue: "回放这条录像（全屏）",
                        })
                  }
                >
                  {t("recordings.play", { defaultValue: "▶ 播放" })}
                </button>
                <a
                  href={api.recordingCastUrl(r.id)}
                  target="_blank"
                  rel="noreferrer"
                  style={linkButton}
                >
                  {t("recordings.rawCast", { defaultValue: "原始 .cast" })}
                </a>
                <button
                  type="button"
                  onClick={() => downloadRecordingCast(r.id)}
                  style={linkButton}
                >
                  {t("recordings.download", { defaultValue: "下载" })}
                </button>
              </div>
            </div>
          );
          })
        )}
      </div>
      {/* M6e: 全屏 modal 播放器。塞在 360px 侧栏里 120 列文字会变成蚂蚁，
          点 ▶ 播放后改成铺满主区域的 overlay；按 Esc 或点右上 × 关掉。 */}
      {playing && (
        <div style={modalBackdrop} onClick={() => setPlayingId(null)}>
          <div style={modalCard} onClick={(e) => e.stopPropagation()}>
            <div style={modalHeader}>
              <span style={{ flex: 1, color: "#cbd5f5", fontSize: 13 }}>
                <strong>{playing.id}</strong>
                <span style={{ color: "#64748b", marginLeft: 8 }}>
                  · {playing.agent_id} · {playing.cols}×{playing.rows}
                </span>
                {playing.duration_ms != null && (
                  <span style={{ color: "#64748b", marginLeft: 8 }}>
                    · {formatDuration(playing.duration_ms)}
                  </span>
                )}
              </span>
              <button
                onClick={() => setPlayingId(null)}
                style={modalCloseBtn}
                title={t("recordings.closeEsc", { defaultValue: "关闭（Esc）" })}
              >
                {t("recordings.close", { defaultValue: "× 关闭" })}
              </button>
            </div>
            {playing.finalized_at == null && (
              // P2-3：live 录像还在写，回放只能播到「打开时已落盘的字节」为止，
              // 到尾部会无预兆截断。提前提示「录制中 / 内容不完整」。
              <div style={liveTruncatedHint}>
                {t("recordings.liveTruncatedHint", {
                  defaultValue:
                    "● 录制中，内容不完整：回放只到当前已落盘的部分，结尾可能突然截断。完结后重新打开可看到完整录像。",
                })}
              </div>
            )}
            <div style={modalPlayerHost}>
              <AsciicastPlayer
                src={api.recordingCastUrl(playing.id)}
                cols={playing.cols}
                rows={playing.rows}
                autoPlay
              />
            </div>
            <div style={modalFooterHint}>
              {t("recordings.shortcutsLabel", { defaultValue: "快捷键：" })}
              <kbd>{t("recordings.keySpace", { defaultValue: "空格" })}</kbd>{" "}
              {t("recordings.shortcutPlayPause", { defaultValue: "播放/暂停" })} ·{" "}
              <kbd>f</kbd>{" "}
              {t("recordings.shortcutFullscreen", { defaultValue: "全屏" })} ·{" "}
              <kbd>Esc</kbd>{" "}
              {t("recordings.shortcutClose", { defaultValue: "关闭" })}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function formatTime(ms: number): string {
  return new Date(ms).toLocaleString();
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(1)}s`;
  const m = Math.floor(s / 60);
  const rem = (s - m * 60).toFixed(0);
  return `${m}m${rem}s`;
}

const headerRow: React.CSSProperties = {
  display: "flex",
  gap: 4,
  padding: "6px 8px",
  borderBottom: "1px solid #374151",
};

const listStyle: React.CSSProperties = {
  flex: 1,
  overflowY: "auto",
  padding: "6px 8px",
  display: "flex",
  flexDirection: "column",
  gap: 6,
  minHeight: 0,
};

const row: React.CSSProperties = {
  borderLeft: "2px solid #374151",
  paddingLeft: 6,
};

const input: React.CSSProperties = {
  background: "#0b1220",
  color: "#e2e8f0",
  border: "1px solid #374151",
  borderRadius: 4,
  padding: "4px 6px",
  fontSize: 12,
  fontFamily: "inherit",
};

const linkButton: React.CSSProperties = {
  fontSize: 11,
  padding: "2px 6px",
  background: "#1f2937",
  color: "#cbd5f5",
  border: "1px solid #374151",
  borderRadius: 4,
  textDecoration: "none",
};

const emptyHint: React.CSSProperties = {
  color: "#64748b",
  fontSize: 12,
  textAlign: "center",
  marginTop: 16,
};

// P2-4：错误态独立成块（标题 + 详情 + 重试），不和「暂无录像」空态共存。
const errorBlock: React.CSSProperties = {
  display: "flex",
  flexDirection: "column",
  alignItems: "center",
  gap: 8,
  marginTop: 24,
  padding: "0 12px",
  textAlign: "center",
};

const errorTitle: React.CSSProperties = {
  color: "#fca5a5",
  fontSize: 13,
};

const errorDetail: React.CSSProperties = {
  color: "#94a3b8",
  fontSize: 11,
  wordBreak: "break-word",
};

const retryBtn: React.CSSProperties = {
  fontSize: 12,
  padding: "4px 12px",
  background: "#1f2937",
  color: "#cbd5f5",
  border: "1px solid #374151",
  borderRadius: 4,
  cursor: "pointer",
};

const refreshBtn: React.CSSProperties = {
  cursor: "pointer",
};

const refreshBtnBusy: React.CSSProperties = {
  cursor: "default",
  opacity: 0.6,
};

// P2-3：live 录像回放截断提示条（modal 内，header 下方）。
const liveTruncatedHint: React.CSSProperties = {
  fontSize: 11,
  lineHeight: 1.5,
  color: "#fbbf24",
  padding: "6px 14px",
  background: "rgba(251, 191, 36, 0.08)",
  borderBottom: "1px solid #374151",
};

// M6e: 全屏播放 modal 的 backdrop —— position: fixed 覆盖整个 viewport，
// z-index 提到最高（pane 终端 + 各种侧栏都在下面），点击空白处关闭。
const modalBackdrop: React.CSSProperties = {
  position: "fixed",
  inset: 0,
  background: "rgba(0, 0, 0, 0.7)",
  zIndex: 9999,
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
  padding: 24,
};

const modalCard: React.CSSProperties = {
  background: "#0b1220",
  border: "1px solid #374151",
  borderRadius: 8,
  width: "min(1200px, 95vw)",
  maxHeight: "90vh",
  display: "flex",
  flexDirection: "column",
  overflow: "hidden",
};

const modalHeader: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 8,
  padding: "10px 14px",
  borderBottom: "1px solid #374151",
  background: "#111827",
};

const modalCloseBtn: React.CSSProperties = {
  background: "transparent",
  color: "#cbd5f5",
  border: "1px solid #374151",
  borderRadius: 4,
  padding: "4px 10px",
  fontSize: 12,
  cursor: "pointer",
};

const modalPlayerHost: React.CSSProperties = {
  flex: 1,
  minHeight: 0,
  padding: 16,
  background: "#0d0d0d",
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
  overflow: "auto",
};

const modalFooterHint: React.CSSProperties = {
  fontSize: 11,
  color: "#64748b",
  padding: "8px 14px",
  borderTop: "1px solid #374151",
  background: "#111827",
};
