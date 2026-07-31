import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  checkForUpdate,
  downloadInstallUpdate,
  restartApp,
  UPDATE_PROGRESS_EVENT,
  type AppError,
  type UpdateInfo,
  type UpdaterProgressEvent,
} from "@/lib/api";
import { useSettings } from "@/hooks/use-settings";

/** Where the update flow currently is.
 *
 * `up_to_date` and `error` are terminal *display* states rather than resting
 * ones -- they exist so the settings pane can say something after a manual
 * check, and `idle` means "nothing to report". */
export type UpdaterStatus =
  | "idle"
  | "checking"
  | "up_to_date"
  | "available"
  | "downloading"
  | "ready"
  | "error";

export interface UpdaterDownloadProgress {
  downloaded: number;
  /** Null until the response headers arrive, and for servers that send no
   * content length at all -- the UI must degrade to an indeterminate bar. */
  total: number | null;
}

export interface UpdaterState {
  status: UpdaterStatus;
  detected: UpdateInfo | null;
  progress: UpdaterDownloadProgress | null;
  error: AppError | null;
  /** Drives the dot on the sidebar's settings button and on the Update nav
   * entry. Stays lit after a successful install, because a restart is still
   * owed. */
  hasUpdate: boolean;
  /** `silent` suppresses the error state: a startup check that fails must not
   * leave a red banner waiting in a pane the user opens days later. */
  check: (silent?: boolean) => Promise<void>;
  install: () => Promise<void>;
  restart: () => Promise<void>;
}

const UpdaterContext = createContext<UpdaterState | null>(null);

/**
 * Owns everything about in-app updates.
 *
 * Deliberately a provider at the app root rather than state inside the update
 * pane: `SettingsModal` unmounts its whole subtree on close, and the dot has
 * to outlive that. Deliberately its own context rather than another slice of
 * `app-store.tsx`, which is already the grab bag for browse/dialog UI state.
 */
export function UpdaterProvider({ children }: { children: ReactNode }) {
  const settingsQuery = useSettings();
  const [status, setStatus] = useState<UpdaterStatus>("idle");
  const [detected, setDetected] = useState<UpdateInfo | null>(null);
  const [progress, setProgress] = useState<UpdaterDownloadProgress | null>(null);
  const [error, setError] = useState<AppError | null>(null);

  // Chunk callbacks arrive far faster than the screen refreshes, so bytes are
  // accumulated in refs and flushed at most once per frame. Committing every
  // chunk straight to state would re-render the modal thousands of times over
  // a large download for no visible gain -- the same reasoning behind the
  // engine's 150 ms progress batching on the Rust side.
  const downloadedRef = useRef(0);
  const totalRef = useRef<number | null>(null);
  const frameRef = useRef<number | null>(null);

  const scheduleFlush = useCallback(() => {
    if (frameRef.current !== null) return;
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = null;
      setProgress({ downloaded: downloadedRef.current, total: totalRef.current });
    });
  }, []);

  useEffect(() => {
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;

    void (async () => {
      const fn = await listen<UpdaterProgressEvent>(UPDATE_PROGRESS_EVENT, (event) => {
        const payload = event.payload;
        if (payload.phase === "started") {
          totalRef.current = payload.content_length;
          downloadedRef.current = 0;
          scheduleFlush();
        } else if (payload.phase === "progress") {
          downloadedRef.current += payload.chunk_length;
          scheduleFlush();
        } else {
          // Commit the final tally synchronously: a pending frame would be
          // cancelled on unmount and the bar could freeze just shy of 100%.
          if (frameRef.current !== null) {
            cancelAnimationFrame(frameRef.current);
            frameRef.current = null;
          }
          setProgress({ downloaded: downloadedRef.current, total: totalRef.current });
        }
      });
      // `listen` is async, so the provider can unmount before it resolves --
      // detach immediately in that case rather than leaking the listener.
      if (cancelled) fn();
      else unlisten = fn;
    })();

    return () => {
      cancelled = true;
      unlisten?.();
      if (frameRef.current !== null) cancelAnimationFrame(frameRef.current);
    };
  }, [scheduleFlush]);

  const check = useCallback(async (silent = false) => {
    setStatus("checking");
    setError(null);
    try {
      const info = await checkForUpdate();
      setDetected(info);
      setStatus(info ? "available" : "up_to_date");
    } catch (err) {
      console.warn("Update check failed", err);
      setDetected(null);
      if (silent) {
        // The startup check is invisible by design: no dot, no banner, and
        // no state left behind for the pane to render later.
        setStatus("idle");
      } else {
        setError(err as AppError);
        setStatus("error");
      }
    }
  }, []);

  const install = useCallback(async () => {
    setStatus("downloading");
    setError(null);
    downloadedRef.current = 0;
    totalRef.current = null;
    setProgress({ downloaded: 0, total: null });
    try {
      await downloadInstallUpdate();
      setStatus("ready");
    } catch (err) {
      console.error("Update install failed", err);
      setError(err as AppError);
      setStatus("error");
    }
  }, []);

  const restart = useCallback(async () => {
    try {
      await restartApp();
    } catch (err) {
      // Reaching here means the process did not go down. Surfacing it beats a
      // dead-looking button, even though the update is already applied.
      console.error("Restart failed", err);
      setError(err as AppError);
      setStatus("error");
    }
  }, []);

  // One silent check per launch, once settings have loaded and only if the
  // user left it on. No throttling: `latest.json` is a static asset with no
  // rate limit, so a per-launch request is both cheap and the simplest thing
  // that stays correct. A `--silent-start` launch needs no special case --
  // the webview loads either way, and a dot on a hidden window bothers nobody.
  const autoCheckedRef = useRef(false);
  useEffect(() => {
    if (autoCheckedRef.current || !settingsQuery.data) return;
    autoCheckedRef.current = true;
    if (settingsQuery.data.auto_check_update) void check(true);
  }, [settingsQuery.data, check]);

  const value = useMemo<UpdaterState>(
    () => ({
      status,
      detected,
      progress,
      error,
      hasUpdate: detected !== null,
      check,
      install,
      restart,
    }),
    [status, detected, progress, error, check, install, restart],
  );

  return <UpdaterContext.Provider value={value}>{children}</UpdaterContext.Provider>;
}

export function useUpdater(): UpdaterState {
  const ctx = useContext(UpdaterContext);
  if (!ctx) throw new Error("useUpdater must be used inside <UpdaterProvider>");
  return ctx;
}
