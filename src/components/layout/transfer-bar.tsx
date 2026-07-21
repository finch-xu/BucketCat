import { ArrowUpDown, ChevronDown, Download, Upload, X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { cn } from "@/lib/utils";
import { useApp } from "@/store/app-store";

const DONE_COLOR = "#4bb39a";

export function TransferBar() {
  const { t } = useTranslation();
  const { transfers, transferOpen, toggleTransferPanel, removeTransfer } = useApp();

  const active = transfers.filter((tr) => tr.status === "active");
  const overall = active.length
    ? Math.round(active.reduce((sum, tr) => sum + tr.pct, 0) / active.length)
    : 0;
  const summary = active.length
    ? t("transfer.activeCount", { count: active.length })
    : t("transfer.idle");

  return (
    <div className="relative shrink-0">
      {transferOpen && (
        <div className="absolute bottom-full left-0 right-0 max-h-[284px] overflow-y-auto border-t border-border bg-background shadow-[0_-14px_32px_-14px_var(--shadow)] [animation:bc-up_.18s_ease]">
          <div className="sticky top-0 flex items-center justify-between border-b border-border2 bg-background px-3.5 pt-[11px] pb-[7px]">
            <span className="text-xs font-semibold text-fg2">{t("transfer.title")}</span>
            <span className="text-[11px] text-muted-foreground">{summary}</span>
          </div>
          <div className="px-2 pt-1.5 pb-2">
            {transfers.map((tr) => {
              const DirIcon = tr.dir === "up" ? Upload : Download;
              const color = tr.status === "done" ? DONE_COLOR : "var(--primary)";
              const statusLine =
                tr.status === "done"
                  ? `${t("transfer.done")} · ${tr.size}`
                  : `${tr.dir === "up" ? t("transfer.uploading") : t("transfer.downloading")}${tr.speed ? ` · ${tr.speed}` : ""}`;
              return (
                <div
                  key={tr.id}
                  className="flex items-center gap-[11px] rounded-[10px] px-2 py-[9px] hover:bg-hover"
                >
                  <span className="flex size-[30px] shrink-0 items-center justify-center rounded-lg border border-border bg-panel">
                    <DirIcon className="size-[15px]" style={{ color }} />
                  </span>
                  <div className="min-w-0 flex-1">
                    <div className="flex justify-between gap-2">
                      <span className="truncate text-[12.5px] font-medium">{tr.name}</span>
                      <span className="shrink-0 text-[11px] text-muted-foreground tabular-nums">
                        {tr.size}
                      </span>
                    </div>
                    <div className="mt-1.5 flex items-center gap-2">
                      <div className="h-[5px] flex-1 overflow-hidden rounded-[3px] bg-border2">
                        <div
                          className="h-full rounded-[3px] transition-[width] duration-400"
                          style={{ background: color, width: `${tr.pct}%` }}
                        />
                      </div>
                      <span className="w-[34px] text-right text-[11px] text-muted-foreground tabular-nums">
                        {tr.pct}%
                      </span>
                    </div>
                    <div className="mt-1 text-[10.5px] text-muted2">{statusLine}</div>
                  </div>
                  <button
                    type="button"
                    onClick={() => removeTransfer(tr.id)}
                    className="flex size-[26px] shrink-0 cursor-pointer items-center justify-center rounded-[7px] text-muted-foreground hover:bg-hover hover:text-fg2"
                  >
                    <X className="size-3.5" />
                  </button>
                </div>
              );
            })}
          </div>
        </div>
      )}
      <footer className="flex h-10 items-center gap-2.5 border-t border-border bg-titlebar px-3.5 text-xs text-muted-foreground">
        <ArrowUpDown
          className={cn("size-[13px]", active.length ? "text-primary" : "text-muted-foreground")}
        />
        <span className="text-fg2">{summary}</span>
        {active.length > 0 && (
          <>
            <div className="h-[5px] w-[120px] overflow-hidden rounded-[3px] bg-border2">
              <div
                className="h-full rounded-[3px] bg-primary transition-[width] duration-400"
                style={{ width: `${overall}%` }}
              />
            </div>
            <span className="text-fg2 tabular-nums">{overall}%</span>
          </>
        )}
        <button
          type="button"
          onClick={toggleTransferPanel}
          className="ml-auto flex cursor-pointer items-center gap-1.5 rounded-[7px] px-[9px] py-[5px] text-xs text-muted-foreground hover:bg-hover hover:text-fg2"
        >
          {t("transfer.toggle")}
          <ChevronDown
            className={cn(
              "size-3.5 transition-transform duration-200",
              transferOpen && "rotate-180",
            )}
          />
        </button>
      </footer>
    </div>
  );
}
