/**
 * Compact, i18n-aware "time ago" label. Shared by the notification popover and
 * the task board so relative times read identically across the app. Reuses the
 * existing `notifications.time.*` strings (now / {n}m / {n}h); older than a day
 * falls back to a locale date-time.
 */
import i18n from "@/i18n";
import { dateLocale } from "./dateLocale";

export function relTime(
  ms: number,
  t: (k: string, opts?: Record<string, unknown>) => string,
): string {
  const d = Date.now() - ms;
  if (d < 60_000) return t("notifications.time.now");
  if (d < 3_600_000) return t("notifications.time.minAgo", { n: Math.floor(d / 60_000) });
  if (d < 86_400_000) return t("notifications.time.hourAgo", { n: Math.floor(d / 3_600_000) });
  const locale = dateLocale(i18n.language);
  return new Date(ms).toLocaleString(locale, {
    hour12: locale?.startsWith("zh") ? false : undefined,
  });
}
