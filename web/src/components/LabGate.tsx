/**
 * LabGate — 「实验室功能」路由守卫(R3 形态收敛)。
 *
 * 设置→通用 的 labFeatures 开关(默认 OFF)为关时,深链到 /fusion、/consult
 * 不渲染功能本体,只给一句实话 + 去设置开启的入口。功能代码一行没删 ——
 * 这是收敛默认形态(declutter),不是下线;ON 时 children 原样渲染。
 */

import type { ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { EmptyState } from "@/components/EmptyState";
import { useLabFeatures } from "@/lib/appSettings";

export function LabGate({
  icon,
  feature,
  children,
}: {
  icon: ReactNode;
  /** 功能的人话名字(如「竞赛」「多模对比」),填进提示文案。 */
  feature: string;
  children: ReactNode;
}) {
  const { t } = useTranslation();
  const labOn = useLabFeatures();
  if (labOn) return <>{children}</>;
  return (
    <EmptyState
      icon={icon}
      title={t("lab.gateTitle", { defaultValue: "实验室功能已关闭" })}
      hint={t("lab.gateHint", {
        feature,
        defaultValue: "「{{feature}}」还在实验阶段，默认不展示；开启后即可使用。",
      })}
      primaryAction={{
        label: t("lab.gateAction", { defaultValue: "去设置开启" }),
        href: "/settings/general",
      }}
    />
  );
}
