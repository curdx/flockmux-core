/**
 * BlackboardPanel — list paths, read + write markdown via /api/blackboard.
 *
 * Live updates: when the parent forwards a `blackboard_changed` event via
 * `liveChange`, we refresh the path list. If the changed path equals the
 * one we're currently editing AND the local buffer is clean, we silently
 * pull the new content; if dirty, we surface a warning instead of clobbering.
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import i18n from "@/i18n";
import { api, ApiError, networkErrorCopy } from "../api/http";
import { HTTP_BASE } from "../lib/apiBase";
import type { BlackboardEntry, BlackboardHistoryEntry } from "../api/types";
import {
  ConfirmActionDialog,
  type ConfirmActionState,
} from "@/components/ConfirmActionDialog";

// Single-file delete. The shared `api` client (../api/http) has no delete verb
// for blackboard and that module isn't ours to extend here, so we issue the
// DELETE directly while normalising failures into the SAME ApiError shape the
// rest of this panel already surfaces via `errText` — never a silent catch.
async function deleteBlackboard(path: string): Promise<void> {
  const url = `${HTTP_BASE}/api/blackboard/${path.split("/").map(encodeURIComponent).join("/")}`;
  let res: Response;
  try {
    res = await fetch(url, { method: "DELETE" });
  } catch (e) {
    const friendly = networkErrorCopy();
    // eslint-disable-next-line no-console
    console.warn("[swarmx] network error DELETE blackboard:", e);
    throw new ApiError(0, friendly, friendly);
  }
  if (!res.ok) {
    const raw = await res.text().catch(() => "");
    let detail = raw;
    try {
      const parsed = JSON.parse(raw);
      if (parsed && typeof parsed.error === "string") detail = parsed.error;
    } catch {
      /* not JSON — keep raw text */
    }
    throw new ApiError(res.status, detail, `DELETE ${url} → ${res.status}: ${detail || res.statusText}`);
  }
}

// History list is fetched with this cap; when we hit it the true count is
// unknown (could be more), so the badge shows "50+" rather than lying "50".
const HISTORY_LIMIT = 50;

function errText(e: unknown): string {
  return e instanceof ApiError ? e.detail : (e as Error).message;
}

interface Props {
  /** Latest swarm `blackboard_changed` event observed by the parent. */
  liveChange: { path: string; agent_id: string | null; op: string } | null;
}

