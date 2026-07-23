import { createElement, useEffect, useMemo, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { UploadConflictDialog } from "@/components/modals/upload-conflict-dialog";
import { enqueueUploads } from "@/lib/api";
import { nameCollides, pathToPrefix, uploadKey } from "@/lib/entries";
import { objectsRootKey, useObjects } from "@/hooks/use-objects";
import { useApp } from "@/store/app-store";
import { useTransferStore } from "@/store/transfer-store";

/** One pending upload batch that collided with the current folder's listing
 * and is waiting on the user to pick overwrite-or-skip (see
 * `UploadConflictDialog`). */
export interface UploadConflict {
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

/** Only the final path segment matters -- mirrors `uploadKey`'s own
 * extraction so the name used for the collision check is exactly the name
 * the upload would land under. */
function basenameOf(localPath: string): string {
  return (localPath.split(/[\\/]/).pop() ?? localPath).trim();
}

export interface UseStartUploadsResult {
  /** Enqueues `paths` (absolute local file paths) into the currently browsed
   * folder, prompting for overwrite/skip first if any of them collide with
   * an existing name. Fails closed (does nothing) while the folder's own
   * unfiltered listing has not loaded at least once -- see `guardReady`. */
  startUploads: (paths: string[]) => void;
  /** Whether the collision guard has a usable listing yet. `false` for a
   * moment right after navigating into a not-yet-browsed folder. Callers
   * should surface this rather than let a click/drop silently do nothing --
   * e.g. disable the trigger, or swap in `t("objects.checkingNames")`. */
  guardReady: boolean;
  /** Pre-keyed dialog element (see `UploadConflictDialog`'s "keyed
   * remounting" note) -- render `{dialog}` anywhere under this hook's
   * caller; it renders nothing while no conflict is pending. */
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
 * already had to fix the *same* silent-overwrite hole four times on the
 * rename path (bypassable via the search-scoped listing, failed open while
 * its own guard query was still loading, and used the wrong prefix when the
 * search text contained "/" -- see the `nameCollides`/`parentPrefix` doc
 * comments in `src/lib/entries.ts` and the guards in
 * `src/components/modals/object-dialogs.tsx`). This hook is built to the
 * same shape on purpose: unfiltered listing, fail-closed while unloaded,
 * exclude nothing that would hide a real collision.
 */
export function useStartUploads(): UseStartUploadsResult {
  const { activeConn, activeBucket, path } = useApp();
  const queryClient = useQueryClient();
  const applyState = useTransferStore((s) => s.applyState);
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
  const entries = useMemo(
    () => (listingQuery.data?.pages ?? []).flatMap((p) => p.entries),
    [listingQuery.data],
  );
  // Fail CLOSED, not open: until this listing has resolved at least once,
  // `entries` is `[]` and every upload would look collision-free --
  // indistinguishable from an actually-empty folder. That window is
  // reachable in practice (drop files the instant a large/unbrowsed folder
  // is opened, before its first page has loaded). Deliberately `data !==
  // undefined`, not `!isFetching`: a background refetch of an
  // already-usable listing must not re-block a guard that's already
  // trustworthy.
  //
  // Honest limits, same as every other guard built on `nameCollides`: this
  // only sees page(s) of the current folder already loaded client-side, it
  // cannot see a key created concurrently by another client mid-check, and
  // it is not a server-side guarantee -- the backend still issues a plain
  // `PutObject` with no existence check. This narrows the common case; it
  // does not close the race.
  const guardReady = listingQuery.data !== undefined;

  const [conflict, setConflict] = useState<UploadConflict | null>(null);

  function enqueue(paths: string[]) {
    if (paths.length === 0) return;
    void (async () => {
      const tasks = await enqueueUploads(activeConn, activeBucket, prefix, paths);
      for (const task of tasks) applyState(task);
      setPanelOpen(true);
    })();
  }

  function startUploads(paths: string[]) {
    if (paths.length === 0) return;
    // Refuse silently rather than risk an unguarded overwrite -- see the
    // `guardReady` doc comment above for what a caller should show instead
    // of nothing happening.
    if (!guardReady) return;

    const collidingKeys: string[] = [];
    const nonCollidingPaths: string[] = [];
    for (const localPath of paths) {
      const base = basenameOf(localPath);
      // The target key is what's actually at stake (the exact remote
      // object a collision would silently replace); the match itself is
      // still `nameCollides` against the loaded listing's display names --
      // the same primitive every other collision guard in this codebase
      // uses, kept identical on purpose (see the module doc comment).
      if (nameCollides(entries, base)) collidingKeys.push(uploadKey(prefix, base));
      else nonCollidingPaths.push(localPath);
    }

    if (collidingKeys.length === 0) {
      enqueue(paths);
      return;
    }
    setConflict({ allPaths: paths, nonCollidingPaths, collidingKeys });
  }

  function resolveOverwrite() {
    if (!conflict) return;
    enqueue(conflict.allPaths);
    setConflict(null);
  }

  function resolveSkip() {
    if (!conflict) return;
    enqueue(conflict.nonCollidingPaths);
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
  // previous invocation can flash.
  const dialog = createElement(UploadConflictDialog, {
    key: conflict ? conflict.allPaths.join(" ") : "closed",
    conflict,
    onOverwrite: resolveOverwrite,
    onSkip: resolveSkip,
    onCancel: cancelConflict,
  });

  return { startUploads, guardReady, dialog };
}
