import { ConnectionModal } from "@/components/modals/connection-modal";
import { DeleteConnectionDialog } from "@/components/modals/delete-connection-dialog";
import { useApp } from "@/store/app-store";

/** Mounts the add/edit connection wizard and the delete-confirmation dialog,
 * each keyed per target ("add", `edit:${id}`, `delete:${id}`, or "closed").
 * Switching targets (e.g. Edit A -> Edit B, or a failed delete of B followed
 * by opening delete on A) changes the key, forcing React to unmount the old
 * instance and mount a brand new one instead of patching an existing
 * instance's state via an effect. That's what keeps each dialog's first
 * render already correct for its target -- no one-frame flash of the
 * previous target's fields, and no stale mutation error/pending carried
 * over from whatever was previously open. */
export function ConnectionModals() {
  const { showAdd, editingConnection, deletingConnection } = useApp();
  const connectionModalKey = editingConnection
    ? `edit:${editingConnection.id}`
    : showAdd
      ? "add"
      : "closed";
  const deleteDialogKey = deletingConnection ? `delete:${deletingConnection.id}` : "closed";

  return (
    <>
      <ConnectionModal key={connectionModalKey} />
      <DeleteConnectionDialog key={deleteDialogKey} />
    </>
  );
}
