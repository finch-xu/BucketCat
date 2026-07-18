import { useTranslation } from "react-i18next";
import { FolderOpen, Search, Upload } from "lucide-react";
import { Button } from "@/components/ui/button";

export function MainPanel() {
  const { t } = useTranslation();
  return (
    <section className="flex min-w-0 flex-1 flex-col">
      <div className="flex h-12 items-center gap-3 border-b px-4">
        <span className="truncate text-sm text-muted-foreground">
          {t("main.breadcrumbPlaceholder")}
        </span>
        <div className="ml-auto flex items-center gap-2">
          <div className="flex h-8 w-56 items-center gap-2 rounded-md border px-2 text-muted-foreground">
            <Search className="size-4" />
            <span className="text-xs">{t("main.searchPlaceholder")}</span>
          </div>
          <Button size="sm" disabled className="gap-2">
            <Upload className="size-4" />
            {t("main.upload")}
          </Button>
        </div>
      </div>
      <div className="flex flex-1 flex-col items-center justify-center gap-2 text-muted-foreground">
        <FolderOpen className="size-10" />
        <p className="text-sm font-medium">{t("main.emptyTitle")}</p>
        <p className="text-xs">{t("main.emptyHint")}</p>
      </div>
    </section>
  );
}
