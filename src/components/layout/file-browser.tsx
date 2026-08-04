import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ComponentType,
  type ReactNode,
} from "react";
import {
  AlertTriangle,
  CornerLeftUp,
  Download,
  FolderDown,
  FolderOpen,
  Loader2,
  Pencil,
  Trash2,
  UploadCloud,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { useVirtualizer } from "@tanstack/react-virtual";
import { getCurrentWebview, type DragDropEvent } from "@tauri-apps/api/webview";
import logoIcon from "@/assets/logo-icon.png";
import { cn } from "@/lib/utils";
import type { ObjectEntry } from "@/lib/api";
import { basename, PARENT_ENTRY, withParentRow } from "@/lib/entries";
import { fileMeta } from "@/lib/file-meta";
import { extFromName, formatDate, formatSize } from "@/lib/format";
import { useBrowse, type BrowseQuery } from "@/hooks/use-browse";
import { useErrorText } from "@/hooks/use-error-text";
import { useStartDownloads } from "@/hooks/use-start-downloads";
import { useStartUploads } from "@/hooks/use-start-uploads";
import { useApp, type SelectMode } from "@/store/app-store";

const ROW_HEIGHT = 44;

/** Shift = range, cmd/ctrl = toggle, plain = single (Finder-style). */
function selectModeFromEvent(e: React.MouseEvent): SelectMode {
  if (e.shiftKey) return "range";
  if (e.metaKey || e.ctrlKey) return "toggle";
  return "single";
}

/** Shared empty/loading/error state. Takes either a lucide `icon` or an
 * `image`: the design lets the app icon itself carry exactly one state --
 * "pick a bucket" -- and leaves every other state on a neutral glyph, so
 * the logo stays an event rather than decoration. */
function CenterState({
  icon: Icon,
  image,
  spin = false,
  title,
  hint,
  action,
}: {
  icon?: ComponentType<{ className?: string }>;
  image?: string;
  spin?: boolean;
  title: string;
  hint: string;
  action?: ReactNode;
}) {
  return (
    <div className="flex h-full min-h-[340px] flex-col items-center justify-center gap-1.5 text-muted-foreground">
      {image ? (
        <img src={image} alt="" className="mb-1.5 size-[92px] rounded-[20px] opacity-90" />
      ) : (
        Icon && <Icon className={cn("mb-1.5 size-11 text-muted2", spin && "animate-spin")} />
      )}
      <p className="text-sm font-semibold text-fg2">{title}</p>
      {hint && <p className="max-w-[420px] text-center text-[12.5px]">{hint}</p>}
      {action}
    </div>
  );
}

function useEntryHandlers(orderedFileKeys: string[]) {
  const { selectKey, clearSelection, openFolder, gotoCrumb, path } = useApp();
  return (entry: ObjectEntry) => {
    // The ".." row: same double-click-to-navigate affordance as a real
    // folder, deliberately no single-click navigation either -- clicking it
    // only clears the selection, same as any other folder row. Checked by
    // reference, never by key: the sentinel key exists for React only.
    if (entry === PARENT_ENTRY) {
      return {
        onClick: () => clearSelection(),
        onDoubleClick: () => gotoCrumb(path.length - 2),
      };
    }
    return {
      onClick: (e: React.MouseEvent) => {
        if (entry.is_prefix) {
          clearSelection();
        } else {
          selectKey(entry.key, selectModeFromEvent(e), orderedFileKeys);
        }
      },
      onDoubleClick: () => {
        // Navigate by the entry's key, not its display name -- see the doc
        // comment on `openFolder` in the store for why this matters whenever
        // the current listing was reached via a search term.
        if (entry.is_prefix) openFolder(entry.key);
      },
    };
  };
}

/** Per-row hover actions, revealed on hover and kept keyboard-reachable via
 * `group-focus-within` (same affordance the sidebar's connection row uses).
 *
 * Files get download, rename and delete; folders get download and delete but
 * deliberately NO rename (renaming a prefix means copying every key beneath
 * it, which isn't wired yet). The file download queues a single-object
 * transfer and the folder download recurses the whole prefix -- both routed
 * through the shared `useStartDownloads` at the `FileBrowser` level, whose
 * handlers are threaded down here so one dialog/error surface serves every
 * row. The folder delete routes through the same delete dialog as files --
 * the dialog detects the trailing "/" and switches to the recursive path and
 * its own "folder and all its contents" confirmation copy. This is what
 * finally makes an *empty* in-app folder deletable (M3 left its zero-byte
 * marker unreachable from any UI gesture). */
function RowActions({
  entry,
  selected,
  onDownloadFile,
  onDownloadFolder,
}: {
  entry: ObjectEntry;
  selected: boolean;
  onDownloadFile: (entry: ObjectEntry) => void;
  onDownloadFolder: (entry: ObjectEntry) => void;
}) {
  const { t } = useTranslation();
  const { openRename, openDeleteObjects } = useApp();

  // A selected row keeps its actions on screen and floats them on a raised
  // surface; an unselected row still reveals them only on hover/focus.
  const container = cn(
    "flex w-[92px] shrink-0 items-center justify-end gap-0.5",
    !selected && "opacity-0 group-focus-within:opacity-100 group-hover:opacity-100",
  );
  const neutralAction = cn(
    "flex size-6 cursor-pointer items-center justify-center rounded-[6px] hover:bg-active hover:text-primary",
    selected ? "bg-raised text-primary shadow-[0_1px_2px_var(--shadow)]" : "text-muted-foreground",
  );
  const dangerAction = cn(
    "flex size-6 cursor-pointer items-center justify-center rounded-[6px] hover:bg-destructive/10 hover:text-destructive",
    selected
      ? "bg-raised text-destructive shadow-[0_1px_2px_var(--shadow)]"
      : "text-muted-foreground",
  );

  if (entry.is_prefix) {
    // Folders are never selected -- clicking one clears the selection -- so
    // this branch always renders the unselected styling in practice. It uses
    // the same class builders anyway so the two branches don't drift.
    return (
      <span className={container}>
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onDownloadFolder(entry);
          }}
          title={t("objects.downloadFolder")}
          aria-label={t("objects.downloadFolder")}
          className={neutralAction}
        >
          <FolderDown className="size-3.5" />
        </button>
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            openDeleteObjects([entry.key]);
          }}
          title={t("objects.delete")}
          aria-label={t("objects.delete")}
          className={dangerAction}
        >
          <Trash2 className="size-3.5" />
        </button>
      </span>
    );
  }

  return (
    <span className={container}>
      <button
        type="button"
        onClick={(e) => {
          e.stopPropagation();
          onDownloadFile(entry);
        }}
        title={t("objects.download")}
        aria-label={t("objects.download")}
        className={neutralAction}
      >
        <Download className="size-3.5" />
      </button>
      <button
        type="button"
        onClick={(e) => {
          e.stopPropagation();
          openRename(entry);
        }}
        title={t("objects.rename")}
        aria-label={t("objects.rename")}
        className={neutralAction}
      >
        <Pencil className="size-3.5" />
      </button>
      <button
        type="button"
        onClick={(e) => {
          e.stopPropagation();
          openDeleteObjects([entry.key]);
        }}
        title={t("objects.delete")}
        aria-label={t("objects.delete")}
        className={dangerAction}
      >
        <Trash2 className="size-3.5" />
      </button>
    </span>
  );
}

