import { useEffect, useRef, useState, type ReactNode } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { Download, Link2, Share2, Trash2, X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { fileMeta } from "@/lib/file-meta";
import { extFromName, formatDate, formatSize } from "@/lib/format";
import { previewKind, type PreviewKind } from "@/lib/preview";
import { headObject, presignGet, type AppError, type ObjectHead } from "@/lib/api";
import { useBrowse } from "@/hooks/use-browse";
import { useErrorText } from "@/hooks/use-error-text";
import { useStartDownloads } from "@/hooks/use-start-downloads";
import { useApp } from "@/store/app-store";

/** Expiry choices offered by the Share dropdown, in seconds. Default (first
 * entry) is 1 hour -- the shortest-lived option, so a link that's forgotten
 * about doesn't stay valid for long. */
const EXPIRY_OPTIONS: { secs: number; labelKey: string }[] = [
  { secs: 3600, labelKey: "details.expiry1h" },
  { secs: 21600, labelKey: "details.expiry6h" },
  { secs: 86400, labelKey: "details.expiry24h" },
  { secs: 604800, labelKey: "details.expiry7d" },
];

/** Fixed expiry for inline preview URLs -- deliberately independent of the
 * Share dropdown's `expirySecs` state. The preview never leaves this screen,
 * so 1 hour is plenty and never varies with what the user picked for Share. */
const PREVIEW_EXPIRY_SECS = 3600;

/** In-flight/last-resolved state for the inline preview (image/video/audio/
 * text). Keyed by the entry it belongs to so a stale render (before the
 * effect below has caught up with a just-changed `entry`) can tell its own
 * `url`/`text` apart from a different entry's leftover state -- see the
 * `preview.key === entry.key` check at the call site. */
type PreviewState = {
  key: string;
  phase: "loading" | "ready" | "error";
  url: string | null;
  text: string | null;
};

const EMPTY_PREVIEW: PreviewState = { key: "", phase: "loading", url: null, text: null };

/** Shows the single selected file's real metadata. Download and delete are
 * wired (download queues a transfer via `useStartDownloads`, delete via the
 * object dialogs); ETag/Content-Type come from a `head_object` query and
 * Share generates a real presigned URL via `presign_get`. The `copyLink`
 * button stays a visual placeholder -- Share is the marquee action here. */
export function DetailsPanel() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const { selectedKeys, clearSelection, openDeleteObjects, activeConn, activeBucket } = useApp();
  const { entries } = useBrowse();
  const { startFileDownload, dialog } = useStartDownloads();

  const entry =
    selectedKeys.length === 1
      ? entries.find((e) => e.key === selectedKeys[0] && !e.is_prefix)
      : undefined;

  // Pure classification, safe to compute even when `entry` is undefined --
  // drives both the preview-fetch effect below and the render dispatch
  // after the early return.
  // `entry.size` is `number | null` in the type (null for prefixes), but the
  // `find` above already filters those out -- the `?? 0` is just to satisfy
  // the type, not a real fallback.
  const kind: PreviewKind = entry ? previewKind(entry.name, entry.size ?? 0) : "none";

  // Every hook below must run on every render (entry can be undefined) so
  // the early `return null` below it never changes the hook order.
  const [shareOpen, setShareOpen] = useState(false);
  const [expirySecs, setExpirySecs] = useState(EXPIRY_OPTIONS[0].secs);
  const [shareUrl, setShareUrl] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const copyTimeoutRef = useRef<number | null>(null);
  const [preview, setPreview] = useState<PreviewState>(EMPTY_PREVIEW);

  // Tracks the key of the entry currently on screen so an in-flight presign
  // can tell, once it resolves, whether the user has since switched to a
  // different entry. A ref (not state) so it reflects the latest render
  // synchronously -- `onSuccess` below reads it after an await, not during
  // a render, so state's one-render-behind semantics wouldn't work here.
  const currentKeyRef = useRef<string | undefined>(entry?.key);
  currentKeyRef.current = entry?.key;

  const headQuery = useQuery<ObjectHead, AppError>({
    queryKey: ["head", activeConn, activeBucket, entry?.key ?? ""],
    queryFn: () => headObject(activeConn, activeBucket, entry?.key ?? ""),
    enabled: entry !== undefined,
  });

  // The presigned URL only ever lives in this piece of state -- never
  // persisted, never logged. Switching to a different entry (or clearing
  // the selection) below discards it along with the rest of the share UI.
  // The mutation carries its target key in `variables` so `onSuccess` can
  // drop a result that resolves after the user has already moved on to a
  // different entry -- otherwise a slow presign for entry A could land on
  // entry B's screen once it finally resolves.
  const presignMutation = useMutation<string, AppError, { secs: number; key: string }>({
    mutationFn: ({ secs, key }) => presignGet(activeConn, activeBucket, key, secs),
    onSuccess: (url, variables) => {
      if (variables.key === currentKeyRef.current) setShareUrl(url);
    },
  });

  useEffect(() => {
    setShareOpen(false);
    setShareUrl(null);
    setCopied(false);
    presignMutation.reset();
    // Only re-run when the selected entry actually changes -- `presignMutation`
    // is a fresh object every render and would otherwise loop this effect.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [entry?.key]);

  useEffect(() => {
    return () => {
      if (copyTimeoutRef.current !== null) window.clearTimeout(copyTimeoutRef.current);
    };
  }, []);

  // Auto-fetches the inline preview for previewable kinds. Presigns a GET
  // URL (fixed 1h expiry, independent of the Share dropdown) and, for text,
  // follows up with a `fetch` of its content. Neither the URL nor the text
  // is ever logged or persisted -- both live only in `preview` state and are
  // discarded the moment `entry` changes or this component unmounts.
  //
  // Guards against the same race the Share flow guards against: this effect
  // captures `key` at the time it starts, and every `.then`/`.catch` checks
  // `currentKeyRef.current === key` before calling `setPreview` -- so a slow
  // presign/fetch for entry A that resolves after the user has already
  // switched to entry B is dropped instead of landing on B's screen.
  useEffect(() => {
    if (!entry || kind === "none") return;
    const key = entry.key;
    setPreview({ key, phase: "loading", url: null, text: null });

    presignGet(activeConn, activeBucket, key, PREVIEW_EXPIRY_SECS)
      .then((url) => {
        if (currentKeyRef.current !== key) return;
        if (kind !== "text") {
          setPreview({ key, phase: "ready", url, text: null });
          return;
        }
        return fetch(url)
          .then((res) => (res.ok ? res.text() : Promise.reject(new Error("preview fetch failed"))))
          .then((text) => {
            if (currentKeyRef.current !== key) return;
            setPreview({ key, phase: "ready", url: null, text });
          });
      })
      .catch(() => {
        if (currentKeyRef.current !== key) return;
        setPreview({ key, phase: "error", url: null, text: null });
      });
    // `kind` is derived from `entry` and only changes together with it, so
    // it doesn't need its own re-run trigger; `activeConn`/`activeBucket`
    // are stable for the lifetime of a given entry selection.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [entry?.key, kind]);

  if (!entry) return null;

  const ext = extFromName(entry.name);
  const meta = fileMeta("file", ext);
  const BigIcon = meta.icon;

  const handleCopy = () => {
    if (!shareUrl) return;
    navigator.clipboard
      .writeText(shareUrl)
      .then(() => {
        setCopied(true);
        if (copyTimeoutRef.current !== null) window.clearTimeout(copyTimeoutRef.current);
        copyTimeoutRef.current = window.setTimeout(() => setCopied(false), 2000);
      })
      .catch(() => {
        // Clipboard write failed (e.g. no permission granted in this
        // webview) -- the URL is still visible for manual selection, so
        // this is a silent no-op rather than a surfaced error.
      });
  };

  // `preview` can briefly still hold a previous entry's (or a stale
  // in-flight) result the render right after `entry` changes, before the
  // effect above has caught up -- checking the key here (in addition to the
  // effect's own `currentKeyRef` guard) means that render never shows it.
  const previewData = preview.key === entry.key ? preview : null;
  const handlePreviewMediaError = () => {
    setPreview((p) => (p.key === entry.key ? { ...p, phase: "error" } : p));
  };

  let previewBody: ReactNode;
  if (kind === "none") {
    previewBody = <BigIcon className="size-[46px]" style={{ color: meta.color }} />;
  } else if (!previewData || previewData.phase === "loading") {
    previewBody = (
      <span className="rounded-[7px] border border-border bg-background px-[11px] py-1.5 font-mono text-[11px] text-muted-foreground">
        {t("details.previewLoading")}
      </span>
    );
  } else if (previewData.phase === "error") {
    previewBody = (
      <div className="flex flex-col items-center gap-1.5">
        <BigIcon className="size-[46px]" style={{ color: meta.color }} />
        <span className="text-[10.5px] text-muted-foreground">{t("details.previewUnavailable")}</span>
      </div>
    );
  } else if (kind === "image") {
    previewBody = (
      <img
        src={previewData.url ?? undefined}
        loading="lazy"
        alt={entry.name}
        onError={handlePreviewMediaError}
        className="max-h-full max-w-full object-contain"
      />
    );
  } else if (kind === "video") {
    previewBody = (
      <video
        controls
        preload="metadata"
        src={previewData.url ?? undefined}
        onError={handlePreviewMediaError}
        className="max-h-full max-w-full"
      />
    );
  } else if (kind === "audio") {
    previewBody = (
      <audio
        controls
        src={previewData.url ?? undefined}
        onError={handlePreviewMediaError}
        className="w-full px-3"
      />
    );
  } else {
    previewBody = (
      <pre className="h-full w-full overflow-auto bg-background p-2 font-mono text-[11px] whitespace-pre-wrap break-all text-fg2">
        {previewData.text}
      </pre>
    );
  }

  return (
    <aside className="flex w-[300px] shrink-0 flex-col border-l border-border bg-background">
      <div className="flex h-[46px] shrink-0 items-center justify-between border-b border-border pr-3 pl-4">
        <span className="text-[13px] font-semibold">{t("details.title")}</span>
        <button
          type="button"
          onClick={clearSelection}
          className="flex size-[26px] cursor-pointer items-center justify-center rounded-[7px] text-muted-foreground hover:bg-hover hover:text-fg2"
        >
          <X className="size-3.5" />
        </button>
      </div>
      <div className="flex-1 overflow-y-auto p-4">
        <div className="flex h-[148px] w-full items-center justify-center overflow-hidden rounded-xl border border-border bg-[repeating-linear-gradient(45deg,var(--panel),var(--panel)_11px,var(--border2)_11px,var(--border2)_22px)]">
          {previewBody}
        </div>
        <div className="mt-3.5 text-[15px] leading-[1.35] font-semibold break-all">{entry.name}</div>
        <div className="mt-[3px] text-xs text-muted-foreground">{t(meta.labelKey)}</div>
        <div className="my-4 grid grid-cols-2 gap-2">
          <button
            type="button"
            onClick={() => startFileDownload(entry)}
            className="inline-flex h-[34px] cursor-pointer items-center justify-center gap-1.5 rounded-[9px] bg-primary text-[12.5px] font-semibold text-primary-foreground hover:bg-primary-strong"
          >
            <Download className="size-3.5" />
            {t("details.download")}
          </button>
          <button
            type="button"
            className="inline-flex h-[34px] cursor-pointer items-center justify-center gap-1.5 rounded-[9px] border border-border bg-background text-[12.5px] font-medium text-fg2 hover:bg-hover"
          >
            <Link2 className="size-3.5" />
            {t("details.copyLink")}
          </button>
          <button
            type="button"
            onClick={() => setShareOpen((open) => !open)}
            className={`inline-flex h-[34px] cursor-pointer items-center justify-center gap-1.5 rounded-[9px] border text-[12.5px] font-medium hover:bg-hover ${
              shareOpen ? "border-primary text-primary" : "border-border bg-background text-fg2"
            }`}
          >
            <Share2 className="size-3.5" />
            {t("details.share")}
          </button>
          <button
            type="button"
            onClick={() => openDeleteObjects([entry.key])}
            className="inline-flex h-[34px] cursor-pointer items-center justify-center gap-1.5 rounded-[9px] bg-destructive/10 text-[12.5px] font-medium text-destructive hover:bg-destructive/20"
          >
            <Trash2 className="size-3.5" />
            {t("details.delete")}
          </button>
        </div>
        {shareOpen && (
          <div className="mb-4 flex flex-col gap-2.5 rounded-[9px] border border-border bg-panel p-3">
            <div>
              <div className="mb-[5px] text-[10.5px] tracking-[0.4px] text-muted-foreground uppercase">
                {t("details.shareExpiry")}
              </div>
              <div className="flex items-center gap-2">
                <select
                  value={expirySecs}
                  onChange={(e) => setExpirySecs(Number(e.target.value))}
                  className="h-[30px] flex-1 rounded-[7px] border border-border bg-background px-2 text-[12px] text-fg2 outline-none focus:border-primary"
                >
                  {EXPIRY_OPTIONS.map((opt) => (
                    <option key={opt.secs} value={opt.secs}>
                      {t(opt.labelKey)}
                    </option>
                  ))}
                </select>
                <button
                  type="button"
                  onClick={() => presignMutation.mutate({ secs: expirySecs, key: entry.key })}
                  disabled={presignMutation.isPending}
                  className="h-[30px] shrink-0 cursor-pointer rounded-[7px] bg-primary px-3 text-[12px] font-semibold text-primary-foreground hover:bg-primary-strong disabled:cursor-not-allowed disabled:opacity-60"
                >
                  {presignMutation.isPending ? t("details.generating") : t("details.generateLink")}
                </button>
              </div>
            </div>
            {shareUrl && (
              <div className="flex items-center gap-2">
                <div
                  className="min-w-0 flex-1 truncate rounded-[7px] border border-border bg-background px-2 py-1.5 font-mono text-[11px] text-fg2"
                  title={shareUrl}
                >
                  {shareUrl}
                </div>
                <button
                  type="button"
                  onClick={handleCopy}
                  className="h-[30px] shrink-0 cursor-pointer rounded-[7px] border border-border bg-background px-2.5 text-[12px] font-medium text-fg2 hover:bg-hover"
                >
                  {copied ? t("details.copied") : t("details.copy")}
                </button>
              </div>
            )}
            {presignMutation.isError && (
              <p className="text-[11.5px] text-destructive">
                {t("details.shareFailed")}: {errorText(presignMutation.error)}
              </p>
            )}
          </div>
        )}
        <div className="flex flex-col gap-3 border-t border-border2 pt-3.5">
          <div>
            <div className="mb-[3px] text-[10.5px] tracking-[0.4px] text-muted-foreground uppercase">
              {t("details.objectKey")}
            </div>
            <div className="font-mono text-xs leading-[1.4] break-all text-fg2">{entry.key}</div>
          </div>
          <div className="flex justify-between">
            <span className="text-xs text-muted-foreground">{t("details.size")}</span>
            <span className="text-[12.5px] text-fg2 tabular-nums">{formatSize(entry.size)}</span>
          </div>
          <div className="flex justify-between">
            <span className="text-xs text-muted-foreground">{t("details.storageClass")}</span>
            <span className="text-[12.5px] text-fg2">{entry.storage_class ?? "—"}</span>
          </div>
          <div className="flex justify-between">
            <span className="text-xs text-muted-foreground">{t("details.modified")}</span>
            <span className="text-[12.5px] text-fg2 tabular-nums">
              {formatDate(entry.last_modified)}
            </span>
          </div>
          {/* ETag/Content-Type come from `head_object`, not the list response.
              A failed head degrades silently -- the rows above (from the
              already-loaded listing) still render either way. */}
          {headQuery.isLoading || headQuery.data ? (
            <>
              <div className="flex justify-between gap-3">
                <span className="shrink-0 text-xs text-muted-foreground">{t("details.etag")}</span>
                <span
                  className="truncate font-mono text-[11.5px] text-fg2"
                  title={headQuery.data?.etag ?? undefined}
                >
                  {headQuery.isLoading ? "—" : (headQuery.data?.etag ?? "—")}
                </span>
              </div>
              <div className="flex justify-between gap-3">
                <span className="shrink-0 text-xs text-muted-foreground">
                  {t("details.contentType")}
                </span>
                <span
                  className="truncate text-[12.5px] text-fg2"
                  title={headQuery.data?.content_type ?? undefined}
                >
                  {headQuery.isLoading ? "—" : (headQuery.data?.content_type ?? "—")}
                </span>
              </div>
            </>
          ) : null}
        </div>
      </div>
      {dialog}
    </aside>
  );
}
