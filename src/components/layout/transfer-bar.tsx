import { ArrowUpDown, ChevronDown, Download, Pause, Play, RotateCcw, Upload, X } from "lucide-react";
import { memo, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { cn } from "@/lib/utils";
import { useErrorText } from "@/hooks/use-error-text";
import { useConnections } from "@/hooks/use-connections";
import { formatEta, formatSize, formatSpeed } from "@/lib/format";
import { providerMeta } from "@/lib/providers";
import { colorFor, hasRetryNotice, isThrottled, provenanceTitle, subtitleTone } from "@/lib/transfer-row";
import { ProviderGlyph } from "@/components/icons/provider-chip";
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
} from "@/store/transfer-store";

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
  // React-query cache, shared with the sidebar. Connection add/delete are rare
  // enough that this re-rendering every row in the panel is fine -- not worth
  // the extra machinery to scope it down (see task brief).
  const connections = useConnections();
  const [actionError, setActionError] = useState<AppError | null>(null);
  if (!task) return null;

  const conn = connections.data?.find((c) => c.id === task.connection_id);
  const meta = providerMeta(conn?.provider ?? "generic");

  const DirIcon = task.direction === "upload" ? Upload : Download;
  const color = colorFor(task);
  const tone = subtitleTone(task);
  // In-flight rows get the accent surface; terminal ones (done, failed,
  // canceled) fall back to neutral so the panel reads as "these are still
  // moving" at a glance.
  const inFlight =
    task.status === "running" || task.status === "queued" || task.status === "paused";
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
      subtitle = isThrottled(task)
        ? t("transfer.throttledPaused")
        : errorText({ code: task.error_code ?? "internal", params: task.error_params ?? {} });
      break;
    case "canceled":
      subtitle = t("transfer.canceled");
      break;
    default:
      subtitle = `${t("transfer.done")} · ${formatSize(task.total)}`;
  }
  // Notice only ever shows up while `status === "running"` (see
  // `TransferTask.notice`), so this can never clobber the branches above.
  if (hasRetryNotice(task) && task.notice) {
    subtitle = `${t("transfer.retrying", { attempt: task.notice.attempt, max: task.notice.max })} · ${subtitle}`;
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
      <span
        className={cn(
          "flex size-[30px] shrink-0 items-center justify-center rounded-lg border",
          inFlight ? "border-accent-border bg-accent-tint" : "border-border bg-panel",
        )}
      >
        <DirIcon className="size-[15px]" style={{ color }} />
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex justify-between gap-2">
          <span className="truncate text-[12.5px] font-medium">{task.file_name}</span>
          <span className="shrink-0 text-[11px] text-muted-foreground tabular-nums">
            {formatSize(task.total)}
          </span>
        </div>
        <div
          className="mt-0.5 flex min-w-0 items-center gap-1 text-[10.5px] text-muted2"
          title={provenanceTitle(conn?.name, task.connection_id, task.bucket)}
        >
          <ProviderGlyph meta={meta} className="size-3 shrink-0" />
          <span className="truncate">
            {conn?.name ?? t("transfer.deletedConnection")} · {task.bucket}
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
            tone === "warning" && "text-warning",
            tone === "destructive" && "text-destructive",
            tone === "muted" && "text-muted2",
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
  // Wraps both the panel and the footer (incl. the Toggle button), so
  // `contains(e.target)` naturally treats a click anywhere in either as
  // "inside" -- no separate check needed to keep the Toggle button from
  // reopening the panel it just closed.
  const containerRef = useRef<HTMLDivElement>(null);

  // Auto-collapse the panel on an outside click or Escape while it's open.
  // Known and accepted, not worked around: with a Modal open at the same
  // time, clicking its overlay also closes the panel (it's still a click
  // outside the panel, which should close it), and Escape closes both the
  // Modal and the panel (Escape means "dismiss whatever's showing").
  useEffect(() => {
    if (!panelOpen) return;

    // pointerdown, not click: a DOM `click` fires on the common ancestor of
    // mousedown/mouseup, so dragging to select text inside the panel and
    // releasing outside its bounds would misfire as an outside click (see
    // modal.tsx's `dismissOnOverlayClick` comment for the bug this caused
    // there).
    const onPointerDown = (e: PointerEvent) => {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        useTransferStore.getState().setPanelOpen(false);
      }
    };
    // Same `defaultPrevented` convention as modal.tsx: skip an Escape some
    // other layer (e.g. a Radix `DismissableLayer`) already consumed, and
    // never call `preventDefault()` ourselves -- doing so would swallow the
    // Escape for a Modal open at the same time.
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !e.defaultPrevented) {
        useTransferStore.getState().setPanelOpen(false);
      }
    };

    document.addEventListener("pointerdown", onPointerDown);
    window.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      window.removeEventListener("keydown", onKeyDown);
    };
  }, [panelOpen]);

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
    <div ref={containerRef} className="relative shrink-0">
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
      <footer className="border-t border-border bg-titlebar">
        <button
          type="button"
          aria-expanded={panelOpen}
          onClick={() => useTransferStore.getState().togglePanel()}
          className="group flex h-10 w-full cursor-pointer items-center gap-2.5 px-3.5 text-xs text-muted-foreground hover:bg-hover"
        >
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
          <span className="ml-auto flex items-center gap-1.5 rounded-[7px] px-[9px] py-[5px] text-xs text-muted-foreground group-hover:text-fg2">
            {t("transfer.toggle")}
            <ChevronDown
              className={cn("size-3.5 transition-transform duration-200", panelOpen && "rotate-180")}
            />
          </span>
        </button>
      </footer>
    </div>
  );
}