/** Batch action bar, visible only while objects are selected. Selection is
 * files-only (folder clicks clear it), so every selected key is a real object.
 * `entries` is passed in only to recover each object's display name: when a
 * selected key is still in the loaded listing we name its local file from
 * `entry.name` (exactly as single-file download does); when it is not -- a
 * selected key can outlive the listing, e.g. deleted elsewhere then a manual
 * Refresh re-fetches without clearing the selection -- we fall back to the
 * key's basename so the key is still queued rather than silently dropped. A
 * now-missing object then surfaces as a visible error, not a shorter batch. */
function SelectionBar({
  entries,
  onDownload,
}: {
  entries: ObjectEntry[];
  onDownload: (items: { key: string; name: string }[]) => void;
}) {
  const { t } = useTranslation();
  const { selectedKeys, clearSelection, openDeleteObjects } = useApp();

  if (selectedKeys.length === 0) return null;

  const nameByKey = new Map(entries.map((e) => [e.key, e.name]));
  const selectedItems = selectedKeys.map((key) => ({
    key,
    name: nameByKey.get(key) ?? basename(key),
  }));

  return (
    <div className="flex h-11 shrink-0 items-center justify-between gap-3 border-t border-border bg-background px-4">
      <span className="text-[12.5px] font-medium text-fg2">
        {t("objects.selectedCount", { count: selectedKeys.length })}
      </span>
      <span className="flex items-center gap-2">
        <button
          type="button"
          onClick={clearSelection}
          className="h-7 cursor-pointer rounded-[8px] border border-border bg-background px-3 text-[12.5px] font-medium text-fg2 hover:bg-hover"
        >
          {t("objects.clearSelection")}
        </button>
        <button
          type="button"
          onClick={() => onDownload(selectedItems)}
          className="inline-flex h-7 cursor-pointer items-center gap-1.5 rounded-[8px] bg-primary px-3 text-[12.5px] font-semibold text-primary-foreground hover:bg-primary-strong"
        >
          <Download className="size-3.5" />
          {t("objects.download")}
        </button>
        <button
          type="button"
          onClick={() => openDeleteObjects(selectedKeys)}
          className="inline-flex h-7 cursor-pointer items-center gap-1.5 rounded-[8px] bg-destructive/10 px-3 text-[12.5px] font-semibold text-destructive hover:bg-destructive/20"
        >
          <Trash2 className="size-3.5" />
          {t("objects.delete")}
        </button>
      </span>
    </div>
  );
}

