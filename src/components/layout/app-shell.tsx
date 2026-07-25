import { ConnectionModals } from "@/components/modals/connection-modals";
import { ObjectDialogs } from "@/components/modals/object-dialogs";
import { SettingsModal } from "@/components/modals/settings-modal";
import { AppStoreProvider } from "@/store/app-store";
import { DetailsPanel } from "./details-panel";
import { FileBrowser } from "./file-browser";
import { Sidebar } from "./sidebar";
import { Toolbar } from "./toolbar";
import { TransferBar } from "./transfer-bar";

export function AppShell() {
  return (
    <AppStoreProvider>
      <div className="relative flex h-screen flex-col overflow-hidden bg-background text-foreground">
        <div className="flex min-h-0 flex-1">
          <Sidebar />
          <section className="flex min-w-0 flex-1 flex-col bg-panel">
            <Toolbar />
            <div className="flex min-h-0 flex-1">
              <FileBrowser />
              <DetailsPanel />
            </div>
          </section>
        </div>
        <TransferBar />
        <ConnectionModals />
        <ObjectDialogs />
        <SettingsModal />
      </div>
    </AppStoreProvider>
  );
}
