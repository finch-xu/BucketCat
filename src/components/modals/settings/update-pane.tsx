import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { useQueryClient } from "@tanstack/react-query";
import { getVersion } from "@tauri-apps/api/app";
import { Loader2 } from "lucide-react";
import { Switch } from "@/components/ui/switch";
import { useErrorText } from "@/hooks/use-error-text";
import { settingsKey, useSettings } from "@/hooks/use-settings";
import { openExternal } from "@/lib/external-link";
import { formatSize } from "@/lib/format";
import {
  listUpdateSources,
  setAutoCheckUpdate,
  setUpdateSource,
  type AppError,
  type UpdateSourceDto,
} from "@/lib/api";
import { useTransferSummary } from "@/store/transfer-store";
import { useUpdater } from "@/store/updater-store";
import { Row } from "./shared";

const SECONDARY_BUTTON =
  "cursor-pointer rounded-lg border border-border px-[13px] py-[7px] text-[12.5px] font-medium text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60";

export function UpdatePane() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const queryClient = useQueryClient();
  const settingsQuery = useSettings();
  const { activeCount } = useTransferSummary();
  const { status, detected, progress, error, check, install, restart } = useUpdater();

  const [version, setVersion] = useState<string | null>(null);
  const [sources, setSources] = useState<UpdateSourceDto[]>([]);
  const [source, setSourceState] = useState<string>("");
  const [autoCheck, setAutoCheckState] = useState(true);
  const [sourceError, setSourceError] = useState<AppError | null>(null);

  useEffect(() => {
    let cancelled = false;
    getVersion()
      .then((v) => {
        if (!cancelled) setVersion(v);
      })
      .catch((err) => console.error("Failed to read the app version", err));
    listUpdateSources()
      .then((list) => {
        if (!cancelled) setSources(list);
      })
      .catch((err) => console.error("Failed to list update sources", err));
    return () => {
      cancelled = true;
    };
  }, []);

  // Seeded once, then local state wins -- the same contract `general-pane`
  // uses, so an optimistic value is never clobbered by a settings refetch
  // whose persist is still in flight.
  const seededRef = useRef(false);
  useEffect(() => {
    if (seededRef.current || !settingsQuery.data) return;
    seededRef.current = true;
    setSourceState(settingsQuery.data.update_source);
    setAutoCheckState(settingsQuery.data.auto_check_update);
  }, [settingsQuery.data]);

  function handleSourceChange(id: string) {
    const previous = source;
    setSourceState(id);
    setSourceError(null);
    setUpdateSource(id)
      .then(() => queryClient.invalidateQueries({ queryKey: settingsKey }))
      .catch((err: AppError) => {
        setSourceState(previous);
        setSourceError(err);
        console.error("Failed to persist the update source", err);
      });
  }

  function handleAutoCheckChange(enabled: boolean) {
    setAutoCheckState(enabled);
    setAutoCheckUpdate(enabled)
      .then(() => queryClient.invalidateQueries({ queryKey: settingsKey }))
      .catch((err) => {
        setAutoCheckState(!enabled);
        console.error("Failed to persist the auto-check setting", err);
      });
  }

  const selectedSource = sources.find((s) => s.id === source);
  const busy = status === "checking" || status === "downloading";
  // Installing swaps the app bundle out from under any running transfer, and
  // on Windows the NSIS installer terminates the process outright -- there is
  // no "finish the current part first". Blocking is the same instinct
  // `close_to_tray` follows by refusing to let a window close kill a transfer.
  const blockedByTransfers = activeCount > 0;

  return (
    <div>
      <Row label={t("settings.currentVersion")}>
        <span className="text-[12.5px] text-muted-foreground">{version ? `v${version}` : "—"}</span>
      </Row>

      <Row
        label={
          <div>
            <div>{t("settings.updateSource")}</div>
            {selectedSource && (
              <div className="mt-0.5 max-w-[360px] truncate text-[11.5px] text-muted-foreground">
                {selectedSource.manifest_url}
              </div>
            )}
            {sourceError && (
              <div className="mt-0.5 text-[11.5px] text-destructive">{errorText(sourceError)}</div>
            )}
          </div>
        }
      >
        <select
          value={source}
          onChange={(e) => handleSourceChange(e.target.value)}
          disabled={busy || sources.length === 0}
          className="h-[30px] rounded-[7px] border border-border bg-background px-2 text-[12.5px] text-fg2 outline-none focus:border-primary disabled:cursor-not-allowed disabled:opacity-60"
        >
          {sources.map((s) => (
            <option key={s.id} value={s.id}>
              {t(`settings.updateSourceName.${s.id}`, { defaultValue: s.id })}
            </option>
          ))}
        </select>
      </Row>

      <Row
        label={
          <div>
            <div>{t("settings.autoCheckUpdate")}</div>
            <div className="mt-0.5 text-[11.5px] text-muted-foreground">
              {t("settings.autoCheckUpdateHint")}
            </div>
          </div>
        }
      >
        <Switch checked={autoCheck} onChange={handleAutoCheckChange} />
      </Row>

      {/* Separated the way `about-pane` separates its link row: a section
        * title here would only repeat the button's own label. */}
      <div className="mt-3 border-t border-border2 pt-1" />
      <Row
        label={
          <div>
            <StatusLine
              status={status}
              version={detected?.version}
              progress={progress}
              error={error}
              errorText={errorText}
              t={t}
            />
            {status === "available" && blockedByTransfers && detected?.installable && (
              <div className="mt-0.5 text-[11.5px] text-muted-foreground">
                {t("settings.transfersBlockUpdate", { count: activeCount })}
              </div>
            )}
            {status === "available" && detected && !detected.installable && (
              <div className="mt-0.5 text-[11.5px] text-muted-foreground">
                {t("settings.manualDownloadHint")}
              </div>
            )}
          </div>
        }
      >
        <div className="flex shrink-0 items-center gap-2">
          {status === "available" && detected && !detected.installable && selectedSource && (
            <button
              type="button"
              onClick={() => void openExternal(selectedSource.release_page_url)}
              className={SECONDARY_BUTTON}
            >
              {t("settings.openReleasePage")}
            </button>
          )}
          {status === "available" && detected?.installable && (
            <button
              type="button"
              onClick={() => void install()}
              disabled={blockedByTransfers}
              className={SECONDARY_BUTTON}
            >
              {t("settings.downloadInstall")}
            </button>
          )}
          {status === "ready" && (
            <button type="button" onClick={() => void restart()} className={SECONDARY_BUTTON}>
              {t("settings.restartNow")}
            </button>
          )}
          <button
            type="button"
            onClick={() => void check()}
            disabled={busy}
            className={SECONDARY_BUTTON}
          >
            {status === "checking" && <Loader2 className="mr-1.5 inline size-3.5 animate-spin" />}
            {t("settings.checkUpdate")}
          </button>
        </div>
      </Row>

      {detected?.body && status !== "downloading" && (
        <div className="mt-1 mb-2 max-h-[132px] overflow-y-auto rounded-lg border border-border2 bg-panel px-3 py-2.5 text-[11.5px] leading-[1.6] whitespace-pre-wrap text-muted-foreground">
          {detected.body}
        </div>
      )}

      {status === "downloading" && <ProgressBar progress={progress} />}
    </div>
  );
}

