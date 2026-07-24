import { createElement, useCallback, useState, type ReactNode } from "react";
import { AlertTriangle } from "lucide-react";
import { useTranslation } from "react-i18next";
import { open, save } from "@tauri-apps/plugin-dialog";
import { downloadDir, join } from "@tauri-apps/api/path";
import { Modal } from "@/components/ui/modal";
import { useErrorText } from "@/hooks/use-error-text";
import {
  enqueueDownload,
  enqueueFolderDownload,
  toAppError,
  type AppError,
  type ObjectEntry,
} from "@/lib/api";
import { useApp } from "@/store/app-store";
import { useTransferStore } from "@/store/transfer-store";

export interface UseStartDownloadsResult {
  /** Downloads one file object. Opens a save dialog pre-filled with the
   * system download directory and the object's own name; on confirm, queues
   * the transfer, pushes the returned task into the transfer store and opens
   * the panel. A cancelled dialog (null) does nothing. */
  startFileDownload: (entry: ObjectEntry) => void;
  /** Downloads a whole folder (a common-prefix entry). Opens a directory
   * picker (defaulting to the system download directory); on confirm, queues
   * one task per real object under the prefix, pushes every returned task and
   * opens the panel. A cancelled picker does nothing. */
  startFolderDownload: (entry: ObjectEntry) => void;
  /** Downloads several selected file objects at once (the selection bar's
   * batch action). Opens a single directory picker (defaulting to the system
   * download directory); on confirm, queues one task per item into that
   * directory (local path = dir/name), pushes every returned task and opens
   * the panel. A cancelled picker does nothing, and an empty list is a no-op.
   *
   * Takes `{ key, name }` pairs rather than `ObjectEntry`s on purpose: the
   * caller works from the selection (a set of keys), and a selected key can
   * outlive the loaded listing it was picked from -- e.g. an object deleted
   * elsewhere, then a manual Refresh that re-fetches the listing without
   * clearing the selection. Passing keys straight through means such a key is
   * still queued and its now-missing object surfaces as a visible
   * `storage/key-not-found` error, instead of being silently dropped when it
   * no longer resolves to an entry. Selection is files-only (clicking a folder
   * clears it), so every item is a real object -- no prefix recursion, unlike
   * the folder download; each file lands flat in the chosen directory. */
  startBatchDownload: (items: { key: string; name: string }[]) => void;
  /** Pre-keyed error dialog element -- render `{dialog}` anywhere under this
   * hook's caller; it is `null` while nothing has failed. Mirrors the way
   * `useStartUploads` surfaces a rejected command instead of letting it be an
   * invisible unhandled promise rejection (a click that appeared to do
   * nothing at all). */
  dialog: ReactNode;
}

/**
 * The shared download entry point: the details panel's Download button and
 * the object list's per-row file/folder download actions all call this hook
 * directly (each caller gets its own independent instance/state -- fine, only
 * one is ever driving a download at a time, same as `useStartUploads`).
 *
 * Unlike uploads there is no overwrite guard here: the save/directory dialogs
 * pick a *local* destination and the OS dialog owns its own overwrite prompt,
 * so nothing on the remote store is at stake. For the same reason a completed
 * download deliberately does NOT invalidate the object listing -- a download
 * never changes what the bucket contains (`useStartUploads` invalidates only
 * because an upload adds objects).
 */
