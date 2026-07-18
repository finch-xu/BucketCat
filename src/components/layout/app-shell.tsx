import { Sidebar } from "./sidebar";
import { MainPanel } from "./main-panel";
import { TransferBar } from "./transfer-bar";

export function AppShell() {
  return (
    <div className="flex h-screen flex-col bg-background text-foreground">
      <div className="flex min-h-0 flex-1">
        <Sidebar />
        <MainPanel />
      </div>
      <TransferBar />
    </div>
  );
}
