import {
  FolderPlus,
  LayoutGrid,
  List,
  Loader2,
  RefreshCw,
  Search,
  Upload,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { useQueryClient } from "@tanstack/react-query";
import { open } from "@tauri-apps/plugin-dialog";
import { cn } from "@/lib/utils";
import { objectsRootKey } from "@/hooks/use-objects";
import { useStartUploads } from "@/hooks/use-start-uploads";
import { useConnections } from "@/hooks/use-connections";
import { providerMeta } from "@/lib/providers";
import { useApp } from "@/store/app-store";

export function Toolbar() {
  const { t } = useTranslation();
  const { activeConn, activeBucket, search, setSearch, view, setView, openNewFolder } = useApp();
  const queryClient = useQueryClient();
  const { startUploads, guardReady, reportError, dialog } = useStartUploads();
  const connectionsQuery = useConnections();
  const connection = connectionsQuery.data?.find((item) => item.id === activeConn);
  const provider = connection ? providerMeta(connection.provider) : undefined;
  const providerName = provider?.nameKey ? t(provider.nameKey) : provider?.name;

  const noBucket = activeBucket === "";

  async function handleUpload() {
    const picked = await open({ multiple: true, title: t("upload.choose") });
    if (!picked) return;
    startUploads(Array.isArray(picked) ? picked : [picked]);
  }

  return (
    <div className="workbench-toolbar shrink-0 bg-background">
      {/* Tauri requires the actual pointer target to carry the drag attribute.
          Keep the location text transparent to pointer events. The full path
          still lives in PathBar, where long prefixes have space to expand. */}
      <div data-tauri-drag-region className="flex h-[72px] items-center border-b border-border2 px-5">
        <div className="pointer-events-none min-w-0">
          <h1 className="truncate text-[18px] leading-6 font-semibold tracking-[-0.3px]">
            {activeBucket || t("app.name")}
          </h1>
          <div className="mt-1 flex min-w-0 items-center gap-2 text-[12px] text-muted-foreground">
            {connection ? (
              <>
                <span className="truncate">{connection.name}</span>
                <span className="shrink-0 text-muted2" aria-hidden="true">/</span>
                <span className="shrink-0">{providerName}</span>
              </>
            ) : <span className="truncate">{t("main.breadcrumbPlaceholder")}</span>}
          </div>
        </div>
      </div>
      <div data-tauri-drag-region className="flex h-14 items-center gap-2 border-b border-border px-5">
        <div
          className={cn(
            "mr-auto flex h-8 min-w-[120px] max-w-[280px] flex-1 items-center gap-2 rounded-md border border-border bg-panel px-2.5 text-muted-foreground focus-within:border-primary focus-within:ring-2 focus-within:ring-primary-soft",
            noBucket && "opacity-50",
          )}
        >
          <Search className="size-[15px]" />
          <input
            value={search}
            disabled={noBucket}
            onChange={(e) => setSearch(e.target.value)}
            placeholder={t("main.searchPlaceholder")}
            aria-label={t("main.searchPlaceholder")}
            className="min-w-0 flex-1 border-none bg-transparent text-[13px] text-foreground outline-none disabled:cursor-not-allowed"
          />
        </div>
        <div className="flex shrink-0 gap-0.5 rounded-md bg-panel p-0.5">
          <button
            type="button"
            onClick={() => setView("list")}
            title={t("main.listView")}
            aria-label={t("main.listView")}
            aria-pressed={view === "list"}
            className={cn(
              "flex h-7 w-[30px] cursor-pointer items-center justify-center rounded-[4px]",
              view === "list"
                ? "bg-active text-primary"
                : "text-muted-foreground",
            )}
          >
            <List className="size-[15px]" />
          </button>
          <button
            type="button"
            onClick={() => setView("grid")}
            title={t("main.gridView")}
            aria-label={t("main.gridView")}
            aria-pressed={view === "grid"}
            className={cn(
              "flex h-7 w-[30px] cursor-pointer items-center justify-center rounded-[4px]",
              view === "grid"
                ? "bg-active text-primary"
                : "text-muted-foreground",
            )}
          >
            <LayoutGrid className="size-[15px]" />
          </button>
        </div>
        <button
          type="button"
          title={t("objects.newFolder")}
          aria-label={t("objects.newFolder")}
          disabled={noBucket}
          onClick={openNewFolder}
          className="flex h-8 shrink-0 cursor-pointer items-center justify-center gap-2 rounded-md border border-border bg-background px-2.5 text-[12px] text-fg2 hover:bg-hover hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
        >
          <FolderPlus className="size-[15px]" />
          <span className="new-folder-label">{t("objects.newFolder")}</span>
        </button>
        <button
          type="button"
          title={t("main.refresh")}
          aria-label={t("main.refresh")}
          disabled={noBucket}
          onClick={() =>
            queryClient.invalidateQueries({ queryKey: objectsRootKey(activeConn, activeBucket) })
          }
          className="flex size-8 shrink-0 cursor-pointer items-center justify-center rounded-md text-muted-foreground hover:bg-hover hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
        >
          <RefreshCw className="size-[15px]" />
        </button>
        <button
          type="button"
          // Disabled for no-bucket (required) and additionally while the
          // current folder's collision guard hasn't loaded yet -- clicking
          // through that window would otherwise look like the button just
          // did nothing (`useStartUploads` refuses silently on purpose; see
          // its `guardReady` doc comment). The title makes the second reason
          // legible instead of just inert.
          disabled={noBucket || !guardReady}
          title={!noBucket && !guardReady ? t("objects.checkingNames") : undefined}
          // `.catch` is not optional: `open()` is a plugin IPC call and can
          // reject (an unregistered dialog plugin rejects every invocation).
          // Unhandled, that is an console-only rejection and a button that
          // looks like it did nothing -- `reportError` turns it into a visible
          // message through the same dialog the upload flow already renders.
          onClick={() => void handleUpload().catch(reportError)}
          className="inline-flex h-8 shrink-0 cursor-pointer items-center gap-2 rounded-md bg-primary px-3.5 text-[13px] font-medium text-primary-foreground hover:bg-primary-strong disabled:cursor-not-allowed disabled:opacity-60"
        >
          {!noBucket && !guardReady ? (
            <Loader2 className="size-[15px] animate-spin" />
          ) : (
            <Upload className="size-[15px]" />
          )}
          {t("main.upload")}
        </button>
      </div>
      {dialog}
    </div>
  );
}
