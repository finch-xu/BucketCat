import { AppShell } from "@/components/layout/app-shell";
import { useTransferEvents } from "@/hooks/use-transfer-events";
import { UpdaterProvider } from "@/store/updater-store";

function App() {
  // Mounted once at the app root: there is exactly one transfer store, so
  // exactly one subscription to the engine's event streams is needed
  // regardless of how many panes (transfer bar, upload flows) read from it.
  useTransferEvents();
  // Wraps the shell rather than living in the Settings modal: the modal
  // unmounts on close, and the "an update is waiting" dot has to outlive it
  // (the sidebar's settings button carries one too).
  return (
    <UpdaterProvider>
      <AppShell />
    </UpdaterProvider>
  );
}

export default App;
