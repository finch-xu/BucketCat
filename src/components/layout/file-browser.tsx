import { useEffect, useMemo, useRef, type ComponentType, type ReactNode } from "react";
import {
  AlertTriangle,
  FolderOpen,
  Loader2,
  MousePointerClick,
  Pencil,
  Trash2,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "@/lib/utils";
import type { ObjectEntry } from "@/lib/api";
import { fileMeta } from "@/lib/file-meta";
import { extFromName, formatDate, formatSize } from "@/lib/format";
import { useBrowse, type BrowseQuery } from "@/hooks/use-browse";
import { useErrorText } from "@/hooks/use-error-text";
import { useApp, type SelectMode } from "@/store/app-store";

const ROW_HEIGHT = 44;

/** Shift = range, cmd/ctrl = toggle, plain = single (Finder-style). */
function selectModeFromEvent(e: React.MouseEvent): SelectMode {
  if (e.shiftKey) return "range";
  if (e.metaKey || e.ctrlKey) return "toggle";
  return "single";
}

function CenterState({
  icon: Icon,
  spin = false,
  title,
  hint,
  action,
}: {
  icon: ComponentType<{ className?: string }>;
  spin?: boolean;
  title: string;
  hint: string;
  action?: ReactNode;
}) {
  return (
    <div className="flex h-full min-h-[340px] flex-col items-center justify-center gap-1.5 text-muted-foreground">
      <Icon className={cn("mb-1.5 size-11 text-muted2", spin && "animate-spin")} />
      <p className="text-sm font-semibold text-fg2">{title}</p>
      {hint && <p className="max-w-[420px] text-center text-[12.5px]">{hint}</p>}
      {action}
    </div>
  );
}

function useEntryHandlers(orderedFileKeys: string[]) {
  const { selectKey, clearSelection, openFolder } = useApp();
  return (entry: ObjectEntry) => ({
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
  });
}

/** Per-row hover actions, revealed on hover and kept keyboard-reachable via
 * `group-focus-within` (same affordance the sidebar's connection row uses).
 *
 * Folders deliberately get no actions: renaming a prefix means copying every
 * key underneath it and deleting one means a recursive delete -- both need
 * M4's batch engine, and offering a button that silently touched only the
 * zero-byte marker object would be worse than offering nothing. */
function RowActions({ entry }: { entry: ObjectEntry }) {
  const { t } = useTranslation();
  const { openRename, openDeleteObjects } = useApp();

  if (entry.is_prefix) return <span className="w-[64px] shrink-0" />;

  return (
    <span className="flex w-[64px] shrink-0 items-center justify-end gap-0.5 opacity-0 group-focus-within:opacity-100 group-hover:opacity-100">
      <button
        type="button"
        onClick={(e) => {
          e.stopPropagation();
          openRename(entry);
        }}
        title={t("objects.rename")}
        aria-label={t("objects.rename")}
        className="flex size-6 cursor-pointer items-center justify-center rounded-[6px] text-muted-foreground hover:bg-active hover:text-primary"
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
        className="flex size-6 cursor-pointer items-center justify-center rounded-[6px] text-muted-foreground hover:bg-destructive/10 hover:text-destructive"
      >
        <Trash2 className="size-3.5" />
      </button>
    </span>
  );
}

/** Batch action bar, visible only while objects are selected. */
function SelectionBar() {
  const { t } = useTranslation();
  const { selectedKeys, clearSelection, openDeleteObjects } = useApp();

  if (selectedKeys.length === 0) return null;

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
}: {
  entries: ObjectEntry[];
  query: BrowseQuery;
  orderedFileKeys: string[];
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
        <span className="w-[64px]" />
      </div>
      <div className="relative" style={{ height: rowVirtualizer.getTotalSize() }}>
        {virtualItems.map((vi) => {
          const entry = entries[vi.index];
          const ext = entry.is_prefix ? "" : extFromName(entry.name);
          const meta = fileMeta(entry.is_prefix ? "folder" : "file", ext);
          const Icon = meta.icon;
          const selected = selectedKeys.includes(entry.key);
          return (
            <div
              key={entry.key}
              {...handlers(entry)}
              className={cn(
                "group absolute inset-x-0 flex cursor-pointer items-center border-b border-border2 px-4",
                selected ? "bg-active" : "hover:bg-hover",
              )}
              style={{ height: vi.size, transform: `translateY(${vi.start}px)` }}
            >
              <span className="flex min-w-0 flex-1 items-center gap-[11px]">
                <Icon className="size-[18px] shrink-0" style={{ color: meta.color }} />
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
                {t(meta.labelKey)}
              </span>
              <span className="w-[150px] pl-5 text-[12.5px] text-muted-foreground tabular-nums">
                {entry.is_prefix ? "—" : formatDate(entry.last_modified)}
              </span>
              <RowActions entry={entry} />
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
          const ext = entry.is_prefix ? "" : extFromName(entry.name);
          const meta = fileMeta(entry.is_prefix ? "folder" : "file", ext);
          const Icon = meta.icon;
          const selected = selectedKeys.includes(entry.key);
          return (
            <div
              key={entry.key}
              {...handlers(entry)}
              className={cn(
                "flex cursor-pointer flex-col items-center gap-[11px] rounded-[13px] border px-3 pt-[18px] pb-3.5 hover:border-primary",
                selected ? "border-primary bg-active" : "border-border bg-background",
              )}
            >
              <span className="flex size-[58px] items-center justify-center rounded-[13px] bg-panel">
                <Icon className="size-[30px]" style={{ color: meta.color }} />
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

export function FileBrowser() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const { activeBucket, path, view } = useApp();
  const { query, entries, searching } = useBrowse();
  // Computed once here and passed down instead of recomputed identically in
  // both ListView and GridView.
  const orderedFileKeys = useMemo(
    () => entries.filter((e) => !e.is_prefix).map((e) => e.key),
    [entries],
  );

  // Empty-state semantics (M2 carried finding): no bucket selected is a
  // *placeholder*, never "this bucket is empty".
  if (!activeBucket) {
    return (
      <div className="flex min-w-0 flex-1 flex-col">
        <CenterState
          icon={MousePointerClick}
          title={t("main.selectBucketTitle")}
          hint={t("main.selectBucketHint")}
        />
      </div>
    );
  }

  if (query.isPending) {
    return (
      <div className="flex min-w-0 flex-1 flex-col">
        <CenterState icon={Loader2} spin title={t("main.loadingList")} hint="" />
      </div>
    );
  }

  if (query.isError) {
    return (
      <div className="flex min-w-0 flex-1 flex-col">
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
      </div>
    );
  }

  if (entries.length === 0) {
    // `query.isPlaceholderData` (set by `useObjects`'s `keepPreviousData`)
    // means `entries` is still the *previous* location's listing, kept on
    // screen while this one's real page loads. An empty previous listing
    // says nothing about whether *this* folder is empty, so the settled
    // empty/no-match copy must wait for the real fetch -- otherwise a
    // placeholder carried over from an empty folder would flash "this
    // folder is empty" over whatever the new location actually turns out
    // to hold.
    if (query.isPlaceholderData) {
      return (
        <div className="flex min-w-0 flex-1 flex-col">
          <CenterState icon={Loader2} spin title={t("main.loadingList")} hint="" />
        </div>
      );
    }
    return (
      <div className="flex min-w-0 flex-1 flex-col">
        {searching ? (
          <CenterState icon={FolderOpen} title={t("main.noMatchTitle")} hint={t("main.noMatchHint")} />
        ) : (
          <CenterState
            icon={FolderOpen}
            title={path.length === 0 ? t("main.emptyTitle") : t("main.emptyFolderTitle")}
            hint={t("main.emptyHint")}
          />
        )}
      </div>
    );
  }

  return (
    <div className="flex min-w-0 flex-1 flex-col">
      {view === "list" ? (
        <ListView entries={entries} query={query} orderedFileKeys={orderedFileKeys} />
      ) : (
        <GridView entries={entries} query={query} orderedFileKeys={orderedFileKeys} />
      )}
      <SelectionBar />
    </div>
  );
}
