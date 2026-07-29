import { ConnectionModals } from "@/components/modals/connection-modals";
import { ObjectDialogs } from "@/components/modals/object-dialogs";
import { SettingsModal } from "@/components/modals/settings-modal";
import { AppStoreProvider } from "@/store/app-store";
import { useTrayLabels } from "@/hooks/use-tray-labels";
import { DetailsPanel } from "./details-panel";
import { FileBrowser } from "./file-browser";
import { PathBar } from "./path-bar";
import { Sidebar } from "./sidebar";
import { Toolbar } from "./toolbar";
import { TransferBar } from "./transfer-bar";

export function AppShell() {
  useTrayLabels();
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
            {/* Spans the whole content section, below the details panel rather
                than inside the browser column -- that keeps its width at
                (window - sidebar) regardless of whether the details panel is
                open, which its ResizeObserver relies on. See `path-bar.tsx`. */}
            <PathBar />
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
