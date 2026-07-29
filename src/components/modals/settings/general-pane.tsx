import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Segmented } from "@/components/ui/segmented";
import { Switch } from "@/components/ui/switch";
import { setLocale } from "@/i18n";
import type { AppLocale } from "@/i18n/resolve-locale";
import { useErrorText } from "@/hooks/use-error-text";
import { settingsKey, useSettings } from "@/hooks/use-settings";
import {
  getAutostart,
  setAutostart,
  setCloseToTray,
  setShareExpiry,
  type AppError,
} from "@/lib/api";
import { useApp, type ViewMode } from "@/store/app-store";
import type { ThemeMode } from "@/lib/theme";
import { Row, SectionTitle } from "./shared";

/** Share-link expiry choices, in seconds -- the same fixed set the details
 * panel's Share dropdown offers (`EXPIRY_OPTIONS` in
 * `src/components/layout/details-panel.tsx`), reusing its `details.expiry*`
 * copy rather than duplicating four near-identical i18n keys under
 * `settings.*`. */
const SHARE_EXPIRY_OPTIONS: { secs: number; labelKey: string }[] = [
  { secs: 3600, labelKey: "details.expiry1h" },
  { secs: 21600, labelKey: "details.expiry6h" },
  { secs: 86400, labelKey: "details.expiry24h" },
  { secs: 604800, labelKey: "details.expiry7d" },
];

export function GeneralPane() {
  const { t, i18n } = useTranslation();
  const { themeMode, setThemeMode, defaultView, setDefaultView } = useApp();
  const errorText = useErrorText();
  const queryClient = useQueryClient();
  const settingsQuery = useSettings();

  // Backend defaults (see `Settings::default()` in
  // `src-tauri/src/store/settings.rs`) stand in until the query resolves.
  const [shareExpirySecs, setShareExpirySecsState] = useState(3600);
  const [closeToTray, setCloseToTrayState] = useState(true);
  // Autostart is not part of `Settings` on purpose -- the registration lives
  // in the OS, which is its single source of truth (see `getAutostart`).
  // Hence its own state and its own fetch below.
  const [autostart, setAutostartState] = useState(false);
  const [autostartError, setAutostartError] = useState<AppError | null>(null);

  // Seed the local state from the query exactly once. After that the local
  // copy leads: it carries optimistic values whose persist may still be in
  // flight, and an unguarded sync would let a refetch stomp on one of them.
  const seededRef = useRef(false);
  useEffect(() => {
    if (seededRef.current || !settingsQuery.data) return;
    seededRef.current = true;
    setShareExpirySecsState(settingsQuery.data.share_expiry_secs);
    setCloseToTrayState(settingsQuery.data.close_to_tray);
  }, [settingsQuery.data]);

  // Separate round trip because autostart is read from the OS registration,
  // not from settings.json -- see `getAutostart`'s doc comment. Re-read on
  // every mount (i.e. every time the modal opens) precisely because the user
  // can revoke the registration outside the app, which would make a
  // longer-lived cache lie.
  useEffect(() => {
    let cancelled = false;
    getAutostart()
      .then((enabled) => {
        if (!cancelled) setAutostartState(enabled);
      })
      .catch((err) => {
        console.error("Failed to read the autostart registration", err);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  function handleShareExpiryChange(secs: number) {
    const previous = shareExpirySecs;
    setShareExpirySecsState(secs);
    setShareExpiry(secs)
      .then(() => queryClient.invalidateQueries({ queryKey: settingsKey }))
      .catch((err) => {
        setShareExpirySecsState(previous);
        console.error("Failed to persist share expiry", err);
      });
  }

  function handleCloseToTrayChange(v: boolean) {
    setCloseToTrayState(v);
    setCloseToTray(v)
      .then(() => queryClient.invalidateQueries({ queryKey: settingsKey }))
      .catch((err) => {
        setCloseToTrayState(!v);
        console.error("Failed to persist close-to-tray setting", err);
      });
  }

  // Unlike every other switch here, this one writes outside the app: it
  // registers or removes a login item with the OS, which can genuinely be
  // refused (permissions, a sandboxed or unsigned build). A silent revert
  // would look like the switch simply refused to move, so the reason is
  // surfaced instead of only logged.
  function handleAutostartChange(v: boolean) {
    setAutostartError(null);
    setAutostartState(v);
    setAutostart(v).catch((err: AppError) => {
      setAutostartState(!v);
      setAutostartError(err);
    });
  }

  const locale: AppLocale = i18n.language === "zh-CN" ? "zh-CN" : "en";

  return (
    <div>
      <Row label={t("settings.theme")}>
        <Segmented<ThemeMode>
          value={themeMode}
          onChange={setThemeMode}
          options={[
            { value: "light", label: t("settings.themeLight") },
            { value: "dark", label: t("settings.themeDark") },
            { value: "system", label: t("settings.themeSystem") },
          ]}
        />
      </Row>
      <Row label={t("settings.language")}>
        <Segmented<AppLocale>
          value={locale}
          onChange={setLocale}
          options={[
            { value: "zh-CN", label: "中文" },
            { value: "en", label: "English" },
          ]}
        />
      </Row>
      <Row label={t("settings.defaultView")}>
        <Segmented<ViewMode>
          value={defaultView}
          onChange={setDefaultView}
          options={[
            { value: "list", label: t("main.listView") },
            { value: "grid", label: t("main.gridView") },
          ]}
        />
      </Row>
      <Row label={t("settings.shareExpiry")}>
        <select
          value={shareExpirySecs}
          onChange={(e) => handleShareExpiryChange(Number(e.target.value))}
          className="h-[30px] rounded-[7px] border border-border bg-background px-2 text-[12.5px] text-fg2 outline-none focus:border-primary"
        >
          {SHARE_EXPIRY_OPTIONS.map((opt) => (
            <option key={opt.secs} value={opt.secs}>
              {t(opt.labelKey)}
            </option>
          ))}
        </select>
      </Row>

      <SectionTitle>{t("settings.startup")}</SectionTitle>
      <Row
        label={
          <div>
            <div>{t("settings.closeToTray")}</div>
            <div className="mt-0.5 text-[11.5px] text-muted-foreground">
              {t("settings.closeToTrayHint")}
            </div>
          </div>
        }
      >
        <Switch checked={closeToTray} onChange={handleCloseToTrayChange} />
      </Row>
      <Row
        label={
          <div>
            <div>{t("settings.autostart")}</div>
            <div className="mt-0.5 text-[11.5px] text-muted-foreground">
              {t("settings.autostartHint")}
            </div>
            {autostartError && (
              <div className="mt-0.5 text-[11.5px] text-destructive">
                {errorText(autostartError)}
              </div>
            )}
          </div>
        }
      >
        <Switch checked={autostart} onChange={handleAutostartChange} />
      </Row>
    </div>
  );
}
