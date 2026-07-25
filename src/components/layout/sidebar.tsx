import {
  ChevronDown,
  ChevronRight,
  Folder,
  Moon,
  Pencil,
  Plus,
  Settings,
  Sun,
  Trash2,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import logoIcon from "@/assets/logo-icon.png";
import { cn } from "@/lib/utils";
import { isMac } from "@/lib/platform";
import { providerMeta } from "@/lib/providers";
import { useApp } from "@/store/app-store";
import { useBuckets, useConnections } from "@/hooks/use-connections";
import { useErrorText } from "@/hooks/use-error-text";
import type { ConnectionDto } from "@/lib/api";

function SkeletonBar({ className }: { className?: string }) {
  return <span className={cn("block animate-pulse rounded bg-muted", className)} />;
}

function ConnRowSkeleton() {
  return (
    <div className="flex items-center gap-[9px] rounded-[9px] px-2 py-[7px]">
      <span className="w-3 shrink-0" />
      <SkeletonBar className="size-[26px] shrink-0 rounded-[7px]" />
      <span className="min-w-0 flex-1 space-y-1.5 py-0.5">
        <SkeletonBar className="h-2.5 w-3/5" />
        <SkeletonBar className="h-2 w-2/5" />
      </span>
    </div>
  );
}

function BucketRowSkeleton() {
  return (
    <div className="flex items-center gap-2 px-[9px] py-1.5">
      <SkeletonBar className="size-3.5 shrink-0 rounded-[4px]" />
      <SkeletonBar className="h-2.5 w-2/3" />
    </div>
  );
}

interface BucketListProps {
  connId: string;
  connRegion: string;
  isOpen: boolean;
  activeConn: string;
  activeBucket: string;
  onSelect: (bucket: string) => void;
}

/** Renders a connection's bucket list. `useBuckets` only actually fires the
 * IPC call once `isOpen` flips true (see its `enabled` param), so collapsed
 * connections never pay the round-trip. */
function BucketList({
  connId,
  connRegion,
  isOpen,
  activeConn,
  activeBucket,
  onSelect,
}: BucketListProps) {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const bucketsQuery = useBuckets(connId, isOpen);

  if (!isOpen) return null;

  return (
    <div className="mt-px mb-1 ml-[22px] border-l border-border pl-1.5">
      {bucketsQuery.isPending && (
        <div aria-busy="true" aria-live="polite">
          <span className="sr-only">{t("sidebar.loadingBuckets")}</span>
          <BucketRowSkeleton />
          <BucketRowSkeleton />
        </div>
      )}
      {bucketsQuery.isError && (
        <div role="alert" className="flex items-center justify-between gap-2 px-[9px] py-1.5">
          <span className="truncate text-[12px] text-destructive">
            {errorText(bucketsQuery.error)}
          </span>
          <button
            type="button"
            onClick={() => bucketsQuery.refetch()}
            className="shrink-0 cursor-pointer text-[12px] font-semibold text-primary hover:underline"
          >
            {t("sidebar.retry")}
          </button>
        </div>
      )}
      {bucketsQuery.isSuccess && bucketsQuery.data.length === 0 && (
        <div className="px-[9px] py-1.5 text-[12px] text-muted-foreground">
          {t("sidebar.noBuckets")}
        </div>
      )}
      {bucketsQuery.isSuccess &&
        bucketsQuery.data.map((bucket) => {
          const active = activeConn === connId && activeBucket === bucket.name;
          return (
            <div
              key={bucket.name}
              onClick={() => onSelect(bucket.name)}
              className={cn(
                "flex cursor-pointer items-center gap-2 rounded-lg px-[9px] py-1.5",
                active ? "bg-active text-primary" : "text-fg2 hover:bg-hover",
              )}
            >
              <Folder className={cn("size-3.5", active ? "text-primary" : "text-muted-foreground")} />
              <span
                className={cn(
                  "min-w-0 flex-1 truncate text-[12.5px]",
                  active ? "font-semibold" : "font-medium",
                )}
              >
                {bucket.name}
              </span>
              {/* Cross-region label (design: M5b task-1, auto-routed since
               * M5b task-2) -- a bucket whose OSS-reported region differs
               * from this connection's configured region is still fully
               * reachable: `S3Provider::client_for` picks a client for the
               * bucket's own region automatically. Purely informational,
               * neutral styling only (no accent/warning color, no
               * click-behavior change). */}
              {bucket.region && bucket.region !== connRegion && (
                <span
                  className="shrink-0 text-[10.5px] text-muted-foreground"
                  title={t("sidebar.bucketRegionHint", { region: bucket.region })}
                >
                  {bucket.region}
                </span>
              )}
            </div>
          );
        })}
    </div>
  );
}

interface ConnectionRowProps {
  conn: ConnectionDto;
  isOpen: boolean;
  activeConn: string;
  activeBucket: string;
  onToggle: () => void;
  onSelectBucket: (bucket: string) => void;
  onEdit: () => void;
  onDelete: () => void;
}

function ConnectionRow({
  conn,
  isOpen,
  activeConn,
  activeBucket,
  onToggle,
  onSelectBucket,
  onEdit,
  onDelete,
}: ConnectionRowProps) {
  const { t } = useTranslation();
  const meta = providerMeta(conn.provider);
  const Icon = meta.icon;

  return (
    <div className="mb-px">
      <div
        onClick={onToggle}
        className={cn(
          "group flex cursor-pointer items-center gap-[9px] rounded-[9px] px-2 py-[7px]",
          isOpen ? "bg-raised shadow-[0_1px_2px_var(--shadow)]" : "hover:bg-hover",
        )}
      >
        <span className="flex w-3 justify-center text-muted2">
          {isOpen ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
        </span>
        <span
          className="flex size-[26px] shrink-0 items-center justify-center rounded-[7px] shadow-[0_1px_2px_rgba(0,0,0,0.18)]"
          style={{ background: meta.color }}
        >
          <Icon className="size-[15px] text-white" />
        </span>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[13px] font-semibold text-foreground">
            {conn.name}
          </span>
          <span className="block truncate text-[11px] text-muted-foreground">
            {meta.nameKey ? t(meta.nameKey) : meta.name}
          </span>
        </span>
        {/* Hover/focus-only actions -- kept keyboard-reachable via
         * group-focus-within so tabbing to either button reveals them even
         * without a mouse hover. */}
        <span className="flex shrink-0 items-center gap-0.5 opacity-0 group-focus-within:opacity-100 group-hover:opacity-100">
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              onEdit();
            }}
            title={t("conn.edit")}
            aria-label={t("conn.edit")}
            className="flex size-6 cursor-pointer items-center justify-center rounded-[6px] text-muted-foreground hover:bg-active hover:text-primary"
          >
            <Pencil className="size-3.5" />
          </button>
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              onDelete();
            }}
            title={t("conn.delete")}
            aria-label={t("conn.delete")}
            className="flex size-6 cursor-pointer items-center justify-center rounded-[6px] text-muted-foreground hover:bg-destructive/10 hover:text-destructive"
          >
            <Trash2 className="size-3.5" />
          </button>
        </span>
      </div>
      <BucketList
        connId={conn.id}
        connRegion={conn.region}
        isOpen={isOpen}
        activeConn={activeConn}
        activeBucket={activeBucket}
        onSelect={onSelectBucket}
      />
    </div>
  );
}

