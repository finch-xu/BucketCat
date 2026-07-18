import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import { resolveLocale } from "./resolve-locale";
import en from "./locales/en.json";
import zhCN from "./locales/zh-CN.json";

i18n.use(initReactI18next).init({
  resources: {
    "zh-CN": { translation: zhCN },
    en: { translation: en },
  },
  lng: resolveLocale(navigator.language),
  fallbackLng: "en",
  interpolation: { escapeValue: false },
});

export default i18n;
