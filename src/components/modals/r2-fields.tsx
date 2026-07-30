import { CheckCircle2, Loader2, Lock, Search, XCircle } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Segmented } from "@/components/ui/segmented";
import { Select } from "@/components/ui/select";
import { useErrorText } from "@/hooks/use-error-text";
import type { AppError } from "@/lib/api";
import { r2ProbeToken } from "@/lib/api";
import { R2_JURISDICTIONS, R2_REGION, parseR2Endpoint, r2Endpoint } from "@/lib/r2";
import { cn } from "@/lib/utils";

/** How the user is supplying credentials for an R2 connection. */
export type R2CredMode = "token" | "keys";

/** Stand-in value for the default jurisdiction inside the `<Select>` only.
 *
 * The default jurisdiction's canonical id is the empty string -- correctly so,
 * since it is the absence of a hostname label -- but Radix Select reserves
 * `""` to mean "nothing selected", so an item with that value renders as the
 * placeholder and can never be chosen. Same class of problem, and same fix, as
 * `REGION_KEEP_CURRENT` in `region-picker.tsx`.
 *
 * The mapping lives here and nowhere else: `@/lib/r2` and its Rust twin both
 * keep `""` as the real id, so nothing downstream has to know this sentinel
 * exists. Underscores make it collision-proof by construction: a jurisdiction
 * id has to be a valid hostname label, so no real one can ever look like
 * this. */
const JURISDICTION_DEFAULT = "__default__";

const toSelectValue = (jurisdiction: string) => jurisdiction || JURISDICTION_DEFAULT;
const fromSelectValue = (value: string) => (value === JURISDICTION_DEFAULT ? "" : value);

const INPUT_CLASS =
  "h-9 w-full rounded-[9px] border border-border bg-panel px-3 text-[13px] text-foreground outline-none focus:border-primary focus:ring-[3px] focus:ring-primary-soft";
const INPUT_ERROR_CLASS = "border-destructive";
const READONLY_INPUT_CLASS =
  "h-9 w-full cursor-default rounded-[9px] border border-border bg-hover px-3 text-[13px] text-fg2 outline-none";

/** Outcome of the "probe token" round trip. `accountFound` distinguishes the
 * two successful shapes: an admin-tier token that filled the account id in for
 * the user, and a low-privilege one that could not — see `r2ProbeToken`. */
type ProbeStatus =
  | { kind: "idle" }
  | { kind: "pending" }
  | { kind: "done"; accountFound: boolean }
  | { kind: "error"; error: AppError };

/**
 * The R2-specific block of the connection form.
 *
 * R2 deliberately does **not** use `regionCatalog` (`@/lib/regions`) the way
 * OSS/Qiniu/Rainyun do. A catalog maps a region id to a fixed endpoint, but
 * R2's host depends on the *account*, and its `eu`/`fedramp` jurisdictions are
 * separate namespaces rather than alternate routes to the same buckets — so
 * "pick a region, get an endpoint" is the wrong shape entirely.
 *
 * Like `RegionPicker`, every persisted value lives in the parent form; the
 * only state held here is the transient probe status, which is pure UI.
 *
 * ## Why account id and jurisdiction are parent state, not derived
 *
 * They look derivable from the endpoint via `parseR2Endpoint`, but they are
 * not: while the user is clearing the account field the endpoint passes
 * through `https://.r2.cloudflarestorage.com`, which correctly fails to parse
 * — so a derived account id would snap back to whatever it was and the field
 * could never be emptied. The parent holds both and derives the endpoint from
 * them, which is the direction that has no such intermediate state.
 */
