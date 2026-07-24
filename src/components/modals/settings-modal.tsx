import { useQueryClient } from "@tanstack/react-query";
import { X } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import logo from "@/assets/logo.png";
import { Modal } from "@/components/ui/modal";
import { Segmented } from "@/components/ui/segmented";
import { Switch } from "@/components/ui/switch";
import { setLocale } from "@/i18n";
import type { AppLocale } from "@/i18n/resolve-locale";
import { useErrorText } from "@/hooks/use-error-text";
import { formatSize } from "@/lib/format";
import {
  cleanCheckpointResidue,
  clearFinishedTransfers,
  getResumeEnabled,
  getSettings,
  setMaxParts,
  setMaxTasks,
  setResumeEnabled,
  setShareExpiry,
  type AppError,
  type CleanResult,
} from "@/lib/api";
import { useTransferStore } from "@/store/transfer-store";
import { useApp, type ViewMode } from "@/store/app-store";
import type { ThemeMode } from "@/lib/theme";

/** Share-link expiry choices, in seconds -- the same fixed set the details
 * panel's Share dropdown offers (`EXPIRY_OPTIONS` in
 * `src/components/layout/details-panel.tsx`), reusing its `details.expiry*`
 * copy rather than duplicating four near-identical i18n keys under
 * `settings.*`. */
const SHARE_EXPIRY_OPTIONS: { secs: number; labelKey: string }[] = [
  { secs: 3600, labelKey: "details.expiry1h" },
  { secs: 21600, labelKey: "details.expiry6h" },
  { secs: 86400, labelKey: "details.expiry24h" },
  { secs: 604800, labelKey: "details.expiry7d" },
];

function SectionTitle({ children, first }: { children: React.ReactNode; first?: boolean }) {
  return (
    <div
      className={`mb-3 text-[11px] font-semibold tracking-[0.6px] text-muted-foreground uppercase ${first ? "" : "mt-5"}`}
    >
      {children}
    </div>
  );
}

function Row({ label, children }: { label: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between py-2.5">
      <span className="text-[13.5px] text-fg2">{label}</span>
      {children}
    </div>
  );
}

/** Bounded +/- numeric stepper shared by the max-tasks and max-parts rows.
 * `onChange` only ever receives a value already inside `[min, max]` -- the
 * buttons clamp before calling it -- but callers still clamp again before
 * persisting, since this is also the value shown optimistically.
 * `disabled` is set by callers while their own persist is in flight so a
 * disabled button never fires `onClick` -- at most one persist per field can
 * ever be in flight, which rules out an out-of-order optimistic revert. */
function Stepper({
  value,
  min,
  max,
  onChange,
  disabled = false,
}: {
  value: number;
  min: number;
  max: number;
  onChange: (n: number) => void;
  disabled?: boolean;
}) {
  return (
    <div className="flex items-center gap-0.5 overflow-hidden rounded-[9px] border border-border bg-panel">
      <button
        type="button"
        onClick={() => onChange(Math.max(min, value - 1))}
        disabled={disabled}
        className="size-[30px] cursor-pointer text-base text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60"
      >
        −
      </button>
      <span className="w-[34px] text-center text-[13px] font-semibold tabular-nums">{value}</span>
      <button
        type="button"
        onClick={() => onChange(Math.min(max, value + 1))}
        disabled={disabled}
        className="size-[30px] cursor-pointer text-base text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60"
      >
        +
      </button>
    </div>
  );
}

