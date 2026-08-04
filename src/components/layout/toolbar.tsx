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
import { useApp } from "@/store/app-store";

export function Toolbar() {
  const { t } = useTranslation();
  const { activeConn, activeBucket, search, setSearch, view, setView, openNewFolder } = useApp();
  const queryClient = useQueryClient();
  const { startUploads, guardReady, reportError, dialog } = useStartUploads();

  const noBucket = activeBucket === "";

  async function handleUpload() {
    const picked = await open({ multiple: true, title: t("upload.choose") });
    if (!picked) return;
    startUploads(Array.isArray(picked) ? picked : [picked]);
  }

  return (
    <div data-tauri-drag-region className="flex h-[52px] shrink-0 items-center gap-2.5 border-b border-border bg-background px-4">
      {/* Toolbar drag region: Tauri's drag detection requires the event to hit
       * this element directly, so we mark both the root and the spacer below.
       * Without either, the window cannot be dragged on this overlay bar. */}
      {/* The breadcrumb used to live here. It moved to `PathBar`, along the
          bottom of the content area: a row of its own gives it ~820px, where
          this slot left it ~357px once the rigid controls to the right had
          taken theirs. This spacer keeps those controls right-aligned. */}
      <div data-tauri-drag-region className="flex-1" />
      <div
        className={cn(
          "flex h-8 w-[206px] shrink-0 items-center gap-[7px] rounded-[9px] border border-border bg-panel px-2.5 text-muted-foreground focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary-soft",
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
      <div className="flex shrink-0 rounded-[9px] border border-border bg-panel p-0.5">
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
        className="flex size-8 shrink-0 cursor-pointer items-center justify-center rounded-[9px] border border-border bg-raised text-fg2 hover:bg-hover hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
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
        className="flex size-8 shrink-0 cursor-pointer items-center justify-center rounded-[9px] border border-border bg-raised text-fg2 hover:bg-hover hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
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
        className="inline-flex h-8 shrink-0 cursor-pointer items-center gap-[7px] rounded-[9px] bg-primary px-3.5 text-[13px] font-semibold text-primary-foreground shadow-[0_2px_6px_-1px_var(--primary-shadow)] hover:bg-primary-strong disabled:cursor-not-allowed disabled:opacity-60"
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
