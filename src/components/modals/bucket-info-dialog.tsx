import { useQuery } from "@tanstack/react-query";
import { CheckCircle2, ExternalLink, Loader2, X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Modal } from "@/components/ui/modal";
import { formatSize } from "@/lib/format";
import { r2BucketInfo, type AppError, type R2BucketInfo } from "@/lib/api";
import { useErrorText } from "@/hooks/use-error-text";

/** One label/value row. `muted` renders the value as explanatory copy rather
 * than data — used for every "unavailable, and here's why" state, so those
 * never read as if they were the bucket's actual configuration. */
function Row({
  label,
  children,
  muted,
}: {
  label: string;
  children: React.ReactNode;
  muted?: boolean;
}) {
  return (
    <div className="flex items-start gap-3 py-[5px]">
      <span className="w-[76px] shrink-0 text-[12px] text-muted-foreground">{label}</span>
      <span
        className={
          muted
            ? "min-w-0 flex-1 text-[12px] text-muted-foreground"
            : "min-w-0 flex-1 text-[12.5px] break-all text-foreground"
        }
      >
        {children}
      </span>
    </div>
  );
}

/**
 * Read-only metadata panel for one Cloudflare R2 bucket.
 *
 * Everything here degrades independently, which is the whole design: the
 * location hint comes from the S3 plane and is available even to an
 * object-scoped token, while usage and public-access info need a Cloudflare
 * API token and can be refused on their own. A refusal is reported as a
 * *reason*, not an error state — the dialog still opens and still shows what
 * it could get.
 */
export function BucketInfoDialog({
  connectionId,
  bucket,
  onClose,
}: {
  connectionId: string;
  bucket: string;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const errorText = useErrorText();

  const query = useQuery<R2BucketInfo, AppError>({
    queryKey: ["r2-bucket-info", connectionId, bucket],
    queryFn: () => r2BucketInfo(connectionId, bucket),
    // Usage counters move constantly, so a cached panel would show stale
    // numbers on reopen; the call is cheap and only fires on an explicit click.
    staleTime: 0,
  });

  const info = query.data;

  /** Why the Cloudflare-API-sourced rows are missing, or null when they are
   * present. Distinguishes "no token configured" (an actionable setup gap)
   * from "the token was refused" (a permissions fact). */
  const apiUnavailable = (): string | null => {
    if (!info) return null;
    if (!info.has_api_token) return t("bucketInfo.noToken");
    if (info.api_error) {
      return info.api_error === "auth/access-denied"
        ? t("bucketInfo.tokenDenied")
        : errorText({ code: info.api_error, params: {} });
    }
    return null;
  };
  const unavailable = apiUnavailable();

  return (
    <Modal onClose={onClose} className="w-[420px]">
      <div className="flex items-start justify-between px-[22px] pt-[22px] pb-1">
        <div className="min-w-0">
          <div className="truncate text-[15px] font-bold">{bucket}</div>
          <div className="mt-[3px] text-[12.5px] text-muted-foreground">
            {t("bucketInfo.title")}
          </div>
        </div>
        <button
          type="button"
          onClick={onClose}
          className="flex size-[30px] shrink-0 cursor-pointer items-center justify-center rounded-lg text-muted-foreground hover:bg-hover hover:text-fg2"
        >
          <X className="size-[17px]" />
        </button>
      </div>

      <div className="px-[22px] pt-3 pb-5">
        {query.isPending && (
          <div className="flex items-center gap-2 py-4 text-[12.5px] text-muted-foreground">
            <Loader2 className="size-3.5 animate-spin" />
            {t("bucketInfo.loading")}
          </div>
        )}

        {query.isError && (
          <div className="py-4 text-[12.5px] text-destructive">{errorText(query.error)}</div>
        )}

        {info && (
          <>
            <Row label={t("bucketInfo.location")} muted={!info.location}>
              {info.location ?? t("bucketInfo.locationUnknown")}
            </Row>
            <Row
              label={t("bucketInfo.jurisdiction")}
              muted={!info.meta?.jurisdiction}
            >
              {info.meta?.jurisdiction ?? unavailable ?? "—"}
            </Row>
            <Row label={t("bucketInfo.storageClass")} muted={!info.meta?.storage_class}>
              {info.meta?.storage_class ?? unavailable ?? "—"}
            </Row>

            <div className="my-2.5 border-t border-border2" />

            <Row label={t("bucketInfo.objects")} muted={!info.usage}>
              {info.usage ? info.usage.object_count.toLocaleString() : (unavailable ?? "—")}
            </Row>
            <Row label={t("bucketInfo.size")} muted={!info.usage}>
              {info.usage ? formatSize(info.usage.payload_size) : (unavailable ?? "—")}
            </Row>

            <div className="my-2.5 border-t border-border2" />

            <Row label={t("bucketInfo.publicUrl")} muted={!info.managed_domain}>
              {info.managed_domain ? (
                info.managed_domain.enabled ? (
                  <span className="inline-flex items-center gap-1.5">
                    <CheckCircle2 className="size-[13px] shrink-0 text-emerald-600 dark:text-emerald-400" />
                    <span className="font-mono">{info.managed_domain.domain}</span>
                  </span>
                ) : (
                  <span className="text-muted-foreground">{t("bucketInfo.r2devOff")}</span>
                )
              ) : (
                (unavailable ?? "—")
              )}
            </Row>
            <Row
              label={t("bucketInfo.customDomains")}
              muted={!info.custom_domains || info.custom_domains.length === 0}
            >
              {!info.custom_domains ? (
                (unavailable ?? "—")
              ) : info.custom_domains.length === 0 ? (
                t("bucketInfo.noCustomDomains")
              ) : (
                <span className="flex flex-col gap-1">
                  {info.custom_domains.map((d) => (
                    <span key={d.domain} className="inline-flex items-center gap-1.5">
                      <ExternalLink className="size-[13px] shrink-0 text-muted-foreground" />
                      <span className="font-mono">{d.domain}</span>
                      {d.enabled && d.ssl_status === "active" ? (
                        <CheckCircle2 className="size-[13px] shrink-0 text-emerald-600 dark:text-emerald-400" />
                      ) : (
                        <span className="text-[11px] text-muted-foreground">
                          {t("bucketInfo.domainPending")}
                        </span>
                      )}
                    </span>
                  ))}
                </span>
              )}
            </Row>

            {unavailable && (
              <p className="mt-3 border-t border-border2 pt-2.5 text-[11.5px] text-muted-foreground">
                {unavailable}
              </p>
            )}
          </>
        )}
      </div>
    </Modal>
  );
}
