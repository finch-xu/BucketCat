import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  listTransfers,
  TRANSFER_PROGRESS_EVENT,
  TRANSFER_STATE_EVENT,
  type TransferProgress,
  type TransferTask,
} from "@/lib/api";
import { useTransferStore } from "@/store/transfer-store";

/**
 * Subscribes the transfer store to the engine's two event streams. Mounted
 * once at the app root (a later task does the mounting) -- there is exactly
 * one store, so exactly one subscription is needed regardless of how many
 * panes read from it.
 */
export function useTransferEvents(): void {
  useEffect(() => {
    let cancelled = false;
    let unlistenState: (() => void) | null = null;
    let unlistenProgress: (() => void) | null = null;

    async function setup() {
      // Register both listeners *before* taking the initial snapshot: a
      // task that changes state during that round trip must land as a live
      // event, not get missed in the gap between "start listening" and
      // "list what's already there".
      const [offState, offProgress] = await Promise.all([
        listen<TransferTask>(TRANSFER_STATE_EVENT, (event) => {
          useTransferStore.getState().applyState(event.payload);
        }),
        listen<TransferProgress[]>(TRANSFER_PROGRESS_EVENT, (event) => {
          useTransferStore.getState().applyProgress(event.payload);
        }),
      ]);

      // The component may have unmounted while the above awaits were in
      // flight -- if so, undo the registration immediately instead of
      // leaking listeners past cleanup.
      if (cancelled) {
        offState();
        offProgress();
        return;
      }
      unlistenState = offState;
      unlistenProgress = offProgress;

      try {
        const snapshot = await listTransfers();
        // Events that arrived during the round trip above already built up
        // real state; replaceAll would clobber them, so only apply the
        // snapshot while the store is still empty.
        if (!cancelled && useTransferStore.getState().order.length === 0) {
          useTransferStore.getState().replaceAll(snapshot);
        }
      } catch {
        // A failed snapshot is swallowed: live events still work, and on a
        // fresh launch the snapshot is empty anyway.
      }
    }

    void setup();

    return () => {
      cancelled = true;
      unlistenState?.();
      unlistenProgress?.();
    };
  }, []);
}
