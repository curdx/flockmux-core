/**
 * McpManager — 「快捷装 MCP」独立页面主体(/mcp，见 routes/mcp.tsx)。
 *
 * 真链路 + 共享密钥模型(联网核实的最优解):
 *   - 顶部「运行环境」：探 Node/npm/uv(GET /api/mcp/env)，缺 Node 警告。
 *   - 每个 server 一张卡，Claude / Codex 两个开关反映真实配置(GET /api/mcp/status)，
 *     拨开关 = 真调 `claude/codex mcp add/remove`(upsert)。
 *   - **密钥属于 server、不属于 CLI**(一个账号一把 key)：claude/codex 共用同一把。
 *     · 已设置 → 打码显示后 4 位 + 「改密钥」一处编辑、保存自动同步到两边。
 *     · 启用第二个 CLI 时复用已存 key，**不再让你重填**(后端从已配的那侧取出)。
 *     · 两边 key 不一致(被手改过)→ 标出来，「改密钥」即可统一。
 *   - 全程不显示完整 key(后端只回打码值)。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, ExternalLink, KeyRound, Loader2, TriangleAlert } from "lucide-react";
import { api, type McpEnv, type McpStatus, type RuntimeInfo } from "@/api/http";
import { Switch } from "@/components/ui/switch";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { toast } from "@/lib/toast";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { cn } from "@/lib/cn";
import { MCP_CATALOG, FALLBACK_ICON } from "@/lib/mcpCatalog";

type Cli = "claude" | "codex";

/**
 * 已接入(后端 allowlist 同步)的 server。
 * `keyHintKey` 是 i18n key,在 ApiKeyDialog 里用 t() 解析成本地化提示。
 */
const SERVERS: { id: string; needsKey: boolean; keyHintKey?: string }[] = [
  { id: "chrome-devtools", needsKey: false },
  { id: "context7", needsKey: true, keyHintKey: "mcp.keyHintContext7" },
];

type KeyDialogState = {
  id: string;
  name: string;
  /** 设了 = 「为这个 CLI 启用并填首把 key」；未设 = 「改密钥(同步所有已启用 CLI)」。 */
  cli?: Cli;
  masked?: string | null;
};

