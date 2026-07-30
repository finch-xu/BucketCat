/**
 * Cloudflare R2 endpoint rules — the frontend twin of
 * `src-tauri/src/provider/r2.rs`.
 *
 * The connection form derives the endpoint on every keystroke, so these two
 * functions have to run locally rather than over IPC. They are kept
 * deliberately narrow: **the credential derivation is NOT mirrored here.**
 * `secret_access_key = sha256(token value)` is computed once, in Rust, when a
 * connection is saved or tested (see `r2_secret_from_token`), so there is a
 * single implementation that cannot drift — and the derived secret never
 * exists inside the webview at all.
 *
 * ## Jurisdictions are not regions
 *
 * R2's `eu` and `fedramp` endpoints are separate namespaces, not alternate
 * routes to the same data — verified live on 2026-07-30: one key lists two
 * buckets on the default endpoint and zero on the `eu` one, and asking the
 * `eu` endpoint for a default-jurisdiction bucket returns `404 NoSuchBucket`.
 * That is why R2 does not use `regionCatalog` (`./regions`): a catalog maps a
 * region id to a *fixed* endpoint, but R2's host depends on the account id,
 * and switching jurisdiction changes which buckets exist at all rather than
 * which host serves the same ones.
 */

/** The endpoint hostname suffix every R2 jurisdiction shares. */
const R2_HOST_SUFFIX = ".r2.cloudflarestorage.com";

/** The SigV4 region R2 documents. R2 ignores the value entirely (verified
 * live — signing for `us-east-1` or `wnam` against the same host both return
 * 200), but this is what a connection stores so the UI reads correctly. */
export const R2_REGION = "auto";

/** One selectable jurisdiction. `id` is what goes into the endpoint hostname
 * as its own label; the empty id is the default (worldwide) jurisdiction,
 * whose endpoint carries no label at all. */
export interface R2Jurisdiction {
  id: string;
  /** i18n key for the human label — never a literal label. */
  labelKey: string;
}

/** Every jurisdiction the picker offers, in render order. Mirrors
 * `R2_JURISDICTIONS` in `provider/r2.rs`, which pins the same ids from the
 * Rust side. */
export const R2_JURISDICTIONS: R2Jurisdiction[] = [
  { id: "", labelKey: "r2.jurisdictionDefault" },
  { id: "eu", labelKey: "r2.jurisdictionEu" },
  { id: "fedramp", labelKey: "r2.jurisdictionFedramp" },
];

/** Builds R2's S3 endpoint for an account id and jurisdiction. The default
 * jurisdiction gets no label; every other one becomes its own hostname label.
 * Inputs are trimmed and lowercased — account ids are hex and jurisdiction
 * ids are lowercase ASCII, so their case carries no information. */
export function r2Endpoint(accountId: string, jurisdiction: string): string {
  const account = accountId.trim().toLowerCase();
  const juris = jurisdiction.trim().toLowerCase();
  return juris
    ? `https://${account}.${juris}${R2_HOST_SUFFIX}`
    : `https://${account}${R2_HOST_SUFFIX}`;
}

/** The inverse of `r2Endpoint`: recovers `{ accountId, jurisdiction }` from a
 * saved endpoint so the edit form can prefill its controls.
 *
 * Returns `undefined` for anything that isn't an R2 endpoint — a custom
 * domain, a typo, another provider's host. Callers must treat that as "this
 * connection has an endpoint I can't model" and fall back to a free-text
 * endpoint field rather than guessing, mirroring how `regionFormState`
 * (`./regions`) reports `unknownEndpoint`.
 *
 * The jurisdiction comes back **verbatim** rather than validated against
 * `R2_JURISDICTIONS`: a jurisdiction Cloudflare adds after this build shipped
 * should still round-trip through the form instead of being silently
 * rewritten to the default. Callers rendering a picker check membership
 * themselves. */
export function parseR2Endpoint(
  endpoint: string,
): { accountId: string; jurisdiction: string } | undefined {
  const trimmed = endpoint.trim();
  const withoutScheme = trimmed.includes("://")
    ? trimmed.slice(trimmed.indexOf("://") + 3)
    : trimmed;
  // Drop any path/query, then any `:port`.
  const authority = withoutScheme.split("/")[0] ?? "";
  const host = authority.replace(/:\d+$/, "").trim().toLowerCase();

  if (!host.endsWith(R2_HOST_SUFFIX)) return undefined;
  const head = host.slice(0, -R2_HOST_SUFFIX.length);
  if (!head) return undefined;

  const labels = head.split(".");
  if (labels.length === 1) {
    return { accountId: labels[0], jurisdiction: "" };
  }
  // `{account}.{jurisdiction}` — exactly two labels, both non-empty. A deeper
  // name is not a shape R2 produces, so it is rejected rather than guessed at:
  // silently reading the first label as the account would build a
  // working-looking endpoint pointing somewhere else entirely.
  if (labels.length === 2 && labels[0] && labels[1]) {
    return { accountId: labels[0], jurisdiction: labels[1] };
  }
  return undefined;
}

/** Whether a parsed jurisdiction is one this build can render in the picker.
 * An unknown one (a future Cloudflare jurisdiction, or a hand-edited
 * endpoint) is preserved by `parseR2Endpoint` but cannot be selected, so the
 * form falls back to its free-text endpoint field. */
export function isKnownJurisdiction(jurisdiction: string): boolean {
  return R2_JURISDICTIONS.some((j) => j.id === jurisdiction);
}
