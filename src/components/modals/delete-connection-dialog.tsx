import { AlertTriangle, Loader2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Modal } from "@/components/ui/modal";
import { useDeleteConnection } from "@/hooks/use-connections";
import { useErrorText } from "@/hooks/use-error-text";
import { useApp } from "@/store/app-store";

/** Confirmation dialog for deleting a connection. `delete_connection` is
 * idempotent on the Rust side (always succeeds, even for an id that's
 * already gone), so there's no "not found" error path to handle here --
 * the only failure mode is a genuine IO/store error. On success, clears the
 * app's active connection/bucket/expanded state for this id if it was the
 * one being browsed (via `onConnectionDeleted`). */
export function DeleteConnectionDialog() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const { deletingConnection, closeDeleteConnection, onConnectionDeleted } = useApp();
  const deleteMutation = useDeleteConnection();

  if (!deletingConnection) return null;
  const conn = deletingConnection;

  function handleConfirm() {
    deleteMutation.mutate(conn.id, {
      onSuccess: () => {
        onConnectionDeleted(conn.id);
        closeDeleteConnection();
      },
    });
  }

  return (
    <Modal onClose={closeDeleteConnection} className="w-[420px]">
      <div className="flex items-start gap-3 px-5 pt-5 pb-1">
        <span className="flex size-9 shrink-0 items-center justify-center rounded-full bg-destructive/10 text-destructive">
          <AlertTriangle className="size-[18px]" />
        </span>
        <div className="min-w-0 flex-1 pt-1">
          <div className="text-[15px] font-bold">{t("conn.deleteConfirm")}</div>
          <p className="mt-1.5 text-[13px] text-fg2">
            {t("conn.deleteConfirmBody", { name: conn.name })}
          </p>
        </div>
      </div>
      {deleteMutation.isError && (
        <div className="px-5 pt-2 text-[12.5px] text-destructive">
          {errorText(deleteMutation.error)}
        </div>
      )}
      <div className="mt-3 flex items-center justify-end gap-2.5 border-t border-border2 px-5 py-4">
        <button
          type="button"
          onClick={closeDeleteConnection}
          disabled={deleteMutation.isPending}
          className="h-9 cursor-pointer rounded-[9px] border border-border bg-background px-4 text-[13px] font-medium text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60"
        >
          {t("addConn.cancel")}
        </button>
        <button
          type="button"
          onClick={handleConfirm}
          disabled={deleteMutation.isPending}
          className="inline-flex h-9 cursor-pointer items-center gap-[7px] rounded-[9px] bg-destructive px-4 text-[13px] font-semibold text-white hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-70"
        >
          {deleteMutation.isPending && <Loader2 className="size-3.5 animate-spin" />}
          {t("conn.delete")}
        </button>
      </div>
    </Modal>
  );
}
