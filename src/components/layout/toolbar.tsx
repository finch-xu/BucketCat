import { ChevronRight, Database, LayoutGrid, List, RefreshCw, Search, Upload } from "lucide-react";
import { Fragment } from "react";
import { useTranslation } from "react-i18next";
import { cn } from "@/lib/utils";
import { useApp } from "@/store/app-store";

export function Toolbar() {
  const { t } = useTranslation();
  const { activeBucket, path, gotoCrumb, search, setSearch, view, setView, startMockUpload } =
    useApp();

  const crumbs = [activeBucket, ...path];

  return (
    <div className="flex h-[52px] shrink-0 items-center gap-2.5 border-b border-border bg-background px-4">
      <div className="flex min-w-0 flex-1 items-center gap-px">
        {crumbs.map((label, i) => {
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
                {i === 0 && <Database className="size-[15px] shrink-0 text-sky" />}
                {label}
              </button>
            </Fragment>
          );
        })}
      </div>
      <div className="flex h-8 w-[206px] items-center gap-[7px] rounded-[9px] border border-border bg-panel px-2.5 text-muted-foreground focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary-soft">
        <Search className="size-[15px]" />
        <input
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder={t("main.searchPlaceholder")}
          className="min-w-0 flex-1 border-none bg-transparent text-[13px] text-foreground outline-none"
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
        title={t("main.refresh")}
        className="flex size-8 cursor-pointer items-center justify-center rounded-[9px] border border-border bg-background text-fg2 hover:bg-hover hover:text-foreground"
      >
        <RefreshCw className="size-[15px]" />
      </button>
      <button
        type="button"
        onClick={startMockUpload}
        className="inline-flex h-8 cursor-pointer items-center gap-[7px] rounded-[9px] bg-primary px-3.5 text-[13px] font-semibold text-primary-foreground shadow-[0_2px_6px_-1px_var(--primary-soft)] hover:bg-primary-strong"
      >
        <Upload className="size-[15px]" />
        {t("main.upload")}
      </button>
    </div>
  );
}