function ListView({
  entries,
  query,
  orderedFileKeys,
  onDownloadFile,
  onDownloadFolder,
}: {
  entries: ObjectEntry[];
  query: BrowseQuery;
  orderedFileKeys: string[];
  onDownloadFile: (entry: ObjectEntry) => void;
  onDownloadFolder: (entry: ObjectEntry) => void;
}) {
  const { t } = useTranslation();
  const { selectedKeys } = useApp();
  const handlers = useEntryHandlers(orderedFileKeys);

  const parentRef = useRef<HTMLDivElement>(null);
  const rowVirtualizer = useVirtualizer({
    count: entries.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
  });
  const virtualItems = rowVirtualizer.getVirtualItems();

  const { hasNextPage, isFetchingNextPage, isPlaceholderData, fetchNextPage } = query;
  useEffect(() => {
    // Skip while `entries`/`hasNextPage` are still the *previous* location's
    // placeholder data (see `useObjects`) -- its continuation cursor
    // belongs to that other prefix, not this one, so fetching "next" here
    // would page the wrong listing.
    if (isPlaceholderData) return;
    const last = virtualItems[virtualItems.length - 1];
    if (!last) return;
    if (last.index >= entries.length - 10 && hasNextPage && !isFetchingNextPage) {
      fetchNextPage();
    }
  }, [virtualItems, entries.length, hasNextPage, isFetchingNextPage, isPlaceholderData, fetchNextPage]);

  return (
    <div ref={parentRef} className="min-w-0 flex-1 overflow-y-auto">
      <div className="sticky top-0 z-1 flex h-[34px] items-center border-b border-border2 bg-panel px-4 text-[11px] font-semibold tracking-[0.4px] text-muted-foreground">
        <span className="flex-1">{t("main.colName")}</span>
        <span className="w-24 text-right">{t("main.colSize")}</span>
        <span className="w-[120px] pl-5">{t("main.colType")}</span>
        <span className="w-[150px] pl-5">{t("main.colModified")}</span>
        <span className="w-[92px]" />
      </div>
      <div className="relative" style={{ height: rowVirtualizer.getTotalSize() }}>
        {virtualItems.map((vi) => {
          const entry = entries[vi.index];
          const isParent = entry === PARENT_ENTRY;
          const ext = entry.is_prefix ? "" : extFromName(entry.name);
          // The parent row has no file-type meaning, so it deliberately
          // never goes through `fileMeta` -- it gets its own icon below and
          // the type column shows "—" instead of a `meta.labelKey`.
          const meta = isParent ? null : fileMeta(entry.is_prefix ? "folder" : "file", ext);
          const Icon = isParent ? CornerLeftUp : meta!.icon;
          const selected = selectedKeys.includes(entry.key);
          return (
            <div
              key={entry.key}
              {...handlers(entry)}
              title={isParent ? t("main.goToParent") : undefined}
              className={cn(
                "group absolute inset-x-0 flex cursor-pointer items-center border-b border-border2 px-4",
                selected ? "bg-active" : "hover:bg-hover",
              )}
              style={{ height: vi.size, transform: `translateY(${vi.start}px)` }}
            >
              {/* Selection marker. The row is itself absolutely positioned,
               * which already makes it the containing block for this. */}
              {selected && <span className="absolute inset-y-0 left-0 w-[2px] bg-primary" />}
              <span className="flex min-w-0 flex-1 items-center gap-[11px]">
                <Icon
                  className="size-[18px] shrink-0"
                  style={isParent ? undefined : { color: meta!.color }}
                />
                <span
                  className={cn(
                    "truncate text-[13.5px] text-foreground",
                    entry.is_prefix ? "font-semibold" : "font-medium",
                  )}
                >
                  {entry.name}
                </span>
              </span>
              <span className="w-24 text-right text-[12.5px] text-muted-foreground tabular-nums">
                {entry.is_prefix ? "—" : formatSize(entry.size)}
              </span>
              <span className="w-[120px] truncate pl-5 text-[12.5px] text-muted-foreground">
                {isParent ? "—" : t(meta!.labelKey)}
              </span>
              <span className="w-[150px] pl-5 text-[12.5px] text-muted-foreground tabular-nums">
                {entry.is_prefix ? "—" : formatDate(entry.last_modified)}
              </span>
              {isParent ? (
                <span className="w-[92px] shrink-0" />
              ) : (
                <RowActions
                  entry={entry}
                  selected={selected}
                  onDownloadFile={onDownloadFile}
                  onDownloadFolder={onDownloadFolder}
                />
              )}
            </div>
          );
        })}
      </div>
      {isFetchingNextPage && (
        <div className="flex h-10 items-center justify-center gap-2 text-[12.5px] text-muted-foreground">
          <Loader2 className="size-3.5 animate-spin" />
          {t("main.loadingMore")}
        </div>
      )}
    </div>
  );
}

