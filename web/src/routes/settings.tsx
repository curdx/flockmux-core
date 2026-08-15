/**
 * Preferences — Pencil frame nJqkA.
 *
 * Left nav (200) + right detail. Persistence is localStorage only —
 * swarmx-storage has no kv settings table yet; the existing crates
 * own session state, not user prefs. When that table lands we'll
 * promote read/write to api.getSettings / api.putSettings without
 * changing this surface.
 *
 * Sections beyond 通用 are scaffolded but only General is fully wired —
 * the visual goal is "settings page exists and looks right", not a full
 * preference matrix.
 */

import React, { useEffect, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useTranslation } from "react-i18next";
import i18n from "@/i18n";
import { setTheme } from "@/lib/theme";
import { HTTP_BASE } from "@/lib/apiBase";
import { api } from "@/api/http";
import type {
  CliPluginInfo,
  EngineReadiness,
  ModelConfig,
  ModelsResponse,
} from "@/api/types";
import { useEngineReadiness } from "@/hooks/useEngineReadiness";
import { evidenceOf, EVIDENCE_I18N } from "@/lib/engineEvidence";
import {
  Activity,
  Bell,
  Check,
  CheckCircle2,
  CircleAlert,
  CircleCheck,
  CircleDashed,
  CircleX,
  DownloadCloud,
  Loader2,
  RefreshCw,
  ShieldCheck,
  CircleUser,
  Copy,
  Cpu,
  ExternalLink,
  Globe,
  Info,
  Keyboard,
  Layers,
  Moon,
  Palette,
  Plug,
  Settings as SettingsIcon,
  Shield,
  TriangleAlert,
  Sun,
  SunMoon,
} from "lucide-react";
import { toast } from "@/lib/toast";
import { ApiError } from "@/api/http";
import { Button } from "@/components/ui/button";
import {
  checkForUpdate,
  installUpdate,
  inTauri,
  useAvailableUpdate,
  type Update,
} from "@/lib/updater";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  ConfirmActionDialog,
  type ConfirmActionState,
} from "@/components/ConfirmActionDialog";
import { cn } from "@/lib/cn";
import { formatShortcutChord, getClientPlatformInfo } from "@/lib/platform";

const STORAGE_KEY = "swarmx:settings:v1";

type Lang = "zh" | "en";
type Theme = "light" | "dark" | "system";

interface SettingsState {
  lang: Lang;
  theme: Theme;
  openMainOnLaunch: boolean;
  desktopNotify: boolean;
  killOthersOnFail: boolean;
}

const DEFAULTS: SettingsState = {
  lang: "zh",
  theme: "light",
  openMainOnLaunch: true,
  desktopNotify: true,
  killOthersOnFail: false,
};

function loadSettings(): SettingsState {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) return DEFAULTS;
    return { ...DEFAULTS, ...JSON.parse(raw) };
  } catch {
    return DEFAULTS;
  }
}

function saveSettings(s: SettingsState) {
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(s));
  } catch {
    /* ignore quota errors */
  }
}

const SECTIONS = [
  { id: "general", labelKey: "settings.sections.general", icon: SettingsIcon },
  { id: "appearance", labelKey: "settings.sections.appearance", icon: Palette },
  { id: "shortcuts", labelKey: "settings.sections.shortcuts", icon: Keyboard },
  { id: "models", labelKey: "settings.sections.models", icon: Cpu },
  { id: "plugins", labelKey: "settings.sections.plugins", icon: Plug },
  { id: "privacy", labelKey: "settings.sections.privacy", icon: Shield },
  { id: "about", labelKey: "settings.sections.about", icon: Info },
] as const;

type SectionId = (typeof SECTIONS)[number]["id"];

export default function SettingsRoute() {
  const { t } = useTranslation();
  const { section } = useParams<{ section?: string }>();
  const navigate = useNavigate();
  const [settings, setSettings] = useState<SettingsState>(loadSettings);
  const platform = getClientPlatformInfo();

  const activeId = (SECTIONS.find((s) => s.id === section)?.id ??
    "general") as SectionId;

  // ModelsPanel keeps its dirty flag here so leaving the section (nav click or
  // ⌘1..⌘6) can warn before silently dropping unsaved edits. A ref (not state)
  // because the panel mutates it on every keystroke and we don't want re-renders.
  const modelsDirtyRef = useRef(false);
  const [pendingNav, setPendingNav] = useState<string | null>(null);

  const guardedNavigate = (path: string) => {
    if (activeId === "models" && modelsDirtyRef.current) {
      setPendingNav(path);
      return;
    }
    navigate(path);
  };

  useEffect(() => {
    saveSettings(settings);
    // Runtime side-effects: theme flips data-theme; lang swaps i18n
    // resources. Everything else is passive (read by other code paths).
    setTheme(settings.theme);
    if (i18n.language !== settings.lang) {
      i18n.changeLanguage(settings.lang);
    }
  }, [settings]);

  // ⌘1..⌘6 (Ctrl+1..6 on non-mac) jumps to the nth section while
  // /settings is mounted. Skips ⌘K (palette) and modifier-only combos.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Don't hijack number keys while typing in a field (e.g. model id).
      const el = e.target as HTMLElement;
      if (
        el &&
        (el.tagName === "INPUT" ||
          el.tagName === "TEXTAREA" ||
          el.isContentEditable)
      )
        return;
      const hasModifier = e.metaKey || e.ctrlKey;
      if (!hasModifier || e.shiftKey || e.altKey) return;
      const n = parseInt(e.key, 10);
      if (!Number.isFinite(n) || n < 1 || n > SECTIONS.length) return;
      e.preventDefault();
      guardedNavigate(`/settings/${SECTIONS[n - 1].id}`);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [guardedNavigate]);

  const update = <K extends keyof SettingsState>(k: K, v: SettingsState[K]) =>
    setSettings((prev) => ({ ...prev, [k]: v }));

  return (
    <div className="flex h-full flex-col bg-surface-primary">
      {/* Header */}
      <header className="flex h-14 shrink-0 items-center gap-3 border-b border-border-subtle bg-surface-elevated px-5">
        <span className="flex size-8 items-center justify-center rounded-md bg-accent-primary-soft">
          <SettingsIcon className="size-4 text-accent-primary-deep" />
        </span>
        <div className="flex flex-col">
          <h1 className="font-heading text-sm font-semibold text-foreground-primary">
            {t("settings.title")}
          </h1>
          <span className="font-caption text-[10px] text-foreground-tertiary">
            {t("settings.subtitle")}
          </span>
        </div>
      </header>

      {/* Body */}
      <div className="flex min-h-0 flex-1 flex-col lg:flex-row">
        {/* Nav */}
        <aside className="grid shrink-0 grid-cols-2 gap-1 border-b border-border-subtle bg-surface-secondary p-3 sm:grid-cols-3 lg:flex lg:w-[200px] lg:flex-col lg:border-b-0 lg:border-r">
          {SECTIONS.map((s, i) => {
            const Icon = s.icon;
            const active = s.id === activeId;
            return (
              <button
                key={s.id}
                onClick={() => guardedNavigate(`/settings/${s.id}`)}
                className={cn(
                  "flex min-w-0 items-center gap-2.5 rounded-md px-3 py-2 text-left text-sm lg:w-full",
                  active
                    ? "bg-accent-primary-soft text-foreground-primary"
                    : "text-foreground-secondary hover:bg-surface-tertiary",
                )}
              >
                <Icon className="size-4" />
                <span className="flex-1">{t(s.labelKey)}</span>
                <kbd
                  className={cn(
                    "rounded border border-border-subtle px-1 font-mono text-[9px]",
                    active
                      ? "bg-surface-elevated text-foreground-secondary"
                      : "bg-surface-elevated text-foreground-tertiary",
                  )}
                >
                  {formatShortcutChord(i + 1, platform)}
                </kbd>
              </button>
            );
          })}
          <div className="ml-auto hidden px-3 pt-4 font-caption text-[10px] text-foreground-tertiary lg:mt-auto lg:block">
            <p className="font-mono">swarmx</p>
            <p>v{__APP_VERSION__}</p>
          </div>
        </aside>

        {/* Detail */}
        <section className="min-h-0 flex-1 overflow-y-auto">
          {activeId === "general" && (
            <GeneralPanel settings={settings} update={update} />
          )}
          {activeId === "appearance" && (
            <AppearancePanel settings={settings} update={update} />
          )}
          {activeId === "shortcuts" && <ShortcutsPanel />}
          {activeId === "models" && <ModelsPanel dirtyRef={modelsDirtyRef} />}
          {activeId === "plugins" && <PluginsPanel />}
          {activeId === "privacy" && <PrivacyPanel />}
          {activeId === "about" && <AboutPanel />}
        </section>
      </div>

      {/* Leaving the Models section with unsaved edits — confirm before
          dropping them (the panel unmounts on navigate, losing local state). */}
      <ConfirmActionDialog
        action={
          pendingNav
            ? {
                title: t("settings.models.unsavedLeaveTitle"),
                description: t("settings.models.unsavedLeaveDesc"),
                confirmLabel: t("settings.models.unsavedLeaveConfirm", {
                  defaultValue: "放弃改动",
                }),
                variant: "destructive",
                onConfirm: () => {
                  modelsDirtyRef.current = false;
                  navigate(pendingNav);
                },
              }
            : null
        }
        onOpenChange={(open) => {
          if (!open) setPendingNav(null);
        }}
      />
    </div>
  );
}

