import {
  ChevronRight,
  Database,
  FolderPlus,
  LayoutGrid,
  List,
  Loader2,
  RefreshCw,
  Search,
  Upload,
} from "lucide-react";
import { Fragment } from "react";
import { useTranslation } from "react-i18next";
import { useQueryClient } from "@tanstack/react-query";
import { open } from "@tauri-apps/plugin-dialog";
import { cn } from "@/lib/utils";
import { objectsRootKey } from "@/hooks/use-objects";
import { useStartUploads } from "@/hooks/use-start-uploads";
import { useApp } from "@/store/app-store";

export function Toolbar() {
  const { t } = useTranslation();
  const {
    activeConn,
    activeBucket,
    path,
    gotoCrumb,
    search,
    setSearch,
    view,
    setView,
    openNewFolder,
  } = useApp();
  const queryClient = useQueryClient();
  const { startUploads, guardReady, reportError, dialog } = useStartUploads();

  const noBucket = activeBucket === "";
  const crumbs = [activeBucket, ...path];

  async function handleUpload() {
    const picked = await open({ multiple: true, title: t("upload.choose") });
    if (!picked) return;
    startUploads(Array.isArray(picked) ? picked : [picked]);
  }

  return (
    <div className="flex h-[52px] shrink-0 items-center gap-2.5 border-b border-border bg-background px-4">
      <div className="flex min-w-0 flex-1 items-center gap-px">
        {noBucket ? (
          <span className="truncate px-2 py-[5px] text-[13.5px] text-muted-foreground">
            {t("main.breadcrumbPlaceholder")}
          </span>
        ) : (
          crumbs.map((label, i) => {
            const last = i === crumbs.length - 1;
            return (
              <Fragment key={`${i}-${label}`}>
                {i > 0 && <ChevronRight className="size-[13px] shrink-0 text-muted2" />}
                <button
                  type="button"
                  onClick={() => gotoCrumb(i - 1)}
                  className={cn(
                    "flex max-w-[190px] cursor-pointer items-center gap-[7px] truncate rounded-[7px] px-2 py-[5px] text-[13.5px] hover:bg-hover",
                    last ? "font-semibold text-foreground" : "font-medium text-muted-foreground",
                  )}
                >
                  {i === 0 && <Database className="size-[15px] shrink-0 text-muted-foreground" />}
                  {label}
                </button>
              </Fragment>
            );
          })
        )}
      </div>
      <div
        className={cn(
          "flex h-8 w-[206px] items-center gap-[7px] rounded-[9px] border border-border bg-panel px-2.5 text-muted-foreground focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary-soft",
          noBucket && "opacity-50",
        )}
      >
        <Search className="size-[15px]" />
        <input
          value={search}
          disabled={noBucket}
          onChange={(e) => setSearch(e.target.value)}
          placeholder={t("main.searchPlaceholder")}
          className="min-w-0 flex-1 border-none bg-transparent text-[13px] text-foreground outline-none disabled:cursor-not-allowed"
        />
      </div>
      <div className="flex rounded-[9px] border border-border bg-panel p-0.5">
        <button
          type="button"
          onClick={() => setView("list")}
          title={t("main.listView")}
          className={cn(
            "flex h-[26px] w-[30px] cursor-pointer items-center justify-center rounded-[7px]",
            view === "list"
              ? "bg-background text-primary shadow-[0_1px_2px_rgba(0,0,0,0.08)]"
              : "text-muted-foreground",
          )}
        >
          <List className="size-[15px]" />
        </button>
        <button
          type="button"
          onClick={() => setView("grid")}
          title={t("main.gridView")}
          className={cn(
            "flex h-[26px] w-[30px] cursor-pointer items-center justify-center rounded-[7px]",
            view === "grid"
              ? "bg-background text-primary shadow-[0_1px_2px_rgba(0,0,0,0.08)]"
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
        className="flex size-8 cursor-pointer items-center justify-center rounded-[9px] border border-border bg-background text-fg2 hover:bg-hover hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
      >
        <FolderPlus className="size-[15px]" />
      </button>
      <button
        type="button"
        title={t("main.refresh")}
        aria-label={t("main.refresh")}
        disabled={noBucket}
        onClick={() =>
          queryClient.invalidateQueries({ queryKey: objectsRootKey(activeConn, activeBucket) })
        }
        className="flex size-8 cursor-pointer items-center justify-center rounded-[9px] border border-border bg-background text-fg2 hover:bg-hover hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
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
        className="inline-flex h-8 cursor-pointer items-center gap-[7px] rounded-[9px] bg-primary px-3.5 text-[13px] font-semibold text-primary-foreground shadow-[0_2px_6px_-1px_var(--primary-soft)] hover:bg-primary-strong disabled:cursor-not-allowed disabled:opacity-60"
      >
        {!noBucket && !guardReady ? (
          <Loader2 className="size-[15px] animate-spin" />
        ) : (
          <Upload className="size-[15px]" />
        )}
        {t("main.upload")}
      </button>
      {dialog}
    </div>
  );
}
