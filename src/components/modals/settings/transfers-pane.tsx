import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Segmented } from "@/components/ui/segmented";
import { Switch } from "@/components/ui/switch";
import { settingsKey, useSettings } from "@/hooks/use-settings";
import {
  getResumeEnabled,
  setMaxParts,
  setMaxTasks,
  setResumeEnabled,
  setTransferPreset,
  setTransferTuning,
  type Settings,
  type TransferTuningPatch,
} from "@/lib/api";
import { useApp } from "@/store/app-store";
import { Row, SectionTitle, Stepper } from "./shared";

const MB = 1024 * 1024;

/** Upload/download threshold choices, in MB (spec §4.7). Both directions
 * share the same list -- the backend clamps to the identical `[16MB, 1GB]`
 * range for either field (`clamp_threshold`). */
const THRESHOLD_OPTIONS_MB = [16, 32, 64, 128, 256, 512, 1024];

/** Upload part / download chunk floor choices, in MB. Both directions share
 * the same list -- the backend clamps to the identical `[8MB, 256MB]` range
 * (`clamp_part_floor`). Target part/chunk counts are never exposed in the UI
 * (spec §4.7). */
const PART_OPTIONS_MB = [8, 16, 32, 64, 128, 256];

/** Mirrors `Settings.transfer_preset` on the wire: one of the three built-in
 * presets, or `"custom"` once any advanced field has been hand-edited. */
type TransferPreset = "conservative" | "balanced" | "aggressive" | "custom";