export function useStartDownloads(): UseStartDownloadsResult {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const { activeConn, activeBucket } = useApp();
  const applyState = useTransferStore((s) => s.applyState);
  const setPanelOpen = useTransferStore((s) => s.setPanelOpen);

  const [error, setError] = useState<AppError | null>(null);

  const startFileDownload = useCallback(
    (entry: ObjectEntry) => {
      void (async () => {
        try {
          // `save()`, `downloadDir()` and `join()` are all plugin/IPC calls
          // that can reject; wrap the whole flow so a failure surfaces in the
          // dialog rather than as a console-only rejection.
          const defaultPath = await join(await downloadDir(), entry.name);
          const localPath = await save({ defaultPath });
          if (!localPath) return; // user cancelled the save dialog
          const task = await enqueueDownload(activeConn, activeBucket, entry.key, localPath);
          applyState(task);
          setPanelOpen(true);
        } catch (reason) {
          setError(toAppError(reason));
        }
      })();
    },
    [activeConn, activeBucket, applyState, setPanelOpen],
  );

  const startFolderDownload = useCallback(
    (entry: ObjectEntry) => {
      void (async () => {
        try {
          const localDir = await open({ directory: true, defaultPath: await downloadDir() });
          if (!localDir) return; // user cancelled the directory picker
          const tasks = await enqueueFolderDownload(
            activeConn,
            activeBucket,
            entry.key,
            localDir,
          );
          // A folder can expand into many objects -- push every returned task.
          for (const task of tasks) applyState(task);
          if (tasks.length > 0) setPanelOpen(true);
        } catch (reason) {
          setError(toAppError(reason));
        }
      })();
    },
    [activeConn, activeBucket, applyState, setPanelOpen],
  );

  const startBatchDownload = useCallback(
    (items: { key: string; name: string }[]) => {
      if (items.length === 0) return; // nothing selected -- no dialog, no-op
      void (async () => {
        try {
          const localDir = await open({ directory: true, defaultPath: await downloadDir() });
          if (!localDir) return; // user cancelled the directory picker
          // One picker, then one task per file into that directory. Enqueued
          // sequentially so a mid-list failure surfaces in the dialog with the
          // earlier files already queued and running (same fail-forward shape
          // as the per-folder path -- the panel just doesn't pop on error).
          for (const item of items) {
            const localPath = await join(localDir, item.name);
            const task = await enqueueDownload(activeConn, activeBucket, item.key, localPath);
            applyState(task);
          }
          setPanelOpen(true);
        } catch (reason) {
          setError(toAppError(reason));
        }
      })();
    },
    [activeConn, activeBucket, applyState, setPanelOpen],
  );

  // Plain `createElement`, not JSX -- this is a `.ts` file (same reasoning as
  // `useStartUploads`'s dialog). Keeps the exact visual language of
  // `UploadNoticeDialog` so a download failure reads identically to an upload
  // one, but with download-specific copy.
  let dialog: ReactNode = null;
  if (error) {
    const header = createElement(
      "div",
      { key: "head", className: "flex items-start gap-3 px-5 pt-5 pb-1" },
      createElement(
        "span",
        {
          className:
            "flex size-9 shrink-0 items-center justify-center rounded-full bg-destructive/10 text-destructive",
        },
        createElement(AlertTriangle, { className: "size-[18px]" }),
      ),
      createElement(
        "div",
        { className: "min-w-0 flex-1 pt-1" },
        createElement("div", { className: "text-[15px] font-bold" }, t("objects.downloadFailed")),
        createElement("p", { className: "mt-1.5 text-[13px] text-fg2" }, errorText(error)),
      ),
    );
    const footer = createElement(
      "div",
      {
        key: "foot",
        className: "mt-3 flex items-center justify-end gap-2.5 border-t border-border2 px-5 py-4",
      },
      createElement(
        "button",
        {
          type: "button",
          onClick: () => setError(null),
          autoFocus: true,
          className:
            "inline-flex h-9 cursor-pointer items-center gap-[7px] rounded-[9px] bg-primary px-4 text-[13px] font-semibold text-primary-foreground hover:bg-primary-strong",
        },
        t("objects.close"),
      ),
    );
    dialog = createElement(Modal, {
      key: `download-error:${error.code}`,
      onClose: () => setError(null),
      className: "w-[420px]",
      children: [header, footer],
    });
  }

  return { startFileDownload, startFolderDownload, startBatchDownload, dialog };
}
