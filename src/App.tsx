import { AppShell } from "@/components/layout/app-shell";
import { useTransferEvents } from "@/hooks/use-transfer-events";

function App() {
  // Mounted once at the app root: there is exactly one transfer store, so
  // exactly one subscription to the engine's event streams is needed
  // regardless of how many panes (transfer bar, upload flows) read from it.
  useTransferEvents();
  return <AppShell />;
}

export default App;
