/**
 * CreateWizard — 极简三步：确认引擎能干活 + 起名字 + 选项目文件夹。
 *
 * 步 1 引擎体检 (UX-006)：「已安装」≠「能干活」—— 没登录的引擎要到派活时
 * 才露馅，是 onboarding 头号杀手。所以开工前先摆出诚实四态（未安装 / 已安装
 * 未验证 / 需登录 / 可干活）；全部不能干活时拦住「开始」并说明原因，和 footer
 * 既有的"为什么还点不了"是同一套诚实逻辑。
 *
 * UI 走 shadcn primitives (Dialog / Button / Input / Label)。Dialog 自带
 * focus trap + portal + aria-modal + ESC 关闭。我们额外禁掉 backdrop 点
 * 关闭（用户填了一半 path 不小心点旁边会丢全部输入，体验巨差）。
 *
 * 提交后的事：
 *   1. 调 `runSpell("init", workspace_dir=dirs[0])` — init spell 启动一个
 *      orchestrator（规划；scout 角色已删，Magentic-One 单接待员模型），
 *      进目录做 Phase A 扫描、写黑板 + 给 user 发开场白。
 *   2. wizard 切 loading 视图，订阅 /ws/swarm，看到本工作空间的
 *      `{workspace_id}/{thread_slug}/task.ledger.md` 黑板写入（orchestrator
 *      Phase A 的完成标记，见 roles/orchestrator.md 步骤 2）就关闭 wizard
 *      进群。
 *   3. 超时 / 用户跳过 → 也直接关闭进群，orchestrator 在后台继续跑（黑板
 *      和它发给 user 的开场白会自然出现在 chat 里）。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  ChevronDown,
  ChevronRight,
  Check,
  CircleAlert,
  CircleCheck,
  CircleX,
  Folder,
  FolderOpen,
  FolderPlus,
  Layers3,
  Loader2,
  PlugZap,
  Plus,
  RefreshCw,
  Trash2,
} from "lucide-react";
import { api, ApiError } from "../../api/http";
import type { CliPluginInfo, EngineReadiness, SpellInfo, SwarmEvent, Workspace } from "../../api/types";
import { useSwarmFeed } from "../../hooks/useSwarmFeed";
import { useEngineReadiness, type EngineReadinessState } from "../../hooks/useEngineReadiness";
import {
  ACCENT_OPTIONS,
  isScanDoneBlackboardPath,
} from "../../lib/workspace";
import { notifySpawnFallbacks } from "../../lib/engineFallback";
import { engineStatusKey } from "../../lib/engineEvidence";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import i18n from "@/i18n";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/cn";

const INIT_SPELL = "init";
const SCAN_TIMEOUT_MS = 60_000;
// Sentinel for "use the orchestrator role's default_cli" — Radix Select can't
// take an empty-string value, so we map this to `captain_cli: undefined`.
const CAPTAIN_DEFAULT = "__default__";

// Tauri runtime detection — only the desktop shell exposes
// __TAURI_INTERNALS__ on window. Vite dev (plain browser) is undefined,
// so we hide the "选择文件夹" button there (browser security sandbox
// can't return an absolute filesystem path anyway, so the button would
// be cosmetic).
const IS_TAURI =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

// Tauri 2.x plugin-dialog. Bring it in via dynamic import so vite-dev (no
// Tauri runtime) doesn't try to bundle the native bridge at module init.
async function pickDirectoryViaTauri(): Promise<string | null> {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const result = await open({
      directory: true,
      multiple: false,
      title: i18n.t("wizard.pickFolderTitle", {
        defaultValue: "选择项目文件夹",
      }),
    });
    if (typeof result === "string") return result;
    return null;
  } catch (err) {
    // eslint-disable-next-line no-console
    console.warn("[wizard] dialog.open failed", err);
    return null;
  }
}


interface Props {
  open: boolean;
  onClose: () => void;
  /** Called when scan finishes (or times out). Receives the workspace
   *  that was just created so the parent can navigate the user into it
   *  — wizard itself is layer-agnostic and does not pull react-router.
   *  May be undefined if `createWorkspace` failed before the row was
   *  persisted; parents should fall back to a plain refresh in that case. */
  onCreated?: (workspace?: Workspace) => void;
}

interface ScanState {
  startedAt: number;
  workspace: Workspace;
}

/** One folder row in the wizard. Index 0 = primary project; the rest are
 *  attached source roots, where `role` ("dependency" | "tool") is meaningful. */
/** One folder row in the wizard tree. Row id 0 is the PRIMARY project
 *  (workspace cwd). Other rows are roots: role "project" = a top-level peer
 *  project; "dependency"/"tool" = a source mount whose `parent` is the row id
 *  of the project it hangs under (0 = the primary). `parent`/`role` are
 *  ignored for row 0. */
interface DirEntry {
  id: number;
  path: string;
  role: string;
  parent: number;
}

type PathValidationState =
  | { state: "idle" }
  | { state: "checking" }
  | { state: "ok" }
  | { state: "error"; message: string };

/** Last path segment, for compact display in the parent-project dropdown. */
function baseName(p: string): string {
  return p.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || p;
}

/** Translate a raw server path-validation error into friendly localized text.
 *  The backend emits plain English strings from a handful of distinct sources:
 *    - `directory does not exist: <p>`        (workspaces.rs create / add-root)
 *    - `<p>: No such file or directory`       (files.rs canonicalize on a gone path)
 *    - `not a directory: <p>` / `... is not a directory` (a file, not a dir)
 *    - `<p>: Permission denied` / `read_dir <p>: Permission denied (os error 13)`
 *  Match case-insensitively on each known phrase (substring, not one brittle
 *  regex) and fall back to the raw string only when nothing matches — never
 *  swallow it. Order matters: "not a directory" is checked before the generic
 *  missing-path phrases so a file-vs-dir mistake gets its own message. */