// ── Sections ─────────────────────────────────────────────────────────────

function GeneralPanel({
  settings,
  update,
}: {
  settings: SettingsState;
  update: <K extends keyof SettingsState>(k: K, v: SettingsState[K]) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="mx-auto flex max-w-2xl flex-col gap-8 p-8">
      <PanelTitle
        title={t("settings.general.title")}
        hint={t("settings.general.hint")}
      />

      <Field
        label={t("settings.general.lang")}
        hint={t("settings.general.langHint")}
      >
        <div className="grid grid-cols-2 gap-3">
          <ChoiceCard
            active={settings.lang === "zh"}
            onClick={() => update("lang", "zh")}
            title="中文"
            sub="简体中文"
            icon={<Globe className="size-5" />}
          />
          <ChoiceCard
            active={settings.lang === "en"}
            onClick={() => update("lang", "en")}
            title="English"
            sub="US English"
            icon={<Globe className="size-5" />}
          />
        </div>
      </Field>

      <Field
        label={t("settings.general.launch")}
        hint={t("settings.general.launchHint")}
      >
        <div className="flex flex-col gap-3">
          <ToggleRow
            label={t("settings.general.openMain")}
            hint={t("settings.general.openMainHint")}
            value={settings.openMainOnLaunch}
            onChange={(v) => update("openMainOnLaunch", v)}
          />
          <ToggleRow
            label={t("settings.general.desktopNotify")}
            hint={t("settings.general.desktopNotifyHint")}
            value={settings.desktopNotify}
            onChange={(v) => update("desktopNotify", v)}
          />
        </div>
      </Field>

      <Field
        label={t("settings.general.failure")}
        hint={t("settings.general.failureHint")}
      >
        <ToggleRow
          label={t("settings.general.killOthers")}
          hint={t("settings.general.killOthersHint")}
          value={settings.killOthersOnFail}
          onChange={(v) => update("killOthersOnFail", v)}
        />
      </Field>
    </div>
  );
}

function AppearancePanel({
  settings,
  update,
}: {
  settings: SettingsState;
  update: <K extends keyof SettingsState>(k: K, v: SettingsState[K]) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="mx-auto flex max-w-2xl flex-col gap-8 p-8">
      <PanelTitle
        title={t("settings.appearance.title")}
        hint={t("settings.appearance.hint")}
      />
      <Field
        label={t("settings.appearance.theme")}
        hint={t("settings.appearance.themeHint")}
      >
        <div className="grid grid-cols-3 gap-3">
          <ThemeCard
            active={settings.theme === "light"}
            onClick={() => update("theme", "light")}
            label={t("settings.appearance.themes.light")}
            preview="light"
            icon={<Sun className="size-4" />}
          />
          <ThemeCard
            active={settings.theme === "dark"}
            onClick={() => update("theme", "dark")}
            label={t("settings.appearance.themes.dark")}
            preview="dark"
            icon={<Moon className="size-4" />}
          />
          <ThemeCard
            active={settings.theme === "system"}
            onClick={() => update("theme", "system")}
            label={t("settings.appearance.themes.system")}
            preview="system"
            icon={<SunMoon className="size-4" />}
          />
        </div>
      </Field>
    </div>
  );
}

// ── Shortcuts panel ─────────────────────────────────────────────────────

interface Binding {
  keys: string[];
  descKey: string;
}
interface Scope {
  id: "global" | "player" | "modal";
  bindings: Binding[];
}

const SHORTCUT_SCOPES: Scope[] = [
  {
    id: "global",
    bindings: [
      { keys: ["⌘", "K"], descKey: "settings.shortcuts.k.cmdk" },
      { keys: ["Esc"], descKey: "settings.shortcuts.k.esc" },
    ],
  },
  {
    id: "player",
    bindings: [
      { keys: ["␣"], descKey: "settings.shortcuts.k.playPause" },
      { keys: ["←", "/", "→"], descKey: "settings.shortcuts.k.skip" },
      { keys: ["."], descKey: "settings.shortcuts.k.frame" },
      { keys: ["Esc"], descKey: "settings.shortcuts.k.back" },
    ],
  },
];