export function McpManager() {
  const { t } = useTranslation();
  const [env, setEnv] = useState<McpEnv | null>(null);
  const [status, setStatus] = useState<McpStatus | null>(null);
  const [busy, setBusy] = useState<{ id: string; cli?: Cli } | null>(null);
  const [error, setError] = useState<string | null>(null);
  // 初次加载(env+status)是否失败 —— 失败时渲染「连不上后端/加载失败 + 重试」，
  // 而不是永久 spinner(P1-13)。
  const [loadError, setLoadError] = useState(false);
  const [keyDialog, setKeyDialog] = useState<KeyDialogState | null>(null);
  // Disabling rewrites the user's real ~/.claude|~/.codex config and hits every
  // running agent immediately — gate it behind a confirm (FAULT-026).
  const [confirmOff, setConfirmOff] = useState<{
    id: string;
    cli: Cli;
    name: string;
  } | null>(null);
  const [confirmOn, setConfirmOn] = useState<{
    id: string;
    cli: Cli;
    name: string;
    needsKey: boolean;
    keyKnown: boolean;
  } | null>(null);
  // 本次会话填过的 key —— 让"刚填完、立刻启用另一个 CLI"也不必再问(后端落盘后
  // 其实也能 recover，这只是更快的本地路径)。
  const sessionKeys = useRef<Record<string, string>>({});

  // 真实刷新 —— 任一请求失败就抛出，让调用方区分「初次加载失败(显示重试)」
  // 和「操作后复查失败(保留已知 status,只提示刷新失败)」。成功才更新状态。
  const reload = useCallback(async () => {
    const [e, s] = await Promise.all([api.mcpEnv(), api.mcpStatus()]);
    setEnv(e);
    setStatus(s);
  }, []);
  useEffect(() => {
    setLoadError(false);
    reload().catch((err) => {
      setLoadError(true);
      setError((err as Error)?.message ?? String(err));
    });
  }, [reload]);

  // node usable = present AND new enough. A present-but-too-old node (v14)
  // can't run npx MCP servers, so it must NOT count as OK (used to gate toggles
  // + drive the chip/warning).
  const nodeOk = (env?.node.present ?? true) && (env?.node.adequate ?? true);
  // Distinguish "missing" from "present but too old" for the warning copy.
  const nodeTooOld = !!env?.node.present && env?.node.adequate === false;

  const runOp = useCallback(
    async (mark: { id: string; cli?: Cli }, op: () => Promise<unknown>) => {
      setBusy(mark);
      setError(null);
      try {
        await op();
      } catch (err) {
        const msg = (err as Error).message || String(err);
        setError(msg);
        toast.error(
          t("mcp.opFailed", { defaultValue: "操作失败" }),
          { description: msg },
        );
        setBusy(null);
        return;
      }
      // 操作本身成功 —— 复查刷新若失败,绝不清空已知 status(否则会把刚装好的
      // 开关显示成"未装"),只提示"已操作成功，但状态刷新失败"(P1-12)。
      try {
        await reload();
      } catch (err) {
        toast.warning(
          t("mcp.opOkRefreshFailed", {
            defaultValue: "已操作成功，但状态刷新失败，显示的开关可能不是最新",
          }),
          { description: (err as Error)?.message },
        );
      } finally {
        setBusy(null);
      }
    },
    [reload, t],
  );

  const enable = (id: string, cli: Cli, apiKey?: string) =>
    runOp({ id, cli }, () => api.mcpInstall(id, cli, apiKey));
  const disable = (id: string, cli: Cli) =>
    runOp({ id, cli }, () => api.mcpUninstall(id, cli));

  const enableFlow = (
    id: string,
    cli: Cli,
    name: string,
    needsKey: boolean,
    keyKnown: boolean,
  ) => {
    if (!needsKey) {
      enable(id, cli);
    } else if (keyKnown) {
      // 复用已存 key：传 session key；没有就让后端从已配处 recover。
      enable(id, cli, sessionKeys.current[id]);
    } else {
      // 两边都没配过 → 弹一次框收 key。
      setKeyDialog({ id, name, cli });
    }
  };

  /** 改密钥：写进所有已启用该 server 的 CLI(同步、保持一致)。 */
  const syncKey = (id: string, key: string) =>
    runOp({ id }, async () => {
      sessionKeys.current[id] = key;
      const clis: Cli[] = [];
      if (status?.claude.includes(id)) clis.push("claude");
      if (status?.codex.includes(id)) clis.push("codex");
      for (const cli of clis) await api.mcpInstall(id, cli, key);
    });

  const retry = () => {
    setLoadError(false);
    setError(null);
    reload().catch((err) => {
      setLoadError(true);
      setError((err as Error)?.message ?? String(err));
    });
  };

  // 初次加载失败:连不上后端 / 接口报错。绝不渲染"你还没装任何 MCP"的空态,
  // 也不能永久卡 spinner —— 明确告知失败并给重试(P1-13)。
  if (loadError && !status && !env) {
    return (
      <div className="mx-auto flex w-full max-w-3xl flex-col items-center gap-3 p-6 md:p-8">
        <div className="flex flex-col items-center gap-3 rounded-lg border border-state-danger/40 bg-status-danger-soft px-6 py-8 text-center">
          <TriangleAlert className="size-6 text-state-danger" />
          <p className="font-heading text-sm font-semibold text-foreground-primary">
            {t("mcp.loadFailed", "加载 MCP 配置失败")}
          </p>
          {error && (
            <p className="max-w-md break-words font-caption text-xs text-foreground-tertiary">
              {error}
            </p>
          )}
          <Button variant="outline" onClick={retry}>
            {t("common.retry", "重试")}
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="mx-auto flex w-full max-w-3xl flex-col gap-7 p-6 md:p-8">
      {/* 运行环境 */}
      <section className="flex flex-col gap-2.5">
        <SectionLabel text={t("mcp.runtimeTitle", "运行环境")} />
        <div className="flex flex-wrap items-center gap-2">
          <RuntimeChip label="Node.js" info={env?.node} required />
          <RuntimeChip label="npm" info={env?.npm} />
          <RuntimeChip label="uv" info={env?.uv} />
        </div>
        {env && !env.node.present && (
          <p className="flex items-center gap-1.5 font-caption text-xs text-state-danger" role="alert">
            <TriangleAlert className="size-3.5 shrink-0" />
            {t("mcp.nodeMissing", "未检测到 Node.js — npx 类 MCP（含 chrome-devtools）无法运行，请先安装 Node.js LTS。")}
          </p>
        )}
        {nodeTooOld && (
          <p className="flex items-center gap-1.5 font-caption text-xs text-state-warning" role="alert">
            <TriangleAlert className="size-3.5 shrink-0" />
            {t("mcp.nodeTooOld", {
              version: env?.node.version ?? "",
              min: env?.node.minMajor ?? 18,
              defaultValue:
                "Node.js {{version}} 版本过低 — npx 类 MCP（含 chrome-devtools）需 Node {{min}}+（LTS），请升级后再启用。",
            })}
          </p>
        )}
      </section>

      {/* 服务器 */}
      <section className="flex flex-col gap-2.5">
        <SectionLabel text={t("mcp.serversTitle")} />
        <p className="flex items-start gap-1.5 rounded-md border border-status-warning/40 bg-status-warning-soft px-3 py-2 font-caption text-[11px] leading-relaxed text-foreground-secondary">
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-status-warning" />
          <span>
            {t("mcp.userScopeWarning")}
          </span>
        </p>
        <div className="flex flex-col gap-3">
          {SERVERS.map((srv) => {
            const meta = MCP_CATALOG.find((s) => s.id === srv.id);
            const Icon = meta?.icon ?? FALLBACK_ICON;
            const inClaude = status?.claude.includes(srv.id) ?? false;
            const inCodex = status?.codex.includes(srv.id) ?? false;
            const keyState = status?.keys?.[srv.id];
            const keyKnown = keyState?.present || !!sessionKeys.current[srv.id];

            const onToggle = (cli: Cli, on: boolean) => {
              if (!on) {
                setConfirmOff({ id: srv.id, cli, name: meta?.name ?? srv.id });
              } else {
                setConfirmOn({
                  id: srv.id,
                  cli,
                  name: meta?.name ?? srv.id,
                  needsKey: srv.needsKey,
                  keyKnown,
                });
              }
            };

            return (
              <div
                key={srv.id}
                className="flex flex-col gap-3 rounded-lg border border-border-subtle bg-surface-elevated p-4"
              >
                <div className="flex items-start gap-3">
                  <span className="flex size-9 shrink-0 items-center justify-center rounded-md bg-surface-tertiary text-foreground-secondary">
                    <Icon className="size-[18px]" />
                  </span>
                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-1.5">
                      <span className="font-heading text-sm font-semibold text-foreground-primary">
                        {meta?.name ?? srv.id}
                      </span>
                      <span className="rounded bg-surface-tertiary px-1 font-caption text-[9px] text-foreground-tertiary">
                        {t("mcp.tagStdio")}
                      </span>
                      <span className="font-caption text-[10px] text-foreground-tertiary">
                        {t("mcp.needsNode", "需 Node.js LTS")}
                      </span>
                      <a
                        href={meta?.docsUrl}
                        target="_blank"
                        rel="noreferrer"
                        className="inline-flex size-8 items-center justify-center rounded-md text-foreground-tertiary transition-colors hover:bg-surface-tertiary hover:text-accent-primary"
                        aria-label={t("mcp.docs", "文档")}
                        title={t("mcp.docs", "文档")}
                      >
                        <ExternalLink className="size-3.5" />
                      </a>
                    </div>
                    <p className="mt-0.5 font-caption text-[11px] leading-snug text-foreground-tertiary">
                      {meta?.purpose ? t(meta.purpose) : null}
                    </p>

                    {/* 密钥行(共享一把,claude/codex 通用) */}
                    {srv.needsKey && (
                      <div className="mt-1.5 flex flex-wrap items-center gap-1.5 font-caption text-[11px]">
                        <KeyRound className="size-3 shrink-0 text-foreground-tertiary" />
                        {keyState?.present ? (
                          <>
                            <span className="font-mono text-foreground-secondary">{keyState.masked}</span>
                            {keyState.consistent ? (
                              <span className="text-foreground-tertiary">{t("mcp.keyShared", "· claude / codex 共用")}</span>
                            ) : (
                              <span className="flex items-center gap-0.5 text-state-warning">
                                <TriangleAlert className="size-3" />
                                {t("mcp.keyDrift", "· 两边 key 不一致")}
                              </span>
                            )}
                            <button
                              type="button"
                              disabled={busy?.id === srv.id}
                              onClick={() =>
                                setKeyDialog({ id: srv.id, name: meta?.name ?? srv.id, masked: keyState.masked })
                              }
                              className="inline-flex min-h-8 items-center rounded px-2 text-accent-primary hover:bg-accent-primary-soft disabled:opacity-50"
                            >
                              {t("mcp.changeKey", "改密钥")}
                            </button>
                          </>
                        ) : (
                          <span className="text-foreground-tertiary">
                            {t("mcp.keyUnset", "密钥未设置（打开开关时填写一次，两边通用）")}
                          </span>
                        )}
                      </div>
                    )}
                  </div>
                </div>

                <div className="flex items-center gap-6 border-t border-border-subtle pt-3">
                  <CliToggle
                    label="Claude"
                    on={inClaude}
                    pending={busy?.id === srv.id && busy?.cli === "claude"}
                    disabled={!status || busy?.id === srv.id || (!nodeOk && !inClaude)}
                    onChange={(v) => onToggle("claude", v)}
                  />
                  <CliToggle
                    label="Codex"
                    on={inCodex}
                    pending={busy?.id === srv.id && busy?.cli === "codex"}
                    disabled={!status || busy?.id === srv.id || (!nodeOk && !inCodex)}
                    onChange={(v) => onToggle("codex", v)}
                  />
                  {(busy?.id === srv.id && busy?.cli === undefined) && (
                    <span className="ml-auto flex items-center gap-1.5 font-caption text-[11px] text-foreground-tertiary">
                      <Loader2 className="size-3.5 animate-spin" />
                      {t("mcp.syncing", "同步中…")}
                    </span>
                  )}
                  {!status && (
                    <span className="ml-auto flex items-center gap-1.5 font-caption text-[11px] text-foreground-tertiary">
                      <Loader2 className="size-3.5 animate-spin" />
                      {t("common.loading", "加载中…")}
                    </span>
                  )}
                </div>
              </div>
            );
          })}
        </div>

        {error && (
          <p className="font-caption text-xs text-state-danger" role="alert">
            {error}
          </p>
        )}
        <p className="font-caption text-[11px] leading-relaxed text-foreground-tertiary">
          {t("mcp.realWriteHint", "开关直接改 claude / codex 的 MCP 配置（用户级，对所有工作区的 agent 生效）。更多 MCP 陆续接入。")}
        </p>
      </section>

      {/* API key 弹框(设首把 key / 改密钥) */}
      <ApiKeyDialog
        target={keyDialog}
        hintKey={SERVERS.find((s) => s.id === keyDialog?.id)?.keyHintKey}
        onCancel={() => setKeyDialog(null)}
        onConfirm={(key) => {
          if (!keyDialog) return;
          const { id, cli } = keyDialog;
          setKeyDialog(null);
          sessionKeys.current[id] = key;
          if (cli) enable(id, cli, key);
          else syncKey(id, key);
        }}
      />

      {/* 开启确认 — 同样会改用户级真实配置，不能误触即生效。 */}
      <Dialog
        open={confirmOn !== null}
        onOpenChange={(o) => !o && setConfirmOn(null)}
      >
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle>
              {t("mcp.enableTitle", "启用这个 MCP？")}
              {confirmOn ? ` — ${confirmOn.name}` : ""}
            </DialogTitle>
            <DialogDescription>
              {t(
                "mcp.enableDesc",
                "会写入该 CLI 的用户级配置，对所有工作区的 agent 生效。请只在信任当前本机项目与该 MCP server 时启用。",
              )}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmOn(null)}>
              {t("common.cancel", "取消")}
            </Button>
            <Button
              onClick={() => {
                if (!confirmOn) return;
                const { id, cli, name, needsKey, keyKnown } = confirmOn;
                setConfirmOn(null);
                enableFlow(id, cli, name, needsKey, keyKnown);
              }}
            >
              {t("mcp.enableConfirm", "启用")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 关闭确认 — 改的是用户真实配置 + 即时影响在跑 agent (FAULT-026) */}
      <Dialog
        open={confirmOff !== null}
        onOpenChange={(o) => !o && setConfirmOff(null)}
      >
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle>
              {t("mcp.disableTitle", "关闭这个 MCP？")}
              {confirmOff ? ` — ${confirmOff.name}` : ""}
            </DialogTitle>
            <DialogDescription>
              {t(
                "mcp.disableDesc",
                "会从该 CLI 的用户级配置里移除此 MCP，对所有工作区的 agent 即时生效；正在依赖它的 agent 可能失能。随时可重新开启。",
              )}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmOff(null)}>
              {t("common.cancel", "取消")}
            </Button>
            <Button
              variant="destructive"
              onClick={() => {
                if (!confirmOff) return;
                const { id, cli } = confirmOff;
                setConfirmOff(null);
                disable(id, cli);
              }}
            >
              {t("mcp.disableConfirm", "关闭")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

// ── 子组件 ──────────────────────────────────────────────────────────

function displayToolVersion(v: string | undefined): string | undefined {
  if (!v) return v;
  return v.trim().split(/[\s(]/)[0] || v;
}

function SectionLabel({ text }: { text: string }) {
  return (
    <span className="font-heading text-xs font-semibold tracking-wider text-foreground-tertiary">
      {text}
    </span>
  );
}

function RuntimeChip({
  label,
  info,
  required,
}: {
  label: string;
  info?: RuntimeInfo;
  required?: boolean;
}) {
  const { t } = useTranslation();
  const present = info?.present;
  const loading = info === undefined;
  // present but too old (node only) → not a clean ✓: amber, not green/red.
  const tooOld = present === true && info?.adequate === false;
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-md border px-2 py-1 font-mono text-[11px]",
        loading
          ? "border-border-subtle bg-surface-elevated text-foreground-tertiary"
          : tooOld
            ? "border-state-warning/40 bg-status-warning-soft text-state-warning"
            : present
              ? "border-border-subtle bg-surface-elevated text-foreground-secondary"
              : "border-state-danger/40 bg-status-danger-soft text-state-danger",
      )}
    >
      {loading ? (
        <Loader2 className="size-3 animate-spin" />
      ) : present && !tooOld ? (
        <Check className="size-3 text-state-success" />
      ) : (
        <TriangleAlert className="size-3" />
      )}
      <span>{label}</span>
      {info?.version ? (
        <span className="opacity-80">{displayToolVersion(info.version)}</span>
      ) : present === false ? (
        <span>{required ? t("mcp.notInstalled", { defaultValue: "未安装" }) : "—"}</span>
      ) : null}
    </span>
  );
}

