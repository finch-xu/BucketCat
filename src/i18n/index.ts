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

const initialLocale = storedLocale() ?? resolveLocale(navigator.language);

i18n.use(initReactI18next).init({
  resources: {
    "zh-CN": { translation: zhCN },
    en: { translation: en },
  },
  lng: initialLocale,
  fallbackLng: "en",
  interpolation: { escapeValue: false },
});

// Keep <html lang> in sync with the active locale (a11y + correct font and
// hyphenation selection). Updated again on manual switch in setLocale().
document.documentElement.lang = initialLocale;

export function setLocale(locale: AppLocale) {
  localStorage.setItem(STORAGE_KEY, locale);
  i18n.changeLanguage(locale);
  document.documentElement.lang = locale;
}

export default i18n;
