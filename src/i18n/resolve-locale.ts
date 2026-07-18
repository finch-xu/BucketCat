export const SUPPORTED_LOCALES = ["zh-CN", "en"] as const;
export type AppLocale = (typeof SUPPORTED_LOCALES)[number];

export function resolveLocale(
  systemLocale: string | undefined | null,
): AppLocale {
  if (!systemLocale) return "en";
  return systemLocale.toLowerCase().startsWith("zh") ? "zh-CN" : "en";
}
