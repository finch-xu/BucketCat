import { AlertTriangle } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Modal } from "@/components/ui/modal";
import type { UploadConflict } from "@/hooks/use-start-uploads";

const CANCEL_CLASS =
  "h-9 cursor-pointer rounded-[9px] border border-border bg-background px-4 text-[13px] font-medium text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60";
const PRIMARY_CLASS =
  "inline-flex h-9 cursor-pointer items-center gap-[7px] rounded-[9px] bg-primary px-4 text-[13px] font-semibold text-primary-foreground hover:bg-primary-strong disabled:cursor-not-allowed disabled:opacity-60";

/** Overwrite/skip prompt for an upload batch that collides with the current
 * folder's listing (see `useStartUploads`). Always mounted; renders nothing
 * while `conflict` is null. The caller keys this component per invocation
 * (same convention as `object-dialogs.tsx`'s `ObjectDialogs`) so a previous
 * invocation's collision list never flashes into a new one. */
export function UploadConflictDialog({
  conflict,
  onOverwrite,
  onSkip,
  onCancel,
}: {
  conflict: UploadConflict | null;
  onOverwrite: () => void;
  onSkip: () => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();

  if (!conflict) return null;

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
        <button type="button" onClick={onSkip} className={CANCEL_CLASS}>
          {t("upload.skip")}
        </button>
        <button type="button" onClick={onOverwrite} autoFocus className={PRIMARY_CLASS}>
          {t("upload.overwrite")}
        </button>
      </div>
    </Modal>
  );
}