export function BlackboardPanel({ liveChange }: Props) {
  const { t } = useTranslation();
  const [entries, setEntries] = useState<BlackboardEntry[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [content, setContent] = useState("");
  const [originalContent, setOriginalContent] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [info, setInfo] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [newPath, setNewPath] = useState("");
  const [historyOpen, setHistoryOpen] = useState(false);
  const [history, setHistory] = useState<BlackboardHistoryEntry[]>([]);
  const [historyLoading, setHistoryLoading] = useState(false);
  const [versionPreview, setVersionPreview] = useState<BlackboardHistoryEntry | null>(null);
  // M6d-4: HITL gate quick-actions. Only meaningful when the user has
  // `design.md` selected (the architect's draft from a `fullstack-
  // feature-gated` run). Approve writes `design.approved` with a
  // sentinel body; Reject opens an inline reason input and writes
  // `design.rejected` with `{"reason": ...}` (which architect is
  // subscribed to — it wakes and revises).
  const [rejectOpen, setRejectOpen] = useState(false);
  const [rejectReason, setRejectReason] = useState("");
  const [gateBusy, setGateBusy] = useState(false);
  const [discardConfirm, setDiscardConfirm] = useState<ConfirmActionState | null>(null);
  const [deleteConfirm, setDeleteConfirm] = useState<ConfirmActionState | null>(null);

  const isDirty = useMemo(() => content !== originalContent, [content, originalContent]);

  // P2-1: the info banner must not linger forever or bleed across files.
  // Success-class messages auto-clear after a few seconds; warnings (disk
  // conflict with unsaved edits) stay put until the user acts. Any new
  // showInfo / clearInfo cancels the previous timer so they never stack.
  const infoTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const clearInfo = () => {
    if (infoTimerRef.current) {
      clearTimeout(infoTimerRef.current);
      infoTimerRef.current = null;
    }
    setInfo(null);
  };
  const showInfo = (msg: string, opts?: { sticky?: boolean }) => {
    if (infoTimerRef.current) {
      clearTimeout(infoTimerRef.current);
      infoTimerRef.current = null;
    }
    setInfo(msg);
    if (!opts?.sticky) {
      infoTimerRef.current = setTimeout(() => {
        infoTimerRef.current = null;
        setInfo(null);
      }, 2500);
    }
  };
  // Clear any pending timer on unmount so it can't fire into a dead component.
  useEffect(
    () => () => {
      if (infoTimerRef.current) clearTimeout(infoTimerRef.current);
    },
    [],
  );

  const refreshList = async () => {
    try {
      const rows = await api.listBlackboard();
      setEntries(rows);
      setError(null);
    } catch (e) {
      setError(errText(e));
    }
  };

  const refreshHistory = async (path: string) => {
    setHistoryLoading(true);
    try {
      const rows = await api.listBlackboardHistory(path, HISTORY_LIMIT, false);
      setHistory(rows);
    } catch (e) {
      setError(errText(e));
    } finally {
      setHistoryLoading(false);
    }
  };

  const loadPath = async (path: string) => {
    setSelected(path);
    clearInfo();
    setVersionPreview(null);
    try {
      const snap = await api.readBlackboard(path);
      setContent(snap.content);
      setOriginalContent(snap.content);
      setError(null);
    } catch (e) {
      setError(errText(e));
      setContent("");
      setOriginalContent("");
    }
    // Best-effort: history count drives the toggle label; failures are
    // logged into `error` but don't block the editor.
    refreshHistory(path);
  };

  // P2-3: a single dirty-check gate. Any flow that would replace what the
  // editor shows (switching files, creating, previewing a past version)
  // must route through this — previously only list-click was guarded, so
  // create/history-preview silently discarded unsaved edits. `targetLabel`
  // is what the confirm dialog says the user is moving to. When clean, or
  // nothing is selected, the action runs immediately.
  const guardDirty = (targetLabel: string, action: () => void) => {
    if (isDirty && selected) {
      setDiscardConfirm({
        title: t("blackboard.discardConfirmTitle", {
          defaultValue: "放弃未保存的修改？",
        }),
        description: t("blackboard.discardConfirmDesc", {
          defaultValue: "“{{path}}”还有未保存内容，{{target}}会丢弃这些修改。",
          path: selected,
          target: targetLabel,
        }),
        confirmLabel: t("blackboard.discardConfirmAction", {
          defaultValue: "放弃修改",
        }),
        variant: "destructive",
        onConfirm: action,
      });
      return;
    }
    action();
  };

  const openPath = (path: string) => {
    if (selected === path) {
      void loadPath(path);
      return;
    }
    guardDirty(
      t("blackboard.switchTo", { defaultValue: "切换到“{{path}}”", path }),
      () => void loadPath(path),
    );
  };

  const openVersion = (entry: BlackboardHistoryEntry) => {
    guardDirty(t("blackboard.viewHistoryVersion", { defaultValue: "查看历史版本" }), () => {
      // The list call strips content by default; fetch the full row for the
      // selected version. We re-query the same endpoint with include_content
      // so each click is at most one byte-heavy request.
      setVersionPreview(entry);
      api
        .listBlackboardHistory(entry.path, 200, true)
        .then((rows) => {
          const full = rows.find((r) => r.id === entry.id);
          if (full) setVersionPreview(full);
        })
        .catch((e) => setError(errText(e)));
    });
  };

  const save = async () => {
    if (!selected) return;
    setSaving(true);
    try {
      await api.writeBlackboard(selected, { content });
      setOriginalContent(content);
      showInfo(t("blackboard.savedInfo", { defaultValue: "已保存 {{path}}", path: selected }));
      await refreshList();
      setError(null);
    } catch (e) {
      setError(errText(e));
    } finally {
      setSaving(false);
    }
  };

  const createNew = async () => {
    const path = newPath.trim();
    if (!path) return;
    // P0-8: "new" must never clobber an existing file. writeBlackboard is a
    // blind overwrite, so creating a name that already exists would blank a
    // file other agents depend on (design.md, *.ledger.md). If it already
    // exists, just open it instead of writing empty content over it.
    if (entries.some((e) => e.path === path)) {
      // Opening an existing file replaces the editor → route through the
      // dirty guard like any other switch (P2-3).
      guardDirty(t("blackboard.openLabel", { defaultValue: "打开“{{path}}”", path }), () => {
        setNewPath("");
        void loadPath(path).then(() => {
          setError(
            t("blackboard.alreadyExists", {
              defaultValue: "「{{path}}」已存在，已为你打开（未覆盖）",
              path,
            }),
          );
        });
      });
      return;
    }
    // Creating also replaces the editor with the fresh empty file (P2-3).
    guardDirty(t("blackboard.createLabel", { defaultValue: "新建“{{path}}”", path }), () => {
      void (async () => {
        try {
          await api.writeBlackboard(path, { content: "" });
          setNewPath("");
          await refreshList();
          await loadPath(path);
          setError(null);
        } catch (e) {
          setError(errText(e));
        }
      })();
    });
  };

  // Single-file delete, gated behind a second-confirm dialog (mistaken
  // deletes are unrecoverable from the UI). On success we clear the editor
  // and refresh the list; on failure we surface the ApiError via `error`
  // (toast-equivalent inline banner) — never a silent catch.
  const removeSelected = () => {
    if (!selected || deleting) return;
    const path = selected;
    setDeleteConfirm({
      title: t("blackboard.deleteConfirmTitle", { defaultValue: "删除该文件？" }),
      description: t("blackboard.deleteConfirmDesc", {
        defaultValue: "“{{path}}”将从共享区移除，此操作无法撤销。",
        path,
      }),
      confirmLabel: t("blackboard.deleteConfirmAction", { defaultValue: "删除文件" }),
      variant: "destructive",
      onConfirm: () => {
        void (async () => {
          setDeleting(true);
          try {
            await deleteBlackboard(path);
            // Clear the editor for the now-gone file.
            setSelected(null);
            setContent("");
            setOriginalContent("");
            setHistory([]);
            setVersionPreview(null);
            clearInfo();
            await refreshList();
            setError(null);
          } catch (e) {
            setError(errText(e));
          } finally {
            setDeleting(false);
          }
        })();
      },
    });
  };

  // M6d-4: write the architect-approval companion to whatever the
  // operator just read. Body is intentionally minimal — the gate
  // mechanism only cares that the key exists with non-empty content;
  // anything more is for human eyes.
  const approveDesign = async () => {
    setGateBusy(true);
    try {
      await api.writeBlackboard("design.approved", {
        content: "approved via UI",
      });
      setError(null);
      await refreshList();
      // showInfo only after refreshList confirms the write is visible (was:
      // claimed before the round-trip). P1-30: state only the written fact —
      // whether agents actually wake depends on wake.rs/subscriptions, so don't
      // package that future as a done deal.
      showInfo(
        t("blackboard.approvedInfo", {
          defaultValue: "已写入通过标记 design.approved · 等待 agent 据此推进",
        }),
      );
    } catch (e) {
      setError(errText(e));
    } finally {
      setGateBusy(false);
    }
  };

  // M6d-4: companion to approveDesign for the M6d-2 revision loop.
  // The body MUST be the JSON shape the architect's prompt expects
  // (`{"reason": "..."}`) — architect's wake handler reads it and
  // rewrites design.md addressing the feedback.
  const rejectDesign = async () => {
    const reason = rejectReason.trim();
    if (!reason) return;
    setGateBusy(true);
    try {
      await api.writeBlackboard("design.rejected", {
        content: JSON.stringify({ reason }, null, 2),
      });
      setError(null);
      setRejectOpen(false);
      setRejectReason("");
      await refreshList();
      // Claim success only AFTER refreshList confirms the write actually
      // landed (was: setInfo fired before the round-trip, asserting a
      // persistence that hadn't been verified). P1-30: state only the written
      // fact — whether the architect actually revises depends on wake.rs/its
      // subscription, so don't promise that future as already happened.
      showInfo(
        t("blackboard.rejectedInfo", {
          defaultValue: "已写入拒绝标记 design.rejected · 等待 architect 据此推进",
        }),
      );
    } catch (e) {
      setError(errText(e));
    } finally {
      setGateBusy(false);
    }
  };

  useEffect(() => {
    refreshList();
  }, []);

  useEffect(() => {
    if (!liveChange) return;
    refreshList();
    if (selected && liveChange.path === selected) {
      refreshHistory(selected);
      if (isDirty) {
        // Sticky: this warns the user their unsaved edits now diverge from
        // disk — it must persist until they act, not vanish on a timer.
        showInfo(
          t("blackboard.diskChangedWarn", {
            defaultValue: "⚠ {{path}} 在磁盘上发生变化 (op={{op}}) — 你有未保存的修改",
            path: liveChange.path,
            op: liveChange.op,
          }),
          { sticky: true },
        );
      } else {
        // Silently refresh the buffer.
        api
          .readBlackboard(selected)
          .then((snap) => {
            setContent(snap.content);
            setOriginalContent(snap.content);
            showInfo(
              t("blackboard.refreshedInfo", {
                defaultValue: "已根据 {{op}} 刷新",
                op: liveChange.op,
              }),
            );
          })
          .catch(() => {
            /* ignore */
          });
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [liveChange]);

  return (
    <>
    <div style={{ display: "flex", height: "100%", minHeight: 0 }}>
      <div style={leftCol}>
        <div style={headerRow}>
          <input
            name="blackboard-new-path"
            value={newPath}
            onChange={(e) => setNewPath(e.target.value)}
            placeholder={t("blackboard.newPathPlaceholder", {
              defaultValue: "新文件路径（如 tasks.md）",
            })}
            style={{ ...input, flex: 1 }}
            onKeyDown={(e) => {
              if (e.key === "Enter") createNew();
            }}
          />
          <button
            onClick={createNew}
            disabled={!newPath.trim()}
            title={t("blackboard.createTitle", { defaultValue: "创建" })}
            aria-label={t("blackboard.createFile", { defaultValue: "新建文件" })}
          >
            +
          </button>
          <button
            onClick={refreshList}
            title={t("blackboard.refreshTitle", { defaultValue: "刷新" })}
            aria-label={t("blackboard.refresh", { defaultValue: "刷新列表" })}
          >
            ↻
          </button>
        </div>
        <div style={pathList}>
          {entries.length === 0 && (
            <div style={emptyHint}>
              {t("blackboard.emptyFiles", { defaultValue: "暂无文件" })}
            </div>
          )}
          {entries.map((e) => (
            <button
              key={e.path}
              onClick={() => openPath(e.path)}
              style={{
                ...pathRow,
                background: e.path === selected ? "#1e3a8a" : "transparent",
              }}
              title={`${e.op} · ${formatTime(e.at)} · ${e.sha256.slice(0, 8)}`}
            >
              {e.path}
            </button>
          ))}
        </div>
      </div>

      <div style={rightCol}>
        {selected ? (
          <>
            <div style={headerRow}>
              <span style={{ flex: 1, fontSize: 12, color: "#cbd5f5" }}>
                {selected}{" "}
                {isDirty && (
                  <em style={{ color: "#fbbf24" }}>
                    {t("blackboard.unsavedTag", { defaultValue: "(未保存)" })}
                  </em>
                )}
              </span>
              {selected === "design.md" && (
                <>
                  <button
                    onClick={approveDesign}
                    disabled={gateBusy}
                    title={t("blackboard.approveTitle", {
                      defaultValue: "批准该设计 — 写入 design.approved，唤醒前后端",
                    })}
                    aria-label={t("blackboard.approveDesign", { defaultValue: "批准设计" })}
                    style={approveBtn}
                  >
                    {t("blackboard.approveBtn", { defaultValue: "✓ 通过" })}
                  </button>
                  <button
                    onClick={() => setRejectOpen((v) => !v)}
                    disabled={gateBusy}
                    title={t("blackboard.rejectTitle", {
                      defaultValue: "要求重做 — 写入 design.rejected 并附原因；architect 会重新起稿",
                    })}
                    aria-label={t("blackboard.rejectDesign", { defaultValue: "驳回设计" })}
                    style={rejectBtn}
                  >
                    {t("blackboard.rejectBtn", { defaultValue: "✗ 驳回" })}
                  </button>
                </>
              )}
              <button
                onClick={() => setHistoryOpen((v) => !v)}
                title={t("blackboard.viewHistory", { defaultValue: "查看写入历史" })}
                aria-label={t("blackboard.viewHistory", { defaultValue: "查看写入历史" })}
                style={historyToggle}
              >
                {t("blackboard.historyLabel", {
                  defaultValue: "历史 ({{count}}{{loading}})",
                  count: history.length >= HISTORY_LIMIT ? `${HISTORY_LIMIT}+` : history.length,
                  loading: historyLoading ? "…" : "",
                  interpolation: { escapeValue: false },
                })}
              </button>
              <button
                onClick={save}
                disabled={saving || !isDirty}
                aria-label={t("blackboard.save", { defaultValue: "保存" })}
              >
                {t("blackboard.save", { defaultValue: "保存" })}
              </button>
              <button
                onClick={removeSelected}
                disabled={deleting}
                title={t("blackboard.deleteFileTitle", { defaultValue: "删除该文件" })}
                aria-label={t("blackboard.deleteFile", { defaultValue: "删除文件" })}
                style={deleteBtn}
              >
                {t("blackboard.deleteShort", { defaultValue: "删除" })}
              </button>
            </div>
            {selected === "design.md" && rejectOpen && (
              <div style={rejectInlineRow}>
                <input
                  name="blackboard-reject-reason"
                  type="text"
                  value={rejectReason}
                  onChange={(e) => setRejectReason(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && rejectReason.trim()) {
                      rejectDesign();
                    } else if (e.key === "Escape") {
                      setRejectOpen(false);
                      setRejectReason("");
                    }
                  }}
                  placeholder={t("blackboard.rejectReasonPlaceholder", {
                    defaultValue: "驳回原因（一句话）— architect 会读这条并改稿",
                  })}
                  style={rejectInput}
                  autoFocus
                />
                <button
                  onClick={rejectDesign}
                  disabled={gateBusy || !rejectReason.trim()}
                  style={rejectConfirmBtn}
                >
                  {t("blackboard.sendReject", { defaultValue: "发送驳回" })}
                </button>
                <button
                  onClick={() => {
                    setRejectOpen(false);
                    setRejectReason("");
                  }}
                  style={historyToggle}
                >
                  {t("blackboard.cancel", { defaultValue: "取消" })}
                </button>
              </div>
            )}
            {historyOpen && (
              <div style={historyDrawer}>
                {history.length === 0 && (
                  <div style={{ ...emptyHint, marginTop: 4 }}>
                    {t("blackboard.emptyHistory", { defaultValue: "暂无历史" })}
                  </div>
                )}
                {history.map((h) => {
                  const isPreview = versionPreview?.id === h.id;
                  return (
                    <button
                      key={h.id}
                      onClick={() => openVersion(h)}
                      style={{
                        ...historyRow,
                        background: isPreview ? "#1e3a8a" : "transparent",
                      }}
                      title={new Date(h.at).toLocaleString()}
                    >
                      <span style={{ color: "#fbbf24" }}>
                        {h.sha256.slice(0, 12)}
                      </span>
                      <span style={{ color: "#94a3b8", marginLeft: 6 }}>
                        {h.agent_id ??
                          t("blackboard.externalSource", { defaultValue: "外部" })}
                      </span>
                      <span style={{ color: "#64748b", marginLeft: 6 }}>
                        {h.op}
                      </span>
                      <span style={{ color: "#64748b", marginLeft: 6 }}>
                        {formatRelative(h.at)}
                      </span>
                    </button>
                  );
                })}
              </div>
            )}
            {info && <div style={infoRow}>{info}</div>}
            {versionPreview ? (
              <>
                <div style={versionHeader}>
                  <span style={{ flex: 1 }}>
                    {t("blackboard.viewingVersion", {
                      defaultValue: "正在查看版本 {{sha}}…（{{source}} · {{time}}）",
                      sha: versionPreview.sha256.slice(0, 12),
                      source:
                        versionPreview.agent_id ??
                        t("blackboard.externalSource", { defaultValue: "外部" }),
                      time: new Date(versionPreview.at).toLocaleString(),
                      interpolation: { escapeValue: false },
                    })}
                  </span>
                  <button onClick={() => setVersionPreview(null)}>
                    {t("blackboard.close", { defaultValue: "关闭" })}
                  </button>
                </div>
                <textarea
                  value={
                    versionPreview.content ??
                    t("blackboard.contentNotLoaded", { defaultValue: "(尚未加载内容)" })
                  }
                  readOnly
                  aria-label={t("blackboard.versionPreviewArea", {
                    defaultValue: "历史版本内容（只读）",
                  })}
                  style={{ ...editor, background: "#101a2f" }}
                  spellCheck={false}
                />
              </>
            ) : (
              <textarea
                value={content}
                onChange={(e) => {
                  // Editing invalidates any lingering success banner
                  // ("已保存" / "已根据…刷新") so it can't claim a stale state.
                  if (info) clearInfo();
                  setContent(e.target.value);
                }}
                aria-label={t("blackboard.editorArea", {
                  defaultValue: "文件内容编辑器",
                })}
                style={editor}
                spellCheck={false}
              />
            )}
          </>
        ) : (
          <div style={emptyHint}>
            {t("blackboard.selectFileHint", {
              defaultValue: "请在左侧选择一个文件",
            })}
          </div>
        )}
        {error && <div style={errorRow}>{error}</div>}
      </div>
    </div>
    <ConfirmActionDialog
      action={discardConfirm}
      onOpenChange={(open) => {
        if (!open) setDiscardConfirm(null);
      }}
    />
    <ConfirmActionDialog
      action={deleteConfirm}
      onOpenChange={(open) => {
        if (!open) setDeleteConfirm(null);
      }}
    />
    </>
  );
}

function formatRelative(ms: number): string {
  const diff = Date.now() - ms;
  if (diff < 60_000) return i18n.t("blackboard.relJustNow", { defaultValue: "刚刚" });
  if (diff < 3600_000)
    return i18n.t("blackboard.relMinutes", {
      defaultValue: "{{n}} 分钟前",
      n: Math.floor(diff / 60_000),
    });
  if (diff < 86400_000)
    return i18n.t("blackboard.relHours", {
      defaultValue: "{{n}} 小时前",
      n: Math.floor(diff / 3600_000),
    });
  return i18n.t("blackboard.relDays", {
    defaultValue: "{{n}} 天前",
    n: Math.floor(diff / 86400_000),
  });
}

function formatTime(ms: number): string {
  return new Date(ms).toLocaleString();
}

const leftCol: React.CSSProperties = {
  width: 160,
  display: "flex",
  flexDirection: "column",
  borderRight: "1px solid #374151",
  minHeight: 0,
};

const rightCol: React.CSSProperties = {
  flex: 1,
  display: "flex",
  flexDirection: "column",
  minWidth: 0,
  minHeight: 0,
};

const headerRow: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 4,
  padding: "6px 8px",
  borderBottom: "1px solid #374151",
};

const pathList: React.CSSProperties = {
  flex: 1,
  overflowY: "auto",
  display: "flex",
  flexDirection: "column",
};

const pathRow: React.CSSProperties = {
  textAlign: "left",
  padding: "4px 8px",
  fontSize: 12,
  border: "none",
  borderBottom: "1px solid #1f2937",
  color: "#cbd5f5",
  cursor: "pointer",
  whiteSpace: "nowrap",
  overflow: "hidden",
  textOverflow: "ellipsis",
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

const editor: React.CSSProperties = {
  flex: 1,
  background: "#0b1220",
  color: "#e2e8f0",
  border: "none",
  padding: "8px",
  fontSize: 12,
  fontFamily:
    "ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace",
  resize: "none",
  outline: "none",
  minHeight: 0,
};

const emptyHint: React.CSSProperties = {
  color: "#64748b",
  fontSize: 12,
  textAlign: "center",
  marginTop: 16,
  padding: "0 8px",
};

const errorRow: React.CSSProperties = {
  color: "#fca5a5",
  fontSize: 11,
  padding: "4px 8px",
  background: "#1f2937",
};

const infoRow: React.CSSProperties = {
  color: "#86efac",
  fontSize: 11,
  padding: "4px 8px",
  background: "#1f2937",
};

const historyToggle: React.CSSProperties = {
  background: "transparent",
  color: "#94a3b8",
  border: "1px solid #374151",
  borderRadius: 4,
  fontSize: 11,
  padding: "2px 6px",
  cursor: "pointer",
};

// M6d-4: HITL gate quick-action buttons. Approve is green (the
// loud-and-positive default action when the operator's read the
// design and is happy). Reject is a quieter outline-only red — the
// user is asking for a revision, not erroring out, so it shouldn't
// look like a destructive action.
const approveBtn: React.CSSProperties = {
  background: "#16a34a",
  color: "#fff",
  border: "1px solid #15803d",
  borderRadius: 4,
  fontSize: 11,
  fontWeight: 600,
  padding: "2px 8px",
  cursor: "pointer",
};

const rejectBtn: React.CSSProperties = {
  background: "transparent",
  color: "#f87171",
  border: "1px solid #b91c1c",
  borderRadius: 4,
  fontSize: 11,
  fontWeight: 600,
  padding: "2px 8px",
  cursor: "pointer",
};

// Delete is a quiet red outline (not a loud filled button): it's a
// secondary, destructive action gated behind a confirm dialog, so it
// should read as "available but careful", matching rejectBtn's restraint.
const deleteBtn: React.CSSProperties = {
  background: "transparent",
  color: "#f87171",
  border: "1px solid #b91c1c",
  borderRadius: 4,
  fontSize: 11,
  padding: "2px 8px",
  cursor: "pointer",
};

const rejectInlineRow: React.CSSProperties = {
  display: "flex",
  gap: 6,
  padding: "6px 8px",
  borderBottom: "1px solid #374151",
  background: "#1a0f0f",
  alignItems: "center",
};

const rejectInput: React.CSSProperties = {
  flex: 1,
  background: "#0b1220",
  color: "#e2e8f0",
  border: "1px solid #b91c1c",
  borderRadius: 4,
  padding: "3px 6px",
  fontSize: 12,
  fontFamily: "inherit",
};

const rejectConfirmBtn: React.CSSProperties = {
  background: "#b91c1c",
  color: "#fff",
  border: "1px solid #7f1d1d",
  borderRadius: 4,
  fontSize: 11,
  fontWeight: 600,
  padding: "2px 8px",
  cursor: "pointer",
};

const historyDrawer: React.CSSProperties = {
  borderBottom: "1px solid #374151",
  maxHeight: 180,
  overflowY: "auto",
  background: "#0b1220",
  display: "flex",
  flexDirection: "column",
};

const historyRow: React.CSSProperties = {
  textAlign: "left",
  border: "none",
  borderBottom: "1px solid #1f2937",
  padding: "4px 8px",
  fontSize: 11,
  color: "#cbd5f5",
  cursor: "pointer",
  fontFamily:
    "ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace",
};

const versionHeader: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 4,
  padding: "4px 8px",
  background: "#1f2937",
  fontSize: 11,
  color: "#cbd5f5",
  borderBottom: "1px solid #374151",
};
