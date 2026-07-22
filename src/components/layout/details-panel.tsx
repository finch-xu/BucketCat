import { Download, Link2, Share2, Trash2, X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { fileMeta, isImageExt } from "@/lib/file-meta";
import { extFromName, formatDate, formatSize } from "@/lib/format";
import { useBrowse } from "@/hooks/use-browse";
import { useApp } from "@/store/app-store";

/** Shows the single selected file's real metadata. Download/copy-link/share
 * stay visual placeholders until M4 (transfers) and M6 (presign); delete is
 * wired to the object dialogs. ETag/head_object detail arrives in M6. */
export function DetailsPanel() {
  const { t } = useTranslation();
  const { selectedKeys, clearSelection } = useApp();
  const { entries } = useBrowse();

  const entry =
    selectedKeys.length === 1
      ? entries.find((e) => e.key === selectedKeys[0] && !e.is_prefix)
      : undefined;
  if (!entry) return null;

  const ext = extFromName(entry.name);
  const meta = fileMeta("file", ext);
  const BigIcon = meta.icon;

  return (
    <aside className="flex w-[300px] shrink-0 flex-col border-l border-border bg-background">
      <div className="flex h-[46px] shrink-0 items-center justify-between border-b border-border pr-3 pl-4">
        <span className="text-[13px] font-semibold">{t("details.title")}</span>
        <button
          type="button"
          onClick={clearSelection}
          className="flex size-[26px] cursor-pointer items-center justify-center rounded-[7px] text-muted-foreground hover:bg-hover hover:text-fg2"
        >
          <X className="size-3.5" />
        </button>
      </div>
      <div className="flex-1 overflow-y-auto p-4">
        <div className="flex h-[148px] w-full items-center justify-center overflow-hidden rounded-xl border border-border bg-[repeating-linear-gradient(45deg,var(--panel),var(--panel)_11px,var(--border2)_11px,var(--border2)_22px)]">
          {isImageExt(ext) ? (
            <span className="rounded-[7px] border border-border bg-background px-[11px] py-1.5 font-mono text-[11px] text-muted-foreground">
              {t("details.imagePreview")}
            </span>
          ) : (
            <BigIcon className="size-[46px]" style={{ color: meta.color }} />
          )}
        </div>
        <div className="mt-3.5 text-[15px] leading-[1.35] font-semibold break-all">{entry.name}</div>
        <div className="mt-[3px] text-xs text-muted-foreground">{t(meta.labelKey)}</div>
        <div className="my-4 grid grid-cols-2 gap-2">
          <button
            type="button"
            className="inline-flex h-[34px] cursor-pointer items-center justify-center gap-1.5 rounded-[9px] bg-primary text-[12.5px] font-semibold text-primary-foreground hover:bg-primary-strong"
          >
            <Download className="size-3.5" />
            {t("details.download")}
          </button>
          <button
            type="button"
            className="inline-flex h-[34px] cursor-pointer items-center justify-center gap-1.5 rounded-[9px] border border-border bg-background text-[12.5px] font-medium text-fg2 hover:bg-hover"
          >
            <Link2 className="size-3.5" />
            {t("details.copyLink")}
          </button>
          <button
            type="button"
            className="inline-flex h-[34px] cursor-pointer items-center justify-center gap-1.5 rounded-[9px] border border-border bg-background text-[12.5px] font-medium text-fg2 hover:bg-hover"
          >
            <Share2 className="size-3.5" />
            {t("details.share")}
          </button>
          <button
            type="button"
            className="inline-flex h-[34px] cursor-pointer items-center justify-center gap-1.5 rounded-[9px] bg-destructive/10 text-[12.5px] font-medium text-destructive hover:bg-destructive/20"
          >
            <Trash2 className="size-3.5" />
            {t("details.delete")}
          </button>
        </div>
        <div className="flex flex-col gap-3 border-t border-border2 pt-3.5">
          <div>
            <div className="mb-[3px] text-[10.5px] tracking-[0.4px] text-muted-foreground uppercase">
              {t("details.objectKey")}
            </div>
            <div className="font-mono text-xs leading-[1.4] break-all text-fg2">{entry.key}</div>
          </div>
          <div className="flex justify-between">
            <span className="text-xs text-muted-foreground">{t("details.size")}</span>
            <span className="text-[12.5px] text-fg2 tabular-nums">{formatSize(entry.size)}</span>
          </div>
          <div className="flex justify-between">
            <span className="text-xs text-muted-foreground">{t("details.storageClass")}</span>
            <span className="text-[12.5px] text-fg2">{entry.storage_class ?? "—"}</span>
          </div>
          <div className="flex justify-between">
            <span className="text-xs text-muted-foreground">{t("details.modified")}</span>
            <span className="text-[12.5px] text-fg2 tabular-nums">
              {formatDate(entry.last_modified)}
            </span>
          </div>
        </div>
      </div>
    </aside>
  );
}
