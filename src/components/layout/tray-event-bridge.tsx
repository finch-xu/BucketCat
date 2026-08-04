import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  OPEN_SETTINGS_EVENT,
  OPEN_TRANSFERS_EVENT,
  type OpenSettingsPayload,
} from "@/lib/api";
import { useApp } from "@/store/app-store";
import { useUpdater } from "@/store/updater-store";
import { useTransferStore } from "@/store/transfer-store";

/**
 * Renders nothing -- subscribes the app store (and transfer store) to the
 * tray's two "bring me to the front" events, emitted after the Rust side has
 * already shown the main window (see `src-tauri/src/tray.rs`).
 *
 * Known small gap: for the first few hundred ms after launch, before this
 * component's effect has registered its listeners, a tray click racing that
 * window is dropped rather than queued. Same class of gap as the tray's
 * English-label fallback before the webview reports its locale -- narrow and
 * accepted rather than engineered around.
 */
export function TrayEventBridge() {
  const { openSettings } = useApp();
  const { check } = useUpdater();

  useEffect(() => {
    let cancelled = false;
    let unlistenSettings: (() => void) | null = null;
    let unlistenTransfers: (() => void) | null = null;

    async function setup() {
      const [offSettings, offTransfers] = await Promise.all([
        listen<OpenSettingsPayload>(OPEN_SETTINGS_EVENT, (event) => {
          const { pane, auto_check } = event.payload;
          openSettings(pane ?? "general");
          // Not silent: this only fires from an explicit user gesture (the
          // tray's "Check for Updates…" item), so a failure must show up in
          // the pane rather than disappear the way a startup check does.
          if (auto_check) void check(false);
        }),
        listen(OPEN_TRANSFERS_EVENT, () => {
          // Zustand, not the app store -- no Provider needed to reach it.
          useTransferStore.getState().setPanelOpen(true);
        }),
      ]);

      // The component may have unmounted while the above awaits were in
      // flight -- if so, undo the registration immediately instead of
      // leaking listeners past cleanup.
      if (cancelled) {
        offSettings();
        offTransfers();
        return;
      }
      unlistenSettings = offSettings;
      unlistenTransfers = offTransfers;
    }

    void setup();

    return () => {
      cancelled = true;
      unlistenSettings?.();
      unlistenTransfers?.();
    };
  }, [openSettings, check]);

  return null;
}