/** The one-line verdict under the check button. Every branch is an inline
 * banner rather than a toast, matching the rest of the settings modal (the
 * app has no notification system). */
function StatusLine({
  status,
  version,
  progress,
  error,
  errorText,
  t,
}: {
  status: ReturnType<typeof useUpdater>["status"];
  version: string | undefined;
  progress: ReturnType<typeof useUpdater>["progress"];
  error: ReturnType<typeof useUpdater>["error"];
  errorText: (e: NonNullable<ReturnType<typeof useUpdater>["error"]>) => string;
  t: (key: string, opts?: Record<string, unknown>) => string;
}) {
  if (status === "checking") return <div>{t("settings.checking")}</div>;
  if (status === "up_to_date") return <div>{t("settings.upToDate")}</div>;
  if (status === "available")
    return <div>{t("settings.updateAvailable", { version: version ?? "" })}</div>;
  if (status === "downloading")
    return (
      <div>
        {t("settings.downloading", {
          done: formatSize(progress?.downloaded ?? 0),
          // `formatSize(null)` already renders the unknown-size placeholder,
          // which is what a server that sent no content length deserves.
          total: formatSize(progress?.total ?? null),
        })}
      </div>
    );
  if (status === "ready") return <div>{t("settings.updateReady")}</div>;
  if (status === "error" && error)
    return <div className="text-destructive">{errorText(error)}</div>;
  // `idle` is reachable in normal use: auto-check off, or a startup check that
  // failed and was deliberately swallowed (see `updater-store`).
  return <div className="text-muted-foreground">{t("settings.notChecked")}</div>;
}

function ProgressBar({ progress }: { progress: ReturnType<typeof useUpdater>["progress"] }) {
  const total = progress?.total ?? null;
  const done = progress?.downloaded ?? 0;
  // No content length means no honest percentage; a full-width bar at partial
  // opacity reads as "working" without claiming progress it cannot know.
  const pct = total && total > 0 ? Math.min(100, Math.round((done / total) * 100)) : null;
  return (
    <div className="mt-1 mb-2 h-1.5 w-full overflow-hidden rounded-full bg-hover">
      <div
        className={pct === null ? "h-full w-full bg-primary/40" : "h-full bg-primary"}
        style={pct === null ? undefined : { width: `${pct}%` }}
      />
    </div>
  );
}