function CliToggle({
  label,
  on,
  pending,
  disabled,
  onChange,
}: {
  label: string;
  on: boolean;
  pending: boolean;
  disabled: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <span className="flex items-center gap-1.5">
      <span className="font-heading text-[13px] leading-none text-foreground-secondary">{label}</span>
      {pending ? (
        <Loader2 className="size-3.5 animate-spin text-foreground-tertiary" />
      ) : (
        <Switch
          size="sm"
          checked={on}
          disabled={disabled}
          onCheckedChange={onChange}
          aria-label={label}
        />
      )}
    </span>
  );
}

function ApiKeyDialog({
  target,
  hintKey,
  onCancel,
  onConfirm,
}: {
  target: KeyDialogState | null;
  hintKey?: string;
  onCancel: () => void;
  onConfirm: (key: string) => void;
}) {
  const { t } = useTranslation();
  const hint = hintKey
    ? t(hintKey, { defaultValue: "context7.com 免费申请" })
    : undefined;
  const [key, setKey] = useState("");
  useEffect(() => {
    setKey("");
  }, [target]);
  const open = target !== null;
  const editing = target ? target.cli === undefined : false;
  return (
    <Dialog open={open} onOpenChange={(o) => !o && onCancel()}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>
            {editing ? t("mcp.changeKey", "改密钥") : t("mcp.keyTitle", "填写 API Key")}
            {target ? ` — ${target.name}` : ""}
          </DialogTitle>
          <DialogDescription>
            {t("mcp.keyDesc", "claude 和 codex 共用同一把 key，填一次即可；保存会同步到两边已启用的 CLI。")}
            {hint ? ` ${hint}` : ""}
          </DialogDescription>
        </DialogHeader>
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="mcp-api-key">API Key</Label>
          <Input
            id="mcp-api-key"
            name="mcp-api-key"
            value={key}
            autoFocus
            type="password"
            onChange={(e) => setKey(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && key.trim()) onConfirm(key.trim());
            }}
            placeholder={
              editing && target?.masked
                ? t("mcp.keyOverridePlaceholder", {
                    masked: target.masked,
                    defaultValue: "{{masked}}（输入新 key 覆盖）",
                  })
                : "ctx7sk-…"
            }
            className="font-mono text-xs"
            spellCheck={false}
          />
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={onCancel}>
            {t("common.cancel", "取消")}
          </Button>
          <Button onClick={() => key.trim() && onConfirm(key.trim())} disabled={!key.trim()}>
            {editing ? t("mcp.save", "保存") : t("mcp.add", "添加")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
