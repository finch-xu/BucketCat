import { useMemo, useState, type FormEvent } from "react";
import { AlertTriangle, FolderPlus, Loader2, Pencil } from "lucide-react";
import { useTranslation } from "react-i18next";
import { useMutation, useQueryClient, type UseMutationResult } from "@tanstack/react-query";
import { Modal } from "@/components/ui/modal";
import {
  objectsRootKey,
  useCreateFolder,
  useDeleteObjects,
  useObjects,
  useRenameObject,
} from "@/hooks/use-objects";
import { deletePrefix, type AppError, type BatchResult } from "@/lib/api";
import { useErrorText } from "@/hooks/use-error-text";
import {
  isValidObjectName,
  listingGuard,
  nameCollides,
  parentPrefix,
  pathToPrefix,
  renameKey,
} from "@/lib/entries";
import { useApp } from "@/store/app-store";

const INPUT_CLASS =
  "h-9 w-full rounded-[9px] border border-border bg-panel px-3 text-[13px] text-foreground outline-none focus:border-primary focus:ring-[3px] focus:ring-primary-soft";
const CANCEL_CLASS =
  "h-9 cursor-pointer rounded-[9px] border border-border bg-background px-4 text-[13px] font-medium text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60";
const PRIMARY_CLASS =
  "inline-flex h-9 cursor-pointer items-center gap-[7px] rounded-[9px] bg-primary px-4 text-[13px] font-semibold text-primary-foreground hover:bg-primary-strong disabled:cursor-not-allowed disabled:opacity-60";
const DANGER_CLASS =
  "inline-flex h-9 cursor-pointer items-center gap-[7px] rounded-[9px] bg-destructive px-4 text-[13px] font-semibold text-white hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-70";

/** Create a folder = PUT a zero-byte `<current path>/<name>/` marker object.
 * The name is validated client-side (non-empty, no "/") before the command
 * is ever issued; the backend re-validates in `folder_marker_key`. */
