/**
 * Consult (多模对比) — the answer/research fusion view. Ask a question, pick a
 * panel of zulu models; the server runs panel → judge → synthesis and returns
 * the structured analysis + final answer. Distinct from the code-competition
 * 竞赛 (Fusion) view.
 */
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Loader2, Users, Sparkles } from "lucide-react";
import { EmptyState } from "@/components/EmptyState";
import { api } from "@/api/http";
import type { FusionConsultResponse } from "@/api/types";
import { useWorkspaceContext } from "../Shell";
import { LabGate } from "@/components/LabGate";

// R3:实验室功能(默认 OFF)。深链进来时给诚实空态 + 去设置开启;
// ON 时走 ConsultViewBody,行为不变。
export default function ConsultView() {
  const { t } = useTranslation();
  return (
    <LabGate
      icon={<Users className="size-8" />}
      feature={t("chat.tabs.consult")}
    >
      <ConsultViewBody />
    </LabGate>
  );
}

function ConsultViewBody() {
  const { t } = useTranslation();
  const { workspace } = useWorkspaceContext();
  const workspaceId = workspace.workspaceId;
  const [models, setModels] = useState<{ displayName: string }[] | null>(null);
  const [picked, setPicked] = useState<string[]>([]);
  const [question, setQuestion] = useState("");
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<FusionConsultResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .getZuluModels()
      .then((m) => {
        setModels(m);
        // Default panel: first 3 models from the real list. (A hardcoded
        // cross-vendor trio used to live here — the names could never match
        // getZuluModels output, so the fallback always won anyway.)
        const names = m.map((x) => x.displayName);
        setPicked(names.slice(0, 3));
      })
      .catch((e) => setError((e as Error).message));
  }, []);

  const toggle = (name: string) =>
    setPicked((cur) =>
      cur.includes(name) ? cur.filter((n) => n !== name) : cur.length < 8 ? [...cur, name] : cur,
    );

  const run = async () => {
    if (!workspaceId || !question.trim() || picked.length < 2) return;
    setRunning(true);
    setError(null);
    setResult(null);
    try {
      const r = await api.fusionConsult(workspaceId, { question: question.trim(), panel: picked });
      setResult(r);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setRunning(false);
    }
  };

  return (
    <div className="mx-auto flex max-w-3xl flex-col gap-4 overflow-y-auto p-6">
      <div className="flex items-center gap-2">
        <Users className="size-5 text-accent-primary" />
        <h1 className="text-lg font-semibold">{t("consult.title")}</h1>
        <span className="font-caption text-xs text-foreground-tertiary">
          {t("consult.subtitle")}
        </span>
      </div>

      {models === null && !error && (
        <p className="font-caption text-xs text-foreground-tertiary">
          {t("consult.loading", "正在加载模型列表…")}
        </p>
      )}
      {models && models.length === 0 && (
        <EmptyState
          icon={<Users className="size-8" />}
          title={t("consult.noModels")}
          hint={t("consult.noModelsHint")}
          primaryAction={{
            label: t("consult.noModelsAction"),
            href: "/settings/plugins",
          }}
        />
      )}

      {models && models.length > 0 && (
        <>
          <textarea
            value={question}
            onChange={(e) => setQuestion(e.target.value)}
            placeholder={t("consult.placeholder")}
            className="min-h-20 w-full resize-y rounded-md border border-border-subtle bg-surface-elevated px-3 py-2 text-sm text-foreground focus:border-accent-primary focus:outline-none"
          />
          <div className="flex flex-col gap-1">
            <span className="font-caption text-[11px] text-foreground-tertiary">
              {t("consult.panelLabel", { count: picked.length })}
            </span>
            <div className="flex flex-wrap gap-1.5">
              {models.map((m) => {
                const on = picked.includes(m.displayName);
                return (
                  <button
                    key={m.displayName}
                    type="button"
                    onClick={() => toggle(m.displayName)}
                    className={`rounded-md border px-2 py-1 font-mono text-[11px] transition-colors ${
                      on
                        ? "border-accent-primary bg-accent-primary-soft text-accent-primary"
                        : "border-border-subtle bg-surface-elevated text-foreground-secondary hover:bg-surface-tertiary"
                    }`}
                  >
                    {m.displayName}
                  </button>
                );
              })}
            </div>
          </div>
          <div className="flex items-center gap-3">
            <button
              type="button"
              onClick={run}
              disabled={running || !question.trim() || picked.length < 2}
              className="inline-flex items-center gap-1.5 rounded-md bg-accent-primary px-3 py-1.5 text-sm font-medium text-white transition-colors hover:opacity-90 disabled:opacity-50"
            >
              {running ? <Loader2 className="size-4 animate-spin" /> : <Sparkles className="size-4" />}
              {running ? t("consult.running", "对比中…（约 1–2 分钟）") : t("consult.start", "开始对比")}
            </button>
            <span className="font-caption text-[11px] text-foreground-tertiary">
              {t("consult.cost", "成本约等于（所选模型数 + 2）次调用；适合高价值问题，不要当默认操作。")}
            </span>
          </div>
        </>
      )}

      {error && (
        <div className="rounded-md border border-state-danger/40 bg-status-danger-soft px-3 py-2 text-xs text-state-danger">
          {error}
        </div>
      )}

      {result && <ConsultResult result={result} />}
    </div>
  );
}

