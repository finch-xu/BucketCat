import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { getVersion } from "@tauri-apps/api/app";
import logo from "@/assets/logo-icon.png";
import { openExternal } from "@/lib/external-link";
import { Row } from "./shared";

const REPO_URL = "https://github.com/finch-xu/BucketCat";

/** Author and license are compile-time constants that mirror
 * `src-tauri/Cargo.toml` and `package.json`; there is no build-time injection
 * to keep them in sync, so update them here if those ever change. The version
 * deliberately is NOT one of them -- see below. */
const AUTHOR = "虚拟世界的懒猫";
const LICENSE = "Apache-2.0";

export function AboutPane() {
  const { t } = useTranslation();
  // Read at runtime from `tauri.conf.json`'s `version` rather than hardcoded,
  // so a release bump has exactly one place to change. A rejection leaves
  // `version` null and the badge simply is not rendered: the version is
  // decorative, and an error message in its place would draw far more
  // attention than the information is worth.
  const [version, setVersion] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    getVersion()
      .then((v) => {
        if (!cancelled) setVersion(v);
      })
      .catch((err) => {
        console.error("Failed to read the app version", err);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div>
      <div className="flex items-center gap-3.5 pt-1.5 pb-1">
        <img
          src={logo}
          alt={t("app.name")}
          className="size-[52px] rounded-[13px] shadow-[0_0_0_1px_var(--border)]"
        />
        <div className="flex-1">
          <div className="text-[15px] font-bold">
            {t("app.name")}
            {version && (
              <span className="ml-2 rounded-[20px] border border-border bg-panel px-[7px] py-px text-[11.5px] font-medium text-muted-foreground">
                v{version}
              </span>
            )}
          </div>
          <div className="mt-[3px] text-[12.5px] text-muted-foreground">{t("app.tagline")}</div>
        </div>
      </div>

      <div className="mt-4 border-t border-border2 pt-1">
        <Row label={t("settings.author")}>
          <span className="text-[12.5px] text-muted-foreground">{AUTHOR}</span>
        </Row>
        <Row label={t("settings.license")}>
          <span className="text-[12.5px] text-muted-foreground">{LICENSE}</span>
        </Row>
      </div>

      <div className="mt-3">
        <button
          type="button"
          onClick={() => void openExternal(REPO_URL)}
          className="cursor-pointer rounded-lg border border-border px-[13px] py-[7px] text-[12.5px] text-fg2 hover:bg-hover"
        >
          {t("settings.github")}
        </button>
      </div>
    </div>
  );
}
