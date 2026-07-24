import { AlertTriangle } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Modal } from "@/components/ui/modal";
import { useErrorText } from "@/hooks/use-error-text";
import type { UploadConflict, UploadNotice } from "@/hooks/use-start-uploads";

const CANCEL_CLASS =
  "h-9 cursor-pointer rounded-[9px] border border-border bg-background px-4 text-[13px] font-medium text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60";
const PRIMARY_CLASS =
  "inline-flex h-9 cursor-pointer items-center gap-[7px] rounded-[9px] bg-primary px-4 text-[13px] font-semibold text-primary-foreground hover:bg-primary-strong disabled:cursor-not-allowed disabled:opacity-60";
// Same danger styling the delete-objects dialog uses (`object-dialogs.tsx`).
// Overwriting an object is exactly as irreversible as deleting it, so it gets
// exactly as loud a button.
const DANGER_CLASS =
  "inline-flex h-9 cursor-pointer items-center gap-[7px] rounded-[9px] bg-destructive px-4 text-[13px] font-semibold text-white hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-70";

/** Overwrite/skip prompt for an upload batch that collides with the current
 * folder's listing (see `useStartUploads`). Rendered only while a conflict is
 * pending; the caller keys it per invocation (same convention as
 * `object-dialogs.tsx`'s `ObjectDialogs`) so a previous invocation's
 * collision list never flashes into a new one.
 *
 * Three exits, deliberately distinct: skip (upload only the non-colliding
 * files), cancel (abandon the whole batch), overwrite (destroy the listed
 * objects). Overwrite is styled as the destructive action and is NOT
 * autofocused -- Enter lands on "skip", the safe path, because there is no
 * undo for the alternative. Cancel exists as a real button rather than only
 * as Esc/backdrop: "skip these files" is not the same gesture as "I did not
 * mean to start this upload at all". */
export function UploadConflictDialog({
  conflict,
  onOverwrite,
  onSkip,
  onCancel,
}: {
  conflict: UploadConflict;
  onOverwrite: () => void;
  onSkip: () => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();

  return (
    <Modal onClose={onCancel} className="w-[460px]">
      <div className="flex items-start gap-3 px-5 pt-5 pb-1">
        <span className="flex size-9 shrink-0 items-center justify-center rounded-full bg-destructive/10 text-destructive">
          <AlertTriangle className="size-[18px]" />
        </span>
        <div className="min-w-0 flex-1 pt-1">
          <div className="text-[15px] font-bold">{t("upload.conflictTitle")}</div>
          <p className="mt-1.5 text-[13px] text-fg2">
            {t("upload.conflictBody", { count: conflict.collidingKeys.length })}
          </p>
          <ul className="mt-2.5 max-h-[180px] overflow-y-auto rounded-[9px] border border-border2 bg-panel p-2.5">
            {conflict.collidingKeys.map((key) => (
              <li key={key} className="truncate py-1 font-mono text-[12px] text-fg2">
                {key}
              </li>
            ))}
          </ul>
        </div>
      </div>
      <div className="mt-3 flex items-center justify-end gap-2.5 border-t border-border2 px-5 py-4">
        <button type="button" onClick={onCancel} className={CANCEL_CLASS}>
          {t("objects.cancel")}
        </button>
        <button type="button" onClick={onSkip} autoFocus className={PRIMARY_CLASS}>
          {t("upload.skip")}
        </button>
        <button type="button" onClick={onOverwrite} className={DANGER_CLASS}>
          {t("upload.overwrite")}
        </button>
      </div>
    </Modal>
  );
}

/** After-the-fact report for an upload gesture that did not do the obvious
 * thing: a rejected command, a partially-queued batch, or a drop refused
 * because the collision guard wasn't ready. Every one of these previously
 * ended in nothing at all appearing on screen.
 *
 * Dedicated `upload.*` copy for all three cases: `upload.skippedSummary` for
 * the queued/skipped counts (the delete dialog's `objects.partialSummary` is
 * about succeeded/failed deletes, not queued/skipped uploads -- reusing it
 * misdescribed what happened) and `upload.notReadyBody` for the refused drop
 * (`objects.checkingNames` is a fragment meant to sit next to other UI, not
 * read as a whole sentence on its own). */
export function UploadNoticeDialog({
  notice,
  onClose,
}: {
  notice: UploadNotice;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const errorText = useErrorText();

  let body: string;
  if (notice.kind === "error") body = errorText(notice.error);
  else if (notice.kind === "skipped") {
    body = t("upload.skippedSummary", { queued: notice.queued, skipped: notice.skipped });
  } else body = t("upload.notReadyBody");

  return (
    <Modal onClose={onClose} className="w-[420px]">
      <div className="flex items-start gap-3 px-5 pt-5 pb-1">
        <span className="flex size-9 shrink-0 items-center justify-center rounded-full bg-destructive/10 text-destructive">
          <AlertTriangle className="size-[18px]" />
        </span>
        <div className="min-w-0 flex-1 pt-1">
          <div className="text-[15px] font-bold">{t("upload.noticeTitle")}</div>
          <p className="mt-1.5 text-[13px] text-fg2">{body}</p>
        </div>
      </div>
      <div className="mt-3 flex items-center justify-end gap-2.5 border-t border-border2 px-5 py-4">
        <button type="button" onClick={onClose} autoFocus className={PRIMARY_CLASS}>
          {t("objects.close")}
        </button>
      </div>
    </Modal>
  );
}