function NewFolderDialog() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const { activeConn, activeBucket, path, showNewFolder, closeNewFolder } = useApp();
  const createMutation = useCreateFolder(activeConn, activeBucket);
  // Deliberately the *unfiltered* current-path listing, not `useBrowse`'s
  // search-scoped one: if the guard used the search-scoped listing, an
  // active search term could hide a same-named sibling from the check,
  // letting a collision through unnoticed (the exact bug this guard exists
  // to prevent). In the common case this is a cache hit -- the folder was
  // already browsed unfiltered before a search or this dialog narrowed
  // anything -- otherwise it's one extra list call, which is an acceptable
  // cost for a destructive-action guard.
  const pathPrefix = pathToPrefix(path);
  const pathListingQuery = useObjects(activeConn, activeBucket, pathPrefix);
  // Fail CLOSED on "never resolved" AND on "still showing the previous
  // location's placeholder" -- `listingGuard` is the single definition of
  // both conditions and of the entries a guard may check against. Blocking
  // submission is reachable in practice on both counts: the New Folder
  // button is enabled the instant a bucket is selected (before any listing
  // has loaded), and `useObjects`'s `keepPreviousData` keeps the previous
  // folder's rows in `data` across every navigation.
  const guard = useMemo(
    () => listingGuard(pathListingQuery.data, pathListingQuery.isPlaceholderData),
    [pathListingQuery.data, pathListingQuery.isPlaceholderData],
  );
  const { ready: guardReady, entries: pathEntries } = guard;
  const [name, setName] = useState("");
  const [touched, setTouched] = useState(false);

  if (!showNewFolder) return null;

  const trimmed = name.trim();
  const valid = isValidObjectName(name);
  // A folder is a zero-byte `<name>/` marker PUT with no existence check on
  // the backend, so a name collision here doesn't overwrite data the way a
  // colliding rename does -- but it would still silently produce two rows
  // with the same displayed name. Checked only against already-loaded
  // page(s) of the unfiltered current-path listing, not the server (and not
  // narrowed by whatever search text happens to be active elsewhere).
  const collision = valid && nameCollides(pathEntries, trimmed);

  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setTouched(true);
    if (!guardReady || !valid || collision || createMutation.isPending) return;
    createMutation.mutate(pathToPrefix(path) + trimmed, {
      onSuccess: () => closeNewFolder(),
    });
  }

  return (
    <Modal onClose={closeNewFolder} className="w-[420px]">
      <form onSubmit={handleSubmit}>
        <div className="flex items-start gap-3 px-5 pt-5 pb-1">
          <span className="flex size-9 shrink-0 items-center justify-center rounded-full bg-primary-soft text-primary">
            <FolderPlus className="size-[18px]" />
          </span>
          <div className="min-w-0 flex-1 pt-1">
            <div className="text-[15px] font-bold">{t("objects.newFolderTitle")}</div>
            <label className="mt-3 block text-[12px] font-medium text-muted-foreground">
              {t("objects.newFolderLabel")}
            </label>
            <input
              autoFocus
              value={name}
              onChange={(e) => setName(e.target.value)}
              onBlur={() => setTouched(true)}
              placeholder={t("objects.newFolderPlaceholder")}
              className={`${INPUT_CLASS} mt-1.5`}
            />
            {!guardReady ? (
              <p className="mt-1.5 text-[12px] text-muted-foreground">
                {t("objects.checkingNames")}
              </p>
            ) : (
              <>
                {touched && !valid && (
                  <p className="mt-1.5 text-[12px] text-destructive">{t("objects.invalidName")}</p>
                )}
                {touched && valid && collision && (
                  <p className="mt-1.5 text-[12px] text-destructive">{t("objects.nameExists")}</p>
                )}
              </>
            )}
            {createMutation.isError && (
              <p className="mt-1.5 text-[12px] text-destructive">
                {errorText(createMutation.error)}
              </p>
            )}
          </div>
        </div>
        <div className="mt-3 flex items-center justify-end gap-2.5 border-t border-border2 px-5 py-4">
          <button
            type="button"
            onClick={closeNewFolder}
            disabled={createMutation.isPending}
            className={CANCEL_CLASS}
          >
            {t("objects.cancel")}
          </button>
          <button
            type="submit"
            disabled={createMutation.isPending || !guardReady}
            className={PRIMARY_CLASS}
          >
            {createMutation.isPending && <Loader2 className="size-3.5 animate-spin" />}
            {createMutation.isPending ? t("objects.creating") : t("objects.create")}
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** Rename one object: the backend does copy-then-delete. Only the final path
 * segment is editable -- moving an object between prefixes is a different
 * gesture (drag/move, not in M3). */
function RenameObjectDialog() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const { activeConn, activeBucket, renameTarget, closeRename, clearSelection } = useApp();
  const renameMutation = useRenameObject(activeConn, activeBucket);
  // Deliberately the *unfiltered* listing of the prefix the RENAME TARGET
  // itself lives under, not the store's browsed `path` -- see the matching
  // `pathToPrefix(path)` comment in `NewFolderDialog`, which is safe there
  // only because a new folder is always created directly under the browsed
  // path. Rename can't reuse that: the browsed listing prefix is
  // `pathToPrefix(path) + search` (design §6), so a search term containing
  // "/" lists rows that live under a deeper prefix than `pathToPrefix(path)`
  // alone. Deriving the guard from `target.key` via `parentPrefix` keeps it
  // correct regardless of how this listing was reached -- and it matters
  // even more here than in `NewFolderDialog`: rename is copy-then-delete, so
  // checking the wrong (too-shallow) listing would silently overwrite a
  // same-named sibling instead of just producing a confusing duplicate row.
  //
  // `renameTarget` can be `null` on this render (the component always
  // mounts; it only returns `null` below once we know there's no target),
  // so this falls back to the bucket root ("") -- irrelevant in practice
  // since the early return right after makes the rest of this component,
  // and thus `pathEntries`, unused for that render. Computed unconditionally
  // (not inside a hook) so `useObjects`'s hook call stays unconditional.
  const guardPrefix = renameTarget ? parentPrefix(renameTarget.key) : "";
  // Passing "" as the connection id when there's no target leans on
  // `useObjects`'s own `enabled: connectionId.length > 0 && ...` guard to
  // keep this query idle for as long as this "closed" instance is mounted
  // (`ObjectDialogs` keys this component by target, so a `null` target here
  // means EVERY render of this instance has a `null` target -- see that
  // comment). Without this, the "closed" instance would run a live,
  // continuously-refetched ListObjectsV2 against the bucket root the whole
  // time the user browses (refetched on every objectsRootKey invalidation:
  // delete/rename/create-folder/refresh) only to be thrown away the moment
  // it resolves. Once a real target mounts a fresh instance (a different
  // `key`, per `ObjectDialogs`), `activeConn` flows through unchanged and
  // `guardReady` below still fails closed exactly as before until that
  // target's own parent-prefix listing has loaded.
  const pathListingQuery = useObjects(renameTarget ? activeConn : "", activeBucket, guardPrefix);
  // Fail CLOSED -- see the matching comment in `NewFolderDialog` and
  // `listingGuard` itself. Both halves are reachable here: navigate into a
  // large folder, search fast, and rename before the unfiltered first page
  // has resolved (never-loaded); or open rename right after switching
  // folders, while `keepPreviousData` still serves the previous prefix's
  // rows (placeholder). Rename is copy-then-delete, so a guard checking the
  // wrong folder's listing destroys the collision target just as thoroughly
  // as an upload would.
  const guard = useMemo(
    () => listingGuard(pathListingQuery.data, pathListingQuery.isPlaceholderData),
    [pathListingQuery.data, pathListingQuery.isPlaceholderData],
  );
  const { ready: guardReady, entries: pathEntries } = guard;
  // Safe as the initial value because `ObjectDialogs` keys this component by
  // the target's key -- a different target mounts a fresh instance.
  const [name, setName] = useState(renameTarget?.name ?? "");
  const [touched, setTouched] = useState(false);

  if (!renameTarget) return null;
  const target = renameTarget;

  const trimmed = name.trim();
  const valid = isValidObjectName(name) && trimmed !== target.name;
  // Rename is copy-then-delete: colliding with another existing key would
  // silently overwrite it (copy target = the sibling's key) before the
  // original is deleted. Excludes the target's own current entry so
  // renaming it back to its unchanged name isn't flagged as a "collision"
  // -- that no-op case is already blocked above by `trimmed !== target.name`.
  // Checked only against already-loaded page(s) of the unfiltered listing
  // of `target`'s own parent prefix, not the server (and not narrowed by
  // whatever search text happens to be active elsewhere).
  const collision = valid && nameCollides(pathEntries, trimmed, target.key);

  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setTouched(true);
    if (!guardReady || !valid || collision || renameMutation.isPending) return;
    renameMutation.mutate(
      { fromKey: target.key, toKey: renameKey(target.key, trimmed) },
      {
        onSuccess: () => {
          clearSelection();
          closeRename();
        },
      },
    );
  }

  return (
    <Modal onClose={closeRename} className="w-[420px]">
      <form onSubmit={handleSubmit}>
        <div className="flex items-start gap-3 px-5 pt-5 pb-1">
          <span className="flex size-9 shrink-0 items-center justify-center rounded-full bg-primary-soft text-primary">
            <Pencil className="size-[18px]" />
          </span>
          <div className="min-w-0 flex-1 pt-1">
            <div className="text-[15px] font-bold">{t("objects.renameTitle")}</div>
            <div className="mt-1 truncate font-mono text-[11.5px] text-muted-foreground">
              {target.key}
            </div>
            <label className="mt-3 block text-[12px] font-medium text-muted-foreground">
              {t("objects.renameLabel")}
            </label>
            <input
              autoFocus
              value={name}
              onChange={(e) => setName(e.target.value)}
              onBlur={() => setTouched(true)}
              className={`${INPUT_CLASS} mt-1.5`}
            />
            {!guardReady ? (
              <p className="mt-1.5 text-[12px] text-muted-foreground">
                {t("objects.checkingNames")}
              </p>
            ) : (
              <>
                {touched && !valid && (
                  <p className="mt-1.5 text-[12px] text-destructive">{t("objects.invalidName")}</p>
                )}
                {touched && valid && collision && (
                  <p className="mt-1.5 text-[12px] text-destructive">{t("objects.nameExists")}</p>
                )}
              </>
            )}
            {renameMutation.isError && (
              <p className="mt-1.5 text-[12px] text-destructive">
                {errorText(renameMutation.error)}
              </p>
            )}
          </div>
        </div>
        <div className="mt-3 flex items-center justify-end gap-2.5 border-t border-border2 px-5 py-4">
          <button
            type="button"
            onClick={closeRename}
            disabled={renameMutation.isPending}
            className={CANCEL_CLASS}
          >
            {t("objects.cancel")}
          </button>
          <button
            type="submit"
            disabled={renameMutation.isPending || !guardReady}
            className={PRIMARY_CLASS}
          >
            {renameMutation.isPending && <Loader2 className="size-3.5 animate-spin" />}
            {renameMutation.isPending ? t("objects.renaming") : t("objects.renameSubmit")}
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** Recursive folder delete as a mutation, mirroring `useDeleteObjects` but
 * targeting a whole prefix via `delete_prefix`. Lives here (not in
 * `use-objects.ts`) beside its only caller, and invalidates the same
 * `objectsRootKey` so the deleted folder disappears from every listing. */
function useDeletePrefix(
  connectionId: string,
  bucket: string,
): UseMutationResult<BatchResult, AppError, string> {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (prefix: string) => deletePrefix(connectionId, bucket, prefix),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: objectsRootKey(connectionId, bucket) });
    },
  });
}