export function SettingsModal() {
  const { t, i18n } = useTranslation();
  const {
    showSettings,
    closeSettings,
    themeMode,
    setThemeMode,
    defaultView,
    setDefaultView,
    transferSettings,
    setTransferSettings,
  } = useApp();
  const errorText = useErrorText();
  const queryClient = useQueryClient();
  const [resumeEnabled, setResumeEnabledState] = useState(true);
  // Real backend settings (M6c): fall back to the backend's own defaults
  // (see `Settings::default()` in `src-tauri/src/store/settings.rs`) until
  // `getSettings()` resolves below.
  const [maxTasks, setMaxTasksState] = useState(3);
  const [maxParts, setMaxPartsState] = useState(4);
  const [maxTasksPending, setMaxTasksPending] = useState(false);
  const [maxPartsPending, setMaxPartsPending] = useState(false);
  const [shareExpirySecs, setShareExpirySecsState] = useState(3600);
  const [cleanResult, setCleanResult] = useState<CleanResult | null>(null);
  const [cleanError, setCleanError] = useState<AppError | null>(null);
  const [cleanPending, setCleanPending] = useState(false);
  const [clearError, setClearError] = useState<AppError | null>(null);
  const [clearPending, setClearPending] = useState(false);

  useEffect(() => {
    let cancelled = false;
    getResumeEnabled()
      .then((enabled) => {
        if (!cancelled) setResumeEnabledState(enabled);
      })
      .catch((err) => {
        console.error("Failed to load resume-transfers setting", err);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    getSettings()
      .then((s) => {
        if (cancelled) return;
        setMaxTasksState(s.max_tasks);
        setMaxPartsState(s.max_parts);
        setShareExpirySecsState(s.share_expiry_secs);
      })
      .catch((err) => {
        console.error("Failed to load settings", err);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // The modal never unmounts (it self-gates below instead) so a stale
  // result/error banner from a previous open would otherwise reappear next
  // time the modal is shown. Reset them on close instead.
  useEffect(() => {
    if (!showSettings) {
      setCleanResult(null);
      setCleanError(null);
      setClearError(null);
    }
  }, [showSettings]);

  if (!showSettings) return null;

  function handleMaxTasksChange(n: number) {
    const clamped = Math.min(5, Math.max(1, n));
    const previous = maxTasks;
    setMaxTasksState(clamped);
    setMaxTasksPending(true);
    setMaxTasks(clamped)
      .catch((err) => {
        // Persist rejected: revert the optimistic local state, same pattern
        // as the resume-transfers switch below.
        setMaxTasksState(previous);
        console.error("Failed to persist max tasks", err);
      })
      .finally(() => setMaxTasksPending(false));
  }

  function handleMaxPartsChange(n: number) {
    const clamped = Math.min(8, Math.max(1, n));
    const previous = maxParts;
    setMaxPartsState(clamped);
    setMaxPartsPending(true);
    setMaxParts(clamped)
      .catch((err) => {
        setMaxPartsState(previous);
        console.error("Failed to persist max parts", err);
      })
      .finally(() => setMaxPartsPending(false));
  }

  function handleShareExpiryChange(secs: number) {
    const previous = shareExpirySecs;
    setShareExpirySecsState(secs);
    setShareExpiry(secs)
      .then(() => queryClient.invalidateQueries({ queryKey: ["settings"] }))
      .catch((err) => {
        setShareExpirySecsState(previous);
        console.error("Failed to persist share expiry", err);
      });
  }

  function handleCleanResidue() {
    setCleanError(null);
    setCleanResult(null);
    setCleanPending(true);
    cleanCheckpointResidue()
      .then((result) => setCleanResult(result))
      .catch((err: AppError) => setCleanError(err))
      .finally(() => setCleanPending(false));
  }

  // Drops the known-finished tasks locally from the shared transfer store,
  // the same pattern `TransferBar.handleClearFinished` uses -- so the
  // transfer panel reflects the clear immediately instead of waiting on a
  // `transfer://state` event that terminal tasks never re-emit.
  function handleClearHistory() {
    setClearError(null);
    setClearPending(true);
    clearFinishedTransfers()
      .then(() => {
        const { tasks, drop } = useTransferStore.getState();
        for (const [id, task] of Object.entries(tasks)) {
          if (task.status === "completed" || task.status === "canceled") drop(id);
        }
      })
      .catch((err: AppError) => setClearError(err))
      .finally(() => setClearPending(false));
  }

  const locale: AppLocale = i18n.language === "zh-CN" ? "zh-CN" : "en";

  return (
    <Modal onClose={closeSettings} className="w-[600px]">
      <div className="sticky top-0 z-1 flex items-center justify-between border-b border-border2 bg-background px-[22px] pt-5 pb-4">
        <div className="text-[17px] font-bold">{t("settings.title")}</div>
        <button
          type="button"
          onClick={closeSettings}
          className="flex size-[30px] cursor-pointer items-center justify-center rounded-lg text-muted-foreground hover:bg-hover hover:text-fg2"
        >
          <X className="size-[17px]" />
        </button>
      </div>
      <div className="px-[22px] py-5">
        <SectionTitle first>{t("settings.general")}</SectionTitle>
        <Row label={t("settings.theme")}>
          <Segmented<ThemeMode>
            value={themeMode}
            onChange={setThemeMode}
            options={[
              { value: "light", label: t("settings.themeLight") },
              { value: "dark", label: t("settings.themeDark") },
              { value: "system", label: t("settings.themeSystem") },
            ]}
          />
        </Row>
        <Row label={t("settings.language")}>
          <Segmented<AppLocale>
            value={locale}
            onChange={setLocale}
            options={[
              { value: "zh-CN", label: "中文" },
              { value: "en", label: "English" },
            ]}
          />
        </Row>
        <Row label={t("settings.defaultView")}>
          <Segmented<ViewMode>
            value={defaultView}
            onChange={setDefaultView}
            options={[
              { value: "list", label: t("main.listView") },
              { value: "grid", label: t("main.gridView") },
            ]}
          />
        </Row>
        <Row label={t("settings.shareExpiry")}>
          <select
            value={shareExpirySecs}
            onChange={(e) => handleShareExpiryChange(Number(e.target.value))}
            className="h-[30px] rounded-[7px] border border-border bg-background px-2 text-[12.5px] text-fg2 outline-none focus:border-primary"
          >
            {SHARE_EXPIRY_OPTIONS.map((opt) => (
              <option key={opt.secs} value={opt.secs}>
                {t(opt.labelKey)}
              </option>
            ))}
          </select>
        </Row>

        <SectionTitle>{t("settings.transfers")}</SectionTitle>
        <Row label={t("settings.concurrency")}>
          <Stepper
            value={maxTasks}
            min={1}
            max={5}
            onChange={handleMaxTasksChange}
            disabled={maxTasksPending}
          />
        </Row>
        <Row label={t("settings.maxParts")}>
          <Stepper
            value={maxParts}
            min={1}
            max={8}
            onChange={handleMaxPartsChange}
            disabled={maxPartsPending}
          />
        </Row>
        <div className="-mt-1 mb-1 text-[11.5px] text-muted-foreground">
          <div>{t("settings.concurrencyHint", { total: maxTasks * maxParts })}</div>
          <div>{t("settings.restartHint")}</div>
        </div>
        <Row label={t("settings.partSize")}>
          <Segmented<number>
            value={transferSettings.partSizeMb}
            onChange={(v) => setTransferSettings({ partSizeMb: v })}
            options={[
              { value: 8, label: "8 MB" },
              { value: 16, label: "16 MB" },
              { value: 64, label: "64 MB" },
            ]}
          />
        </Row>
        <Row label={t("settings.verify")}>
          <Switch
            checked={transferSettings.verify}
            onChange={(v) => setTransferSettings({ verify: v })}
          />
        </Row>
        <Row label={t("settings.overwrite")}>
          <Switch
            checked={transferSettings.overwrite}
            onChange={(v) => setTransferSettings({ overwrite: v })}
          />
        </Row>
        <Row
          label={
            <div>
              <div>{t("settings.resumeTransfers")}</div>
              <div className="mt-0.5 text-[11.5px] text-muted-foreground">
                {t("settings.resumeTransfersHint")}
              </div>
            </div>
          }
        >
          <Switch
            checked={resumeEnabled}
            onChange={(v) => {
              setResumeEnabledState(v);
              setResumeEnabled(v).catch((err) => {
                // The persist was rejected: revert the optimistic local state so
                // the switch never shows a value the backend/file did not take.
                setResumeEnabledState(!v);
                console.error("Failed to persist resume-transfers setting", err);
              });
            }}
          />
        </Row>

        <SectionTitle>{t("settings.advanced")}</SectionTitle>
        <Row
          label={
            <div>
              <div>{t("settings.cleanResidue")}</div>
              {cleanResult && (
                <div className="mt-0.5 text-[11.5px] text-muted-foreground">
                  {t("settings.cleanResidueDone", {
                    count: cleanResult.removed,
                    size: formatSize(cleanResult.freed_bytes),
                  })}
                </div>
              )}
              {cleanError && (
                <div className="mt-0.5 text-[11.5px] text-destructive">{errorText(cleanError)}</div>
              )}
            </div>
          }
        >
          <button
            type="button"
            onClick={handleCleanResidue}
            disabled={cleanPending}
            className="cursor-pointer rounded-lg border border-border px-[13px] py-[7px] text-[12.5px] font-medium text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60"
          >
            {t("settings.cleanResidue")}
          </button>
        </Row>
        <Row
          label={
            <div>
              <div>{t("settings.clearHistory")}</div>
              {clearError && (
                <div className="mt-0.5 text-[11.5px] text-destructive">{errorText(clearError)}</div>
              )}
            </div>
          }
        >
          <button
            type="button"
            onClick={handleClearHistory}
            disabled={clearPending}
            className="cursor-pointer rounded-lg border border-border px-[13px] py-[7px] text-[12.5px] font-medium text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60"
          >
            {t("settings.clearHistory")}
          </button>
        </Row>

        <SectionTitle>{t("settings.about")}</SectionTitle>
        <div className="flex items-center gap-3.5 pt-1.5 pb-1">
          <img
            src={logo}
            alt="BucketCat"
            className="size-[52px] rounded-[13px] shadow-[0_0_0_1px_var(--border)]"
          />
          <div className="flex-1">
            <div className="text-[15px] font-bold">
              {t("app.name")}{" "}
              <span className="ml-1 rounded-[20px] border border-border bg-panel px-[7px] py-px text-[11.5px] font-medium text-muted-foreground">
                v0.1.0
              </span>
            </div>
            <div className="mt-[3px] text-[12.5px] text-muted-foreground">{t("app.tagline")}</div>
          </div>
        </div>
        <div className="mt-3 flex gap-2.5">
          <a
            href="https://github.com/finch-xu/BucketCat"
            target="_blank"
            rel="noreferrer"
            className="rounded-lg border border-border px-[13px] py-[7px] text-[12.5px] text-fg2 hover:bg-hover"
          >
            {t("settings.github")}
          </a>
          <a
            href="#"
            className="rounded-lg border border-border px-[13px] py-[7px] text-[12.5px] text-fg2 hover:bg-hover"
          >
            {t("settings.checkUpdate")}
          </a>
        </div>
      </div>
    </Modal>
  );
}
