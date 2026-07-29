import { X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Modal } from "@/components/ui/modal";
import { AboutPane } from "@/components/modals/settings/about-pane";
import { AdvancedPane } from "@/components/modals/settings/advanced-pane";
import { GeneralPane } from "@/components/modals/settings/general-pane";
import { TransfersPane } from "@/components/modals/settings/transfers-pane";
import { SectionTitle } from "@/components/modals/settings/shared";
import { useApp } from "@/store/app-store";

export function SettingsModal() {
  const { t } = useTranslation();
  const { showSettings, closeSettings } = useApp();

  if (!showSettings) return null;

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
        <GeneralPane />

        <SectionTitle>{t("settings.transfers")}</SectionTitle>
        <TransfersPane />

        <SectionTitle>{t("settings.advanced")}</SectionTitle>
        <AdvancedPane />

        <SectionTitle>{t("settings.about")}</SectionTitle>
        <AboutPane />
      </div>
    </Modal>
  );
}