function GridView({
  entries,
  query,
  orderedFileKeys,
}: {
  entries: ObjectEntry[];
  query: BrowseQuery;
  orderedFileKeys: string[];
}) {
  const { t } = useTranslation();
  const { selectedKeys } = useApp();
  const handlers = useEntryHandlers(orderedFileKeys);
  const { hasNextPage, isFetchingNextPage, isPlaceholderData, fetchNextPage } = query;
  return (
    <div className="min-w-0 flex-1 overflow-y-auto">
      <div className="grid grid-cols-[repeat(auto-fill,minmax(148px,1fr))] gap-3.5 p-[18px]">
        {entries.map((entry) => {
          const isParent = entry === PARENT_ENTRY;
          const ext = entry.is_prefix ? "" : extFromName(entry.name);
          const meta = isParent ? null : fileMeta(entry.is_prefix ? "folder" : "file", ext);
          const Icon = isParent ? CornerLeftUp : meta!.icon;
          const selected = selectedKeys.includes(entry.key);
          return (
            <div
              key={entry.key}
              {...handlers(entry)}
              title={isParent ? t("main.goToParent") : undefined}
              className={cn(
                "flex cursor-pointer flex-col items-center gap-[11px] rounded-[13px] border px-3 pt-[18px] pb-3.5 hover:border-primary",
                selected ? "border-primary bg-active" : "border-border bg-background",
              )}
            >
              <span className="flex size-[58px] items-center justify-center rounded-[13px] bg-panel">
                <Icon
                  className="size-[30px]"
                  style={isParent ? undefined : { color: meta!.color }}
                />
              </span>
              <span className="max-w-full truncate text-center text-[12.5px] font-medium text-foreground">
                {entry.name}
              </span>
              <span className="text-[11px] text-muted-foreground">
                {entry.is_prefix ? "—" : formatSize(entry.size)}
              </span>
            </div>
          );
        })}
      </div>
      {/* Same stale-cursor guard as the list view's auto-fetch effect --
       * `hasNextPage` still describes the previous location while
       * `isPlaceholderData` is true, so the button stays hidden rather than
       * page the wrong listing. */}
      {hasNextPage && !isPlaceholderData && (
        <div className="flex justify-center pb-4">
          <button
            type="button"
            onClick={() => fetchNextPage()}
            disabled={isFetchingNextPage}
            className="inline-flex h-8 cursor-pointer items-center gap-1.5 rounded-[9px] border border-border bg-background px-3.5 text-[12.5px] font-medium text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60"
          >
            {isFetchingNextPage && <Loader2 className="size-3.5 animate-spin" />}
            {isFetchingNextPage ? t("main.loadingMore") : t("main.loadMore")}
          </button>
        </div>
      )}
    </div>
  );
}

