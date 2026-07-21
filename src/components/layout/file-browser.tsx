import { FolderOpen, MoreHorizontal } from "lucide-react";
import { useTranslation } from "react-i18next";
import { cn } from "@/lib/utils";
import { fileMeta } from "@/lib/file-meta";
import type { ObjectEntry } from "@/lib/mock-data";
import { useApp } from "@/store/app-store";

function EmptyState() {
  const { t } = useTranslation();
  const { rawEntries } = useApp();
  const filteredOut = rawEntries.length > 0;
  return (
    <div className="flex h-full min-h-[340px] flex-col items-center justify-center gap-1.5 text-muted-foreground">
      <FolderOpen className="mb-1.5 size-11 text-muted2" />
      <p className="text-sm font-semibold text-fg2">
        {filteredOut ? t("main.noMatchTitle") : t("main.emptyTitle")}
      </p>
      <p className="text-[12.5px]">{filteredOut ? t("main.noMatchHint") : t("main.emptyHint")}</p>
    </div>
  );
}

function useEntryHandlers() {
  const { selectEntry, openFolder } = useApp();
  return (e: ObjectEntry) => ({
    onClick: () => selectEntry(e.name),
    onDoubleClick: () => {
      if (e.kind === "folder") openFolder(e.name);
    },
  });
}

function ListView({ entries }: { entries: ObjectEntry[] }) {
  const { t } = useTranslation();
  const { selected } = useApp();
  const handlers = useEntryHandlers();
  return (
    <>
      <div className="sticky top-0 z-1 flex h-[34px] items-center border-b border-border2 bg-panel px-4 text-[11px] font-semibold tracking-[0.4px] text-muted-foreground">
        <span className="flex-1">{t("main.colName")}</span>
        <span className="w-24 text-right">{t("main.colSize")}</span>
        <span className="w-[120px] pl-5">{t("main.colType")}</span>
        <span className="w-[150px] pl-5">{t("main.colModified")}</span>
        <span className="w-[30px]" />
      </div>
      {entries.map((e) => {
        const meta = fileMeta(e.kind, e.ext);
        const Icon = meta.icon;
        const isFolder = e.kind === "folder";
        return (
          <div
            key={e.name}
            {...handlers(e)}
            className={cn(
              "flex h-11 cursor-pointer items-center border-b border-border2 px-4",
              selected === e.name ? "bg-active" : "hover:bg-hover",
            )}
          >
            <span className="flex min-w-0 flex-1 items-center gap-[11px]">
              <Icon className="size-[18px] shrink-0" style={{ color: meta.color }} />
              <span
                className={cn(
                  "truncate text-[13.5px] text-foreground",
                  isFolder ? "font-semibold" : "font-medium",
                )}
              >
                {e.name}
              </span>
            </span>
            <span className="w-24 text-right text-[12.5px] text-muted-foreground tabular-nums">
              {isFolder ? "—" : e.size}
            </span>
            <span className="w-[120px] truncate pl-5 text-[12.5px] text-muted-foreground">
              {t(meta.labelKey)}
            </span>
            <span className="w-[150px] pl-5 text-[12.5px] text-muted-foreground tabular-nums">
              {e.modified}
            </span>
            <span className="flex w-[30px] justify-end text-muted2">
              <MoreHorizontal className="size-4" />
            </span>
          </div>
        );
      })}
    </>
  );
}

function GridView({ entries }: { entries: ObjectEntry[] }) {
  const { selected } = useApp();
  const handlers = useEntryHandlers();
  return (
    <div className="grid grid-cols-[repeat(auto-fill,minmax(148px,1fr))] gap-3.5 p-[18px]">
      {entries.map((e) => {
        const meta = fileMeta(e.kind, e.ext);
        const Icon = meta.icon;
        const isSelected = selected === e.name;
        return (
          <div
            key={e.name}
            {...handlers(e)}
            className={cn(
              "flex cursor-pointer flex-col items-center gap-[11px] rounded-[13px] border px-3 pt-[18px] pb-3.5 hover:border-primary",
              isSelected ? "border-primary bg-active" : "border-border bg-background",
            )}
          >
            <span className="flex size-[58px] items-center justify-center rounded-[13px] bg-panel">
              <Icon className="size-[30px]" style={{ color: meta.color }} />
            </span>
            <span className="max-w-full truncate text-center text-[12.5px] font-medium text-foreground">
              {e.name}
            </span>
            <span className="text-[11px] text-muted-foreground">
              {e.kind === "folder" ? "—" : e.size}
            </span>
          </div>
        );
      })}
    </div>
  );
}

export function FileBrowser() {
  const { entries, view } = useApp();
  return (
    <div className="min-w-0 flex-1 overflow-y-auto">
      {entries.length === 0 ? (
        <EmptyState />
      ) : view === "list" ? (
        <ListView entries={entries} />
      ) : (
        <GridView entries={entries} />
      )}
    </div>
  );
}
