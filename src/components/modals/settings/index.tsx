import { ArrowUpDown, Info, RefreshCw, Settings2, Wrench, X, type LucideIcon } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Modal } from "@/components/ui/modal";
import { cn } from "@/lib/utils";
import { useApp, type SettingsPane } from "@/store/app-store";
import { useUpdater } from "@/store/updater-store";
import { AboutPane } from "./about-pane";
import { AdvancedPane } from "./advanced-pane";
import { GeneralPane } from "./general-pane";
import { SectionTitle } from "./shared";
import { TransfersPane } from "./transfers-pane";
import { UpdatePane } from "./update-pane";

const CATEGORIES: {
  id: SettingsPane;
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
  { id: "update", icon: RefreshCw, navKey: "settings.navUpdate", titleKey: "settings.update" },
  { id: "about", icon: Info, navKey: "settings.navAbout", titleKey: "settings.about" },
];

export function SettingsModal() {
  const { showSettings } = useApp();
  // `SettingsModal` is rendered unconditionally by `app-shell.tsx`, so this
  // guard must sit in the outermost component: it's the only way to make
  // `SettingsContent` -- and the panes' own local state (pending flags,
  // fetched values) -- actually unmount when the modal closes, rather than
  // merely rendering null while staying resident. The active pane itself no
  // longer depends on this; it lives in the app store (see below).
  if (!showSettings) return null;
  return <SettingsContent />;
}

function SettingsContent() {
  const { t } = useTranslation();
  const { closeSettings, settingsPane: active, setSettingsPane: setActive } = useApp();
  const { hasUpdate } = useUpdater();
  // Pane state lives in the app store, not a local `useState`, even though
  // this component remounts fresh every time the modal opens (see
  // `SettingsModal` above) and so never needs to *persist* it: the tray's
  // "Check for Updates…" item needs to retarget the pane from outside this
  // subtree, including while the modal is already open, and only the store
  // is reachable from there.

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
              {id === "update" && hasUpdate && (
                <span
                  aria-label={t("settings.updateAvailableDot")}
                  className="ml-auto size-1.5 shrink-0 rounded-full bg-primary"
                />
              )}
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
              {id === "update" && <UpdatePane />}
              {id === "about" && <AboutPane />}
            </div>
          ))}
        </div>
      </div>
    </Modal>
  );
}
