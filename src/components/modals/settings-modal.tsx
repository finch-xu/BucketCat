import { X } from "lucide-react";
import { useTranslation } from "react-i18next";
import logo from "@/assets/logo.png";
import { Modal } from "@/components/ui/modal";
import { Segmented } from "@/components/ui/segmented";
import { Switch } from "@/components/ui/switch";
import { setLocale } from "@/i18n";
import type { AppLocale } from "@/i18n/resolve-locale";
import { useApp, type ViewMode } from "@/store/app-store";
import type { ThemeMode } from "@/lib/theme";

function SectionTitle({ children, first }: { children: React.ReactNode; first?: boolean }) {
  return (
    <div
      className={`mb-3 text-[11px] font-semibold tracking-[0.6px] text-muted-foreground uppercase ${first ? "" : "mt-5"}`}
    >
      {children}
    </div>
  );
}

function Row({ label, children }: { label: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between py-2.5">
      <span className="text-[13.5px] text-fg2">{label}</span>
      {children}
    </div>
  );
}

export function SettingsModal() {
  const { t, i18n } = useTranslation();
  const {
    showSettings,
    closeSettings,
    themeMode,
    setThemeMode,
    defaultView,
    setDefaultView,
    transferSettings,
    setTransferSettings,
  } = useApp();

  if (!showSettings) return null;

  const locale: AppLocale = i18n.language === "zh-CN" ? "zh-CN" : "en";

  return (
    <Modal onClose={closeSettings} className="w-[600px]">
      <div className="sticky top-0 z-1 flex items-center justify-between border-b border-border2 bg-background px-[22px] pt-5 pb-4">
        <div className="text-[17px] font-bold">{t("settings.title")}</div>
        <button
          type="button"
          onClick={closeSettings}
          className="flex size-[30px] cursor-pointer items-center justify-center rounded-lg text-muted-foreground hover:bg-hover hover:text-fg2"
        >
          <X className="size-[17px]" />
        </button>
      </div>
      <div className="px-[22px] py-5">
        <SectionTitle first>{t("settings.general")}</SectionTitle>
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

        <SectionTitle>{t("settings.transfers")}</SectionTitle>
        <Row label={t("settings.concurrency")}>
          <div className="flex items-center gap-0.5 overflow-hidden rounded-[9px] border border-border bg-panel">
            <button
              type="button"
              onClick={() =>
                setTransferSettings({ concurrency: Math.max(1, transferSettings.concurrency - 1) })
              }
              className="size-[30px] cursor-pointer text-base text-fg2 hover:bg-hover"
            >
              −
            </button>
            <span className="w-[34px] text-center text-[13px] font-semibold tabular-nums">
              {transferSettings.concurrency}
            </span>
            <button
              type="button"
              onClick={() =>
                setTransferSettings({ concurrency: Math.min(16, transferSettings.concurrency + 1) })
              }
              className="size-[30px] cursor-pointer text-base text-fg2 hover:bg-hover"
            >
              +
            </button>
          </div>
        </Row>
        <Row label={t("settings.partSize")}>
          <Segmented<number>
            value={transferSettings.partSizeMb}
            onChange={(v) => setTransferSettings({ partSizeMb: v })}
            options={[
              { value: 8, label: "8 MB" },
              { value: 16, label: "16 MB" },
              { value: 64, label: "64 MB" },
            ]}
          />
        </Row>
        <Row label={t("settings.verify")}>
          <Switch
            checked={transferSettings.verify}
            onChange={(v) => setTransferSettings({ verify: v })}
          />
        </Row>
        <Row label={t("settings.overwrite")}>
          <Switch
            checked={transferSettings.overwrite}
            onChange={(v) => setTransferSettings({ overwrite: v })}
          />
        </Row>

        <SectionTitle>{t("settings.about")}</SectionTitle>
        <div className="flex items-center gap-3.5 pt-1.5 pb-1">
          <img
            src={logo}
            alt="BucketCat"
            className="size-[52px] rounded-[13px] shadow-[0_0_0_1px_var(--border)]"
          />
          <div className="flex-1">
            <div className="text-[15px] font-bold">
              {t("app.name")}{" "}
              <span className="ml-1 rounded-[20px] border border-border bg-panel px-[7px] py-px text-[11.5px] font-medium text-muted-foreground">
                v0.1.0
              </span>
            </div>
            <div className="mt-[3px] text-[12.5px] text-muted-foreground">{t("app.tagline")}</div>
          </div>
        </div>
        <div className="mt-3 flex gap-2.5">
          <a
            href="https://github.com/finch-xu/BucketCat"
            target="_blank"
            rel="noreferrer"
            className="rounded-lg border border-border px-[13px] py-[7px] text-[12.5px] text-fg2 hover:bg-hover"
          >
            {t("settings.github")}
          </a>
          <a
            href="#"
            className="rounded-lg border border-border px-[13px] py-[7px] text-[12.5px] text-fg2 hover:bg-hover"
          >
            {t("settings.checkUpdate")}
          </a>
        </div>
      </div>
    </Modal>
  );
}
