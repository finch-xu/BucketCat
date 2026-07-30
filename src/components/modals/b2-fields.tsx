import { AlertTriangle, Lock } from "lucide-react";
import { useTranslation } from "react-i18next";
import { RegionPicker } from "@/components/modals/region-picker";
import { B2_CATALOG, b2RegionFromKeyId, looksLikeB2MasterKeyId } from "@/lib/b2-regions";
import { regionFromEndpoint, type Network } from "@/lib/regions";
import { cn } from "@/lib/utils";

const INPUT_CLASS =
  "h-9 w-full rounded-[9px] border border-border bg-panel px-3 text-[13px] text-foreground outline-none focus:border-primary focus:ring-[3px] focus:ring-primary-soft";
const INPUT_ERROR_CLASS = "border-destructive";
const READONLY_INPUT_CLASS =
  "h-9 w-full cursor-default rounded-[9px] border border-border bg-hover px-3 text-[13px] text-fg2 outline-none";

/**
 * The Backblaze B2-specific block of the connection form.
 *
 * ## Why B2 doesn't just render `RegionPicker` like OSS/Qiniu/Rainyun
 *
 * A B2 account's region is fixed when the account is created -- the user
 * cannot choose it, and often does not know it. Worse, picking the wrong one
 * fails in a way that points nowhere near the cause: verified live on
 * 2026-07-30, a `004`-cluster key against `s3.us-east-005.backblazeb2.com`
 * comes back `403 InvalidAccessKeyId / "The key '004...' is not valid"`. A
 * user reading "the key is not valid" rebuilds their credentials over and
 * over and never learns it was the region.
 *
 * So the region is *derived* from the keyID (see `b2RegionFromKeyId`) and
 * shown read-only. `RegionPicker` still renders -- but only as the fallback
 * for the cases where derivation can't answer: a master key id, a cluster
 * this build's table doesn't know, or an existing connection saved with a
 * hand-typed endpoint.
 *
 * ## Where the authoritative answer comes from
 *
 * The keyID prefix convention is **undocumented** (see `b2-regions.ts`), so it
 * is only ever a preview. The parent's Test button calls `b2ProbeKey`, which
 * asks Backblaze's own `b2_authorize_account` for the account's `s3ApiUrl` and
 * corrects the form if they disagree -- that path also works for regions
 * Backblaze launches after this build shipped.
 *
 * Purely presentational, like `RegionPicker` and `R2Fields`: every persisted
 * value lives in the parent form. Unlike `R2Fields` there is no local state at
 * all -- the derivation is a pure function of the keyID, and the parent
 * performs it in its change handler (never on render, for the reason spelled
 * out in `R2Fields`' doc comment: a value derived during render can't be
 * cleared, because the intermediate empty state fails to parse and snaps back).
 */