function ConsultResult({ result }: { result: FusionConsultResponse }) {
  const { t } = useTranslation();
  const a = result.analysis;
  const sections: [string, string[]][] = [
    [t("consult.consensus", "共识"), a.consensus],
    [t("consult.contradictions", "矛盾"), a.contradictions],
    [t("consult.insights", "独特洞察"), a.unique_insights],
    [t("consult.blindspots", "盲区"), a.blind_spots],
  ];
  const hasAnalysis = sections.some(([, items]) => items.length > 0);
  return (
    <div className="flex flex-col gap-4">
      <div className="rounded-md border border-accent-primary/40 bg-accent-primary-soft/30 p-4">
        <div className="mb-1 flex items-center gap-1.5 text-sm font-semibold text-accent-primary">
          <Sparkles className="size-4" /> {t("consult.synthesis", "综合定稿")}
        </div>
        <div className="whitespace-pre-wrap text-sm text-foreground">{result.synthesis}</div>
      </div>

      {hasAnalysis ? (
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          {sections.map(([title, items]) => (
            <div key={title} className="rounded-md border border-border-subtle bg-surface-secondary p-3">
              <div className="mb-1 font-caption text-xs font-medium text-foreground-secondary">
                {title}（{items.length}）
              </div>
              {items.length === 0 ? (
                <div className="font-caption text-[11px] text-foreground-tertiary">—</div>
              ) : (
                <ul className="list-disc space-y-1 pl-4 text-xs text-foreground-secondary">
                  {items.map((it, i) => (
                    <li key={i}>{it}</li>
                  ))}
                </ul>
              )}
            </div>
          ))}
        </div>
      ) : (
        // Structured comparison didn't parse this run — the synthesis (which reads
        // the raw judge text) is still authoritative; don't show four empty boxes.
        <div className="rounded-md border border-border-subtle bg-surface-secondary px-3 py-2 text-[11px] text-foreground-tertiary">
          {t("consult.noAnalysis", "本次结构化对比未能解析，综合定稿已给出结论；可展开下方原始答案自行比对。")}
        </div>
      )}

      <details className="rounded-md border border-border-subtle bg-surface-secondary p-3">
        <summary className="cursor-pointer font-caption text-xs text-foreground-secondary">
          {t("consult.rawPanel", { defaultValue: "Panel 原始答案（{{count}}）", count: result.panel.length })} · {result.cost_note}
        </summary>
        <div className="mt-2 flex flex-col gap-2">
          {result.panel.map((p) => (
            <div key={p.model} className="border-t border-border-subtle pt-2 first:border-t-0">
              <div className="font-mono text-[11px] text-foreground-tertiary">
                {p.model} · {p.ok ? `${p.elapsed_ms}ms` : t("consult.failed", "失败")}
              </div>
              <div className="whitespace-pre-wrap text-xs text-foreground-secondary">{p.answer}</div>
            </div>
          ))}
        </div>
      </details>
    </div>
  );
}