function ShortcutsPanel() {
  const { t } = useTranslation();
  const modKey = getClientPlatformInfo().modifierKeyLabel;
  return (
    <div className="mx-auto flex max-w-2xl flex-col gap-8 p-8">
      <PanelTitle
        title={t("settings.shortcuts.title")}
        hint={t("settings.shortcuts.hint")}
      />
      {SHORTCUT_SCOPES.map((scope) => (
        <Field key={scope.id} label={t(`settings.shortcuts.scope.${scope.id}`)}>
          <div className="overflow-hidden rounded-lg border border-border-subtle">
            {scope.bindings.map((b, i) => {
              const keys =
                scope.id === "global" && i === 0
                  ? [modKey, "K"]
                  : b.keys;
              return (
              <div
                key={i}
                className={cn(
                  "flex items-center gap-4 px-4 py-3",
                  i > 0 && "border-t border-border-subtle",
                )}
              >
                <div className="flex flex-1 items-center gap-1.5">
                  {keys.map((k, j) => (
                    <kbd
                      key={j}
                      className="rounded border border-border-subtle bg-surface-elevated px-2 py-0.5 font-mono text-[11px] text-foreground-primary shadow-sm"
                    >
                      {k}
                    </kbd>
                  ))}
                </div>
                <span className="font-caption text-xs text-foreground-secondary">
                  {t(b.descKey)}
                </span>
              </div>
            );
            })}
          </div>
        </Field>
      ))}
    </div>
  );
}

// ── Models panel (F1: per-CLI tier→concrete-model mapping) ───────────────

const MODEL_TIERS = ["opus", "sonnet", "haiku"] as const;

function formNamePart(value: string): string {
  return value.toLowerCase().replace(/[^a-z0-9_-]+/g, "-");
}

/** Friendly copy for an error: ApiError already carries server-unwrapped detail. */
function errText(e: unknown): string {
  return e instanceof ApiError ? e.detail : (e as Error).message;
}