export function R2Fields({
  credMode,
  onCredModeChange,
  apiToken,
  onApiTokenChange,
  accountId,
  onAccountIdChange,
  jurisdiction,
  onJurisdictionChange,
  accessKeyId,
  onAccessKeyIdChange,
  secretAccessKey,
  onSecretAccessKeyChange,
  isEdit,
  hasApiToken,
  fieldErrors,
}: {
  credMode: R2CredMode;
  onCredModeChange: (mode: R2CredMode) => void;
  apiToken: string;
  onApiTokenChange: (token: string) => void;
  accountId: string;
  onAccountIdChange: (accountId: string) => void;
  jurisdiction: string;
  onJurisdictionChange: (jurisdiction: string) => void;
  accessKeyId: string;
  onAccessKeyIdChange: (accessKeyId: string) => void;
  secretAccessKey: string;
  onSecretAccessKeyChange: (secret: string) => void;
  isEdit: boolean;
  hasApiToken: boolean;
  fieldErrors: { endpoint?: string; access_key_id?: string; secret_access_key?: string };
}) {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const [probe, setProbe] = useState<ProbeStatus>({ kind: "idle" });

  const endpoint = accountId.trim() ? r2Endpoint(accountId, jurisdiction) : "";

  /** Accepts a pasted endpoint URL as well as a bare account id — the URL is
   * what the R2 dashboard actually shows, so it is what users copy. Pasting
   * one also sets the jurisdiction, which is otherwise easy to miss. */
  function handleAccountInput(value: string) {
    const parsed = parseR2Endpoint(value);
    if (parsed) {
      onAccountIdChange(parsed.accountId);
      onJurisdictionChange(parsed.jurisdiction);
      return;
    }
    onAccountIdChange(value);
  }

  async function handleProbe() {
    const token = apiToken.trim();
    if (!token) return;
    setProbe({ kind: "pending" });
    try {
      const result = await r2ProbeToken(token);
      onAccessKeyIdChange(result.access_key_id);
      // An empty account list is a success, not an error: object-scoped
      // tokens cannot enumerate accounts. Only prefill when there is exactly
      // one — with several, guessing would silently point the connection at
      // the wrong account.
      const only = result.accounts.length === 1 ? result.accounts[0] : undefined;
      if (only) onAccountIdChange(only.id);
      setProbe({ kind: "done", accountFound: Boolean(only) });
    } catch (err) {
      setProbe({ kind: "error", error: err as AppError });
    }
  }

  return (
    <>
      <div className="mb-3.5">
        <label className="mb-1.5 block text-xs font-medium text-fg2">
          {t("r2.credMode")}
        </label>
        <Segmented<R2CredMode>
          value={credMode}
          onChange={onCredModeChange}
          options={[
            { value: "token", label: t("r2.credModeToken") },
            { value: "keys", label: t("r2.credModeKeys") },
          ]}
        />
        <p className="mt-1 text-[11.5px] text-muted-foreground">
          {credMode === "token" ? t("r2.credModeTokenHint") : t("r2.credModeKeysHint")}
        </p>
      </div>

      {credMode === "token" ? (
        <>
          <div className="mb-3.5">
            <label className="mb-1.5 block text-xs font-medium text-fg2">
              {t("r2.apiToken")}
            </label>
            <div className="flex gap-2">
              <div className="flex h-9 flex-1 items-center gap-2 rounded-[9px] border border-border bg-panel px-3 focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary-soft">
                <Lock className="size-3.5 text-muted-foreground" />
                <input
                  type="password"
                  value={apiToken}
                  onChange={(e) => {
                    onApiTokenChange(e.target.value);
                    if (probe.kind !== "idle") setProbe({ kind: "idle" });
                  }}
                  placeholder={
                    isEdit && hasApiToken ? t("r2.apiTokenKeep") : "cfut_••••••••••••••••"
                  }
                  className="flex-1 border-none bg-transparent font-mono text-[13px] text-foreground outline-none"
                />
              </div>
              <button
                type="button"
                onClick={handleProbe}
                disabled={probe.kind === "pending" || !apiToken.trim()}
                className="inline-flex h-9 shrink-0 cursor-pointer items-center gap-[7px] rounded-[9px] border border-border bg-background px-3.5 text-[13px] font-medium text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60"
              >
                {probe.kind === "pending" ? (
                  <Loader2 className="size-3.5 animate-spin text-muted-foreground" />
                ) : (
                  <Search className="size-3.5 text-muted-foreground" />
                )}
                {t("r2.probe")}
              </button>
            </div>
            <div className="mt-1 text-[11.5px]">
              {probe.kind === "idle" && (
                <span className="text-muted-foreground">
                  {isEdit && hasApiToken ? t("r2.apiTokenKeep") : t("r2.apiTokenHint")}
                </span>
              )}
              {probe.kind === "pending" && (
                <span className="text-muted-foreground">{t("r2.probing")}</span>
              )}
              {probe.kind === "done" && (
                <span className="inline-flex items-center gap-1.5 text-emerald-600 dark:text-emerald-400">
                  <CheckCircle2 className="size-[13px] shrink-0" />
                  {probe.accountFound ? t("r2.probeOk") : t("r2.probeOkNoAccount")}
                </span>
              )}
              {probe.kind === "error" && (
                <span className="inline-flex min-w-0 items-center gap-1.5 text-destructive">
                  <XCircle className="size-[13px] shrink-0" />
                  <span className="truncate">
                    {t("r2.probeFail")}: {errorText(probe.error)}
                  </span>
                </span>
              )}
            </div>
          </div>

          <div className="mb-3.5">
            <label className="mb-1.5 block text-xs font-medium text-fg2">
              {t("addConn.accessKey")}
            </label>
            <input
              value={accessKeyId}
              readOnly
              placeholder={t("r2.accessKeyDerived")}
              className={cn(READONLY_INPUT_CLASS, "font-mono")}
            />
            {fieldErrors.access_key_id && (
              <p className="mt-1 text-[11.5px] text-destructive">{fieldErrors.access_key_id}</p>
            )}
          </div>
        </>
      ) : (
        <>
          <div className="mb-3.5">
            <label className="mb-1.5 block text-xs font-medium text-fg2">
              {t("addConn.accessKey")}
            </label>
            <input
              value={accessKeyId}
              onChange={(e) => onAccessKeyIdChange(e.target.value)}
              placeholder="••••••••••••••••••••••••••••••••"
              className={cn(
                INPUT_CLASS,
                "font-mono",
                fieldErrors.access_key_id && INPUT_ERROR_CLASS,
              )}
            />
          </div>
          <div className="mb-3.5">
            <label className="mb-1.5 block text-xs font-medium text-fg2">
              {t("addConn.secretKey")}
            </label>
            <div
              className={cn(
                "flex h-9 items-center gap-2 rounded-[9px] border border-border bg-panel px-3 focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary-soft",
                fieldErrors.secret_access_key && INPUT_ERROR_CLASS,
              )}
            >
              <Lock className="size-3.5 text-muted-foreground" />
              <input
                type="password"
                value={secretAccessKey}
                onChange={(e) => onSecretAccessKeyChange(e.target.value)}
                placeholder={isEdit ? t("addConn.secretKeep") : "••••••••••••••••••••"}
                className="flex-1 border-none bg-transparent font-mono text-[13px] text-foreground outline-none"
              />
            </div>
            {fieldErrors.secret_access_key && (
              <p className="mt-1 text-[11.5px] text-destructive">
                {fieldErrors.secret_access_key}
              </p>
            )}
          </div>
        </>
      )}

      <div className="mb-3.5">
        <label className="mb-1.5 block text-xs font-medium text-fg2">
          {t("r2.accountId")}
        </label>
        <input
          value={accountId}
          onChange={(e) => handleAccountInput(e.target.value)}
          placeholder="a1b2c3d4e5f60718293a4b5c6d7e8f90"
          className={cn(INPUT_CLASS, "font-mono", fieldErrors.endpoint && INPUT_ERROR_CLASS)}
        />
        {/* The endpoint is derived and read-only, so an "endpoint missing"
            error belongs on the field the user can actually fix. */}
        {fieldErrors.endpoint ? (
          <p className="mt-1 text-[11.5px] text-destructive">{fieldErrors.endpoint}</p>
        ) : (
          <p className="mt-1 text-[11.5px] text-muted-foreground">{t("r2.accountIdHint")}</p>
        )}
      </div>

      <div className="mb-3.5">
        <label className="mb-1.5 block text-xs font-medium text-fg2">
          {t("r2.jurisdiction")}
        </label>
        <Select
          value={toSelectValue(jurisdiction)}
          onChange={(v) => onJurisdictionChange(fromSelectValue(v))}
          options={R2_JURISDICTIONS.map((j) => ({
            value: toSelectValue(j.id),
            label: t(j.labelKey),
          }))}
        />
        <p className="mt-1 text-[11.5px] text-muted-foreground">{t("r2.jurisdictionHint")}</p>
      </div>

      <div className="mb-3.5 grid grid-cols-[2fr_1fr] gap-3">
        <div>
          <label className="mb-1.5 block text-xs font-medium text-fg2">
            {t("addConn.endpoint")}
          </label>
          <input
            value={endpoint}
            readOnly
            placeholder={t("r2.endpointNeedsAccount")}
            className={cn(READONLY_INPUT_CLASS, "font-mono")}
          />
        </div>
        <div>
          <label className="mb-1.5 block text-xs font-medium text-fg2">
            {t("addConn.region")}
          </label>
          <input value={R2_REGION} readOnly className={cn(READONLY_INPUT_CLASS, "font-mono")} />
        </div>
      </div>
    </>
  );
}