/**
 * Subscribes to the webview's drag-drop stream and returns an unsubscribe
 * function that is safe to call at any point -- including while the
 * registration is still in flight.
 *
 * That cancellation flag is the whole point (same discipline as
 * `useTransferEvents`, which is where the pattern comes from). Registration
 * is an async IPC round trip, so an effect torn down before it resolves used
 * to leave `unlisten` undefined: cleanup no-oped and the listener registered
 * a moment later lived forever. `<React.StrictMode>` (see `src/main.tsx`)
 * double-invokes effects in dev -- setup, cleanup, setup -- so every dev
 * session reliably ended up with two live handlers and one drop uploaded
 * every file twice. Any leaked handler also keeps a stale `prefix` closure,
 * so the duplicate lands in whatever folder was browsed when it registered.
 *
 * Exported so the leak can be regression-tested directly; `FileBrowser` is
 * its only production caller.
 */
export function watchDragDrop(onEvent: (payload: DragDropEvent) => void): () => void {
  let cancelled = false;
  let unlisten: (() => void) | undefined;

  void (async () => {
    const off = await getCurrentWebview().onDragDropEvent((event) => onEvent(event.payload));
    // Torn down while the round trip was in flight -- undo the registration
    // immediately instead of leaking it past cleanup.
    if (cancelled) {
      off();
      return;
    }
    unlisten = off;
  })();

  return () => {
    cancelled = true;
    unlisten?.();
  };
}