export function Sidebar() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const {
    activeConn,
    activeBucket,
    expanded,
    dark,
    toggleTheme,
    toggleConn,
    selectBucket,
    openAdd,
    openSettings,
    openEditConnection,
    openDeleteConnection,
  } = useApp();
  const connectionsQuery = useConnections();

  return (
    <aside className="flex w-[248px] shrink-0 flex-col border-r border-border bg-sidebar">
      {/* Brand strip, doubling as the window's drag region. The title bar is
       * gone (see `titleBarStyle: "Overlay"` in tauri.conf.json), and Tauri
       * documents that Overlay leaves a window with NO default draggable
       * area -- without this attribute the window could not be moved at all.
       * On macOS the native traffic lights float over the left of this strip,
       * so it has to yield 76px; Windows/Linux keep their native title bar
       * and need no inset.
       * The children are `pointer-events-none` on purpose: Tauri decides
       * what is draggable by looking at the event target, so a logo or label
       * that can receive pointer events would swallow drags aimed at it. */}
      <div
        data-tauri-drag-region
        className={cn("flex h-[52px] shrink-0 items-center gap-2.5 px-3.5", isMac && "pl-[76px]")}
      >
        <img
          src={logoIcon}
          alt="BucketCat"
          className="pointer-events-none size-[30px] rounded-lg shadow-[0_0_0_1px_var(--border)]"
        />
        <span className="pointer-events-none text-[15.5px] font-bold tracking-[0.1px]">
          {t("app.name")}
        </span>
      </div>
      <div className="flex items-center justify-between px-3.5 pt-[9px] pb-2">
        <span className="text-[11px] font-semibold tracking-[0.7px] text-muted-foreground uppercase">
          {t("sidebar.connections")}
        </span>
        <button
          type="button"
          onClick={openAdd}
          title={t("sidebar.addConnection")}
          className="flex size-6 cursor-pointer items-center justify-center rounded-[7px] border border-accent-border bg-raised text-primary hover:border-primary hover:bg-accent-tint"
        >
          <Plus className="size-[15px]" />
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-2 pt-0.5 pb-2">
        {connectionsQuery.isPending && (
          <div aria-busy="true" aria-live="polite">
            <span className="sr-only">{t("sidebar.loading")}</span>
            <ConnRowSkeleton />
            <ConnRowSkeleton />
            <ConnRowSkeleton />
          </div>
        )}
        {connectionsQuery.isError && (
          <div
            role="alert"
            className="mx-1 mt-2 flex flex-col items-start gap-2 rounded-[9px] border border-border bg-panel px-3 py-2.5"
          >
            <span className="text-[12.5px] text-destructive">
              {errorText(connectionsQuery.error)}
            </span>
            <button
              type="button"
              onClick={() => connectionsQuery.refetch()}
              className="cursor-pointer text-[12px] font-semibold text-primary hover:underline"
            >
              {t("sidebar.retry")}
            </button>
          </div>
        )}
        {connectionsQuery.isSuccess && connectionsQuery.data.length === 0 && (
          <div className="flex flex-col items-center gap-1.5 px-3 pt-9 pb-4 text-center">
            <span className="text-[13px] font-semibold text-fg2">{t("sidebar.emptyTitle")}</span>
            <span className="text-[12px] text-muted-foreground">{t("sidebar.emptyHint")}</span>
          </div>
        )}
        {connectionsQuery.isSuccess &&
          connectionsQuery.data.map((conn) => (
            <ConnectionRow
              key={conn.id}
              conn={conn}
              isOpen={!!expanded[conn.id]}
              activeConn={activeConn}
              activeBucket={activeBucket}
              onToggle={() => toggleConn(conn.id)}
              onSelectBucket={(bucket) => selectBucket(conn.id, bucket)}
              onEdit={() => openEditConnection(conn)}
              onDelete={() => openDeleteConnection(conn)}
            />
          ))}
      </div>
      <div className="flex items-center gap-1.5 border-t border-border p-2">
        <button
          type="button"
          onClick={openSettings}
          className="flex flex-1 cursor-pointer items-center gap-[9px] rounded-[9px] px-2.5 py-2 text-[13px] text-fg2 hover:bg-hover"
        >
          <Settings className="size-4" />
          {t("sidebar.settings")}
        </button>
        <button
          type="button"
          onClick={toggleTheme}
          title={t("sidebar.toggleTheme")}
          aria-label={t("sidebar.toggleTheme")}
          className="flex size-[30px] shrink-0 cursor-pointer items-center justify-center rounded-[9px] text-fg2 hover:bg-hover"
        >
          {dark ? <Sun className="size-4" /> : <Moon className="size-4" />}
        </button>
      </div>
    </aside>
  );
}
