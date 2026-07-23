import { ArrowUpDown, ChevronDown, Download, Pause, Play, RotateCcw, Upload, X } from "lucide-react";
import { memo, useState } from "react";
import { useTranslation } from "react-i18next";
import { cn } from "@/lib/utils";
import { useErrorText } from "@/hooks/use-error-text";
import { formatEta, formatSize, formatSpeed } from "@/lib/format";
import {
  cancelTransfer,
  clearFinishedTransfers,
  pauseTransfer,
  resumeTransfer,
  retryTransfer,
  type AppError,
} from "@/lib/api";
import {
  useTransferOrder,
  useTransferPanelOpen,
  useTransferStore,
  useTransferSummary,
  useTransferTask,
  type LiveTransfer,
} from "@/store/transfer-store";

const DONE_COLOR = "#4bb39a";

function colorFor(status: LiveTransfer["status"]): string {
  if (status === "completed") return DONE_COLOR;
  if (status === "failed") return "var(--destructive)";
  if (status === "canceled") return "var(--muted2)";
  return "var(--primary)";
}

/** Small icon-only action button shared by every row's button cluster. */
function RowButton({
  onClick,
  title,
  danger,
  children,
}: {
  onClick: () => void;
  title: string;
  danger?: boolean;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      aria-label={title}
      className={cn(
        "flex size-[26px] shrink-0 cursor-pointer items-center justify-center rounded-[7px] text-muted-foreground hover:bg-hover hover:text-fg2",
        danger && "hover:bg-destructive/10 hover:text-destructive",
      )}
    >
      {children}
    </button>
  );
}

/** One transfer row. Subscribes to exactly its own task via
 * `useTransferTask(taskId)` -- nothing else in the store -- so a progress
 * tick on task A never re-renders task B's row (or the panel header, or the
 * rest of the app). Renders nothing if the task has been dropped out from
 * under it (see `useTransferTask`'s doc comment).
 *
 * Wrapped in `memo` below: `TransferBar` recreates every `<TransferRow>`
 * element on each progress tick (it re-renders at ~6.7Hz), and without this
 * every row would re-render regardless of whether its own `taskId` prop
 * actually changed. The prop is a single string, so the default shallow
 * comparison is exactly what's needed. */
function TransferRowImpl({ taskId }: { taskId: string }) {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const task = useTransferTask(taskId);
  const [actionError, setActionError] = useState<AppError | null>(null);
  if (!task) return null;

  const DirIcon = task.direction === "upload" ? Upload : Download;
  const color = colorFor(task.status);
  const pct =
    task.status === "completed" ? 100 : task.total > 0 ? Math.round((task.transferred / task.total) * 100) : 0;

  let subtitle: string;
  switch (task.status) {
    case "running": {
      subtitle = formatSpeed(task.speed);
      if (task.eta_secs !== null) {
        subtitle += ` · ${t("transfer.remaining", { eta: formatEta(task.eta_secs) })}`;
      }
      break;
    }
    case "queued":
      subtitle = t("transfer.queued");
      break;
    case "paused":
      subtitle = t("transfer.paused");
      break;
    case "failed":
      subtitle = errorText({ code: task.error_code ?? "internal", params: {} });
      break;
    case "canceled":
      subtitle = t("transfer.canceled");
      break;
    default:
      subtitle = `${t("transfer.done")} · ${formatSize(task.total)}`;
  }

  // Imperative IPC calls. A successful call's status change comes back
  // through `transfer://state` and lands in this same row via the store, not
  // through any local state here -- `actionError` only ever holds a
  // *rejection*, cleared at the start of the next attempt.
  function runAction(command: (id: string) => Promise<void>) {
    setActionError(null);
    command(taskId).catch((error: AppError) => setActionError(error));
  }

  const actions = {
    pause: () => runAction(pauseTransfer),
    resume: () => runAction(resumeTransfer),
    cancel: () => runAction(cancelTransfer),
    retry: () => runAction(retryTransfer),
    dismiss: () => useTransferStore.getState().drop(taskId),
  };

  return (
    <div className="flex items-center gap-[11px] rounded-[10px] px-2 py-[9px] hover:bg-hover">
      <span className="flex size-[30px] shrink-0 items-center justify-center rounded-lg border border-border bg-panel">
        <DirIcon className="size-[15px]" style={{ color }} />
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex justify-between gap-2">
          <span className="truncate text-[12.5px] font-medium">{task.file_name}</span>
          <span className="shrink-0 text-[11px] text-muted-foreground tabular-nums">
            {formatSize(task.total)}
          </span>
        </div>
        <div className="mt-1.5 flex items-center gap-2">
          <div className="h-[5px] flex-1 overflow-hidden rounded-[3px] bg-border2">
            <div
              className="h-full rounded-[3px] transition-[width] duration-400"
              style={{ background: color, width: `${pct}%` }}
            />
          </div>
          <span className="w-[34px] text-right text-[11px] text-muted-foreground tabular-nums">
            {pct}%
          </span>
        </div>
        <div
          className={cn(
            "mt-1 truncate text-[10.5px]",
            task.status === "failed" ? "text-destructive" : "text-muted2",
          )}
        >
          {subtitle}
        </div>
        {actionError && (
          <div role="alert" className="mt-0.5 truncate text-[10.5px] text-destructive">
            {errorText(actionError)}
          </div>
        )}
      </div>
      <span className="flex shrink-0 items-center gap-0.5">
        {task.status === "running" && (
          <>
            <RowButton onClick={actions.pause} title={t("transfer.pause")}>
              <Pause className="size-3.5" />
            </RowButton>
            <RowButton onClick={actions.cancel} title={t("transfer.cancel")} danger>
              <X className="size-3.5" />
            </RowButton>
          </>
        )}
        {task.status === "queued" && (
          <RowButton onClick={actions.cancel} title={t("transfer.cancel")} danger>
            <X className="size-3.5" />
          </RowButton>
        )}
        {task.status === "paused" && (
          <>
            <RowButton onClick={actions.resume} title={t("transfer.resume")}>
              <Play className="size-3.5" />
            </RowButton>
            <RowButton onClick={actions.cancel} title={t("transfer.cancel")} danger>
              <X className="size-3.5" />
            </RowButton>
          </>
        )}
        {task.status === "failed" && (
          <>
            <RowButton onClick={actions.retry} title={t("transfer.retry")}>
              <RotateCcw className="size-3.5" />
            </RowButton>
            <RowButton onClick={actions.cancel} title={t("transfer.cancel")} danger>
              <X className="size-3.5" />
            </RowButton>
          </>
        )}
        {(task.status === "completed" || task.status === "canceled") && (
          <RowButton onClick={actions.dismiss} title={t("transfer.dismiss")}>
            <X className="size-3.5" />
          </RowButton>
        )}
      </span>
    </div>
  );
}