export function TransfersPane() {
  const { t } = useTranslation();
  const { transferSettings, setTransferSettings } = useApp();
  const queryClient = useQueryClient();
  const settingsQuery = useSettings();

  // Backend defaults (`Settings::default()` in
  // `src-tauri/src/store/settings.rs`) stand in until the query resolves.
  const [maxTasks, setMaxTasksState] = useState(3);
  const [maxParts, setMaxPartsState] = useState(4);
  const [maxTasksPending, setMaxTasksPending] = useState(false);
  const [maxPartsPending, setMaxPartsPending] = useState(false);
  const [resumeEnabled, setResumeEnabledState] = useState(true);

  const [preset, setPresetState] = useState<TransferPreset>("balanced");
  const [presetPending, setPresetPending] = useState(false);
  const [uploadThresholdMb, setUploadThresholdMb] = useState(32);
  const [uploadPartFloorMb, setUploadPartFloorMb] = useState(16);
  const [downloadThresholdMb, setDownloadThresholdMb] = useState(64);
  const [downloadChunkFloorMb, setDownloadChunkFloorMb] = useState(32);
  const [tuningPending, setTuningPending] = useState(false);

  // Seed once, then let the local copy lead -- see the same guard in
  // general-pane.tsx.
  const seededRef = useRef(false);
  useEffect(() => {
    if (seededRef.current || !settingsQuery.data) return;
    seededRef.current = true;
    const data = settingsQuery.data;
    setMaxTasksState(data.max_tasks);
    setMaxPartsState(data.max_parts);
    setPresetState(data.transfer_preset as TransferPreset);
    setUploadThresholdMb(Math.round(data.upload_threshold / MB));
    setUploadPartFloorMb(Math.round(data.upload_part_floor / MB));
    setDownloadThresholdMb(Math.round(data.download_threshold / MB));
    setDownloadChunkFloorMb(Math.round(data.download_chunk_floor / MB));
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
      .then(() => queryClient.invalidateQueries({ queryKey: settingsKey }))
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
      .then(() => queryClient.invalidateQueries({ queryKey: settingsKey }))
      .catch((err) => {
        setMaxPartsState(previous);
        console.error("Failed to persist max parts", err);
      })
      .finally(() => setMaxPartsPending(false));
  }

  /** Selecting a built-in preset also rewrites `max_tasks`/`max_parts` and
   * every advanced tuning field on the backend (spec §4.2) -- so, unlike the
   * single-field handlers below, a successful persist here re-seeds every
   * one of those local copies from the freshly invalidated query cache
   * rather than just flipping the one value the user touched. */
  function handlePresetChange(name: "conservative" | "balanced" | "aggressive") {
    const previous = {
      preset,
      maxTasks,
      maxParts,
      uploadThresholdMb,
      uploadPartFloorMb,
      downloadThresholdMb,
      downloadChunkFloorMb,
    };
    setPresetState(name);
    setPresetPending(true);
    setTransferPreset(name)
      .then(() => queryClient.invalidateQueries({ queryKey: settingsKey }))
      .then(() => {
        const data = queryClient.getQueryData<Settings>(settingsKey);
        if (!data) return;
        setMaxTasksState(data.max_tasks);
        setMaxPartsState(data.max_parts);
        setUploadThresholdMb(Math.round(data.upload_threshold / MB));
        setUploadPartFloorMb(Math.round(data.upload_part_floor / MB));
        setDownloadThresholdMb(Math.round(data.download_threshold / MB));
        setDownloadChunkFloorMb(Math.round(data.download_chunk_floor / MB));
      })
      .catch((err) => {
        setPresetState(previous.preset);
        setMaxTasksState(previous.maxTasks);
        setMaxPartsState(previous.maxParts);
        setUploadThresholdMb(previous.uploadThresholdMb);
        setUploadPartFloorMb(previous.uploadPartFloorMb);
        setDownloadThresholdMb(previous.downloadThresholdMb);
        setDownloadChunkFloorMb(previous.downloadChunkFloorMb);
        console.error("Failed to persist transfer preset", err);
      })
      .finally(() => setPresetPending(false));
  }

  /** Shared by the four advanced-tuning selects: persists one field (in
   * bytes, converted from the select's MB value), and -- since the backend
   * always flips `transfer_preset` to `"custom"` the moment any of these is
   * hand-edited -- optimistically reflects that in the preset segmented
   * control too. */
  function handleTuningFieldChange(
    field: keyof TransferTuningPatch,
    mb: number,
    currentMb: number,
    setMb: (mb: number) => void,
  ) {
    const previousPreset = preset;
    setMb(mb);
    setPresetState("custom");
    setTuningPending(true);
    setTransferTuning({ [field]: mb * MB })
      .then(() => queryClient.invalidateQueries({ queryKey: settingsKey }))
      .catch((err) => {
        setMb(currentMb);
        setPresetState(previousPreset);
        console.error(`Failed to persist ${field}`, err);
      })
      .finally(() => setTuningPending(false));
  }

  return (
    <div>
      <Row label={t("settings.preset")}>
        <Segmented<TransferPreset>
          value={preset}
          onChange={(v) => {
            if (v === "custom") return;
            handlePresetChange(v);
          }}
          options={[
            {
              value: "conservative",
              label: t("settings.presetConservative"),
              disabled: presetPending,
            },
            { value: "balanced", label: t("settings.presetBalanced"), disabled: presetPending },
            {
              value: "aggressive",
              label: t("settings.presetAggressive"),
              disabled: presetPending,
            },
            { value: "custom", label: t("settings.presetCustom"), disabled: true },
          ]}
        />
      </Row>
      <div className="-mt-1 mb-1 text-[11.5px] text-muted-foreground">
        {t("settings.presetHint")}
      </div>

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
        {t("settings.concurrencyHint", { total: maxTasks * maxParts })}
      </div>

      <SectionTitle>{t("settings.advancedTuning")}</SectionTitle>
      <Row label={t("settings.uploadThreshold")}>
        <select
          value={uploadThresholdMb}
          disabled={tuningPending}
          onChange={(e) =>
            handleTuningFieldChange(
              "upload_threshold",
              Number(e.target.value),
              uploadThresholdMb,
              setUploadThresholdMb,
            )
          }
          className="h-[30px] rounded-[7px] border border-border bg-background px-2 text-[12.5px] text-fg2 outline-none focus:border-primary"
        >
          {THRESHOLD_OPTIONS_MB.map((mb) => (
            <option key={mb} value={mb}>
              {mb} MB
            </option>
          ))}
        </select>
      </Row>
      <div className="-mt-1 mb-1 text-[11.5px] text-muted-foreground">
        {t("settings.thresholdHint")}
      </div>
      <Row label={t("settings.uploadPartSize")}>
        <select
          value={uploadPartFloorMb}
          disabled={tuningPending}
          onChange={(e) =>
            handleTuningFieldChange(
              "upload_part_floor",
              Number(e.target.value),
              uploadPartFloorMb,
              setUploadPartFloorMb,
            )
          }
          className="h-[30px] rounded-[7px] border border-border bg-background px-2 text-[12.5px] text-fg2 outline-none focus:border-primary"
        >
          {PART_OPTIONS_MB.map((mb) => (
            <option key={mb} value={mb}>
              {mb} MB
            </option>
          ))}
        </select>
      </Row>
      <div className="-mt-1 mb-1 text-[11.5px] text-muted-foreground">
        {t("settings.partSizeHint")}
      </div>
      <Row label={t("settings.downloadThreshold")}>
        <select
          value={downloadThresholdMb}
          disabled={tuningPending}
          onChange={(e) =>
            handleTuningFieldChange(
              "download_threshold",
              Number(e.target.value),
              downloadThresholdMb,
              setDownloadThresholdMb,
            )
          }
          className="h-[30px] rounded-[7px] border border-border bg-background px-2 text-[12.5px] text-fg2 outline-none focus:border-primary"
        >
          {THRESHOLD_OPTIONS_MB.map((mb) => (
            <option key={mb} value={mb}>
              {mb} MB
            </option>
          ))}
        </select>
      </Row>
      <div className="-mt-1 mb-1 text-[11.5px] text-muted-foreground">
        {t("settings.thresholdHint")}
      </div>
      <Row label={t("settings.downloadChunkSize")}>
        <select
          value={downloadChunkFloorMb}
          disabled={tuningPending}
          onChange={(e) =>
            handleTuningFieldChange(
              "download_chunk_floor",
              Number(e.target.value),
              downloadChunkFloorMb,
              setDownloadChunkFloorMb,
            )
          }
          className="h-[30px] rounded-[7px] border border-border bg-background px-2 text-[12.5px] text-fg2 outline-none focus:border-primary"
        >
          {PART_OPTIONS_MB.map((mb) => (
            <option key={mb} value={mb}>
              {mb} MB
            </option>
          ))}
        </select>
      </Row>
      <div className="-mt-1 mb-1 text-[11.5px] text-muted-foreground">
        {t("settings.partSizeHint")}
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
            setResumeEnabled(v)
              .then(() => queryClient.invalidateQueries({ queryKey: settingsKey }))
              .catch((err) => {
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