function ModelsPanel({
  dirtyRef,
}: {
  dirtyRef: React.MutableRefObject<boolean>;
}) {
  const { t } = useTranslation();
  const [data, setData] = useState<ModelsResponse | null>(null);
  const [cfg, setCfg] = useState<ModelConfig | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(() => {
    let cancelled = false;
    setLoadError(null);
    api
      .getModels()
      .then((r) => {
        if (!cancelled) {
          setData(r);
          setCfg(r.config);
        }
      })
      .catch((e) => {
        if (!cancelled) setLoadError(errText(e));
      });
    return () => {
      cancelled = true;
    };
  }, [reloadKey]);

  // Edits live only in `cfg`; the saved baseline is `data.config`. They diverge
  // the moment the user types, and re-converge after a successful save (which
  // refreshes both). Stringify is fine here — the shape is small + JSON-stable.
  const dirty =
    !!data && !!cfg && JSON.stringify(cfg) !== JSON.stringify(data.config);

  // Surface the dirty flag to the parent so leaving the section can warn first.
  useEffect(() => {
    dirtyRef.current = dirty;
    return () => {
      dirtyRef.current = false;
    };
  }, [dirty, dirtyRef]);

  // Closing the page/window mid-edit also drops the changes — let the browser's
  // native "leave site?" prompt fire (same pattern as useComposerDraft).
  useEffect(() => {
    if (!dirty) return;
    const onBeforeUnload = (e: BeforeUnloadEvent) => {
      e.preventDefault();
      e.returnValue = "";
    };
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, [dirty]);

  const cliOf = (id: string) =>
    cfg?.clis[id] ?? { default: "", tiers: {}, effort: "" };
  const setDefault = (id: string, v: string) =>
    setCfg((prev) =>
      prev
        ? {
            ...prev,
            clis: {
              ...prev.clis,
              [id]: { ...(prev.clis[id] ?? { default: "", tiers: {} }), default: v },
            },
          }
        : prev,
    );
  const setTier = (id: string, tier: string, v: string) =>
    setCfg((prev) => {
      if (!prev) return prev;
      const c = prev.clis[id] ?? { default: "", tiers: {} };
      return {
        ...prev,
        clis: { ...prev.clis, [id]: { ...c, tiers: { ...c.tiers, [tier]: v } } },
      };
    });
  const setEffort = (id: string, v: string) =>
    setCfg((prev) => {
      if (!prev) return prev;
      const c = prev.clis[id] ?? { default: "", tiers: {} };
      return { ...prev, clis: { ...prev.clis, [id]: { ...c, effort: v } } };
    });

  const save = async () => {
    // No diff vs. the saved baseline → nothing to PUT. Skip the round-trip but
    // still confirm "saved" so the click never feels like a no-op.
    if (!cfg || busy || !dirty) return;
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      const r = await api.putModels(cfg);
      setData((prev) => (prev ? { ...prev, config: r.config } : prev));
      setCfg(r.config);
      setSaved(true);
      window.setTimeout(() => setSaved(false), 2500);
    } catch (e) {
      const msg = errText(e);
      setError(msg);
      toast.error(t("settings.models.saveFailed", { error: msg }));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="mx-auto flex max-w-2xl flex-col gap-6 p-4 sm:p-8">
      <PanelTitle
        title={t("settings.models.title")}
        hint={t("settings.models.hint")}
      />
      {/* Load failure ≠ empty: show the error + a Retry, never the bare panel. */}
      {loadError && !data && (
        <div className="flex flex-col items-start gap-2 rounded-md border border-state-danger/40 bg-status-danger-soft px-3 py-2.5 text-xs text-state-danger">
          <span>{t("settings.models.loadError", { error: loadError })}</span>
          <Button
            size="sm"
            variant="outline"
            onClick={() => setReloadKey((k) => k + 1)}
          >
            {t("common.retry")}
          </Button>
        </div>
      )}
      {!data && !loadError && (
        <p className="font-caption text-sm text-foreground-tertiary">
          {t("common.loading")}
        </p>
      )}
      {/* Persistent unsaved-changes banner — so switching sections / closing
          the page never silently drops in-flight edits without a heads-up. */}
      {dirty && (
        <div className="flex items-center gap-2 rounded-md border border-status-warning/45 bg-status-warning-soft px-3 py-2 text-xs text-status-warning">
          <TriangleAlert className="size-3.5 shrink-0" />
          {t("settings.models.unsaved")}
        </div>
      )}
      {data &&
        cfg &&
        data.clis.map((cli) => (
          <div
            key={cli.id}
            className="flex flex-col gap-4 rounded-lg border border-border-subtle bg-surface-elevated p-4 sm:p-5"
          >
            <div className="flex items-center gap-2.5">
              <span className="flex size-8 items-center justify-center rounded-md bg-accent-primary-soft text-accent-primary-deep">
                <Cpu className="size-4" />
              </span>
              <div className="flex flex-col">
                <span className="font-heading text-sm font-semibold text-foreground-primary">
                  {cli.display_name}
                </span>
                <span className="font-mono text-[11px] text-foreground-tertiary">
                  {cli.id}
                </span>
              </div>
            </div>

            {!cli.supports_model ? (
              <p className="font-caption text-xs text-foreground-tertiary">
                {t("settings.models.noOverride")}
              </p>
            ) : (
              <div className="flex flex-col gap-3">
                <ModelRow
                  name={`model-${formNamePart(cli.id)}-default`}
                  label={t("settings.models.default")}
                  placeholder={t("settings.models.cliDefaultPlaceholder")}
                  value={cliOf(cli.id).default}
                  onChange={(v) => setDefault(cli.id, v)}
                />
                {/* opus/sonnet/haiku rows ONLY for CLIs that natively have
                    those tiers (claude). codex (gpt-5.x) doesn't — showing them
                    there is a leaky abstraction, so it gets just default+effort. */}
                {cli.native_tiers &&
                  MODEL_TIERS.map((tier) => (
                    <ModelRow
                      key={tier}
                      name={`model-${formNamePart(cli.id)}-${tier}`}
                      label={tier}
                      placeholder={t("settings.models.tierPlaceholder")}
                      value={cliOf(cli.id).tiers[tier] ?? ""}
                      onChange={(v) => setTier(cli.id, tier, v)}
                      mono
                    />
                  ))}
                <div className="mt-1 h-px bg-border-subtle" />
                <EffortRow
                  name={`model-${formNamePart(cli.id)}-effort`}
                  label={t("settings.models.effort")}
                  hint={t("settings.models.effortHint")}
                  // codex's reasoning effort tops out at xhigh (极高); picking
                  // 最大/max silently downgrades server-side. The per-direction
                  // picker says so in its level descriptions — mirror that here
                  // so the global default isn't a silent surprise.
                  note={
                    cli.id === "codex"
                      ? t("settings.models.effortCodexNote")
                      : undefined
                  }
                  value={cliOf(cli.id).effort ?? ""}
                  onChange={(v) => setEffort(cli.id, v)}
                />
              </div>
            )}
          </div>
        ))}

      {data && cfg && (
        <div className="flex flex-wrap items-center gap-3">
          {/* Disabled when clean (no diff) or mid-save — no no-op requests. */}
          <Button
            onClick={save}
            disabled={busy || !dirty}
            className="self-start"
          >
            {busy && <RefreshCw className="size-3.5 animate-spin" />}
            {busy ? t("common.loading") : t("settings.models.save")}
          </Button>
          {saved && (
            <span className="flex items-center gap-1 font-caption text-xs text-status-success">
              <Check className="size-3.5" />
              {t("settings.models.saved")}
            </span>
          )}
          {error && !busy && (
            <span className="font-caption text-xs text-state-danger">
              {t("settings.models.saveFailed", { error })}
            </span>
          )}
        </div>
      )}
    </div>
  );
}

function ModelRow({
  name,
  label,
  placeholder,
  value,
  onChange,
  mono,
}: {
  name: string;
  label: string;
  placeholder?: string;
  value: string;
  onChange: (v: string) => void;
  mono?: boolean;
}) {
  const id = React.useId();
  return (
    <div className="flex flex-col items-stretch gap-1.5 sm:flex-row sm:items-center sm:gap-3">
      <Label
        htmlFor={id}
        className={cn(
          "shrink-0 text-sm text-foreground-secondary sm:w-20",
          mono && "font-mono",
        )}
      >
        {label}
      </Label>
      <Input
        id={id}
        name={name}
        type="text"
        spellCheck={false}
        value={value}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        className="w-full font-mono text-xs sm:flex-1"
      />
    </div>
  );
}

/** Default reasoning-effort selector for a CLI (a fixed-option dropdown, not a
 *  free-text model id). "" = the model's own default. */
function EffortRow({
  name,
  label,
  hint,
  note,
  value,
  onChange,
}: {
  name: string;
  label: string;
  hint?: string;
  /** Extra CLI-specific caveat shown below the hint (e.g. codex effort cap). */
  note?: string;
  value: string;
  onChange: (v: string) => void;
}) {
  const { t } = useTranslation();
  const id = React.useId();
  const LEVELS = ["low", "medium", "high", "xhigh", "max"] as const;
  return (
    <div className="flex flex-col items-stretch gap-1.5 sm:flex-row sm:items-start sm:gap-3">
      <Label
        htmlFor={id}
        className="shrink-0 text-sm text-foreground-secondary sm:mt-1.5 sm:w-20"
      >
        {label}
      </Label>
      <div className="flex flex-1 flex-col gap-1">
        <Select
          name={name}
          value={value || "__default__"}
          onValueChange={(next) => onChange(next === "__default__" ? "" : next)}
        >
          <SelectTrigger id={id} className="w-full text-xs">
            <SelectValue placeholder={t("model.default")} />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="__default__">{t("model.default")}</SelectItem>
            {LEVELS.map((lv) => (
              <SelectItem key={lv} value={lv}>
                {t(`model.effort.${lv}`)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        {hint && (
          <span className="font-caption text-[10px] text-foreground-tertiary">
            {hint}
          </span>
        )}
        {note && (
          <span className="font-caption text-[10px] text-state-warning">
            {note}
          </span>
        )}
      </div>
    </div>
  );
}

// ── Plugins panel ───────────────────────────────────────────────────────

/** Small real-usability verdict pill shown next to an installed engine's
 *  "已安装" tag. Neutral "未验证" until a probe runs — never a fake green. */
function ProbeBadge({ verdict }: { verdict?: EngineReadiness }) {
  const { t } = useTranslation();
  const state = verdict?.state ?? "unknown";
  if (state === "not_installed") return null;
  const map = {
    usable: {
      cls: "bg-status-success-soft text-status-success",
      icon: <CircleCheck className="size-3" />,
      label: t("settings.plugins.verdictUsable", "可用"),
    },
    unknown: {
      cls: "bg-surface-tertiary text-foreground-tertiary",
      icon: <Info className="size-3" />,
      label: t("settings.plugins.verdictUnverified", "未验证"),
    },
    needs_login: {
      cls: "bg-status-warning-soft text-status-warning",
      icon: <CircleAlert className="size-3" />,
      label: t("settings.plugins.verdictNeedsLogin", "需登录"),
    },
    not_usable: {
      cls: "bg-status-danger-soft text-state-danger",
      icon: <CircleX className="size-3" />,
      label: t("settings.plugins.verdictNotUsable", "无法启动"),
    },
  } as const;
  const v = map[state];
  return (
    <>
      <span
        className={cn(
          "inline-flex items-center gap-1 rounded-full px-2 py-0.5 font-caption text-[10px]",
          v.cls,
        )}
        title={verdict?.reason ?? undefined}
      >
        {v.icon}
        {v.label}
      </span>
      <EvidencePill verdict={verdict} />
    </>
  );
}

/** Secondary pill on a "usable" engine showing HOW it was verified — a real
 *  one-turn check (已验证回合) vs live use (使用中) vs launch-only (仅启动) — so a
 *  green "可用" isn't ambiguous about its evidence. */
function EvidencePill({ verdict }: { verdict?: EngineReadiness }) {
  const { t } = useTranslation();
  const ev = evidenceOf({
    state: verdict?.state ?? "unknown",
    method: verdict?.method,
  });
  if (ev === "none") return null;
  const meta = {
    verified: {
      icon: <ShieldCheck className="size-3" />,
      cls: "border-status-success/40 text-status-success",
    },
    live: {
      icon: <Activity className="size-3" />,
      cls: "border-accent-primary/40 text-accent-primary-deep",
    },
    launch: {
      icon: <CircleDashed className="size-3" />,
      cls: "border-border-subtle text-foreground-tertiary",
    },
  }[ev];
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 rounded-full border px-1.5 py-0.5 font-caption text-[10px]",
        meta.cls,
      )}
      title={t(EVIDENCE_I18N[ev].detail)}
    >
      {meta.icon}
      {t(EVIDENCE_I18N[ev].label)}
    </span>
  );
}

function ComateLicenseCard() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<{
    configured: boolean;
    source: string;
    hint: string;
  } | null>(null);
  const [value, setValue] = useState("");
  const [saving, setSaving] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [models, setModels] = useState<{ displayName: string }[] | null>(null);

  const refresh = () => {
    api
      .getComate()
      .then((s) => {
        setStatus(s);
        if (s.configured) {
          api
            .getZuluModels()
            .then(setModels)
            .catch(() => setModels(null));
        } else {
          setModels(null);
        }
      })
      .catch((e) => setErr((e as Error).message));
  };
  useEffect(refresh, []);

  const save = async () => {
    setSaving(true);
    setErr(null);
    try {
      await api.putComate(value.trim());
      setValue("");
      refresh();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setSaving(false);
    }
  };

  const envLocked = status?.source === "env";
  return (
    <div className="flex flex-col gap-2 rounded-md border border-border-subtle bg-surface-secondary px-3 py-3">
      <div className="flex items-center justify-between gap-2">
        <span className="font-caption text-xs font-medium text-foreground-secondary">
          {t("settings.plugins.comateLicense", "Comate License（zulu 引擎）")}
        </span>
        {status?.configured ? (
          <span className="font-mono text-[11px] text-foreground-tertiary">
            {status.hint}
            {envLocked ? t("settings.plugins.comateEnv", " · 由环境变量控制") : ""}
          </span>
        ) : (
          <span className="font-caption text-[11px] text-state-warning">
            {t("settings.plugins.comateUnset", "未配置 · zulu 无法运行")}
          </span>
        )}
      </div>
      <p className="font-caption text-[11px] text-foreground-tertiary">
        {t(
          "settings.plugins.comateHint",
          "zulu 用你的 Comate SaaS license 访问模型（Claude/Gemini/DeepSeek/GLM… 一把钥匙）。仅存本机 ~/.swarmx/comate.json。",
        )}
      </p>
      {!envLocked && (
        <div className="flex items-center gap-2">
          <input
            type="password"
            name="comate-license"
            autoComplete="off"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            placeholder={t("settings.plugins.comatePlaceholder", "粘贴 license…")}
            className="min-w-0 flex-1 rounded-md border border-border-subtle bg-surface-elevated px-2.5 py-1 font-mono text-xs text-foreground focus:border-accent-primary focus:outline-none"
          />
          <button
            type="button"
            onClick={save}
            disabled={saving || value.trim().length === 0}
            className="shrink-0 rounded-md border border-border-subtle bg-surface-elevated px-2.5 py-1 font-caption text-[11px] text-foreground-secondary transition-colors hover:bg-surface-tertiary disabled:opacity-60"
          >
            {saving
              ? t("settings.plugins.comateSaving", "保存中…")
              : t("settings.plugins.comateSave", "保存")}
          </button>
        </div>
      )}
      {err && <span className="font-caption text-[11px] text-state-danger">{err}</span>}
      {models && models.length > 0 && (
        <div className="flex flex-col gap-1 border-t border-border-subtle pt-2">
          <span className="font-caption text-[11px] text-foreground-tertiary">
            {t("settings.plugins.comateModels", "可用模型（一把 license，{{n}} 个）", {
              n: models.length,
            })}
          </span>
          <div className="flex flex-wrap gap-1">
            {models.map((m) => (
              <span
                key={m.displayName}
                className="rounded border border-border-subtle bg-surface-elevated px-1.5 py-0.5 font-mono text-[10px] text-foreground-secondary"
              >
                {m.displayName}
              </span>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

function PluginsPanel() {
  const { t } = useTranslation();
  const [items, setItems] = useState<CliPluginInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState<string | null>(null);
  // One-click install: which engine is installing, its streamed log, and the
  // terminal result (null = not run / cleared).
  const [installingId, setInstallingId] = useState<string | null>(null);
  const [installLog, setInstallLog] = useState<string[]>([]);
  const [installResult, setInstallResult] = useState<{ ok: boolean; error?: string } | null>(null);
  // Which engine the current log/result belongs to (persists after install ends).
  const [logFor, setLogFor] = useState<string | null>(null);
  // Real-usability verdicts (actually start each CLI) layered over the install
  // info. Probing is opt-in via the button — it cold-starts every engine.
  const readiness = useEngineReadiness();
  const verdictById = new Map(readiness.engines.map((e) => [e.id, e]));

  const copyCommand = (key: string, command: string) => {
    if (!navigator.clipboard) return;
    navigator.clipboard.writeText(command).then(() => {
      setCopied(key);
      window.setTimeout(() => setCopied((current) => (current === key ? null : current)), 1800);
    });
  };

  const loadPlugins = () =>
    api
      .listPlugins()
      .then((rows) => setItems(rows))
      .catch((e) => setError((e as Error).message));

  const runInstall = async (id: string) => {
    setInstallingId(id);
    setLogFor(id);
    setInstallLog([]);
    setInstallResult(null);
    try {
      await api.installPlugin(id, (ev) => {
        if (ev.type === "line") {
          setInstallLog((prev) => [...prev, ev.text]);
        } else {
          setInstallResult({ ok: ev.ok, error: ev.error });
        }
      });
    } catch (e) {
      setInstallResult({ ok: false, error: (e as Error).message });
    } finally {
      setInstallingId(null);
      // Refresh installed/version + re-run the usability probe so the card flips.
      await loadPlugins();
      readiness.probe();
    }
  };

  useEffect(() => {
    let cancelled = false;
    api
      .listPlugins()
      .then((rows) => {
        if (!cancelled) setItems(rows);
      })
      .catch((e) => {
        if (!cancelled) setError((e as Error).message);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div className="mx-auto flex max-w-2xl flex-col gap-6 p-8">
      <PanelTitle
        title={t("settings.plugins.title")}
        hint={t("settings.plugins.hint")}
      />
      <ComateLicenseCard />
      {/* Real-usability check: "installed" only means the binary resolves —
          this actually starts each engine to prove it can run (catches a
          logged-out CLI or a key-less reasonix). Slow (cold-starts every
          engine) so it's an explicit action, never automatic. */}
      <div className="flex flex-wrap items-center gap-2 rounded-md border border-border-subtle bg-surface-secondary px-3 py-2">
        <button
          type="button"
          onClick={readiness.probe}
          disabled={readiness.probing || readiness.loading}
          className="inline-flex items-center gap-1.5 rounded-md border border-border-subtle bg-surface-elevated px-2.5 py-1 font-caption text-[11px] text-foreground-secondary transition-colors hover:bg-surface-tertiary disabled:opacity-60"
        >
          {readiness.probing ? (
            <Loader2 className="size-3.5 animate-spin" />
          ) : (
            <RefreshCw className="size-3.5" />
          )}
          {readiness.probing
            ? t("settings.plugins.probing", "检测中…")
            : t("settings.plugins.probe", "检测可用性")}
        </button>
        <span className="font-caption text-[11px] text-foreground-tertiary">
          {readiness.probing
            ? t("settings.plugins.probeRunning", "正在逐个真实启动引擎，可能要十几秒…")
            : t(
                "settings.plugins.probeHint",
                "「已安装」只代表能找到命令；点这里真实启动每个引擎，确认能否运行。",
              )}
        </span>
      </div>
      {error && (
        <div className="rounded-md border border-state-danger/40 bg-status-danger-soft px-3 py-2 text-xs text-state-danger">
          {t("settings.plugins.loadError", { error })}
        </div>
      )}
      {items === null && !error && (
        <p className="font-caption text-sm text-foreground-tertiary">
          {t("common.loading")}
        </p>
      )}
      {items?.length === 0 && (
        <p className="font-caption text-sm text-foreground-tertiary">
          {t("settings.plugins.empty")}
        </p>
      )}
      {items && items.length > 0 && (
        <ul className="flex flex-col gap-3">
          {items.map((p) => {
            const status =
              p.installed === true
                ? "installed"
                : p.installed === false
                  ? "missing"
                  : "unknown";
            const installed = status === "installed";
            const missing = status === "missing";
            return (
              <li
                key={p.id}
                className={cn(
                  "flex flex-col gap-3 rounded-lg border p-3.5",
                  missing
                    ? "border-status-warning/45 bg-status-warning-soft/45"
                    : "border-border-subtle bg-surface-elevated",
                )}
              >
                <div className="flex items-start gap-3">
                  <span
                    className={cn(
                      "flex size-9 shrink-0 items-center justify-center rounded-md",
                      installed
                        ? "bg-accent-primary-soft text-accent-primary-deep"
                        : missing
                          ? "bg-status-warning-soft text-status-warning"
                          : "bg-surface-tertiary text-foreground-tertiary",
                    )}
                  >
                    <Plug className="size-4" />
                  </span>
                  <div className="flex min-w-0 flex-1 flex-col gap-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-heading text-sm font-semibold text-foreground-primary">
                        {p.display_name}
                      </span>
                      <span
                        className={cn(
                          "inline-flex items-center gap-1 rounded-full px-2 py-0.5 font-caption text-[10px]",
                          installed
                            ? "bg-status-success-soft text-status-success"
                            : missing
                              ? "bg-status-warning-soft text-status-warning"
                              : "bg-surface-tertiary text-foreground-tertiary",
                        )}
                      >
                        {installed ? (
                          <CheckCircle2 className="size-3" />
                        ) : missing ? (
                          <TriangleAlert className="size-3" />
                        ) : (
                          <Info className="size-3" />
                        )}
                        {installed
                          ? t("settings.plugins.installedTag")
                          : missing
                            ? t("settings.plugins.missingTag")
                            : t("settings.plugins.unknownTag")}
                      </span>
                      {installed && (
                        <ProbeBadge verdict={verdictById.get(p.id)} />
                      )}
                    </div>
                    <span className="break-all font-caption text-[11px] text-foreground-tertiary">
                      <span className="font-mono">{p.id}</span> ·{" "}
                      {t("settings.plugins.binaryLabel")}{" "}
                      <span className="font-mono">{p.binary}</span>
                    </span>
                    {p.resolved_path && (
                      <span className="break-all font-caption text-[11px] text-foreground-tertiary">
                        {t("settings.plugins.pathLabel")}{" "}
                        <span className="font-mono">{p.resolved_path}</span>
                      </span>
                    )}
                    {p.version && (
                      <span className="break-all font-caption text-[11px] text-foreground-tertiary">
                        {t("settings.plugins.versionLabel")}{" "}
                        <span className="font-mono">{p.version}</span>
                      </span>
                    )}
                  </div>
                </div>
                {missing && p.install && (
                  <div className="flex flex-col gap-2 rounded-md border border-status-warning/35 bg-surface-base p-3">
                    <div className="flex flex-col gap-1">
                      <span className="font-heading text-xs font-semibold text-foreground-primary">
                        {t(`settings.plugins.installHeading.${p.id}`, {
                          defaultValue: p.install.title,
                        })}
                      </span>
                      <span className="font-caption text-[11px] text-foreground-secondary">
                        {t(`settings.plugins.installSummary.${p.id}`, {
                          defaultValue: p.install.summary,
                        })}
                      </span>
                    </div>
                    {/* one-click install (runs the whitelisted command server-side) */}
                    <Button
                      type="button"
                      size="sm"
                      onClick={() => runInstall(p.id)}
                      disabled={installingId != null}
                      className="self-start"
                    >
                      {installingId === p.id ? (
                        <Loader2 className="size-3.5 animate-spin" />
                      ) : (
                        <DownloadCloud className="size-3.5" />
                      )}
                      {installingId === p.id
                        ? t("settings.plugins.installing", "安装中…")
                        : t("settings.plugins.oneClickInstall", "一键安装")}
                    </Button>
                    {logFor === p.id && (installingId === p.id || installResult) && (
                      <div className="flex flex-col gap-1">
                        {installLog.length > 0 && (
                          <pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded-md border border-border-subtle bg-surface-base p-2 font-mono text-[10px] leading-4 text-foreground-secondary">
                            {installLog.join("\n")}
                          </pre>
                        )}
                        {installResult && (
                          <span
                            className={cn(
                              "font-caption text-[11px]",
                              installResult.ok ? "text-status-success" : "text-state-danger",
                            )}
                          >
                            {installResult.ok
                              ? t("settings.plugins.installOk", "安装完成 ✓")
                              : t("settings.plugins.installFail", {
                                  defaultValue: "安装失败：{{err}}",
                                  err: installResult.error ?? "",
                                })}
                          </span>
                        )}
                      </div>
                    )}
                    <div className="font-caption text-[11px] text-foreground-tertiary">
                      {t("settings.plugins.orCopy", "或手动复制命令执行：")}
                    </div>
                    <div className="flex flex-col gap-1.5">
                      {p.install.commands.map((command, index) => {
                        const key = `${p.id}:${index}`;
                        const isCopied = copied === key;
                        return (
                          <div
                            key={key}
                            className="flex min-w-0 items-center gap-2 rounded-md border border-border-subtle bg-surface-elevated px-2.5 py-2"
                          >
                            <code className="min-w-0 flex-1 break-all font-mono text-[11px] leading-5 text-foreground-primary">
                              {command}
                            </code>
                            <Button
                              type="button"
                              variant="ghost"
                              size="icon"
                              className="size-8 shrink-0"
                              onClick={() => copyCommand(key, command)}
                              aria-label={
                                isCopied
                                  ? t("settings.plugins.copied")
                                  : t("settings.plugins.copyCommand")
                              }
                              title={
                                isCopied
                                  ? t("settings.plugins.copied")
                                  : t("settings.plugins.copyCommand")
                              }
                            >
                              {isCopied ? (
                                <Check className="size-3.5" />
                              ) : (
                                <Copy className="size-3.5" />
                              )}
                            </Button>
                          </div>
                        );
                      })}
                    </div>
                    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 font-caption text-[11px] text-foreground-tertiary">
                      {p.install.verify_command && (
                        <span>
                          {t("settings.plugins.verifyLabel")}{" "}
                          <span className="font-mono">{p.install.verify_command}</span>
                        </span>
                      )}
                      {p.install.login_command && (
                        <span>
                          {t("settings.plugins.loginLabel")}{" "}
                          <span className="font-mono">{p.install.login_command}</span>
                        </span>
                      )}
                      <a
                        href={p.install.docs_url}
                        target="_blank"
                        rel="noreferrer"
                        className="inline-flex items-center gap-1 text-accent-primary-deep hover:underline"
                      >
                        {t("settings.plugins.docsLink")}
                        <ExternalLink className="size-3" />
                      </a>
                    </div>
                  </div>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

// ── Privacy panel ───────────────────────────────────────────────────────

function swarmxLocalStorageKeys(): string[] {
  const keys: string[] = [];
  for (let i = 0; i < window.localStorage.length; i++) {
    const k = window.localStorage.key(i);
    if (k && k.startsWith("swarmx:")) keys.push(k);
  }
  return keys;
}

function PrivacyPanel() {
  const { t } = useTranslation();
  const [toast, setToast] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<ConfirmActionState | null>(null);

  const exportJson = () => {
    const data: Record<string, unknown> = {};
    for (const k of swarmxLocalStorageKeys()) {
      const raw = window.localStorage.getItem(k);
      if (raw == null) continue;
      try {
        data[k] = JSON.parse(raw);
      } catch {
        data[k] = raw;
      }
    }
    const blob = new Blob([JSON.stringify(data, null, 2)], {
      type: "application/json",
    });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `swarmx-local-${new Date().toISOString().slice(0, 10)}.json`;
    document.body.appendChild(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
  };

  const clearAllNow = () => {
    const keys = swarmxLocalStorageKeys();
    for (const k of keys) window.localStorage.removeItem(k);
    setToast(t("settings.privacy.cleared", { count: keys.length }));
    // P1-27: clearing localStorage alone is a half-truth — the live in-memory
    // settings (theme/lang/toggles in SettingsRoute state) still hold the old
    // values, and the next change writes them straight back. Reload so every
    // bit of in-memory state re-initializes from the now-empty storage.
    window.setTimeout(() => window.location.reload(), 600);
  };

  const clearAll = () => {
    setConfirm({
      title: t("settings.privacy.clearTitle"),
      description: t("settings.privacy.clearConfirm"),
      confirmLabel: t("settings.privacy.clearButton"),
      variant: "destructive",
      onConfirm: clearAllNow,
    });
  };

  return (
    <div className="mx-auto flex max-w-2xl flex-col gap-8 p-8">
      <PanelTitle
        title={t("settings.privacy.title")}
        hint={t("settings.privacy.hint")}
      />

      <Field
        label={t("settings.privacy.approvalMode")}
        hint={t("settings.privacy.approvalModeHint")}
      >
        <div className="flex items-center gap-3 rounded-lg border border-status-warning/40 bg-status-warning-soft px-4 py-3">
          <Shield className="size-4 shrink-0 text-status-warning" />
          <span className="font-mono text-xs text-foreground-primary">
            {t("settings.privacy.approvalCurrent")}
          </span>
        </div>
      </Field>

      <Field
        label={t("settings.privacy.exportTitle")}
        hint={t("settings.privacy.exportHint")}
      >
        <Button variant="outline" onClick={exportJson} className="self-start">
          {t("settings.privacy.exportButton")}
        </Button>
      </Field>

      <Field
        label={t("settings.privacy.clearTitle")}
        hint={t("settings.privacy.clearHint")}
      >
        <Button variant="destructive" onClick={clearAll} className="self-start">
          {t("settings.privacy.clearButton")}
        </Button>
      </Field>

      {toast && (
        <div className="rounded-md border border-status-success-soft bg-status-success-soft px-3 py-2 text-xs text-status-success">
          {toast}
        </div>
      )}
      <ConfirmActionDialog
        action={confirm}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
      />
    </div>
  );
}

// ── About panel ─────────────────────────────────────────────────────────

function AboutPanel() {
  const { t } = useTranslation();
  const apiEndpoint = HTTP_BASE
    ? HTTP_BASE.replace(/^https?:\/\//, "")
    : window.location.host;
  return (
    <div className="mx-auto flex max-w-2xl flex-col gap-8 p-8">
      <PanelTitle
        title={t("settings.about.title")}
        hint={t("settings.about.hint")}
      />

      <section className="flex items-center gap-4 rounded-lg border border-border-subtle bg-surface-elevated p-5">
        <span className="flex size-14 items-center justify-center rounded-lg bg-accent-primary text-foreground-on-accent">
          <Info className="size-6" />
        </span>
        <div className="flex flex-col gap-0.5">
          <span className="font-heading text-lg font-bold text-foreground-primary">
            {t("settings.about.appName")}
          </span>
          <span className="font-caption text-xs text-foreground-tertiary">
            {t("settings.about.version", { v: __APP_VERSION__ })}
          </span>
          <a
            href={`https://${t("settings.about.repoUrl")}`}
            target="_blank"
            rel="noreferrer"
            className="mt-1 font-mono text-xs text-accent-primary hover:underline"
          >
            {t("settings.about.repo")}: {t("settings.about.repoUrl")}
          </a>
        </div>
      </section>

      <UpdateSection />

      <Field label={t("settings.about.endpointTitle")} hint={t("settings.about.endpointHint")}>
        <code className="inline-block rounded border border-border-subtle bg-surface-tertiary px-2 py-1 font-mono text-xs text-foreground-primary">
          {apiEndpoint}
        </code>
      </Field>
    </div>
  );
}

/** Settings → About: manual "check for updates" + install, fully user-driven.
 *  The silent startup check (useAvailableUpdate) pre-fills any found update so
 *  this panel and the sidebar badge agree. No auto-install, no startup popup. */
function UpdateSection() {
  const { t } = useTranslation();
  const stashed = useAvailableUpdate();
  const supported = inTauri();
  const [update, setUpdate] = useState<Update | null>(stashed);
  const [phase, setPhase] = useState<
    "idle" | "checking" | "uptodate" | "downloading" | "error"
  >("idle");
  const [pct, setPct] = useState(0);

  // Adopt whatever the background check found (or later cleared).
  useEffect(() => {
    setUpdate(stashed);
  }, [stashed]);

  const onCheck = async () => {
    setPhase("checking");
    const u = await checkForUpdate();
    if (u) {
      setUpdate(u);
      setPhase("idle");
    } else {
      setUpdate(null);
      setPhase("uptodate");
    }
  };

  const onInstall = async () => {
    if (!update) return;
    setPhase("downloading");
    setPct(0);
    try {
      await installUpdate(update, setPct); // relaunches the app on success
    } catch (e) {
      console.warn("[updater] install failed:", e);
      setPhase("error");
    }
  };

  return (
    <Field
      label={t("settings.about.update.title")}
      hint={t("settings.about.update.hint")}
    >
      <div className="flex flex-col gap-3 rounded-lg border border-border-subtle bg-surface-elevated p-4">
        <div className="flex items-center justify-between gap-3">
          <span className="font-caption text-xs text-foreground-tertiary">
            {t("settings.about.update.current", { v: __APP_VERSION__ })}
          </span>
          {!supported ? (
            <span className="font-caption text-xs text-foreground-tertiary">
              {t("settings.about.update.desktopOnly")}
            </span>
          ) : update ? (
            <Button
              size="sm"
              variant="default"
              onClick={onInstall}
              disabled={phase === "downloading"}
            >
              <DownloadCloud />
              {phase === "downloading"
                ? t("settings.about.update.downloading", { pct })
                : t("settings.about.update.download")}
            </Button>
          ) : (
            <Button
              size="sm"
              variant="secondary"
              onClick={onCheck}
              disabled={phase === "checking"}
            >
              <RefreshCw
                className={phase === "checking" ? "animate-spin" : undefined}
              />
              {phase === "checking"
                ? t("settings.about.update.checking")
                : t("settings.about.update.check")}
            </Button>
          )}
        </div>

        {update && (
          <p className="font-caption text-xs text-accent-primary">
            {t("settings.about.update.available", { v: update.version })}
          </p>
        )}
        {phase === "downloading" && (
          <div className="h-1.5 w-full overflow-hidden rounded-full bg-surface-tertiary">
            <div
              className="h-full rounded-full bg-accent-primary transition-[width]"
              style={{ width: `${pct}%` }}
            />
          </div>
        )}
        {phase === "uptodate" && (
          <p className="flex items-center gap-1.5 font-caption text-xs text-foreground-secondary">
            <CheckCircle2 className="size-3.5 text-accent-primary" />
            {t("settings.about.update.uptodate")}
          </p>
        )}
        {phase === "error" && (
          <p className="flex items-center gap-1.5 font-caption text-xs text-destructive">
            <TriangleAlert className="size-3.5" />
            {t("settings.about.update.failed")}
          </p>
        )}
      </div>
    </Field>
  );
}

// ── Atoms ────────────────────────────────────────────────────────────────

function PanelTitle({ title, hint }: { title: string; hint: string }) {
  return (
    <div className="flex flex-col gap-1">
      <h2 className="font-heading text-xl font-bold text-foreground-primary">
        {title}
      </h2>
      <p className="font-caption text-sm text-foreground-tertiary">{hint}</p>
    </div>
  );
}

function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-2">
      <div className="flex items-baseline gap-2">
        <span className="font-heading text-sm font-semibold text-foreground-primary">
          {label}
        </span>
        {hint && (
          <span className="font-caption text-xs text-foreground-tertiary">
            {hint}
          </span>
        )}
      </div>
      {children}
    </div>
  );
}

function ChoiceCard({
  active,
  onClick,
  title,
  sub,
  icon,
}: {
  active: boolean;
  onClick: () => void;
  title: string;
  sub: string;
  icon: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        "flex items-center gap-3 rounded-lg border-[1.5px] bg-surface-elevated px-4 py-3 text-left transition-colors",
        active
          ? "border-accent-primary bg-surface-accent-tint"
          : "border-border-subtle hover:border-border-strong",
      )}
    >
      <span
        className={cn(
          "flex size-8 items-center justify-center rounded-md",
          active
            ? "bg-accent-primary text-foreground-on-accent"
            : "bg-surface-tertiary text-foreground-secondary",
        )}
      >
        {icon}
      </span>
      <div className="flex flex-col">
        <span className="font-heading text-sm font-semibold text-foreground-primary">
          {title}
        </span>
        <span className="font-caption text-[11px] text-foreground-tertiary">
          {sub}
        </span>
      </div>
    </button>
  );
}

function ThemeCard({
  active,
  onClick,
  label,
  preview,
  icon,
}: {
  active: boolean;
  onClick: () => void;
  label: string;
  preview: "light" | "dark" | "system";
  icon: React.ReactNode;
}) {
  const { t } = useTranslation();
  return (
    <button
      onClick={onClick}
      className={cn(
        "flex flex-col gap-2 overflow-hidden rounded-lg border-[1.5px] transition-colors",
        active ? "border-accent-primary" : "border-border-subtle hover:border-border-strong",
      )}
    >
      <div
        className={cn(
          "flex h-24 items-end justify-end p-2",
          preview === "light" && "bg-gradient-to-b from-surface-elevated to-surface-secondary",
          preview === "dark" && "bg-gradient-to-b from-[#1F1F1F] to-[#0A0A0A]",
          preview === "system" &&
            "bg-[linear-gradient(135deg,var(--color-surface-secondary)_0%,var(--color-surface-secondary)_50%,#1A1A1A_50%,#0A0A0A_100%)]",
        )}
      >
        <span
          className={cn(
            "rounded px-1.5 py-0.5 font-caption text-[9px]",
            preview === "light"
              ? "bg-surface-elevated text-foreground-secondary"
              : preview === "dark"
                ? "bg-[#1F1F1F] text-foreground-inverse-secondary"
                : "bg-black/50 text-white",
          )}
        >
          {t("settings.appearance.preview")}
        </span>
      </div>
      <div className="flex items-center gap-2 px-3 py-2">
        <span className="text-foreground-tertiary">{icon}</span>
        <span className="font-heading text-sm font-medium text-foreground-primary">
          {label}
        </span>
        {active && (
          <span className="ml-auto rounded-full bg-accent-primary px-2 py-0.5 font-caption text-[9px] text-foreground-on-accent">
            {t("settings.appearance.current")}
          </span>
        )}
      </div>
    </button>
  );
}

function ToggleRow({
  label,
  hint,
  value,
  onChange,
}: {
  label: string;
  hint?: string;
  value: boolean;
  onChange: (v: boolean) => void;
}) {
  // 用 React.useId 给 Switch + Label 配对，可访问性 + 点 Label 也能切 Switch。
  const id = React.useId();
  const labelId = `${id}-label`;
  const hintId = hint ? `${id}-hint` : undefined;
  return (
    <div className="flex items-center gap-3 rounded-lg border border-border-subtle bg-surface-elevated px-4 py-3">
      <div className="flex min-w-0 flex-1 flex-col">
        <Label
          id={labelId}
          htmlFor={id}
          className="cursor-pointer font-heading text-sm font-medium text-foreground-primary"
        >
          {label}
        </Label>
        {hint && (
          <span id={hintId} className="font-caption text-[11px] text-foreground-tertiary">
            {hint}
          </span>
        )}
      </div>
      <Switch
        id={id}
        checked={value}
        onCheckedChange={onChange}
        aria-labelledby={labelId}
        aria-describedby={hintId}
      />
    </div>
  );
}

// silence unused-icon imports we'll wire up later
void CircleUser;
void Bell;
void Layers;
