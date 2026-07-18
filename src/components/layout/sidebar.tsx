import { useTranslation } from "react-i18next";
import { Cat, Plus, Settings } from "lucide-react";
import { Button } from "@/components/ui/button";

export function Sidebar() {
  const { t } = useTranslation();
  return (
    <aside className="flex w-60 shrink-0 flex-col border-r">
      <div className="flex h-12 items-center gap-2 px-4">
        <Cat className="size-5 text-primary" />
        <span className="text-sm font-semibold">{t("app.name")}</span>
      </div>
      <div className="flex-1 overflow-y-auto px-3 py-2">
        <p className="px-1 text-xs font-medium text-muted-foreground">
          {t("sidebar.connections")}
        </p>
        <Button
          variant="outline"
          className="mt-2 w-full justify-start gap-2 border-dashed text-muted-foreground"
        >
          <Plus className="size-4" />
          {t("sidebar.addConnection")}
        </Button>
      </div>
      <div className="border-t p-2">
        <Button
          variant="ghost"
          size="sm"
          className="w-full justify-start gap-2 text-muted-foreground"
        >
          <Settings className="size-4" />
          {t("sidebar.settings")}
        </Button>
      </div>
    </aside>
  );
}
