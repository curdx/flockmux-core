/**
 * McpRoute — 独立顶级页面 /mcp。「快捷安装 MCP」的主入口(VS Code 扩展市场式)。
 * 页面 chrome(header 标题 + 副标) + McpManager 卡片网格主体。左侧导航菜单的
 * 「MCP」项链接到这里。
 *
 * 真读真写 claude/codex 的 MCP 配置（详见 McpManager + 后端 routes/mcp_admin.rs）。
 */

import { useTranslation } from "react-i18next";
import { Blocks } from "lucide-react";
import { McpManager } from "@/components/mcp/McpPanel";

export default function McpRoute() {
  const { t } = useTranslation();
  return (
    <div className="flex h-full flex-col bg-surface-primary">
      <header className="flex h-14 shrink-0 items-center gap-3 border-b border-border-subtle bg-surface-elevated px-5">
        <span className="flex size-8 items-center justify-center rounded-md bg-accent-primary-soft">
          <Blocks className="size-4 text-accent-primary-deep" />
        </span>
        <div className="flex flex-col">
          <h1 className="font-heading text-sm font-semibold text-foreground-primary">
            {t("mcp.pageTitle", "MCP")}
          </h1>
          <span className="font-caption text-[10px] text-foreground-tertiary">
            {t("mcp.pageSubtitle", "给 AI 加外部工具。对所有工作空间生效，只保存在这台电脑。")}
          </span>
        </div>
      </header>
      <div className="min-h-0 flex-1 overflow-y-auto">
        <McpManager />
      </div>
    </div>
  );
}
