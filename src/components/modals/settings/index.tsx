import { ArrowUpDown, Info, Settings2, Wrench, X, type LucideIcon } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Modal } from "@/components/ui/modal";
import { cn } from "@/lib/utils";
import { useApp } from "@/store/app-store";
import { AboutPane } from "./about-pane";
import { AdvancedPane } from "./advanced-pane";
import { GeneralPane } from "./general-pane";
import { SectionTitle } from "./shared";
import { TransfersPane } from "./transfers-pane";

type CategoryId = "general" | "transfers" | "advanced" | "about";

const CATEGORIES: {
  id: CategoryId;
  icon: LucideIcon;
  navKey: string;
  titleKey: string;
}[] = [
  { id: "general", icon: Settings2, navKey: "settings.navGeneral", titleKey: "settings.general" },
  {
    id: "transfers",
    icon: ArrowUpDown,
    navKey: "settings.navTransfers",
    titleKey: "settings.transfers",
  },
  { id: "advanced", icon: Wrench, navKey: "settings.navAdvanced", titleKey: "settings.advanced" },
  { id: "about", icon: Info, navKey: "settings.navAbout", titleKey: "settings.about" },
];

export function SettingsModal() {
  const { showSettings } = useApp();
  // `SettingsModal` is rendered unconditionally by `app-shell.tsx`, so this
  // guard must sit in the outermost component: it's the only way to make
  // `SettingsContent` -- and the `active` state it owns -- actually unmount
  // when the modal closes, rather than merely rendering null while staying
  // resident. See the comment on `active` below for why that matters.
  if (!showSettings) return null;
  return <SettingsContent />;
}

function SettingsContent() {
  const { t } = useTranslation();
  const { closeSettings } = useApp();
  // Pane-local view state, deliberately not in the app store: reopening the
  // modal back on "General" is the intuitive default. It stays that way
  // because this component unmounts on close (see `SettingsModal` above),
  // so there is nothing to persist.
  const [active, setActive] = useState<CategoryId>("general");

  return (
    <Modal
      onClose={closeSettings}
      className="flex h-[520px] w-[760px] max-h-[88%] flex-col overflow-hidden"
    >
      <div className="flex shrink-0 items-center justify-between border-b border-border2 px-[22px] pt-5 pb-4">
        <div className="text-[17px] font-bold">{t("settings.title")}</div>
        <button
          type="button"
          onClick={closeSettings}
          className="flex size-[30px] cursor-pointer items-center justify-center rounded-lg text-muted-foreground hover:bg-hover hover:text-fg2"
        >
          <X className="size-[17px]" />
        </button>
      </div>

      <div className="flex min-h-0 flex-1">
        <nav className="flex w-[168px] shrink-0 flex-col gap-0.5 border-r border-border2 bg-sidebar p-2.5">
          {CATEGORIES.map(({ id, icon: Icon, navKey }) => (
            <button
              key={id}
              type="button"
              onClick={() => setActive(id)}
              aria-current={active === id ? "page" : undefined}
              className={cn(
                "flex cursor-pointer items-center gap-2.5 rounded-[9px] px-2.5 py-[7px] text-[13px]",
                active === id
                  ? "bg-active font-medium text-primary"
                  : "text-fg2 hover:bg-hover",
              )}
            >
              <Icon className="size-[15px] shrink-0" />
              {t(navKey)}
            </button>
          ))}
        </nav>

        {/* Every pane stays mounted for as long as the modal is open, hidden
         * rather than unmounted, so switching categories neither refetches
         * nor drops an optimistic value whose persist is still in flight. */}
        <div className="min-w-0 flex-1 overflow-y-auto px-[22px] py-5">
          {CATEGORIES.map(({ id, titleKey }) => (
            <div key={id} hidden={active !== id}>
              {/* `first` because this is the pane's own heading, not a
               * mid-pane section break -- it must not carry the mt-5. */}
              <SectionTitle first>{t(titleKey)}</SectionTitle>
              {id === "general" && <GeneralPane />}
              {id === "transfers" && <TransfersPane />}
              {id === "advanced" && <AdvancedPane />}
              {id === "about" && <AboutPane />}
            </div>
          ))}
        </div>
      </div>
    </Modal>
  );
}
