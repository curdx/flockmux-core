/**
 * MergeDialog — fold a direction (worktree branch) back into the main line.
 *
 * Flow: open → preview "what this direction changed" (GET .../diff) → 合并 →
 *   - clean   → success state ("已把 N 个文件合并到 <base>")
 *   - conflict → an AI agent was spawned to resolve; show "AI 正在协调", the
 *                user returns to chat to watch it (POST .../merge → "resolving").
 *
 * The merge runs server-side in the project's primary worktree; this component
 * is purely the preview + trigger + result surface.
 */

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, Bot, Check, FileText, GitMerge, Trash2 } from "lucide-react";
import { api } from "@/api/http";
import type { MergeResult, ThreadDiff } from "@/api/types";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Workspace UUID (not slug) — matches the server path param. */
  workspaceId: string;
  threadId: string;
  /** Direction display name, for the dialog title. */
  threadName: string;
  /** Optional divergence counts — refine empty-diff copy when behind-only. */
  ahead?: number;
  behind?: number;
  /** Clean up this direction (delete worktree + branch + card, nav to main).
   *  Offered after a clean merge so the merged-and-done direction doesn't linger. */
  onCleanup: (threadId: string) => void;
}

function FileList({ files }: { files: string[] }) {
  return (
    <ul className="max-h-48 overflow-y-auto rounded-md border border-border-subtle bg-surface-secondary">
      {files.map((f) => (
        <li
          key={f}
          className="flex items-center gap-2 px-2.5 py-1.5 font-mono text-[11px] text-foreground-secondary"
        >
          <FileText className="size-3 shrink-0 text-foreground-tertiary" />
          <span className="truncate" title={f}>
            {f}
          </span>
        </li>
      ))}
    </ul>
  );
}

export function MergeDialog({
  open,
  onOpenChange,
  workspaceId,
  threadId,
  threadName,
  ahead = 0,
  behind = 0,
  onCleanup,
}: Props) {
  const { t } = useTranslation();
  const [diff, setDiff] = useState<ThreadDiff | null>(null);
  const [loading, setLoading] = useState(false);
  const [merging, setMerging] = useState(false);
  const [result, setResult] = useState<MergeResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  // (Re)load the diff each time the dialog opens; reset all transient state on close.
  useEffect(() => {
    if (!open) {
      setDiff(null);
      setResult(null);
      setError(null);
      return;
    }
    let cancelled = false;
    setLoading(true);
    api
      .threadDiff(workspaceId, threadId)
      .then((d) => {
        if (!cancelled) setDiff(d);
      })
      .catch((e) => {
        if (!cancelled) setError((e as Error).message);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [open, workspaceId, threadId]);

  const doMerge = async () => {
    setMerging(true);
    setError(null);
    try {
      setResult(await api.mergeThread(workspaceId, threadId));
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setMerging(false);
    }
  };

  const files = diff?.files ?? [];
  const base = diff?.base ?? "main";
  // base_dirty 时 diff 可能为空(服务端拒算)——≠「方向没改动」。撒谎「无需合并」
  // 会让用户以为合完了(UX-015)。
  const baseDirty = !!diff?.base_dirty;
  const trulyNoChanges = !loading && files.length === 0 && !baseDirty;
  const blocked = baseDirty || trulyNoChanges;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent showCloseButton={false} className="sm:max-w-md">
        {/* ── result: clean merge ─────────────────────────────────────── */}
        {result?.status === "merged" ? (
          <>
            <DialogHeader>
              <DialogTitle className="flex items-center gap-2">
                <Check className="size-4 text-status-success" />
                {t("merge.mergedTitle")}
              </DialogTitle>
              <DialogDescription>
                {t("merge.mergedBody", { count: result.files, base: result.base })}
                {" "}
                {t("merge.cleanupHint")}
              </DialogDescription>
            </DialogHeader>
            <div className="flex justify-end gap-2 pt-2">
              <Button variant="outline" onClick={() => onOpenChange(false)}>
                {t("merge.keep")}
              </Button>
              <Button
                onClick={() => {
                  onOpenChange(false);
                  onCleanup(threadId);
                }}
              >
                <Trash2 className="size-3.5" />
                {t("merge.cleanup")}
              </Button>
            </div>
          </>
        ) : result?.status === "resolving" ? (
          /* ── result: conflict → AI resolving ───────────────────────── */
          <>
            <DialogHeader>
              <DialogTitle className="flex items-center gap-2">
                <Bot className="size-4 text-state-warning" />
                {t("merge.conflictTitle")}
              </DialogTitle>
              <DialogDescription>
                {t("merge.conflictBody", { count: result.files.length })}
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-2 pt-1">
              <p className="font-caption text-[10px] uppercase tracking-wider text-foreground-tertiary">
                {t("merge.conflictFiles")}
              </p>
              <FileList files={result.files} />
            </div>
            <div className="flex justify-end pt-2">
              <Button onClick={() => onOpenChange(false)}>
                {t("merge.watchInChat")}
              </Button>
            </div>
          </>
        ) : (
          /* ── preview + confirm ─────────────────────────────────────── */
          <>
            <DialogHeader>
              <DialogTitle className="flex items-center gap-2">
                <GitMerge className="size-4 text-accent-primary" />
                {t("merge.title", { name: threadName })}
              </DialogTitle>
              <DialogDescription>
                {loading
                  ? t("merge.loading")
                  : baseDirty && files.length === 0
                    ? t("merge.dirtyBlocksPreview", { base })
                    : trulyNoChanges && behind > 0 && ahead === 0
                      ? t("merge.behindOnly", {
                          behind,
                          base,
                          defaultValue:
                            "没有领先 {{base}} 的改动可合并；这个方向还落后主线 {{behind}} 个提交（↓{{behind}} 不是「待合并」）。",
                        })
                      : trulyNoChanges
                        ? t("merge.noChanges")
                        : t("merge.intro", { count: files.length, base })}
              </DialogDescription>
            </DialogHeader>

            {!loading && files.length > 0 && <FileList files={files} />}

            {baseDirty && (
              <p className="flex items-start gap-1.5 rounded-md bg-state-warning/10 px-2.5 py-2 font-caption text-[11px] text-state-warning">
                <AlertTriangle className="mt-px size-3.5 shrink-0" />
                {t("merge.baseDirty", { base })}
              </p>
            )}
            {error && (
              <p className="rounded-md bg-state-danger/10 px-2.5 py-2 font-caption text-[11px] text-state-danger">
                {error}
              </p>
            )}

            <div className="flex justify-end gap-2 pt-2">
              <Button variant="outline" onClick={() => onOpenChange(false)}>
                {t("common.cancel")}
              </Button>
              <Button onClick={doMerge} disabled={merging || loading || blocked}>
                <GitMerge className="size-3.5" />
                {merging ? t("merge.merging") : t("merge.confirm")}
              </Button>
            </div>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
