/**
 * Welcome — 第一次打开 swarmx / 没选工作空间时的主介绍屏。
 *
 * 用户进 app 给你的注意力大概 10 秒：要么这 10 秒里看懂"swarmx 是啥
 * 能干啥"、按下"开始"按钮，要么关掉再不打开。所以这屏必须：
 *   1. 一句话讲清楚产品定位 (不用术语)
 *   2. 一个突出主 CTA (新建工作空间)
 *   3. 二级链接 (看文档 / 看 demo) 给犹豫派
 *
 * 同一组件 ChatHome 和 WorkspaceShell empty (wsId 不存在时) 共用，避免
 * 同一句话在两个地方说，也避免左 sidebar 大卡片 + 中间大按钮的双 CTA
 * 重复。Sidebar 的 empty 是另一个组件 (WorkspaceListEmpty) 配合，那里
 * 只是个安静的提示，主战场在中间这屏。
 */

import { useTranslation } from "react-i18next";
import { BrandMark } from "@/components/Brand";
import {
  ArrowRight,
  ExternalLink,
  MessageSquare,
  Sparkles,
  Workflow,
} from "lucide-react";

interface Props {
  /** 触发新建工作空间 (开 wizard)。父组件持有 wizard open state。 */
  onCreateWorkspace: () => void;
}

const HIGHLIGHTS = [
  { icon: MessageSquare, key: "welcome.highlight.chat" },
  { icon: Workflow, key: "welcome.highlight.spell" },
  { icon: Sparkles, key: "welcome.highlight.auto" },
] as const;

export function Welcome({ onCreateWorkspace }: Props) {
  const { t } = useTranslation();
  return (
    <section className="relative flex h-full min-h-0 flex-col overflow-hidden bg-[radial-gradient(circle_at_top,rgba(37,99,235,0.08),transparent_32%),linear-gradient(180deg,transparent,rgba(248,250,252,0.9)_30%,rgba(248,250,252,1))] px-6 py-10 sm:px-10 sm:py-12">
      <div className="mx-auto flex w-full max-w-5xl flex-1 flex-col justify-center gap-8">
        <div className="flex flex-col gap-5">
          <div className="inline-flex w-fit items-center gap-2 rounded-full border border-accent-primary/15 bg-accent-primary/8 px-3 py-1 font-caption text-[11px] font-semibold tracking-wide text-accent-primary">
            <BrandMark size={14} className="rounded-[4px]" />
            {t("welcome.eyebrow", "多 agent 协作工作台")}
          </div>
          <div className="grid gap-8 lg:grid-cols-[minmax(0,1.15fr)_minmax(320px,0.85fr)] lg:items-start">
            <div className="flex min-w-0 flex-col gap-4">
              <h1 className="max-w-3xl font-heading text-4xl font-bold leading-[1.08] tracking-tight text-foreground-primary sm:text-5xl">
                {t("welcome.title")}
              </h1>
              <p className="max-w-2xl font-body text-[15px] leading-7 text-foreground-secondary sm:text-[17px]">
                {t("welcome.subtitle")}
              </p>
            </div>
            <div className="grid gap-3 rounded-2xl border border-border-subtle bg-surface-elevated/80 p-4 shadow-[0_18px_45px_rgba(15,23,42,0.06)] backdrop-blur">
              <div className="flex items-center justify-between gap-3">
                <span className="font-heading text-sm font-semibold text-foreground-primary">
                  {t("welcome.panelTitle", "首次进入通常这样开始")}
                </span>
                <span className="rounded-full bg-surface-tertiary px-2 py-0.5 font-caption text-[10px] text-foreground-tertiary">
                  {t("welcome.stepsBadge", "3 步")}
                </span>
              </div>
              {["welcome.step1", "welcome.step2", "welcome.step3"].map((key, index) => (
                <div
                  key={key}
                  className="flex items-start gap-3 rounded-xl border border-border-subtle/70 bg-surface-primary/70 px-3 py-3"
                >
                  <span className="flex size-7 shrink-0 items-center justify-center rounded-full bg-accent-primary text-[11px] font-semibold text-foreground-on-accent">
                    {index + 1}
                  </span>
                  <div className="min-w-0">
                    <div className="font-heading text-sm font-semibold text-foreground-primary">
                      {t(`${key}.title`)}
                    </div>
                    <div className="mt-0.5 text-[13px] leading-6 text-foreground-secondary">
                      {t(`${key}.body`)}
                    </div>
                  </div>
                </div>
              ))}
            </div>
          </div>
        </div>

        {/* 三个 highlight chip — 解释 "swarmx 跟单开一个 claude 有什么不一样"，
         *  比纯一句话 slogan 更具体，比一整段产品文档轻。 */}
        <div className="grid gap-3 md:grid-cols-3">
        {HIGHLIGHTS.map((h) => {
          const Icon = h.icon;
          return (
            <div
              key={h.key}
              className="flex min-h-[132px] flex-col gap-2 rounded-2xl border border-border-subtle bg-surface-elevated/80 px-4 py-4 text-left shadow-[0_14px_30px_rgba(15,23,42,0.04)]"
            >
              <span className="flex size-10 items-center justify-center rounded-xl bg-accent-primary/10 text-accent-primary">
                <Icon className="size-5" />
              </span>
              <span className="font-heading text-sm font-semibold text-foreground-primary">
                {t(`${h.key}.title`)}
              </span>
              <span className="font-body text-[13px] leading-6 text-foreground-secondary">
                {t(`${h.key}.body`)}
              </span>
            </div>
          );
        })}
        </div>

        <div className="flex flex-col items-start gap-3 sm:flex-row sm:items-center">
          <button
            type="button"
            onClick={onCreateWorkspace}
            className="inline-flex h-11 items-center gap-2 rounded-xl bg-accent-primary px-5 font-heading text-sm font-semibold text-foreground-on-accent shadow-[0_14px_32px_rgba(37,99,235,0.28)] transition-all hover:bg-accent-primary-deep hover:shadow-[0_16px_36px_rgba(29,78,216,0.32)] active:translate-y-px"
          >
            <Sparkles className="size-4" />
            {t("welcome.primary")}
            <ArrowRight className="size-4" />
          </button>
          <a
            href="https://github.com/mugsun/swarmx#readme"
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1.5 font-caption text-[12px] text-foreground-tertiary transition-colors hover:text-foreground-secondary hover:underline"
          >
            {t("welcome.secondary")}
            <ExternalLink className="size-3.5" />
          </a>
        </div>
      </div>
    </section>
  );
}
