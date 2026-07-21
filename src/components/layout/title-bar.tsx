import { Moon, Settings, Sun } from "lucide-react";
import { useTranslation } from "react-i18next";
import logo from "@/assets/logo.png";
import { useApp } from "@/store/app-store";

export function TitleBar() {
  const { t } = useTranslation();
  const { dark, toggleTheme, openSettings } = useApp();

  return (
    <header className="relative flex h-[46px] shrink-0 items-center border-b border-border bg-titlebar px-3.5">
      <div className="pointer-events-none absolute inset-x-0 flex items-center justify-center gap-2">
        <img
          src={logo}
          alt="BucketCat"
          className="size-[22px] rounded-md shadow-[0_0_0_1px_var(--border)]"
        />
        <span className="text-[13px] font-semibold tracking-[0.2px]">{t("app.name")}</span>
      </div>
      <div className="ml-auto flex items-center gap-1">
        <button
          type="button"
          onClick={toggleTheme}
          title={t("titlebar.toggleTheme")}
          className="flex size-[30px] cursor-pointer items-center justify-center rounded-lg text-fg2 hover:bg-hover hover:text-foreground"
        >
          {dark ? <Sun className="size-4" /> : <Moon className="size-4" />}
        </button>
        <button
          type="button"
          onClick={openSettings}
          title={t("titlebar.settings")}
          className="flex size-[30px] cursor-pointer items-center justify-center rounded-lg text-fg2 hover:bg-hover hover:text-foreground"
        >
          <Settings className="size-4" />
        </button>
      </div>
    </header>
  );
}
