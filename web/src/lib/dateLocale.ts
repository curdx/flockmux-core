/** Map i18n language codes onto locales that `Intl` actually understands. */
export function dateLocale(lang?: string | null): string | undefined {
  const raw = (lang ?? "").trim().toLowerCase();
  if (raw.startsWith("zh")) return "zh-CN";
  if (raw.startsWith("en")) return "en-US";
  return raw || undefined;
}
