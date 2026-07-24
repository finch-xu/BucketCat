import {
  createElement,
  useCallback,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import { useQueryClient } from "@tanstack/react-query";
import { UploadConflictDialog, UploadNoticeDialog } from "@/components/modals/upload-conflict-dialog";
import { enqueueUploads, toAppError, type AppError } from "@/lib/api";
import { listingGuard, nameCollides, pathToPrefix, uploadBaseName, uploadKey } from "@/lib/entries";
import { objectsRootKey, useObjects } from "@/hooks/use-objects";
import { useApp } from "@/store/app-store";
import { useTransferStore } from "@/store/transfer-store";

/** One pending upload batch that collided with the current folder's listing
 * and is waiting on the user to pick overwrite-or-skip (see
 * `UploadConflictDialog`). */
export interface UploadConflict {
  /** The prefix the collision was computed against, captured when the
   * conflict opened. Both resolutions enqueue against THIS, never against
   * the prefix of whatever render happens to run when the user finally
   * clicks: the browse location can change underneath an open modal (a
   * background refresh, a restored session, another pane), and enqueueing
   * "overwrite" into a folder whose contents were never checked is the same
   * silent destruction this whole flow exists to prevent. */
  prefix: string;
  /** Every local path from the original request, in original order --
   * re-enqueued whole on "overwrite". */
  allPaths: string[];
  /** The subset of `allPaths` that did NOT collide -- enqueued alone on
   * "skip". */
  nonCollidingPaths: string[];
  /** Full remote keys (`uploadKey(prefix, basename)`) of the colliding
   * files, for the dialog's list -- showing the actual key that would be
   * overwritten, not just a bare file name, so a nested browse location
   * doesn't read as ambiguous. */
  collidingKeys: string[];
}

/** An after-the-fact "that gesture did not do what you'd assume" message.
 * Every one of these covers a case that previously ended in nothing visibly
 * happening at all. */
export type UploadNotice =
  /** An IPC call rejected -- `enqueue_uploads`, or a caller's own command
   * (the toolbar's file picker) routed here via `reportError`. */
  | { kind: "error"; error: AppError }
  /** Fewer tasks came back than paths were sent (the backend skips
   * directories and unreadable paths), or the user chose "skip" for a batch
   * in which EVERY file collided, so nothing at all was queued. */
  | { kind: "skipped"; queued: number; skipped: number }
  /** A drop landed while the collision guard was not ready and was refused.
   * Refusing is correct (see `guardReady`), but the drag overlay's "checking
   * names" hint disappears with the drag, so the gesture would otherwise
   * vanish without a trace. */
  | { kind: "notReady" };

export interface UseStartUploadsResult {
  /** Enqueues `paths` (absolute local file paths) into the currently browsed
   * folder, prompting for overwrite/skip first if any of them collide with
   * an existing name. Refuses (with a `notReady` notice) while the folder's
   * own unfiltered listing has not loaded at least once -- see `guardReady`.
   * Stable identity: safe as an effect dependency. */
  startUploads: (paths: string[]) => void;
  /** Whether the collision guard has a usable listing yet. `false` for a
   * moment right after navigating into a not-yet-browsed folder, AND for the
   * whole window in which `useObjects` is still serving the previous
   * location's listing as placeholder data. Callers should surface this
   * rather than let a click/drop silently do nothing -- e.g. disable the
   * trigger, or swap in `t("objects.checkingNames")`. */
  guardReady: boolean;
  /** Surfaces a rejected IPC call from the caller's own upload-adjacent
   * command (e.g. the toolbar's `open()` file picker) through this hook's
   * dialog, so a failure is never just an unhandled promise rejection in the
   * console. Accepts an unknown rejection reason and narrows it with
   * `toAppError`. Stable identity. */
  reportError: (reason: unknown) => void;
  /** Pre-keyed dialog element (see `UploadConflictDialog`'s "keyed
   * remounting" note) -- render `{dialog}` anywhere under this hook's
   * caller; it is `null` while nothing is pending. */
  dialog: ReactNode;
}

/**
 * The shared upload entry point: both the toolbar's file-picker button and
 * the object list's drag-and-drop call this hook directly (each gets its
 * own independent instance/state -- that's fine, only one of them is ever
 * actually driving an upload at a time).
 *
 * Object storage has no rename and no overwrite prompt -- a `PutObject` onto
 * an existing key silently destroys whatever was there. This project has
 * already had to fix the *same* silent-overwrite hole on the rename path
 * repeatedly (bypassable via the search-scoped listing, failed open while
 * its own guard query was still loading, used the wrong prefix when the
 * search text contained "/", and -- the recombination of the first two --
 * trusted `keepPreviousData`'s placeholder, i.e. the *previous* folder's
 * listing, as if it described this one). The shared `listingGuard` in
 * `src/lib/entries.ts` is the single answer to "may I conclude anything from
 * this listing?"; every guard in the app now goes through it.
 */
export function useStartUploads(): UseStartUploadsResult {
  const { activeConn, activeBucket, path } = useApp();
  const queryClient = useQueryClient();
  // Not subscribing to `applyState`: the enqueue command returns each task's
  // *Queued* snapshot, but those (and the subsequent Running) already arrive as
  // `transfer://state` events via `useTransferEvents`. Re-applying the stale
  // returned snapshot here races the Running event and pins the row at "queued"
  // for the whole upload (a Running->Running non-transition emits no correcting
  // event), hiding the pause control. Events are the single source of truth.
  const setPanelOpen = useTransferStore((s) => s.setPanelOpen);

  const prefix = pathToPrefix(path);
  // Deliberately the *unfiltered* listing of the browsed folder, not
  // `useBrowse`'s search-scoped one -- same reasoning as `NewFolderDialog`
  // and `RenameObjectDialog` in `object-dialogs.tsx`: if this used a
  // search-scoped listing, an active search term could hide a same-named
  // sibling from the check, letting a collision through unnoticed. An
  // upload always lands directly under the browsed path -- `uploadKey` only
  // ever uses the local file's own basename, so a path-like local file name
  // can't redirect the target deeper -- so `pathToPrefix(path)` (not a
  // per-target `parentPrefix`, unlike rename) is always the right prefix to
  // check.
  const listingQuery = useObjects(activeConn, activeBucket, prefix);
  // Fail CLOSED on both "never loaded" and "this is the previous location's
  // placeholder" -- see `listingGuard`. `entries` is gated by the same
  // condition, so a stale listing can neither clear an overwrite nor invent
  // a phantom conflict.
  //
  // Honest limits, same as every other guard built on `nameCollides`: this
  // only sees page(s) of the current folder already loaded client-side, it
  // cannot see a key created concurrently by another client mid-check, and
  // it is not a server-side guarantee -- the backend still issues a plain
  // `PutObject` with no existence check. This narrows the common case; it
  // does not close the race.
  const guard = useMemo(
    () => listingGuard(listingQuery.data, listingQuery.isPlaceholderData),
    [listingQuery.data, listingQuery.isPlaceholderData],
  );
  const { ready: guardReady, entries } = guard;

  const [conflict, setConflict] = useState<UploadConflict | null>(null);
  const [notice, setNotice] = useState<UploadNotice | null>(null);

  const reportError = useCallback((reason: unknown) => {
    setNotice({ kind: "error", error: toAppError(reason) });
  }, []);

  // `targetPrefix` is a parameter, never read from the enclosing render:
  // the conflict dialog resolves against the prefix captured when it opened
  // (see `UploadConflict.prefix`).
  const enqueue = useCallback(
    (targetPrefix: string, paths: string[]) => {
      if (paths.length === 0) return;
      void (async () => {
        try {
          const tasks = await enqueueUploads(activeConn, activeBucket, targetPrefix, paths);
          // The backend silently skips anything that isn't a readable file
          // (a dropped *directory* is the common case), so a short result is
          // the only evidence the user has that part of the gesture went
          // nowhere. Say so rather than let it read as success.
          if (tasks.length < paths.length) {
            setNotice({
              kind: "skipped",
              queued: tasks.length,
              skipped: paths.length - tasks.length,
            });
          }
          if (tasks.length > 0) setPanelOpen(true);
        } catch (reason) {
          // A rejected command used to be an unhandled promise rejection and
          // nothing else: the click appeared to do nothing at all.
          reportError(reason);
        }
      })();
    },
    [activeConn, activeBucket, setPanelOpen, reportError],
  );

  const startUploads = useCallback(
    (paths: string[]) => {
      if (paths.length === 0) return;
      // Refuse rather than risk an unguarded overwrite -- but say so, so the
      // gesture doesn't just evaporate (the drag overlay's "checking names"
      // hint is gone the moment the drag ends).
      if (!guardReady) {
        setNotice({ kind: "notReady" });
        return;
      }
      setNotice(null);

      const collidingKeys: string[] = [];
      const nonCollidingPaths: string[] = [];
      for (const localPath of paths) {
        const base = uploadBaseName(localPath);
        // The target key is what's actually at stake (the exact remote
        // object a collision would silently replace); the match itself is
        // still `nameCollides` against the loaded listing's display names --
        // the same primitive every other collision guard in this codebase
        // uses, kept identical on purpose (see the module doc comment).
        // Both sides derive from `uploadBaseName`, so the name checked here
        // is exactly the name the upload writes.
        if (nameCollides(entries, base)) collidingKeys.push(uploadKey(prefix, base));
        else nonCollidingPaths.push(localPath);
      }

      if (collidingKeys.length === 0) {
        enqueue(prefix, paths);
        return;
      }
      setConflict({ prefix, allPaths: paths, nonCollidingPaths, collidingKeys });
    },
    [enqueue, entries, guardReady, prefix],
  );

  function resolveOverwrite() {
    if (!conflict) return;
    enqueue(conflict.prefix, conflict.allPaths);
    setConflict(null);
  }

  function resolveSkip() {
    if (!conflict) return;
    // "Skip" with nothing left to upload used to close the dialog and do
    // absolutely nothing, which is indistinguishable from a successful
    // queueing of the batch.
    if (conflict.nonCollidingPaths.length === 0) {
      setNotice({ kind: "skipped", queued: 0, skipped: conflict.collidingKeys.length });
    } else {
      enqueue(conflict.prefix, conflict.nonCollidingPaths);
    }
    setConflict(null);
  }

  function cancelConflict() {
    setConflict(null);
  }

  // Invalidate the *uploaded-into* bucket's listing the moment a task
  // reaches `completed`, so the new object appears without a manual
  // refresh. Subscribes to the vanilla store directly instead of through
  // the `useTransferStore` React hook, so this effect never re-renders
  // whichever component called this hook -- it only reacts, at most once
  // per task, to a status transition landing on `completed`, not to every
  // ~6.7Hz progress tick the store otherwise receives.
  useEffect(() => {
    return useTransferStore.subscribe((state, prevState) => {
      if (state.tasks === prevState.tasks) return;
      for (const id in state.tasks) {
        const now = state.tasks[id];
        if (now.status === "completed" && prevState.tasks[id]?.status !== "completed") {
          queryClient.invalidateQueries({
            queryKey: objectsRootKey(now.connection_id, now.bucket),
          });
        }
      }
    });
  }, [queryClient]);

  // Plain `createElement`, not JSX -- this is a `.ts` file (no other JSX in
  // it, so a dedicated `.tsx` just for one element isn't worth it). Keyed
  // remounting per invocation (same convention as `ObjectDialogs`): a fresh
  // conflict always mounts a clean instance, so no stale data from a
  // previous invocation can flash. The conflict takes precedence over a
  // lingering notice -- a decision the user is being asked to make must
  // never be hidden behind an informational message.
  let dialog: ReactNode = null;
  if (conflict) {
    dialog = createElement(UploadConflictDialog, {
      key: `conflict:${conflict.allPaths.join(" ")}`,
      conflict,
      onOverwrite: resolveOverwrite,
      onSkip: resolveSkip,
      onCancel: cancelConflict,
    });
  } else if (notice) {
    dialog = createElement(UploadNoticeDialog, {
      key: `notice:${notice.kind}`,
      notice,
      onClose: () => setNotice(null),
    });
  }

  return { startUploads, guardReady, reportError, dialog };
}
