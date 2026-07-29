import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useErrorText } from "@/hooks/use-error-text";
import { formatSize } from "@/lib/format";
import {
  cleanCheckpointResidue,
  clearFinishedTransfers,
  type AppError,
  type CleanResult,
} from "@/lib/api";
import { useTransferStore } from "@/store/transfer-store";
import { Row } from "./shared";

export function AdvancedPane() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  // These one-shot result/error banners need no reset-on-close effect: the
  // pane unmounts with the modal, so React discards them for us.
  const [cleanResult, setCleanResult] = useState<CleanResult | null>(null);
  const [cleanError, setCleanError] = useState<AppError | null>(null);
  const [cleanPending, setCleanPending] = useState(false);
  const [clearError, setClearError] = useState<AppError | null>(null);
  const [clearPending, setClearPending] = useState(false);

  function handleCleanResidue() {
    setCleanError(null);
    setCleanResult(null);
    setCleanPending(true);
    cleanCheckpointResidue()
      .then((result) => setCleanResult(result))
      .catch((err: AppError) => setCleanError(err))
      .finally(() => setCleanPending(false));
  }

  // Drops the known-finished tasks locally from the shared transfer store,
  // the same pattern `TransferBar.handleClearFinished` uses -- so the
  // transfer panel reflects the clear immediately instead of waiting on a
  // `transfer://state` event that terminal tasks never re-emit.
  function handleClearHistory() {
    setClearError(null);
    setClearPending(true);
    clearFinishedTransfers()
      .then(() => {
        const { tasks, drop } = useTransferStore.getState();
        for (const [id, task] of Object.entries(tasks)) {
          if (task.status === "completed" || task.status === "canceled") drop(id);
        }
      })
      .catch((err: AppError) => setClearError(err))
      .finally(() => setClearPending(false));
  }

  return (
    <div>
      <Row
        label={
          <div>
            <div>{t("settings.cleanResidue")}</div>
            {cleanResult && (
              <div className="mt-0.5 text-[11.5px] text-muted-foreground">
                {t("settings.cleanResidueDone", {
                  count: cleanResult.removed,
                  size: formatSize(cleanResult.freed_bytes),
                })}
              </div>
            )}
            {cleanError && (
              <div className="mt-0.5 text-[11.5px] text-destructive">{errorText(cleanError)}</div>
            )}
          </div>
        }
      >
        <button
          type="button"
          onClick={handleCleanResidue}
          disabled={cleanPending}
          className="cursor-pointer rounded-lg border border-border px-[13px] py-[7px] text-[12.5px] font-medium text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60"
        >
          {t("settings.cleanResidue")}
        </button>
      </Row>
      <Row
        label={
          <div>
            <div>{t("settings.clearHistory")}</div>
            {clearError && (
              <div className="mt-0.5 text-[11.5px] text-destructive">{errorText(clearError)}</div>
            )}
          </div>
        }
      >
        <button
          type="button"
          onClick={handleClearHistory}
          disabled={clearPending}
          className="cursor-pointer rounded-lg border border-border px-[13px] py-[7px] text-[12.5px] font-medium text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60"
        >
          {t("settings.clearHistory")}
        </button>
      </Row>
    </div>
  );
}
