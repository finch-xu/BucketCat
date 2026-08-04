import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { setTrayLabels } from "@/lib/api";

/**
 * Keeps the native tray menu's labels in the app's language.
 *
 * The tray lives in Rust and is built during `setup` with English fallbacks,
 * because the chosen locale is only ever stored in this webview's
 * `localStorage` (`bucketcat.locale`, see `src/i18n/index.ts`) and the backend
 * has no way to read it. Building the tray lazily instead was not an option:
 * during a silent autostart the tray icon is the only sign the app came up at
 * all, so it has to exist before the webview does. This effect closes that gap
 * as soon as i18n has resolved, and again on every language switch.
 *
 * A failure here is logged and swallowed. A tray menu stuck on English is a
 * cosmetic problem; it is not worth surfacing an error dialog over, and there
 * is nothing the user could do about it anyway.
 */
export function useTrayLabels() {
  const { t, i18n } = useTranslation();
  useEffect(() => {
    setTrayLabels({
      show: t("tray.show"),
      quit: t("tray.quit"),
      settings: t("tray.settings"),
      check_update: t("tray.checkUpdate"),
      status_idle: t("tray.statusIdle"),
      status_active: t("tray.statusActive"),
    }).catch((err) => {
      console.error("Failed to localize the tray menu", err);
    });
  }, [t, i18n.language]);
}