export function B2Fields({
  keyId,
  onKeyIdChange,
  applicationKey,
  onApplicationKeyChange,
  region,
  endpoint,
  network,
  unknownEndpoint,
  onRegionChange,
  onNetworkChange,
  isEdit,
  fieldErrors,
}: {
  keyId: string;
  onKeyIdChange: (keyId: string) => void;
  applicationKey: string;
  onApplicationKeyChange: (applicationKey: string) => void;
  region: string;
  endpoint: string;
  network: Network;
  unknownEndpoint: boolean;
  onRegionChange: (regionId: string) => void;
  onNetworkChange: (network: Network) => void;
  isEdit: boolean;
  fieldErrors: { endpoint?: string; access_key_id?: string; secret_access_key?: string };
}) {
  const { t } = useTranslation();

  const derived = b2RegionFromKeyId(keyId);
  // Only flag a master key once the user has typed something that definitely
  // has that shape -- never while the field is still being filled in.
  const isMasterKey = looksLikeB2MasterKeyId(keyId);
  // The read-only pair is shown only when the endpoint actually agrees with
  // what the keyID derives. An existing connection can pair a `004...` keyID
  // with a hand-typed endpoint -- B2 used the free-text branch before this
  // form existed -- and rendering the derived region beside that endpoint
  // would describe a connection that does not exist. Disagreement falls
  // through to the picker, whose `unknownEndpoint` "keep current" option is
  // exactly the escape hatch such a connection needs.
  const showDerived =
    derived !== undefined && regionFromEndpoint(B2_CATALOG, endpoint)?.id === derived.id;

  return (
    <>
      <div className="mb-3.5">
        <label className="mb-1.5 block text-xs font-medium text-fg2">{t("b2.keyId")}</label>
        {/* No placeholder. A sample key id is indistinguishable from a real
            one at a glance, so it reads as a value the form already holds --
            the same trap the secret fields avoid. The hint below says the
            same thing without ever looking like data. */}
        <input
          value={keyId}
          onChange={(e) => onKeyIdChange(e.target.value)}
          className={cn(
            INPUT_CLASS,
            "font-mono",
            (fieldErrors.access_key_id || isMasterKey) && INPUT_ERROR_CLASS,
          )}
        />
        {fieldErrors.access_key_id ? (
          <p className="mt-1 text-[11.5px] text-destructive">{fieldErrors.access_key_id}</p>
        ) : isMasterKey ? (
          <p className="mt-1 inline-flex items-start gap-1.5 text-[11.5px] text-destructive">
            <AlertTriangle className="mt-[1px] size-[13px] shrink-0" />
            <span>{t("b2.masterKeyWarning")}</span>
          </p>
        ) : (
          <p className="mt-1 text-[11.5px] text-muted-foreground">{t("b2.keyIdHint")}</p>
        )}
      </div>

      <div className="mb-3.5">
        <label className="mb-1.5 block text-xs font-medium text-fg2">
          {t("b2.applicationKey")}
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
            value={applicationKey}
            onChange={(e) => onApplicationKeyChange(e.target.value)}
            // Only the edit-mode "leave blank to keep" copy, which is plainly
            // a message rather than a value.
            placeholder={isEdit ? t("addConn.secretKeep") : ""}
            className="flex-1 border-none bg-transparent font-mono text-[13px] text-foreground outline-none"
          />
        </div>
        {fieldErrors.secret_access_key ? (
          <p className="mt-1 text-[11.5px] text-destructive">{fieldErrors.secret_access_key}</p>
        ) : (
          <p className="mt-1 text-[11.5px] text-muted-foreground">
            {isEdit ? t("addConn.secretKeep") : t("b2.applicationKeyHint")}
          </p>
        )}
      </div>

      {showDerived && derived ? (
        <div className="mb-3.5 grid grid-cols-[1fr_2fr] gap-3">
          <div>
            <label className="mb-1.5 block text-xs font-medium text-fg2">
              {t("addConn.region")}
            </label>
            <input
              value={`${derived.label} · ${derived.id}`}
              readOnly
              className={READONLY_INPUT_CLASS}
            />
          </div>
          <div>
            <label className="mb-1.5 block text-xs font-medium text-fg2">
              {t("addConn.endpoint")}
            </label>
            <input value={endpoint} readOnly className={cn(READONLY_INPUT_CLASS, "font-mono")} />
          </div>
          <p className="col-span-2 -mt-2 text-[11.5px] text-muted-foreground">
            {t("b2.regionDerived")}
          </p>
        </div>
      ) : (
        <RegionPicker
          catalog={B2_CATALOG}
          regionId={region}
          network={network}
          endpoint={endpoint}
          unknownEndpoint={unknownEndpoint}
          onRegionChange={onRegionChange}
          onNetworkChange={onNetworkChange}
          endpointError={fieldErrors.endpoint}
          // Four different reasons the picker can be showing, and each calls
          // for its own sentence: "haven't typed a keyID yet" is not a problem
          // at all, "that's a master key" is, "this build doesn't know that
          // cluster" is a third thing, and "your keyID and your saved endpoint
          // disagree" is a fourth that only ever happens on an older
          // connection. One generic "couldn't detect the region" would be
          // wrong for the first and useless for the rest.
          hintKey={
            isMasterKey
              ? "b2.regionBlockedByMasterKey"
              : keyId.trim() === ""
                ? "b2.regionAwaitingKeyId"
                : derived
                  ? "b2.regionEndpointMismatch"
                  : "b2.regionUnknown"
          }
        />
      )}
    </>
  );
}