export function FileBrowser() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const { activeBucket, path, view } = useApp();
  const { query, entries, searching } = useBrowse();

  // ".." only makes sense once inside a folder (the bucket root has no
  // parent to go to) and never while searching (a search listing's rows can
  // live under different parents, so a single "up" target isn't well
  // defined -- see `listPrefix`). `rows` is what actually reaches the row
  // grid; every other consumer below (the empty-state check, ordered file
  // keys for range-select, `SelectionBar`) deliberately keeps using the raw
  // `entries`, since none of them should ever see the synthetic row. One
  // consequence: an EMPTY folder still falls into the `entries.length === 0`
  // branch and shows the empty-state message rather than a lone ".." row --
  // the breadcrumb bar already covers "go back" there, so this is a
  // deliberate simplification, not an oversight.
  const showParent = path.length > 0 && !searching;
  const rows = useMemo(() => withParentRow(entries, showParent), [entries, showParent]);

  const { startUploads, guardReady, dialog } = useStartUploads();
  const { startFileDownload, startFolderDownload, startBatchDownload, dialog: downloadDialog } =
    useStartDownloads();
  const [dragging, setDragging] = useState(false);

  // `useCallback` so this effect's dependency actually holds still.
  // `FileBrowser` re-renders on every `useApp()` change (selection, path,
  // view, search) and every query transition; with an unstable handler the
  // listener was torn down and re-registered -- an async IPC round trip --
  // on each of those, and each re-registration racing a cleanup was another
  // chance to leak a handler.
  const onDragDrop = useCallback(
    (payload: DragDropEvent) => {
      // `enter` fires before the first `over`. Handling only `over` meant
      // the drop affordance appeared one event late, and (on a fast drag
      // straight to a drop) sometimes not at all.
      if (payload.type === "enter" || payload.type === "over") setDragging(true);
      else if (payload.type === "drop") {
        setDragging(false);
        startUploads(payload.paths);
      } else setDragging(false);
    },
    [startUploads],
  );

  // Tauri's own drag-drop event, not HTML5 dragover/drop: a `File` handed to
  // the WebView by an HTML5 drop carries no filesystem path, and the Rust
  // side needs a real path to stream the upload from.
  useEffect(() => watchDragDrop(onDragDrop), [onDragDrop]);

  // Computed once here and passed down instead of recomputed identically in
  // both ListView and GridView.
  const orderedFileKeys = useMemo(
    () => entries.filter((e) => !e.is_prefix).map((e) => e.key),
    [entries],
  );

  let body: ReactNode;
  // Empty-state semantics (M2 carried finding): no bucket selected is a
  // *placeholder*, never "this bucket is empty".
  if (!activeBucket) {
    body = (
      <CenterState
        image={logoIcon}
        title={t("main.selectBucketTitle")}
        hint={t("main.selectBucketHint")}
      />
    );
  } else if (query.isPending) {
    body = <CenterState icon={Loader2} spin title={t("main.loadingList")} hint="" />;
  } else if (query.isError) {
    body = (
      <CenterState
        icon={AlertTriangle}
        title={t("main.loadFailed")}
        hint={errorText(query.error)}
        action={
          <button
            type="button"
            onClick={() => query.refetch()}
            className="mt-2 inline-flex h-8 cursor-pointer items-center rounded-[9px] border border-border bg-background px-3.5 text-[12.5px] font-medium text-fg2 hover:bg-hover"
          >
            {t("main.retry")}
          </button>
        }
      />
    );
  } else if (entries.length === 0) {
    // `query.isPlaceholderData` (set by `useObjects`'s `keepPreviousData`)
    // means `entries` is still the *previous* location's listing, kept on
    // screen while this one's real page loads. An empty previous listing
    // says nothing about whether *this* folder is empty, so the settled
    // empty/no-match copy must wait for the real fetch -- otherwise a
    // placeholder carried over from an empty folder would flash "this
    // folder is empty" over whatever the new location actually turns out
    // to hold.
    if (query.isPlaceholderData) {
      body = <CenterState icon={Loader2} spin title={t("main.loadingList")} hint="" />;
    } else if (searching) {
      body = <CenterState icon={FolderOpen} title={t("main.noMatchTitle")} hint={t("main.noMatchHint")} />;
    } else {
      body = (
        <CenterState
          icon={FolderOpen}
          title={path.length === 0 ? t("main.emptyTitle") : t("main.emptyFolderTitle")}
          hint={t("main.emptyHint")}
        />
      );
    }
  } else {
    body = (
      <>
        {view === "list" ? (
          <ListView
            entries={rows}
            query={query}
            orderedFileKeys={orderedFileKeys}
            onDownloadFile={startFileDownload}
            onDownloadFolder={startFolderDownload}
          />
        ) : (
          <GridView entries={rows} query={query} orderedFileKeys={orderedFileKeys} />
        )}
        <SelectionBar entries={entries} onDownload={startBatchDownload} />
      </>
    );
  }

  return (
    <div className="relative flex min-w-0 flex-1 flex-col">
      {body}
      {dragging && activeBucket && (
        <div className="pointer-events-none absolute inset-2 z-20 flex flex-col items-center justify-center gap-2 rounded-2xl border-2 border-dashed border-primary bg-primary-soft text-primary">
          <UploadCloud className="size-8" />
          <span className="text-[13.5px] font-semibold">
            {guardReady ? t("upload.dropHint") : t("objects.checkingNames")}
          </span>
          {/* Names the destination folder. Skipped at the bucket root, where
           * a bare "/" would carry no information. */}
          {path.length > 0 && <span className="text-[12px] text-fg2">{`${path.join("/")}/`}</span>}
        </div>
      )}
      {dialog}
      {downloadDialog}
    </div>
  );
}
