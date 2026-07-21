import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import { resolveLocale, SUPPORTED_LOCALES, type AppLocale } from "./resolve-locale";
import en from "./locales/en.json";
import zhCN from "./locales/zh-CN.json";

const STORAGE_KEY = "bucketcat.locale";

function storedLocale(): AppLocale | null {
  const stored = localStorage.getItem(STORAGE_KEY);
  return (SUPPORTED_LOCALES as readonly string[]).includes(stored ?? "")
    ? (stored as AppLocale)
    : null;
}

i18n.use(initReactI18next).init({
  resources: {
    "zh-CN": { translation: zhCN },
    en: { translation: en },
  },
  lng: storedLocale() ?? resolveLocale(navigator.language),
  fallbackLng: "en",
  interpolation: { escapeValue: false },
});

export function setLocale(locale: AppLocale) {
  localStorage.setItem(STORAGE_KEY, locale);
  i18n.changeLanguage(locale);
}

export default i18n;