const TransferRow = memo(TransferRowImpl);

export function TransferBar() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  // The point of this component: it subscribes to the summary, the id
  // order, and the panel's open flag -- never the `tasks` map itself.
  // Progress arrives at ~6.7Hz per running task; if this subscribed
  // broadly, every tick would re-render the whole app (including the
  // virtualized object list) instead of just the one row that changed.
  const summary = useTransferSummary();
  const order = useTransferOrder();
  const panelOpen = useTransferPanelOpen();
  const [clearError, setClearError] = useState<AppError | null>(null);

  const summaryLine = summary.activeCount
    ? t("transfer.activeCount", { count: summary.activeCount })
    : t("transfer.idle");

  // Drops the known-finished tasks locally instead of re-fetching a
  // `listTransfers()` snapshot and replacing the whole store with it. A
  // `transfer://state` event can land on some *other* task while that round
  // trip is in flight; replacing wholesale would clobber it with the older
  // snapshot, and since `completed`/`canceled` are terminal the backend
  // never re-emits, so the row would be stuck forever (the same hazard
  // `useTransferEvents` already guards against for the initial snapshot).
  // Dropping locally needs no snapshot at all: "finished" here means the
  // exact same `completed`/`canceled` statuses the backend command clears,
  // so the store already has everything needed to compute it.
  function handleClearFinished() {
    setClearError(null);
    clearFinishedTransfers()
      .then(() => {
        const { tasks, drop } = useTransferStore.getState();
        for (const [id, task] of Object.entries(tasks)) {
          if (task.status === "completed" || task.status === "canceled") drop(id);
        }
      })
      .catch((error: AppError) => setClearError(error));
  }

  return (
    <div className="relative shrink-0">
      {panelOpen && (
        <div className="absolute bottom-full left-0 right-0 max-h-[284px] overflow-y-auto border-t border-border bg-background shadow-[0_-14px_32px_-14px_var(--shadow)] [animation:bc-up_.18s_ease]">
          <div className="sticky top-0 flex items-center justify-between border-b border-border2 bg-background px-3.5 pt-[11px] pb-[7px]">
            <span className="text-xs font-semibold text-fg2">{t("transfer.title")}</span>
            <button
              type="button"
              onClick={handleClearFinished}
              className="cursor-pointer rounded-[7px] px-2 py-1 text-[11px] text-muted-foreground hover:bg-hover hover:text-fg2"
            >
              {t("transfer.clearFinished")}
            </button>
          </div>
          {clearError && (
            <div role="alert" className="border-b border-border2 px-3.5 py-1.5 text-[11px] text-destructive">
              {errorText(clearError)}
            </div>
          )}
          {order.length === 0 ? (
            <div className="px-3.5 py-6 text-center text-[12px] text-muted-foreground">
              {t("transfer.idle")}
            </div>
          ) : (
            <div className="px-2 pt-1.5 pb-2">
              {order.map((id) => (
                <TransferRow key={id} taskId={id} />
              ))}
            </div>
          )}
        </div>
      )}
      <footer className="flex h-10 items-center gap-2.5 border-t border-border bg-titlebar px-3.5 text-xs text-muted-foreground">
        <ArrowUpDown
          className={cn("size-[13px]", summary.activeCount ? "text-primary" : "text-muted-foreground")}
        />
        <span className="text-fg2">{summaryLine}</span>
        {summary.activeCount > 0 && (
          <>
            <div className="h-[5px] w-[120px] overflow-hidden rounded-[3px] bg-border2">
              <div
                className="h-full rounded-[3px] bg-primary transition-[width] duration-400"
                style={{ width: `${summary.pct}%` }}
              />
            </div>
            <span className="text-fg2 tabular-nums">{summary.pct}%</span>
            <span className="text-muted2 tabular-nums">{formatSpeed(summary.speed)}</span>
          </>
        )}
        <button
          type="button"
          onClick={() => useTransferStore.getState().togglePanel()}
          className="ml-auto flex cursor-pointer items-center gap-1.5 rounded-[7px] px-[9px] py-[5px] text-xs text-muted-foreground hover:bg-hover hover:text-fg2"
        >
          {t("transfer.toggle")}
          <ChevronDown
            className={cn("size-3.5 transition-transform duration-200", panelOpen && "rotate-180")}
          />
        </button>
      </footer>
    </div>
  );
}