function mapPathError(
  raw: string,
  t: (key: string, defaultValue: string) => string,
): string {
  const lower = raw.toLowerCase();
  if (lower.includes("permission denied")) {
    return t(
      "wizard.pathPermissionDenied",
      "没有访问该目录的权限 —— 换一个你有读取权限的目录,或在系统里授予访问权。",
    );
  }
  if (lower.includes("not a directory")) {
    return t(
      "wizard.pathNotADirectory",
      "这个路径是一个文件,不是文件夹 —— 请填项目所在的文件夹路径。",
    );
  }
  if (
    lower.includes("directory does not exist") ||
    lower.includes("no such file") ||
    lower.includes("not found") ||
    lower.includes("no such")
  ) {
    return t(
      "wizard.errDirMissing",
      "该目录不存在 —— 请确认路径填写正确,且指向一个已存在的绝对路径。",
    );
  }
  return raw;
}

export function CreateWizard({ open, onClose, onCreated }: Props) {
  const { t } = useTranslation();
  const [name, setName] = useState("");
  const [accent, setAccent] = useState<string>("peach");
  // Row 0 = primary project (cwd). Other rows form a logical tree via
  // `parent` (row id). `nextRowId` hands out stable ids so a dependency can
  // reference the peer-project row it mounts under even across reorders.
  const [dirs, setDirs] = useState<DirEntry[]>([
    { id: 0, path: "", role: "main", parent: 0 },
  ]);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const nextRowId = useRef(1);
  const [spells, setSpells] = useState<SpellInfo[]>([]);
  const [plugins, setPlugins] = useState<CliPluginInfo[]>([]);
  // Which engine runs the captain (orchestrator). Default = role default_cli.
  const [captainCli, setCaptainCli] = useState<string>(CAPTAIN_DEFAULT);
  const [scan, setScan] = useState<ScanState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pathChecks, setPathChecks] = useState<Record<number, PathValidationState>>({});
  // In-flight guard: submit() awaits validation + createWorkspace + runSpell
  // before `scan` flips canSubmit false, so a double-click would fire two full
  // create flows → two workspaces + two orchestrators. The ref blocks re-entry
  // synchronously; isSubmitting drives the button disabled state. Mirrors
  // Shell.tsx::creatingDirRef.
  const submittingRef = useRef(false);
  const [isSubmitting, setIsSubmitting] = useState(false);

  // 步 1 引擎体检 (UX-006)。组件只在 wizard 打开时才挂载（调用方都是
  // `wizardOpen && <CreateWizard/>`)，所以 hook 的首次拉取不会白跑；
  // 真实检测（几十秒）只在用户点「重新检测」时触发，绝不自动探测。
  const readiness = useEngineReadiness();

  // useSwarmFeed 必须无条件调用 — 但只在 scan 进行中才处理事件。
  // ref 让 onEvent 闭包永远拿到最新的 scan 引用，不重开 WS。
  const scanRef = useRef<ScanState | null>(null);
  useEffect(() => {
    scanRef.current = scan;
  }, [scan]);

  const finishScan = useRef(() => {});
  finishScan.current = () => {
    const ws = scanRef.current?.workspace;
    setScan(null);
    onCreated?.(ws);
    onClose();
    // reset 用户输入，让下次打开是空的
    setName("");
    setDirs([{ id: 0, path: "", role: "main", parent: 0 }]);
    setAdvancedOpen(false);
    nextRowId.current = 1;
    setError(null);
    setCaptainCli(CAPTAIN_DEFAULT);
  };

  useSwarmFeed({
    onEvent: (ev: SwarmEvent) => {
      const cur = scanRef.current;
      if (!cur) return;
      // 完成信号 = 本工作空间的 orchestrator 写完 Phase A 的 Task Ledger
      // (`<workspace_id>/<thread_slug>/task.ledger.md`)。按 workspace_id 精确
      // 匹配:历史上听的是 `project.summary.*` 前缀(scout 时代的 key,写入方
      // 已随 scout 角色删除,wizard 每次只能干等 60s 超时),而且不带 workspace
      // 限定的前缀匹配会让别的工作空间的写入误关本次 wizard。
      if (
        ev.type === "blackboard_changed" &&
        isScanDoneBlackboardPath(ev.path, cur.workspace.id) &&
        ev.at >= cur.startedAt
      ) {
        finishScan.current();
      }
    },
  });

  useEffect(() => {
    if (!open) return;
    api
      .listSpells()
      .then((rows) => setSpells(rows))
      .catch((e) => setError((e as Error).message));
    // Engines available to run the captain (claude/codex/opencode …). Best-
    // effort: a failure just leaves the default-only picker.
    api
      .listPlugins()
      .then((rows) => setPlugins(rows))
      .catch(() => {});
  }, [open]);

  // scan 超时兜底：orchestrator 因为 LLM 不可用 / 目录权限等问题没有写黑板，60s
  // 后也直接进群，让用户能看到失败状态、自己处理。
  useEffect(() => {
    if (!scan) return;
    const timer = window.setTimeout(() => {
      if (scanRef.current?.startedAt === scan.startedAt) {
        finishScan.current();
      }
    }, SCAN_TIMEOUT_MS);
    return () => window.clearTimeout(timer);
  }, [scan]);

  const cleanDirs = useMemo(
    () => dirs.map((d) => ({ ...d, path: d.path.trim() })).filter((d) => d.path),
    [dirs],
  );
  const mainPath = (dirs[0]?.path ?? "").trim();
  const invalidPath = cleanDirs.some((d) => pathChecks[d.id]?.state === "error");
  const checkingPath = cleanDirs.some((d) => pathChecks[d.id]?.state === "checking");
  const attachedCount = dirs.filter((d) => d.id !== 0 && d.path.trim()).length;
  // Attached (non-primary) rows that fail validation. When 高级 is collapsed
  // these rows aren't rendered, so their error span is hidden — yet they still
  // pin canSubmit false. Surface a count on the collapsed header so the user
  // knows to expand and fix, instead of staring at a greyed-out 创建 button.
  const attachedErrorCount = dirs.filter(
    (d) => d.id !== 0 && d.path.trim() && pathChecks[d.id]?.state === "error",
  ).length;
  // 门禁哲学：至少一个引擎「可干活 / 已安装未验证」就放行（未验证是诚实中态，
  // 不是红）；全部「未安装 / 需登录 / 无法启动」才拦 —— 那时建了空间也没人
  // 能干活。状态还在读 / 一个引擎都没返回时不拦（best-effort，和 captain 引擎
  // 列表拉取失败就退化成默认选项的既有做法一致）。
  const engineBlocked =
    !readiness.loading &&
    readiness.engines.length > 0 &&
    !readiness.engines.some((e) => e.state === "usable" || e.state === "unknown");
  const canSubmit =
    name.trim().length > 0 && mainPath.length > 0 && !scan && !invalidPath && !checkingPath && !engineBlocked;
  const hasInitSpell = spells.some((s) => s.name === INIT_SPELL);
  // Tell the user WHY Start is grey — a silent disabled button is the #1
  // "is this broken?" moment in the create flow.
  const submitBlockReason = useMemo(() => {
    if (scan || isSubmitting) return null;
    // 引擎门禁排在最前：它是步 1，先修它再谈名字和路径。
    if (engineBlocked) {
      return readiness.probing
        ? t("wizard.engineProbeWait", {
            defaultValue: "正在检测引擎，等结果出来再继续…",
          })
        : t("wizard.noUsableEngine", {
            defaultValue:
              "还没有能干活的 AI 引擎——去「设置」页装一个，或去「终端」页登录后点「重新检测」",
          });
    }
    if (!name.trim()) return t("wizard.needName", "先起个名字");
    if (!mainPath) return t("wizard.needPath", "先填项目路径");
    if (checkingPath) return t("wizard.checkingPath", "正在检查路径…");
    if (invalidPath) {
      if (!advancedOpen && attachedErrorCount > 0) {
        return t(
          "wizard.fixAttached",
          "附加目录有问题——展开「高级」修正，或收起以忽略",
        );
      }
      return t("wizard.pathInvalid", "路径无效或目录还不存在");
    }
    return null;
  }, [
    scan,
    isSubmitting,
    engineBlocked,
    readiness.probing,
    name,
    mainPath,
    checkingPath,
    invalidPath,
    advancedOpen,
    attachedErrorCount,
    t,
  ]);

  const validatePath = useCallback(
    async (path: string): Promise<PathValidationState> => {
      const p = path.trim();
      if (!p) return { state: "idle" };
      if (!/^(\/|[A-Za-z]:[\\/])/.test(p)) {
        return {
          state: "error",
          message: t(
            "wizard.errAbsolutePath",
            "Please enter an absolute path (starting with /), e.g. /Users/you/code/project",
          ),
        };
      }
      try {
        await api.filesList(p, undefined, true);
        return { state: "ok" };
      } catch (e) {
        const raw = e instanceof ApiError ? e.detail : (e as Error).message;
        return { state: "error", message: mapPathError(raw, t) };
      }
    },
    [t],
  );

  useEffect(() => {
    if (!open || scan) return;
    const timers = cleanDirs.map((d) =>
      window.setTimeout(() => {
        setPathChecks((prev) => ({ ...prev, [d.id]: { state: "checking" } }));
        validatePath(d.path).then((res) => {
          setPathChecks((prev) => {
            const current = dirs.find((x) => x.id === d.id)?.path.trim() ?? "";
            if (current !== d.path) return prev;
            return { ...prev, [d.id]: res };
          });
          // A successful re-check clears a stale submit-time path error (e.g. the
          // "目录不存在" left by an earlier failed submit) so the form never shows
          // a red error banner next to a green "目录可访问".
          if (res.state === "ok") setError(null);
        });
      }, 350),
    );
    setPathChecks((prev) => {
      const keep = new Set(cleanDirs.map((d) => d.id));
      const next: Record<number, PathValidationState> = {};
      for (const d of dirs) {
        if (!d.path.trim()) next[d.id] = { state: "idle" };
        else if (keep.has(d.id) && prev[d.id]) next[d.id] = prev[d.id];
      }
      return next;
    });
    return () => {
      for (const timer of timers) window.clearTimeout(timer);
    };
  }, [cleanDirs, dirs, open, scan, validatePath]);

  const submit = async () => {
    if (!canSubmit) return;
    // Synchronous re-entry guard — a second click during the await window
    // (validation → createWorkspace → runSpell) would otherwise spawn a second
    // workspace + orchestrator before `scan` flips canSubmit false.
    if (submittingRef.current) return;
    submittingRef.current = true;
    setIsSubmitting(true);
    setError(null);
    let created: Workspace | null = null;
    // Everything from the path precheck onward runs INSIDE try/finally so the
    // submit lock (submittingRef / isSubmitting) is ALWAYS released. A path that
    // fails validation here used to `return` before the finally below, leaving
    // the "创建" button wedged disabled forever even after the path was fixed
    // (real-machine reproduced).
    try {
      // Client-side precheck: a relative/garbage path can't be a workspace cwd.
      // Catch it here with a clear hint instead of round-tripping for a 400
      // (existence still needs the server — that path's error is translated below).
      if (!/^(\/|[A-Za-z]:[\\/])/.test(mainPath)) {
        setError(
          t(
            "wizard.errAbsolutePath",
            "Please enter an absolute path (starting with /), e.g. /Users/you/code/project",
          ),
        );
        return;
      }
      const checked = await Promise.all(
        cleanDirs.map(async (d) => [d.id, await validatePath(d.path)] as const),
      );
      const nextChecks = Object.fromEntries(checked) as Record<number, PathValidationState>;
      setPathChecks(nextChecks);
      const firstInvalid = checked.find(([, res]) => res.state === "error");
      if (firstInvalid && firstInvalid[1].state === "error") {
        setError(firstInvalid[1].message);
        return;
      }
      const startedAt = Date.now();
      const wsName = name.trim();
      if (!hasInitSpell) {
        // Defensive guard only — the `init` spell is compiled into the server
        // binary now (spells::SpellRegistry::builtin), so on a healthy install
        // this is always present. If we still get here it means the local
        // service didn't report ready (e.g. a transient /api/spells failure),
        // so show the user something actionable, not server-internal jargon.
        throw new Error(
          t(
            "wizard.errBackendNotReady",
            "本地服务尚未就绪,请稍候重试;若一直这样,重启 swarmx 即可。",
          ),
        );
      }
      // workspace-as-first-class refactor: workspace is created in the
      // DB BEFORE the init spell launches. The orchestrator that init spawns
      // (scout 角色已删) inherits the workspace_id via the spell-runner's
      // reverse-lookup (rest.rs::run_spell with workspace_id). Name + accent
      // are workspace table columns now, no blackboard writes needed.
      // Create the bare workspace first (cwd = primary). The tree of roots is
      // then materialised with follow-up POSTs in topological order: peer
      // projects first (so we learn their server ids), then dependency/tool
      // mounts pointing at the right parent. This reuses the plain /roots
      // endpoint instead of teaching the create handler about client temp ids.
      created = await api.createWorkspace({
        name: wsName,
        cwd: mainPath,
        accent,
      });
      // rowId → server root id, for resolving a dependency's parent project.
      const realId = new Map<number, string>();
      const rootRows = cleanDirs.filter((d) => d.id !== 0);
      // 1) peer projects (top-level, parent_id = null)
      for (const d of rootRows.filter((r) => r.role === "project")) {
        try {
          const added = await api.addWorkspaceRoot(created.id, {
            path: d.path,
            role: "project",
          });
          if (added.id) realId.set(d.id, added.id);
        } catch (err) {
          // eslint-disable-next-line no-console
          console.warn("[wizard] add project root failed", d.path, err);
        }
      }
      // 2) dependency / tool mounts under the primary (parent 0 → null) or a
      //    peer project (→ that peer's server id; falls back to primary if the
      //    referenced peer never got created).
      for (const d of rootRows.filter((r) => r.role !== "project")) {
        const parentId = d.parent !== 0 ? realId.get(d.parent) : undefined;
        try {
          await api.addWorkspaceRoot(created.id, {
            path: d.path,
            role: d.role,
            parent_id: parentId,
          });
        } catch (err) {
          // eslint-disable-next-line no-console
          console.warn("[wizard] add source root failed", d.path, err);
        }
      }
      // Stash the created workspace on scan state so finishScan can hand
      // it back to the parent for routing into the new chat URL — without
      // this the parent only knew "something was created, refresh."
      setScan({ startedAt, workspace: created });
      const spellResp = await api.runSpell({
        name: INIT_SPELL,
        task: wsName,
        workspace_dir: mainPath,
        workspace_id: created.id,
        captain_cli: captainCli === CAPTAIN_DEFAULT ? undefined : captainCli,
      });
      // Billing red line: if the chosen captain engine wasn't installed, the
      // server silently spawned an installed fallback — possibly an API-billed
      // one (e.g. reasonix). Announce the substitution instead of letting a
      // paid engine run in disguise.
      notifySpawnFallbacks(t, spellResp.agents);
    } catch (e) {
      setScan(null);
      // Roll back a half-created workspace: if createWorkspace succeeded but
      // runSpell failed (e.g. the cwd doesn't exist), the workspace row is
      // already persisted. Without this the user is left with a dead,
      // 0-member "ghost" workspace pointing at a bad path that they'd have to
      // delete by hand. (The backend also validates the cwd up-front now, but
      // this guards every other runSpell failure too.)
      if (created) {
        api.deleteWorkspace(created.id).catch(() => {});
      }
      // Show the server's plain error string, not the `METHOD path → status`
      // wrapper. Translate the common path-validation failures (does-not-exist /
      // not-a-directory / permission-denied) into Chinese (server messages are
      // English; FAULT-007).
      const raw = e instanceof ApiError ? e.detail : (e as Error).message;
      setError(mapPathError(raw, t));
    } finally {
      // Release the guard. On success `scan` is set so the submit button is
      // swapped for "enter anyway" (no re-entry possible); on failure the
      // button returns and must be clickable again.
      submittingRef.current = false;
      setIsSubmitting(false);
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) onClose();
      }}
    >
      <DialogContent
        showCloseButton={!scan}
        className="flex max-h-[90vh] w-[calc(100vw-24px)] max-w-[680px] flex-col gap-0 overflow-hidden p-0 sm:max-w-[680px]"
        // 禁掉 backdrop / outside click 关闭 — 用户填了一半路径，不小心点空白
        // 全部清空体验巨差。ESC + ✕ + 取消 三个显式入口仍然能关。
        onInteractOutside={(e) => e.preventDefault()}
        // 扫描中禁 ESC，避免误触关了 wizard 但 orchestrator 仍在后台跑。loading
        // 视图有 "直接进群" 按钮显式离开。
        onEscapeKeyDown={(e) => {
          if (scan) e.preventDefault();
        }}
      >
        <DialogHeader className="flex flex-row items-center gap-3 border-b border-border-subtle bg-surface-elevated px-6 py-5">
          <span className="flex size-9 shrink-0 items-center justify-center rounded-md bg-accent-primary-soft">
            <FolderPlus className="size-5 text-accent-primary-deep" />
          </span>
          <div className="flex min-w-0 flex-col text-left">
            <DialogTitle className="font-heading text-base font-semibold text-foreground-primary">
              {t("wizard.title")}
            </DialogTitle>
            <DialogDescription className="font-caption text-[11px] text-foreground-tertiary">
              {t("wizard.subtitle")}
            </DialogDescription>
          </div>
        </DialogHeader>

        {scan ? (
          <ScanView
            label={t("wizard.scanning")}
            hint={t("wizard.scanningHint")}
            startedAt={scan.startedAt}
          />
        ) : (
          <div className="flex min-h-0 flex-1 flex-col gap-6 overflow-y-auto p-4 pb-24 sm:p-6">
            {error && (
              <div className="rounded-md border border-state-danger/40 bg-status-danger-soft px-3 py-2 text-xs text-state-danger">
                {error}
              </div>
            )}

            {/* Step 1: engine readiness — 开工前一眼看清谁能干活 */}
            <EngineStep readiness={readiness} />

            {/* Step 2: name + accent */}
            <section className="flex flex-col gap-3">
              <StepHeader n={2} label={t("wizard.step1")} />
              <div className="flex flex-col gap-3 sm:flex-row sm:items-center">
                <div className="flex min-w-0 flex-1 flex-col gap-1">
                  <Label htmlFor="wizard-name" className="sr-only">
                    {t("wizard.nameLabel", { defaultValue: "名称" })}
                  </Label>
                  <Input
                    id="wizard-name"
                    name="workspace-name"
                    autoFocus
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    maxLength={120}
                    placeholder={t("wizard.namePlaceholder")}
                    className="h-10"
                  />
                </div>
                {/* 5 个标识色 — 之前裸着 5 个圆，新手不知道是干啥的。加
                 *  小 label 说明用途（多 workspace 时一眼区分谁是谁）。 */}
                <div className="flex flex-col items-start gap-1 sm:items-end">
                  <span className="font-caption text-[10px] text-foreground-tertiary">
                    {t("wizard.accentLabel")}
                  </span>
                  <div className="flex items-center gap-1.5">
                    {ACCENT_OPTIONS.map((opt) => {
                      const colorName = t(opt.nameKey);
                      return (
                        <button
                          key={opt.id}
                          type="button"
                          onClick={() => setAccent(opt.id)}
                          className={cn(
                            "size-6 rounded-full transition-transform",
                            accent === opt.id
                              ? "ring-2 ring-foreground-primary ring-offset-2"
                              : "hover:scale-110",
                          )}
                          style={{ background: opt.cssVar }}
                          title={t("wizard.accentTitle", { name: colorName })}
                          aria-label={t("wizard.accentTitle", { name: colorName })}
                        />
                      );
                    })}
                  </div>
                </div>
              </div>
            </section>

            {/* Step 3: dirs */}
            <section className="flex flex-col gap-3">
              <StepHeader
                n={3}
                label={t("wizard.step2")}
                hint={t("wizard.step2Hint")}
              />
              <div className="flex flex-col gap-2">
                {(advancedOpen ? dirs : dirs.slice(0, 1)).map((d, i) => {
                  const isPrimary = i === 0;
                  const isProject = d.role === "project";
                  const pathCheck = pathChecks[d.id] ?? { state: "idle" as const };
                  // Projects this row's source mount can hang under: the
                  // primary (value 0) + every other peer-project row.
                  const parentOptions = dirs.filter(
                    (x) => x.role === "project" && x.path.trim() && x.id !== d.id,
                  );
                  return (
                    <div
                      key={d.id}
                      className="flex flex-col gap-3 rounded-lg border border-border-subtle bg-surface-elevated px-3.5 py-3 shadow-sm sm:flex-row sm:items-start"
                    >
                      {/* Icon (not a number) tells the kinds apart: primary =
                       *  accent open folder, peer project = violet open folder,
                       *  source mount = grey folder. */}
                      <span
                        className={cn(
                          "flex size-9 shrink-0 items-center justify-center rounded-md sm:mt-0.5",
                          isPrimary
                            ? "bg-accent-primary text-foreground-on-accent"
                            : isProject
                              ? "bg-accent-purple text-foreground-on-accent"
                              : "bg-surface-tertiary text-foreground-tertiary",
                        )}
                        title={
                          isPrimary
                            ? t("wizard.primaryLabel")
                            : t("wizard.attachedLabel")
                        }
                      >
                        {isPrimary || isProject ? (
                          <FolderOpen className="size-4" />
                        ) : (
                          <Folder className="size-4" />
                        )}
                      </span>
                      <div className="flex min-w-0 flex-1 flex-col gap-1.5">
                        <Input
                          name={`workspace-root-${d.id}-path`}
                          value={d.path}
                          onChange={(e) =>
                            setDirs((prev) =>
                              prev.map((x, j) =>
                                j === i ? { ...x, path: e.target.value } : x,
                              ),
                            )
                          }
                          placeholder={
                            isPrimary
                              ? IS_TAURI
                                ? t("wizard.dirPlaceholder1Tauri")
                                : t("wizard.dirPlaceholder1")
                              : t("wizard.dirPlaceholderMore")
                          }
                          className="h-8 border-none bg-transparent px-0 font-mono text-sm shadow-none focus-visible:ring-0"
                        />
                        {pathCheck.state === "checking" && (
                          <span className="font-caption text-[10px] text-foreground-tertiary">
                            {t("wizard.pathChecking", "正在检查目录…")}
                          </span>
                        )}
                        {pathCheck.state === "ok" && (
                          <span className="font-caption text-[10px] text-status-success">
                            {t("wizard.pathOk", "目录可访问")}
                          </span>
                        )}
                        {pathCheck.state === "error" && (
                          <span className="font-caption text-[10px] text-state-danger">
                            {pathCheck.message}
                          </span>
                        )}
                        {/* role + (for a source mount) parent-project picker */}
                        {!isPrimary && (
                          <div className="flex flex-wrap items-center gap-2">
                            <Select
                              name={`workspace-root-${d.id}-role`}
                              value={d.role}
                              onValueChange={(next) =>
                                setDirs((prev) =>
                                  prev.map((x, j) =>
                                    j === i ? { ...x, role: next } : x,
                                  ),
                                )
                              }
                            >
                              <SelectTrigger
                                aria-label={t("wizard.attachedLabel")}
                                title={t("wizard.attachedLabel")}
                                className="h-7 w-[132px] shrink-0 text-xs"
                                size="sm"
                              >
                                <SelectValue />
                              </SelectTrigger>
                              <SelectContent>
                                <SelectItem value="project">
                                  {t("wizard.roleProject")}
                                </SelectItem>
                                <SelectItem value="dependency">
                                  {t("wizard.roleDependency")}
                                </SelectItem>
                                <SelectItem value="tool">
                                  {t("wizard.roleTool")}
                                </SelectItem>
                              </SelectContent>
                            </Select>
                            {!isProject && (
                              <>
                                <span className="shrink-0 font-caption text-[11px] text-foreground-tertiary">
                                  {t("chat.mountUnder")}
                                </span>
                                <Select
                                  name={`workspace-root-${d.id}-parent`}
                                  value={String(d.parent)}
                                  onValueChange={(next) =>
                                    setDirs((prev) =>
                                      prev.map((x, j) =>
                                        j === i
                                          ? { ...x, parent: Number(next) }
                                          : x,
                                      ),
                                    )
                                  }
                                >
                                  <SelectTrigger
                                    className="h-7 min-w-0 max-w-[12rem] text-xs"
                                    size="sm"
                                  >
                                    <SelectValue />
                                  </SelectTrigger>
                                  <SelectContent>
                                    <SelectItem value="0">
                                      {name.trim() || t("chat.primaryProject")}
                                    </SelectItem>
                                    {parentOptions.map((p) => (
                                      <SelectItem key={p.id} value={String(p.id)}>
                                        {baseName(p.path)}
                                      </SelectItem>
                                    ))}
                                  </SelectContent>
                                </Select>
                              </>
                            )}
                          </div>
                        )}
                        <span className="font-caption text-[10px] text-foreground-tertiary">
                          {isPrimary
                            ? t("wizard.primaryLabel")
                            : isProject
                              ? t("wizard.projectHint")
                              : t("wizard.attachedHint")}
                        </span>
                      </div>
                      {/* Picker button — Tauri 下打开原生文件夹 dialog；
                       *  浏览器 dev / preview 模式下 disabled + tooltip 解
                       *  释。之前直接 hide，用户根本不知道桌面 app 能直接
                       *  选，audit 时被点出来"看不到选择按钮"。*/}
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={!IS_TAURI}
                        onClick={async () => {
                          if (!IS_TAURI) return;
                          const picked = await pickDirectoryViaTauri();
                          if (picked) {
                            setDirs((prev) =>
                              prev.map((x, j) =>
                                j === i ? { ...x, path: picked } : x,
                              ),
                            );
                          }
                        }}
                        title={
                          IS_TAURI
                            ? t("wizard.pickFolder")
                            : t("wizard.pickFolderUnavailable")
                        }
                        aria-label={
                          IS_TAURI
                            ? t("wizard.pickFolder")
                            : t("wizard.pickFolderUnavailable")
                        }
                        className="h-8 shrink-0 gap-1.5 px-2.5 text-xs sm:mt-0.5"
                      >
                        <FolderOpen className="size-3.5" />
                        {t("wizard.pickFolderShort")}
                      </Button>
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() =>
                          setDirs((prev) => prev.filter((_, j) => j !== i))
                        }
                        disabled={isPrimary}
                        title={t("wizard.removeDir")}
                        aria-label={t("wizard.removeDir")}
                        className="size-7 text-foreground-tertiary sm:mt-0.5"
                      >
                        <Trash2 className="size-3.5" />
                      </Button>
                    </div>
                  );
                })}
                <button
                  type="button"
                  onClick={() =>
                    setAdvancedOpen((wasOpen) => {
                      if (wasOpen) {
                        // Collapsing: drop unfinished / broken attached rows so
                        // they can't silently grey out Start with no visible
                        // error (errors live inside the collapsed section).
                        setDirs((prev) =>
                          prev.filter(
                            (d) =>
                              d.id === 0 ||
                              (d.path.trim() &&
                                pathChecks[d.id]?.state === "ok"),
                          ),
                        );
                      }
                      return !wasOpen;
                    })
                  }
                  className="flex items-center gap-3 rounded-lg border border-border-subtle bg-surface-primary px-3.5 py-3 text-left transition-colors hover:bg-surface-tertiary"
                  aria-expanded={advancedOpen}
                >
                  <span className="flex size-8 shrink-0 items-center justify-center rounded-md bg-surface-tertiary text-foreground-secondary">
                    <Layers3 className="size-4" />
                  </span>
                  <span className="flex min-w-0 flex-1 flex-col gap-0.5">
                    <span className="flex items-center gap-2 font-heading text-xs font-semibold text-foreground-primary">
                      {t("wizard.advancedTitle")}
                      {attachedCount > 0 && (
                        <span className="rounded-full bg-accent-primary-soft px-1.5 py-0.5 font-caption text-[10px] text-accent-primary-deep">
                          {t("wizard.attachedCount", { count: attachedCount })}
                        </span>
                      )}
                      {/* Collapsed + an attached dir is invalid: its error span is
                       *  hidden, so flag it here (otherwise 创建 is greyed with no
                       *  visible reason). Expand to see the per-row message. */}
                      {!advancedOpen && attachedErrorCount > 0 && (
                        <span className="rounded-full bg-status-danger-soft px-1.5 py-0.5 font-caption text-[10px] text-state-danger">
                          {t("wizard.attachedErrorCount", {
                            count: attachedErrorCount,
                            defaultValue: "{{count}} 个附加目录有问题，展开修正",
                          })}
                        </span>
                      )}
                    </span>
                    <span className="font-caption text-[11px] leading-relaxed text-foreground-tertiary">
                      {advancedOpen
                        ? t("wizard.advancedOpenHint")
                        : t("wizard.advancedClosedHint")}
                    </span>
                  </span>
                  {advancedOpen ? (
                    <ChevronDown className="size-4 shrink-0 text-foreground-tertiary" />
                  ) : (
                    <ChevronRight className="size-4 shrink-0 text-foreground-tertiary" />
                  )}
                </button>
                {advancedOpen && (
                  <>
                    {/* Captain engine lives under Advanced — most users just
                        want Start; default = orchestrator role's default_cli. */}
                    <div className="flex flex-col gap-1 rounded-lg border border-border-subtle bg-surface-elevated px-3.5 py-3">
                      <Label
                        htmlFor="wizard-captain"
                        className="font-caption text-[10px] text-foreground-tertiary"
                      >
                        {t("wizard.captainEngine", "规划用引擎")}
                      </Label>
                      <Select value={captainCli} onValueChange={setCaptainCli}>
                        <SelectTrigger
                          id="wizard-captain"
                          size="sm"
                          className="h-9 w-full max-w-[280px] text-sm"
                        >
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          <SelectItem value={CAPTAIN_DEFAULT}>
                            {t("wizard.captainDefault", "自动选择")}
                          </SelectItem>
                          {plugins.map((p) => (
                            <SelectItem
                              key={p.id}
                              value={p.id}
                              disabled={p.installed === false}
                            >
                              {p.display_name}
                              {p.installed === false
                                ? t("wizard.captainNotInstalled", "（未安装）")
                                : ""}
                            </SelectItem>
                          ))}
                        </SelectContent>
                      </Select>
                    </div>
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() =>
                      setDirs((prev) => [
                        ...prev,
                        {
                          id: nextRowId.current++,
                          path: "",
                          role: "dependency",
                          parent: 0,
                        },
                      ])
                    }
                    className="h-auto justify-center gap-2 rounded-lg border-[1.5px] border-dashed border-border-strong bg-transparent py-3 text-xs text-foreground-secondary hover:bg-surface-tertiary"
                  >
                    <span className="flex size-7 items-center justify-center rounded-md bg-accent-primary-soft text-accent-primary-deep">
                      <Plus className="size-4" />
                    </span>
                    {t("wizard.addDir")}
                  </Button>
                  </>
                )}
              </div>
              <details className="group">
                <summary className="cursor-pointer font-caption text-[11px] text-foreground-tertiary hover:text-foreground-secondary">
                  {t("wizard.gitInitSummary", "关于版本控制（可选了解）")}
                </summary>
                <p className="mt-1.5 font-caption text-[11px] leading-relaxed text-foreground-tertiary">
                  {t("wizard.gitInitNotice")}
                </p>
              </details>
            </section>
          </div>
        )}

        {/* 不用 shadcn DialogFooter — 它默认有 `-mx-4 -mb-4 bg-muted/50
            rounded-b-xl border-t`，是给标准 DialogContent p-4 用的，假设
            footer 通过负 margin 顶到 content 边缘。我们的 DialogContent
            是 p-0 + 自己 header/body/footer 控制 padding，那套负 margin
            会把 footer 顶出 modal 边界 16px，配合 overflow-hidden 把
            border-t / rounded-b 都裁掉，看上去 footer 像"飘"在外面没
            分隔线。用普通 div + 我们自己的 border-t / bg / padding 即可。 */}
        <div className="flex shrink-0 flex-col gap-3 border-t border-border-subtle bg-surface-elevated px-4 py-4 sm:flex-row sm:items-center sm:px-6">
          <span className="font-caption text-[11px] text-foreground-tertiary">
            {scan
              ? t("wizard.scanningFootHint")
              : submitBlockReason
                ? submitBlockReason
                : t("wizard.defaultInfo")}
          </span>
          <span className="hidden flex-1 sm:block" />
          {scan ? (
            <Button variant="outline" onClick={() => finishScan.current()}>
              {t("wizard.enterAnyway")}
            </Button>
          ) : (
            <>
              <Button variant="outline" onClick={onClose}>
                {t("wizard.cancel")}
              </Button>
              <Button
                onClick={() => {
                  void submit();
                }}
                disabled={isSubmitting || !canSubmit}
                title={submitBlockReason ?? undefined}
              >
                {isSubmitting ? (
                  <Loader2 className="size-3.5 animate-spin" />
                ) : (
                  <Check className="size-3.5" />
                )}
                {t("wizard.create")}
              </Button>
            </>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

/** 步 1 引擎体检 —— 每个引擎一行：状态图标 + 名字 + chip（engineStatusKey 的
 *  证据标签），问题态附一句人话指引。视觉与 chat EmptyState 的引擎卡一致
 * （同一套图标/配色）；全部可用时就是一张一眼确认的卡，不加任何操作负担。 */
function EngineStep({ readiness }: { readiness: EngineReadinessState }) {
  const { t } = useTranslation();
  const { loading, probing, engines, probe, error } = readiness;
  return (
    <section className="flex flex-col gap-3">
      <StepHeader
        n={1}
        label={t("wizard.stepEngines", { defaultValue: "确认 AI 引擎能干活" })}
        hint={t("wizard.stepEnginesHint", {
          defaultValue: "「重新检测」会真实启动各引擎，约几十秒",
        })}
      />
      <div className="flex flex-col gap-2 rounded-lg border border-border-subtle bg-surface-elevated px-3.5 py-3 shadow-sm">
        <div className="flex items-center justify-between gap-2">
          <span className="font-caption text-[11px] leading-relaxed text-foreground-tertiary">
            {t("wizard.engineStepIntro", {
              defaultValue:
                "「已安装」不代表能干活（可能没登录）。至少一个引擎能干活或已装好，才能继续。",
            })}
          </span>
          <button
            type="button"
            onClick={probe}
            disabled={probing || loading}
            className="inline-flex shrink-0 items-center gap-1 font-caption text-[11px] text-foreground-tertiary transition-colors hover:text-foreground-secondary disabled:opacity-60"
          >
            {probing ? (
              <Loader2 className="size-3 animate-spin" />
            ) : (
              <RefreshCw className="size-3" />
            )}
            {probing
              ? t("wizard.engineProbing", { defaultValue: "检测中…" })
              : t("wizard.engineReprobe", { defaultValue: "重新检测" })}
          </button>
        </div>
        {loading ? (
          <span className="flex items-center gap-1.5 font-caption text-[11px] text-foreground-tertiary">
            <Loader2 className="size-3 animate-spin" />
            {t("wizard.engineLoading", { defaultValue: "正在查看本机引擎…" })}
          </span>
        ) : error ? (
          <span
            className="font-caption text-[11px] text-state-warning"
            title={error}
          >
            {t("wizard.engineLoadFailed", {
              defaultValue: "没能读到引擎状态。可以直接继续，或稍后点「重新检测」。",
            })}
          </span>
        ) : (
          engines.map((e) => <EngineReadinessRow key={e.id} engine={e} />)
        )}
      </div>
    </section>
  );
}

/** 一个引擎一行。图标/配色与 chat EmptyState 的 EngineRow 保持一致；
 *  chip 文案走 engineStatusKey（usable 时给证据标签：已实测/使用中/仅启动）。 */
function EngineReadinessRow({ engine }: { engine: EngineReadiness }) {
  const { t } = useTranslation();
  const { display_name: name, state, reason } = engine;
  const variants = {
    usable: {
      icon: <CircleCheck className="size-3.5 text-status-success" />,
      nameCls: "text-foreground-secondary",
      chipCls: "bg-status-success-soft text-status-success",
    },
    unknown: {
      icon: <PlugZap className="size-3.5 text-foreground-tertiary" />,
      nameCls: "text-foreground-secondary",
      chipCls: "bg-surface-tertiary text-foreground-tertiary",
    },
    needs_login: {
      icon: <CircleAlert className="size-3.5 text-state-warning" />,
      nameCls: "text-foreground-secondary",
      chipCls: "bg-status-warning-soft text-status-warning",
    },
    not_usable: {
      icon: <CircleX className="size-3.5 text-state-danger" />,
      nameCls: "text-foreground-secondary",
      chipCls: "bg-status-danger-soft text-state-danger",
    },
    not_installed: {
      icon: <CircleX className="size-3.5 text-foreground-tertiary" />,
      nameCls: "text-foreground-tertiary",
      chipCls: "bg-surface-tertiary text-foreground-tertiary",
    },
  } as const;
  const v = variants[state];
  // 人话指引（不出现 CLI/PTY/spawn 这类词）：能干活的不说话，有问题的说怎么办。
  const guide =
    state === "needs_login"
      ? t("wizard.engineNeedsLoginGuide", {
          defaultValue: "去「终端」页登录它，然后点「重新检测」。",
        })
      : state === "not_installed"
        ? t("wizard.engineMissingGuide", {
            defaultValue: "去「设置」页一键安装，装好后点「重新检测」。",
          })
        : state === "not_usable"
          ? t("wizard.engineNotUsableGuide", {
              defaultValue:
                "它没能启动（鼠标悬停可看原因）。处理好之后点「重新检测」。",
            })
          : state === "unknown"
            ? t("wizard.engineUnverifiedGuide", {
                defaultValue:
                  "已装好、还没实测过。可以直接继续，也可以点「重新检测」确认。",
              })
            : null;
  return (
    <div className="flex flex-col gap-0.5" title={reason ?? undefined}>
      <div className="flex items-center gap-1.5">
        {v.icon}
        <span className={`font-caption text-[12px] ${v.nameCls}`}>{name}</span>
        <span
          className={`rounded-full px-1.5 py-0.5 font-caption text-[10px] ${v.chipCls}`}
        >
          {t(engineStatusKey(engine))}
        </span>
      </div>
      {guide && (
        <span className="pl-5 font-caption text-[10px] leading-relaxed text-foreground-tertiary">
          {guide}
        </span>
      )}
    </div>
  );
}

function ScanView({
  label,
  hint,
  startedAt,
}: {
  label: string;
  hint: string;
  startedAt: number;
}) {
  const { t } = useTranslation();
  // 实时 elapsed 秒数 — 用户等 30 秒 spinner 转着不动会怀疑挂了，给个
  // 实时跳秒 + 隐式进度条让他知道 "在动、快好"。
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 500);
    return () => window.clearInterval(id);
  }, []);
  const elapsedMs = Math.max(0, now - startedAt);
  const elapsedSec = Math.floor(elapsedMs / 1000);
  // 平均完成时间 ~25s；按 30s 当 90% (留 10% 给最后一公里，看着没卡住)。
  // 超过 30s 后吸到 95% 静止，等真正完成 effect 关闭 modal。
  const pct = Math.min(95, Math.floor((elapsedMs / 30_000) * 90));

  return (
    <div className="flex min-h-[300px] flex-1 flex-col items-center justify-center gap-4 p-10 text-center">
      <span className="flex size-14 items-center justify-center rounded-full bg-accent-primary-soft text-accent-primary-deep">
        <Loader2 className="size-7 animate-spin" />
      </span>
      <h3 className="font-heading text-base font-semibold text-foreground-primary">
        {label}
      </h3>
      <p className="max-w-[420px] font-body text-[13px] leading-relaxed text-foreground-secondary">
        {hint}
      </p>
      {/* 进度条 + 实时秒数。进度条是基于平均时长的"心理安抚条"，不精确
       *  跟后端 orchestrator 真实进度挂钩 (后端没暴露阶段事件)。 */}
      <div className="flex w-full max-w-[320px] flex-col gap-1.5">
        <div className="h-1.5 w-full overflow-hidden rounded-full bg-surface-tertiary">
          <div
            className="h-full rounded-full bg-accent-primary transition-all duration-500 ease-out"
            style={{ width: `${pct}%` }}
          />
        </div>
        <span className="font-mono text-[10px] text-foreground-tertiary">
          {t("wizard.scanningElapsed", {
            s: elapsedSec,
            defaultValue: "已等待 {{s}}s · 通常需几十秒（预估，非实测进度）",
          })}
        </span>
      </div>
    </div>
  );
}

function StepHeader({
  n,
  label,
  hint,
}: {
  n: number;
  label: string;
  hint?: string;
}) {
  return (
    <div className="flex items-center gap-2">
      <span className="flex size-[18px] items-center justify-center rounded-full bg-accent-primary font-heading text-[10px] font-bold text-foreground-on-accent">
        {n}
      </span>
      <span className="font-heading text-[13px] font-bold text-foreground-primary">
        {label}
      </span>
      {hint && (
        <>
          <span className="flex-1" />
          <span className="font-caption text-[11px] text-foreground-tertiary">
            {hint}
          </span>
        </>
      )}
    </div>
  );
}