/** Batch delete with design §7's partial-failure contract: the command
 * resolves (never rejects) with `{ succeeded, failed }`, so a mixed outcome
 * turns this dialog into a report -- 「成功 N / 失败 M」plus the per-key
 * failure list rendered through the same `errors.*` dictionary as top-level
 * errors -- instead of silently closing or pretending the whole batch
 * failed. A clean run closes immediately.
 *
 * A single `prefix/` target (folders never multi-select, and only folder
 * keys end in "/") is a *folder* delete: it routes to the recursive
 * `deletePrefix` command and shows the distinct "folder and everything in
 * it, irreversibly" confirmation. Every other case is an ordinary file
 * batch delete. The design §7 report shape (「成功 N / 失败 M」) is identical
 * for both, so the report branch is shared. */
function DeleteObjectsDialog() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const { activeConn, activeBucket, deleteTargets, closeDeleteObjects, clearSelection } = useApp();
  const deleteMutation = useDeleteObjects(activeConn, activeBucket);
  const deletePrefixMutation = useDeletePrefix(activeConn, activeBucket);

  if (!deleteTargets || deleteTargets.length === 0) return null;
  const keys = deleteTargets;
  const isFolder = keys.length === 1 && keys[0].endsWith("/");
  const mutation = isFolder ? deletePrefixMutation : deleteMutation;
  const result = mutation.data;
  // Non-null exactly when the batch came back with per-key failures. This is
  // what flips the dialog from "confirm" to "report" mode -- and, unlike a
  // boolean flag, it keeps the narrowing TypeScript needs to read
  // `report.succeeded` / `report.failed` in the JSX below.
  const report = result && result.failed.length > 0 ? result : null;

  function handleConfirm() {
    const onSuccess = (batch: BatchResult) => {
      if (batch.failed.length === 0) {
        clearSelection();
        closeDeleteObjects();
      }
    };
    // A recursive folder delete takes the single prefix; a file batch takes
    // the whole key list. Fired on the concrete mutation (not the `mutation`
    // union) so each gets its own argument type.
    if (isFolder) {
      deletePrefixMutation.mutate(keys[0], { onSuccess });
    } else {
      deleteMutation.mutate(keys, { onSuccess });
    }
  }

  function handleClose() {
    if (result && result.succeeded > 0) clearSelection();
    closeDeleteObjects();
  }

  // Form submit is the single Enter-key path for both modes -- `report` is
  // read at submit time, not baked into which handler got wired up, so it
  // can never fire the destructive action once the dialog has flipped to
  // reporting a partial failure.
  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (report) {
      handleClose();
      return;
    }
    if (mutation.isPending) return;
    handleConfirm();
  }

  const singleName = keys[0].replace(/\/$/, "").split("/").pop() ?? keys[0];

  return (
    <Modal onClose={handleClose} className="w-[460px]">
      <form onSubmit={handleSubmit}>
        <div className="flex items-start gap-3 px-5 pt-5 pb-1">
          <span className="flex size-9 shrink-0 items-center justify-center rounded-full bg-destructive/10 text-destructive">
            <AlertTriangle className="size-[18px]" />
          </span>
          <div className="min-w-0 flex-1 pt-1">
            <div className="text-[15px] font-bold">
              {report
                ? t("objects.partialTitle")
                : isFolder
                  ? t("objects.deleteFolderTitle")
                  : keys.length === 1
                    ? t("objects.deleteTitleOne")
                    : t("objects.deleteTitleMany", { count: keys.length })}
            </div>
            {report ? (
              <>
                <p className="mt-1.5 text-[13px] text-fg2">
                  {t("objects.partialSummary", {
                    succeeded: report.succeeded,
                    failed: report.failed.length,
                  })}
                </p>
                <ul className="mt-2.5 max-h-[180px] overflow-y-auto rounded-[9px] border border-border2 bg-panel p-2.5">
                  {report.failed.map((f) => (
                    <li key={f.key} className="py-1 text-[12px]">
                      <span className="block truncate font-mono text-fg2">{f.key}</span>
                      <span className="text-destructive">
                        {errorText({ code: f.code, params: {} })}
                      </span>
                    </li>
                  ))}
                </ul>
              </>
            ) : (
              <p className="mt-1.5 text-[13px] text-fg2">
                {isFolder
                  ? t("objects.deleteFolderBody", { name: singleName })
                  : keys.length === 1
                    ? t("objects.deleteBodyOne", { name: singleName })
                    : t("objects.deleteBodyMany", { count: keys.length })}
              </p>
            )}
            {mutation.error && (
              <p className="mt-1.5 text-[12px] text-destructive">{errorText(mutation.error)}</p>
            )}
          </div>
        </div>
        <div className="mt-3 flex items-center justify-end gap-2.5 border-t border-border2 px-5 py-4">
          {report ? (
            <button type="submit" autoFocus className={PRIMARY_CLASS}>
              {t("objects.close")}
            </button>
          ) : (
            <>
              <button
                type="button"
                onClick={handleClose}
                disabled={mutation.isPending}
                className={CANCEL_CLASS}
              >
                {t("objects.cancel")}
              </button>
              <button type="submit" autoFocus disabled={mutation.isPending} className={DANGER_CLASS}>
                {mutation.isPending && <Loader2 className="size-3.5 animate-spin" />}
                {mutation.isPending ? t("objects.deleting") : t("objects.delete")}
              </button>
            </>
          )}
        </div>
      </form>
    </Modal>
  );
}

/** Mounts all three object dialogs, each keyed per target (same pattern as
 * `ConnectionModals`): switching targets remounts rather than patching, so a
 * dialog's first render is already correct for its target and no stale
 * mutation state (error, partial-failure report, pending flag) carries over
 * from whatever was open before. */
export function ObjectDialogs() {
  const { showNewFolder, renameTarget, deleteTargets } = useApp();
  const newFolderKey = showNewFolder ? "new-folder" : "closed";
  const renameDialogKey = renameTarget ? `rename:${renameTarget.key}` : "closed";
  const deleteDialogKey = deleteTargets ? `delete:${deleteTargets.join("|")}` : "closed";

  return (
    <>
      <NewFolderDialog key={newFolderKey} />
      <RenameObjectDialog key={renameDialogKey} />
      <DeleteObjectsDialog key={deleteDialogKey} />
    </>
  );
}
