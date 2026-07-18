import { useTranslation } from "react-i18next";
import { ArrowUpDown } from "lucide-react";

export function TransferBar() {
  const { t } = useTranslation();
  return (
    <footer className="flex h-9 items-center gap-2 border-t px-4 text-xs text-muted-foreground">
      <ArrowUpDown className="size-3.5" />
      <span>{t("transfer.idle")}</span>
    </footer>
  );
}
