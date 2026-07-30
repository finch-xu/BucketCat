import { ConnectionModal } from "@/components/modals/connection-modal";
import { DeleteConnectionDialog } from "@/components/modals/delete-connection-dialog";
import { useApp } from "@/store/app-store";

/** Mounts the add/edit connection wizard and the delete-confirmation dialog,
 * each keyed per target ("add", `edit:${id}`, `delete:${id}`, or a per-dialog
 * closed sentinel). Switching targets (e.g. Edit A -> Edit B, or a failed
 * delete of B followed by opening delete on A) changes the key, forcing React
 * to unmount the old instance and mount a brand new one instead of patching an
 * existing instance's state via an effect. That's what keeps each dialog's
 * first render already correct for its target -- no one-frame flash of the
 * previous target's fields, and no stale mutation error/pending carried over
 * from whatever was previously open.
 *
 * The two closed sentinels must **differ from each other**: these are sibling
 * children of one fragment, so React requires their keys to be unique, and a
 * shared "closed" makes it warn ("Encountered two children with the same key")
 * and risk remounting the wrong one whenever both dialogs are shut -- which is
 * most of the time. Neither sentinel contains a `:`, while every open-state key
 * for the delete dialog does, so no real target id can ever collide with one. */
export function ConnectionModals() {
  const { showAdd, editingConnection, deletingConnection } = useApp();
  const connectionModalKey = editingConnection
    ? `edit:${editingConnection.id}`
    : showAdd
      ? "add"
      : "connection-closed";
  const deleteDialogKey = deletingConnection
    ? `delete:${deletingConnection.id}`
    : "delete-closed";

  return (
    <>
      <ConnectionModal key={connectionModalKey} />
      <DeleteConnectionDialog key={deleteDialogKey} />
    </>
  );
}
