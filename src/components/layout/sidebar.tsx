import { ChevronDown, ChevronRight, Folder, Plus, Settings } from "lucide-react";
import { useTranslation } from "react-i18next";
import { cn } from "@/lib/utils";
import { MOCK_CONNECTIONS } from "@/lib/mock-data";
import { useApp } from "@/store/app-store";

export function Sidebar() {
  const { t } = useTranslation();
  const { activeConn, activeBucket, expanded, toggleConn, selectBucket, openAdd, openSettings } =
    useApp();

  return (
    <aside className="flex w-[236px] shrink-0 flex-col border-r border-border bg-sidebar">
      <div className="flex items-center justify-between px-3.5 pt-[15px] pb-2">
        <span className="text-[11px] font-semibold tracking-[0.7px] text-muted-foreground uppercase">
          {t("sidebar.connections")}
        </span>
        <button
          type="button"
          onClick={openAdd}
          title={t("sidebar.addConnection")}
          className="flex size-6 cursor-pointer items-center justify-center rounded-[7px] border border-border bg-background text-fg2 hover:border-primary hover:bg-hover hover:text-primary"
        >
          <Plus className="size-[15px]" />
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-2 pt-0.5 pb-2">
        {MOCK_CONNECTIONS.map((c) => {
          const isOpen = !!expanded[c.id];
          const Icon = c.icon;
          return (
            <div key={c.id} className="mb-px">
              <div
                onClick={() => toggleConn(c.id)}
                className="flex cursor-pointer items-center gap-[9px] rounded-[9px] px-2 py-[7px] hover:bg-hover"
              >
                <span className="flex w-3 justify-center text-muted2">
                  {isOpen ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
                </span>
                <span
                  className="flex size-[26px] shrink-0 items-center justify-center rounded-[7px] shadow-[0_1px_2px_rgba(0,0,0,0.18)]"
                  style={{ background: c.color }}
                >
                  <Icon className="size-[15px] text-white" />
                </span>
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-[13px] font-semibold text-foreground">
                    {c.name}
                  </span>
                  <span className="block truncate text-[11px] text-muted-foreground">
                    {c.provider}
                  </span>
                </span>
              </div>
              {isOpen && (
                <div className="mt-px mb-1 ml-[22px] border-l border-border pl-1.5">
                  {c.buckets.map((b) => {
                    const active = activeConn === c.id && activeBucket === b;
                    return (
                      <div
                        key={b}
                        onClick={() => selectBucket(c.id, b)}
                        className={cn(
                          "flex cursor-pointer items-center gap-2 rounded-lg px-[9px] py-1.5",
                          active ? "bg-primary-soft text-primary" : "text-fg2 hover:bg-hover",
                        )}
                      >
                        <Folder className={cn("size-3.5", active ? "text-primary" : "text-sky")} />
                        <span
                          className={cn(
                            "truncate text-[12.5px]",
                            active ? "font-semibold" : "font-medium",
                          )}
                        >
                          {b}
                        </span>
                      </div>
                    );
                  })}
                </div>
              )}
            </div>
          );
        })}
      </div>
      <div className="border-t border-border p-2">
        <button
          type="button"
          onClick={openSettings}
          className="flex w-full cursor-pointer items-center gap-[9px] rounded-[9px] px-2.5 py-2 text-[13px] text-fg2 hover:bg-hover"
        >
          <Settings className="size-4" />
          {t("sidebar.settings")}
        </button>
      </div>
    </aside>
  );
}
