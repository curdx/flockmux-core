/**
 * /chat (no workspace selected) — Pencil ChatHome.
 *
 * Shows the workspace list on the left + a center pane telling the user
 * to pick one. Auto-redirects to the first workspace once any agent is
 * alive — most users land here right after spawn and want the chat
 * immediately, not a "pick something" splash. The splash only persists
 * when nothing's alive (or the wizard hasn't been run yet).
 */

import { lazy, Suspense, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { useNavigate } from "react-router-dom";
import { FolderOpen } from "lucide-react";
import { api, networkErrorCopy } from "../../api/http";
import type { AgentInfo, Workspace } from "../../api/types";
import { useSwarmRefresh } from "../../hooks/useSwarmProjection";
import { accentToCssVar } from "../../lib/workspace";
import { Welcome } from "../../components/Welcome";
import { TooltipProvider } from "@/components/ui/tooltip";
import {
  Sheet,
  SheetContent,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { Button } from "@/components/ui/button";
import { toast } from "@/lib/toast";
import { WorkspaceList, type WorkspaceSummary } from "../workspace/Shell";

const CreateWizard = lazy(() =>
  import("@/components/workspace/CreateWizard").then((m) => ({
    default: m.CreateWizard,
  })),
);

function splitWorkspacePath(path: string): { name: string; parent: string } {
  if (!path || path === "(no workspace)") return { name: path || "", parent: "" };
  const trimmed = path.replace(/[\\/]+$/, "");
  const idx = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  if (idx < 0) return { name: trimmed, parent: "" };
  return { name: trimmed.slice(idx + 1) || trimmed, parent: trimmed.slice(0, idx) };
}

export default function ChatHome() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [workspaceRows, setWorkspaceRows] = useState<Workspace[]>([]);
  const [workspacesLoaded, setWorkspacesLoaded] = useState(false);
  const [workspacesError, setWorkspacesError] = useState(false);
  const [wizardOpen, setWizardOpen] = useState(false);
  const [mobileNavOpen, setMobileNavOpen] = useState(false);

  // CommandPalette → 新建 workspace 走 window event。标记 handled，免得
  // AppShell 的全局兜底宿主再开一次（事件同步派发，本监听器先注册先执行）。
  useEffect(() => {
    const onOpen = (e: Event) => {
      (e as CustomEvent<{ handled: boolean }>).detail.handled = true;
      setWizardOpen(true);
    };
    window.addEventListener("swarmx:open-wizard", onOpen as EventListener);
    return () =>
      window.removeEventListener("swarmx:open-wizard", onOpen as EventListener);
  }, []);

  const refreshAgents = async () => {
    try {
      setAgents(await api.listAgents());
    } catch {
      /* best-effort */
    }
  };

  const refreshWorkspaces = async () => {
    try {
      setWorkspaceRows(await api.listWorkspaces());
      setWorkspacesError(false);
    } catch {
      // P0-5 (co-cause): a backend-down fetch failure must NOT be swallowed into
      // an empty list — that renders the "create your first workspace" splash and
      // hides that the backend (127.0.0.1:7777) is unreachable. Flag it so we can
      // show a real "can't reach backend / retry" state instead of lying.
      setWorkspacesError(true);
    } finally {
      setWorkspacesLoaded(true);
    }
  };

  useEffect(() => {
    refreshAgents();
    refreshWorkspaces();
  }, []);

  useSwarmRefresh((s) => s.rosterGen, () => {
    void refreshAgents();
  });
  useSwarmRefresh((s) => s.reconnectGen, () => {
    void refreshAgents();
    void refreshWorkspaces();
  });

  const workspaces = useMemo<WorkspaceSummary[]>(() => {
    const aliveByWsId = new Map<string, AgentInfo[]>();
    for (const a of agents) {
      if (a.killed_at != null || a.shim_exit != null) continue;
      if (!a.workspace_id) continue;
      const arr = aliveByWsId.get(a.workspace_id) ?? [];
      arr.push(a);
      aliveByWsId.set(a.workspace_id, arr);
    }
    return workspaceRows.map<WorkspaceSummary>((w) => {
      const { name: folder } = splitWorkspacePath(w.cwd);
      return {
        id: w.slug,
        workspaceId: w.id,
        path: w.cwd,
        cwdBranch: w.cwd_branch ?? null,
        name: w.name,
        folder,
        accentColor: accentToCssVar(w.accent),
        members: aliveByWsId.get(w.id) ?? [],
        roots: w.roots ?? [],
        threads: w.threads ?? [],
      };
    });
  }, [workspaceRows, agents]);

  // Auto-redirect once a workspace is available — landing here only makes
  // sense as a transient splash. If multiple are alive we still send the
  // user to the first (the sidebar lets them switch instantly).
  useEffect(() => {
    if (workspacesLoaded && workspaces.length > 0) {
      navigate(`/chat/${workspaces[0].id}`, { replace: true });
    }
  }, [workspacesLoaded, workspaces, navigate]);

  const handleDeleteWorkspace = async (workspaceId: string) => {
    // Kill any live agents that belonged to this workspace BEFORE we
    // soft-delete the row. Otherwise the agents linger as orphan PTYs
    // attached to a workspace that no longer exists in the UI, eating
    // tokens with no way to address them. Per-agent failures don't
    // abort the batch — a half-dead PTY shouldn't block deletion.
    try {
      const all = await api.listAgents();
      const live = all.filter(
        (a) =>
          a.workspace_id === workspaceId &&
          a.killed_at == null &&
          a.shim_exit == null,
      );
      await Promise.all(
        live.map((a) =>
          api.killAgent(a.agent_id).catch((e) => {
            // eslint-disable-next-line no-console
            console.warn("killAgent failed", a.agent_id, e);
          }),
        ),
      );
    } catch (err) {
      // eslint-disable-next-line no-console
      console.warn("listAgents before delete failed", err);
    }
    try {
      await api.deleteWorkspace(workspaceId);
    } catch (err) {
      // 删除是「失败即安全」：后端没删成功 = 数据没丢。我们从不乐观删除列表，
      // 所以失败时只要不动列表即可，但必须让用户看到失败而不是静默吞掉。
      toast.error(t("home.deleteFailed", { defaultValue: "删除工作空间失败" }), {
        description: (err as Error)?.message,
      });
      return;
    }
    setWorkspaceRows((prev) => prev.filter((w) => w.id !== workspaceId));
  };

  return (
    <TooltipProvider delayDuration={300}>
      <div className="flex h-full min-h-0">
        <WorkspaceList
          workspaces={workspaces}
          activeId={null}
          isLoading={!workspacesLoaded}
          wsError={workspacesError}
          onOpenWizard={() => setWizardOpen(true)}
          onDelete={handleDeleteWorkspace}
          onRootsChanged={refreshWorkspaces}
        />
        <div className="flex min-w-0 flex-1 flex-col">
          {/* lg:hidden (not md:) to mirror the sidebar rail's lg:flex — otherwise
              768–1023px is a dead zone where neither the rail nor this toggle
              shows and the workspace list is unreachable. */}
          <div className="flex items-center justify-between gap-3 border-b border-border-subtle px-4 py-3 lg:hidden">
            <Button
              variant="outline"
              size="sm"
              onClick={() => setMobileNavOpen(true)}
              className="gap-2"
            >
              <FolderOpen className="size-4" />
              {t("home.workspaces", { defaultValue: "工作空间" })}
            </Button>
            <span className="font-caption text-[11px] text-foreground-tertiary">
              {workspacesLoaded
                ? t("home.workspaceCount", {
                    count: workspaces.length,
                    defaultValue: "{{count}} 个工作空间",
                  })
                : t("home.loadingWorkspaces", { defaultValue: "正在读取工作空间…" })}
            </span>
          </div>
          {!workspacesLoaded ? (
            <section className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 bg-surface-primary text-foreground-tertiary">
              <FolderOpen className="size-10 opacity-40" />
              <p className="font-caption text-sm">
                {t("home.loadingWorkspaces", { defaultValue: "正在读取工作空间…" })}
              </p>
            </section>
          ) : workspacesError && workspaceRows.length === 0 ? (
            <section className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 bg-surface-primary px-6 text-center">
              <FolderOpen className="size-10 text-state-danger opacity-60" />
              <p className="font-heading text-sm text-foreground-primary">
                {t("home.backendUnreachable", { defaultValue: "连接不上后端服务" })}
              </p>
              <p className="max-w-sm font-caption text-xs text-foreground-tertiary">
                {networkErrorCopy()}
              </p>
              <Button
                variant="secondary"
                onClick={() => {
                  setWorkspacesLoaded(false);
                  refreshWorkspaces();
                }}
              >
                {t("common.retry")}
              </Button>
            </section>
          ) : (
            <Welcome onCreateWorkspace={() => setWizardOpen(true)} />
          )}
        </div>
        <Sheet open={mobileNavOpen} onOpenChange={setMobileNavOpen}>
          <SheetContent side="left" className="w-full max-w-none p-0 sm:max-w-none">
            <SheetHeader className="border-b border-border-subtle">
              <SheetTitle>{t("home.workspaces", { defaultValue: "工作空间" })}</SheetTitle>
            </SheetHeader>
            <WorkspaceList
              mobile
              workspaces={workspaces}
              activeId={null}
              isLoading={!workspacesLoaded}
              wsError={workspacesError}
              onOpenWizard={() => {
                setMobileNavOpen(false);
                setWizardOpen(true);
              }}
              onDelete={handleDeleteWorkspace}
              onRootsChanged={refreshWorkspaces}
              onAfterNavigate={() => setMobileNavOpen(false)}
            />
          </SheetContent>
        </Sheet>
        {wizardOpen && (
          <Suspense fallback={null}>
            <CreateWizard
              open={wizardOpen}
              onClose={() => setWizardOpen(false)}
              onCreated={(ws) => {
                refreshAgents();
                // Await the refetch before navigating so the destination Shell
                // already has the new slug in its workspace list (otherwise its
                // not-found redirect bounces back to the first workspace).
                void refreshWorkspaces().then(() => {
                  if (ws) navigate(`/chat/${ws.slug}`);
                });
              }}
            />
          </Suspense>
        )}
      </div>
    </TooltipProvider>
  );
}
