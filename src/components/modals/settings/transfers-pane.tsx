import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Segmented } from "@/components/ui/segmented";
import { Switch } from "@/components/ui/switch";
import { useSettings } from "@/hooks/use-settings";
import { getResumeEnabled, setMaxParts, setMaxTasks, setResumeEnabled } from "@/lib/api";
import { useApp } from "@/store/app-store";
import { Row, Stepper } from "./shared";

export function TransfersPane() {
  const { t } = useTranslation();
  const { transferSettings, setTransferSettings } = useApp();
  const settingsQuery = useSettings();

  // Backend defaults (`Settings::default()` in
  // `src-tauri/src/store/settings.rs`) stand in until the query resolves.
  const [maxTasks, setMaxTasksState] = useState(3);
  const [maxParts, setMaxPartsState] = useState(4);
  const [maxTasksPending, setMaxTasksPending] = useState(false);
  const [maxPartsPending, setMaxPartsPending] = useState(false);
  const [resumeEnabled, setResumeEnabledState] = useState(true);

  // Seed once, then let the local copy lead -- see the same guard in
  // general-pane.tsx.
  const seededRef = useRef(false);
  useEffect(() => {
    if (seededRef.current || !settingsQuery.data) return;
    seededRef.current = true;
    setMaxTasksState(settingsQuery.data.max_tasks);
    setMaxPartsState(settingsQuery.data.max_parts);
  }, [settingsQuery.data]);

  // Deliberately NOT read from `useSettings()`: `get_resume_enabled` returns
  // the runtime `ResumeFlag` that actually gates checkpoint writing, whereas
  // `Settings.resume_enabled` is the persisted copy. The flag is the
  // authoritative current value, so read it directly.
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

  return (
    <div>
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
    </div>
  );
}
