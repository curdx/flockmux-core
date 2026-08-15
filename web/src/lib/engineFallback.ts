/**
 * Engine-fallback surfacing (billing red line).
 *
 * When the requested CLI isn't installed, the server silently substitutes an
 * installed fallback engine (`select_spawn_plugin` in rest.rs) — and the
 * fallback filter admits API-billed engines (reasonix has
 * `requires_explicit_billing_opt_in = false`). So "user clicked claude,
 * machine only has reasonix" used to run the worker on PAID API billing with
 * only a server-side warn log. Spawn responses now carry `fallback_from` +
 * `billing_surface`; every UI spawn path routes them through here so the
 * substitution is always announced to the user.
 */
import type { TFunction } from "i18next";
import { toast } from "@/lib/toast";

/** Minimal shape both SpawnAgentResponse and RunSpellAgent satisfy. */
export interface SpawnedEngine {
  cli: string;
  fallback_from?: string | null;
  billing_surface?: string | null;
}

/** Human label for a plugin billing-surface token (kebab-case; mirrors the
 *  BillingSurface serde rename in swarmx-server's plugins.rs). */
export function billingSurfaceLabel(
  t: TFunction,
  surface?: string | null,
): string {
  return t(`engineFallback.surface.${surface ?? "unknown"}`, {
    defaultValue: "计费方式未声明",
  });
}

/** One-line notice "claude 未安装，已改用 reasonix（按 API key 计费）". Shared
 *  by the toast and the dispatch system card so the wording can't drift. */
export function fallbackNotice(t: TFunction, a: SpawnedEngine): string {
  return t("engineFallback.notice", {
    from: a.fallback_from,
    to: a.cli,
    surface: billingSurfaceLabel(t, a.billing_surface),
    defaultValue: "{{from}} 未安装，已改用 {{to}}（{{surface}}）",
  });
}

/** Fire a prominent warning toast for every spawned agent whose engine fell
 *  back to a substitute. No-op when every agent got its requested engine. */
export function notifySpawnFallbacks(
  t: TFunction,
  agents: ReadonlyArray<SpawnedEngine>,
): void {
  for (const a of agents) {
    if (!a.fallback_from) continue;
    toast.warning(fallbackNotice(t, a), {
      // Billing-relevant: outlast sonner's ~4s default so it can't be missed.
      duration: 8000,
    });
  }
}
