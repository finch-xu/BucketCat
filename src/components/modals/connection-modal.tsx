import {
  CheckCircle2,
  ChevronLeft,
  ChevronRight,
  Loader2,
  Lock,
  Plug,
  X,
  XCircle,
} from "lucide-react";
import { useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ProviderChip } from "@/components/icons/provider-chip";
import { B2Fields } from "@/components/modals/b2-fields";
import { R2Fields, type R2CredMode } from "@/components/modals/r2-fields";
import { RegionPicker, REGION_KEEP_CURRENT } from "@/components/modals/region-picker";
import { Modal } from "@/components/ui/modal";
import { useAddConnection, useUpdateConnection } from "@/hooks/use-connections";
import { useErrorText } from "@/hooks/use-error-text";
import type { AppError, ConnectionInput } from "@/lib/api";
import { b2ProbeKey, testConnection } from "@/lib/api";
import { b2RegionFromKeyId } from "@/lib/b2-regions";
import { cn } from "@/lib/utils";
import { R2_REGION, isKnownJurisdiction, parseR2Endpoint, r2Endpoint } from "@/lib/r2";
import {
  endpointFor,
  findRegion,
  regionCatalog,
  regionFormState,
  type Network,
} from "@/lib/regions";
import { PROVIDERS, providerMeta } from "@/lib/providers";
import { useApp } from "@/store/app-store";

/** Controlled form fields, snake_case to map 1:1 onto `ConnectionInput` --
 * `default_bucket` stays a plain string here (empty means "unset") and is
 * only converted to `string | null` when building the payload. */
interface FormState {
  name: string;
  endpoint: string;
  region: string;
  access_key_id: string;
  secret_access_key: string;
  default_bucket: string;
  /** R2 only. Blank means "not supplied" — which on edit means "keep the
   * stored token", matching `secret_access_key`'s own convention. */
  api_token: string;
}

const EMPTY_FORM: FormState = {
  name: "",
  endpoint: "",
  region: "",
  access_key_id: "",
  secret_access_key: "",
  default_bucket: "",
  api_token: "",
};

type RequiredField = "name" | "endpoint" | "access_key_id" | "secret_access_key";
const REQUIRED_FIELDS: RequiredField[] = ["name", "endpoint", "access_key_id", "secret_access_key"];

type FieldErrors = Partial<Record<RequiredField, string>>;

type TestStatus =
  | { kind: "idle" }
  | { kind: "pending" }
  /** `corrected` is set only when B2's authoritative `s3ApiUrl` disagreed with
   * the endpoint the form had derived offline, so the change to a read-only
   * field is announced rather than silent. */
  | { kind: "success"; corrected?: string }
  | { kind: "error"; error: AppError };

const INPUT_CLASS =
  "h-9 w-full rounded-[9px] border border-border bg-panel px-3 text-[13px] text-foreground outline-none focus:border-primary focus:ring-[3px] focus:ring-primary-soft";
const INPUT_ERROR_CLASS = "border-destructive";

/** 编辑模式下的初始区域状态：provider 没有区域目录时退化为 public/未知。 */
function initialRegionState(provider: string | undefined, endpoint: string, region: string) {
  const catalog = provider ? regionCatalog(provider) : undefined;
  if (!catalog) return { network: "public" as Network, unknownEndpoint: false };
  const derived = regionFormState(catalog, endpoint, region);
  return { network: derived.network, unknownEndpoint: derived.unknownEndpoint };
}

/** 编辑模式下 R2 的初始账户/辖区：从已存端点反解。
 *
 * 解不出来（自定义域、手改过的端点）时两者留空，用户重填账户 ID 即可重建
 * 端点 —— 这与 `regionFormState` 的 `unknownEndpoint` 处理同一思路：绝不
 * 猜测，只是把不认识的值让给用户处理。辖区若是本版本不认识的（Cloudflare
 * 将来新增的），同样退回默认，避免 Select 出现一个选不中的值。 */
function initialR2State(provider: string | undefined, endpoint: string) {
  if (provider !== "r2") return { accountId: "", jurisdiction: "" };
  const parsed = parseR2Endpoint(endpoint);
  if (!parsed) return { accountId: "", jurisdiction: "" };
  return {
    accountId: parsed.accountId,
    jurisdiction: isKnownJurisdiction(parsed.jurisdiction) ? parsed.jurisdiction : "",
  };
}

function Field({
  label,
  error,
  children,
}: {
  label: React.ReactNode;
  error?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="mb-3.5">
      <label className="mb-1.5 block text-xs font-medium text-fg2">{label}</label>
      {children}
      {error && <p className="mt-1 text-[11.5px] text-destructive">{error}</p>}
    </div>
  );
}

/** Add-connection wizard, reused for editing an existing connection. In
 * "add" mode the user picks a provider on step 1, then fills in credentials
 * on step 2. In "edit" mode (`editingConnection` set) the modal opens
 * directly on step 2, prefilled from the `ConnectionDto` -- which never
 * carries the secret, so that field starts empty with copy explaining that
 * leaving it blank keeps the existing secret (`update_connection`'s
 * contract: an empty/whitespace secret in the input is a no-op on that
 * field).
 *
 * `app-shell`'s `ConnectionModals` wrapper mounts this with a `key` derived
 * from the target ("add", `edit:${id}`, or "closed"), so every distinct
 * add/edit target gets a *fresh component instance* rather than an existing
 * instance being patched via an effect. That's what lets initial state below
 * be plain lazy initializers instead of a post-paint reset effect: a fresh
 * mount's first render already has the right `editingConnection` for this
 * target (no one-frame flash of the previous target's step/fields), and a
 * fresh `useAddConnection`/`useUpdateConnection` mutation instance starts
 * with no stale error/pending from a previous target automatically. */
export function ConnectionModal() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const { showAdd, closeAdd, editingConnection, closeEditConnection } = useApp();
  const addMutation = useAddConnection();
  const updateMutation = useUpdateConnection();

  const isEdit = editingConnection !== null;
  const isOpen = showAdd || isEdit;
  const mutation = isEdit ? updateMutation : addMutation;

  // Wizard step + form state are ephemeral to this modal instance, so they
  // live in component-local state rather than the global app store. Lazy
  // initializers read `editingConnection` only on this instance's mount --
  // safe because a new target always means a new instance (see key above).
  const [step, setStep] = useState<1 | 2>(() => (editingConnection ? 2 : 1));
  const [providerId, setProviderId] = useState<string | null>(
    () => editingConnection?.provider ?? null,
  );
  const [form, setForm] = useState<FormState>(() =>
    editingConnection
      ? {
          name: editingConnection.name,
          endpoint: editingConnection.endpoint,
          region: editingConnection.region,
          access_key_id: editingConnection.access_key_id,
          secret_access_key: "",
          default_bucket: editingConnection.default_bucket ?? "",
          api_token: "",
        }
      : EMPTY_FORM,
  );
  const [fieldErrors, setFieldErrors] = useState<FieldErrors>({});
  const [testStatus, setTestStatus] = useState<TestStatus>({ kind: "idle" });
  // Catalog-provider-only: which network the shown (read-only) endpoint
  // targets, and whether the current endpoint is one this build can't map
  // back to a region -- a connection saved by hand before the region table
  // existed. Derived from the prefilled endpoint on mount (edit mode) or
  // reset whenever the provider picker switches to a provider that ships a
  // catalog (`chooseProvider` below); inert for every other provider.
  // `regionFormState` never rewrites `form.endpoint` itself -- see its doc
  // comment -- so opening the edit dialog can't change a saved connection's
  // endpoint.
  const [regionNetwork, setRegionNetwork] = useState<Network>(
    () =>
      initialRegionState(
        editingConnection?.provider,
        editingConnection?.endpoint ?? "",
        editingConnection?.region ?? "",
      ).network,
  );
  const [unknownEndpoint, setUnknownEndpoint] = useState<boolean>(
    () =>
      initialRegionState(
        editingConnection?.provider,
        editingConnection?.endpoint ?? "",
        editingConnection?.region ?? "",
      ).unknownEndpoint,
  );
  // R2-only. The endpoint is *derived* from these two rather than the other
  // way around -- see `R2Fields`' doc comment for why deriving them from the
  // endpoint cannot work (an emptied account field never round-trips).
  const [r2Account, setR2Account] = useState<string>(
    () => initialR2State(editingConnection?.provider, editingConnection?.endpoint ?? "").accountId,
  );
  const [r2Jurisdiction, setR2Jurisdiction] = useState<string>(
    () =>
      initialR2State(editingConnection?.provider, editingConnection?.endpoint ?? "").jurisdiction,
  );
  // An existing R2 connection is edited in whichever mode it was created in:
  // one with a stored token stays in token mode (so "leave blank to keep"
  // works), one without it -- an S3-key connection, or one saved by a build
  // predating token mode -- opens in key mode so its editable fields are the
  // ones it actually uses.
  const [r2CredMode, setR2CredMode] = useState<R2CredMode>(() =>
    editingConnection && !editingConnection.has_api_token ? "keys" : "token",
  );
  // Guards against a stale in-flight `testConnection` result landing after
  // the user has since edited a field or fired off a newer test -- only the
  // most recent request id's resolution/rejection is allowed to update
  // `testStatus`.
  const testReqIdRef = useRef(0);

  if (!isOpen) return null;

  const provider = providerId ? providerMeta(providerId) : undefined;
  const isR2 = providerId === "r2";
  const isB2 = providerId === "b2";
  /** Which credential/endpoint block step 2 renders. A single three-way switch
   * rather than a `!isR2 && !isB2 &&` guard on each of the four sub-blocks --
   * those guards multiply with every provider that needs its own layout, and
   * getting one of them wrong renders two credential forms at once. */
  const layout: "r2" | "b2" | "generic" = isR2 ? "r2" : isB2 ? "b2" : "generic";
  // Testing needs a usable secret. On edit the stored one is never echoed
  // back, so a blank field would send an empty credential and fail for the
  // wrong reason -- unless a freshly pasted R2 token is present, which the
  // backend derives a secret from.
  const secretBlockedForTest =
    isEdit && form.secret_access_key.trim() === "" && form.api_token.trim() === "";

  function handleClose() {
    if (isEdit) closeEditConnection();
    else closeAdd();
  }

  /** Advances to step 2 for the tapped provider. Re-tapping the SAME
   * provider (e.g. after using the step-2 back button) leaves the form
   * completely untouched. Switching to a DIFFERENT provider preserves
   * whatever the user already typed into name/access_key_id/
   * secret_access_key/default_bucket and only re-seeds endpoint/region
   * from the new provider's metadata. */
  function chooseProvider(id: string) {
    if (id === providerId) {
      setStep(2);
      return;
    }
    const meta = providerMeta(id);
    setProviderId(id);
    // R2's endpoint is built from the account id the user has yet to supply,
    // so it starts empty rather than at a placeholder host -- and its region
    // is fixed.
    setForm((f) =>
      id === "r2"
        ? { ...f, endpoint: "", region: R2_REGION }
        : { ...f, endpoint: meta.endpoint, region: meta.region },
    );
    const derived = initialRegionState(id, meta.endpoint, meta.region);
    setRegionNetwork(derived.network);
    setUnknownEndpoint(derived.unknownEndpoint);
    setR2Account("");
    setR2Jurisdiction("");
    setR2CredMode("token");
    setFieldErrors({});
    setTestStatus({ kind: "idle" });
    testReqIdRef.current++;
    setStep(2);
  }

  /** R2's account id changed. The endpoint is derived here (not on render) so
   * `form.endpoint` stays the single persisted source of truth and every
   * downstream consumer -- validation, `buildInput`, the test button --
   * needs no R2 special case. */
  function handleR2AccountChange(accountId: string) {
    setR2Account(accountId);
    setForm((f) => ({
      ...f,
      endpoint: accountId.trim() ? r2Endpoint(accountId, r2Jurisdiction) : "",
    }));
    if (fieldErrors.endpoint) setFieldErrors((e) => ({ ...e, endpoint: undefined }));
    invalidateTest();
  }

  /** B2's keyID changed. Like `handleR2AccountChange`, the endpoint is derived
   * here rather than on render, so `form.endpoint` stays the single persisted
   * source of truth and validation/`buildInput`/the test button need no B2
   * special case.
   *
   * A keyID the cluster table can't read (still being typed, a master key, a
   * cluster Backblaze added later) leaves region/endpoint exactly as they
   * were: `B2Fields` then shows the region picker so the user can say, and
   * blanking them here would only destroy a value that is still valid. */
  function handleB2KeyIdChange(keyId: string) {
    const derived = b2RegionFromKeyId(keyId);
    setForm((f) =>
      derived
        ? {
            ...f,
            access_key_id: keyId,
            region: derived.id,
            endpoint: endpointFor(derived, "public"),
          }
        : { ...f, access_key_id: keyId },
    );
    if (derived) setUnknownEndpoint(false);
    setFieldErrors((e) =>
      e.access_key_id || e.endpoint ? { ...e, access_key_id: undefined, endpoint: undefined } : e,
    );
    invalidateTest();
  }

  function handleR2JurisdictionChange(jurisdiction: string) {
    setR2Jurisdiction(jurisdiction);
    setForm((f) => ({
      ...f,
      endpoint: r2Account.trim() ? r2Endpoint(r2Account, jurisdiction) : "",
    }));
    invalidateTest();
  }

  function backToProviders() {
    setStep(1);
  }

  // Invalidate any in-flight test request -- its result must not land on a
  // form the user has since changed -- and clear a stale result.
  function invalidateTest() {
    testReqIdRef.current++;
    if (testStatus.kind !== "idle") setTestStatus({ kind: "idle" });
  }

  function updateField(key: keyof FormState, value: string) {
    setForm((f) => ({ ...f, [key]: value }));
    if (fieldErrors[key as RequiredField]) {
      setFieldErrors((e) => ({ ...e, [key]: undefined }));
    }
    invalidateTest();
  }

  /** Region select changed. Every real option resolves via `findRegion`, so
   * the endpoint (read-only, derived) always gets overwritten from the table
   * -- this IS the "changing the region overwrites a custom endpoint"
   * behavior the UI warns about for `unknownEndpoint` connections. The one
   * value that can't resolve is the temporary "current value" option, whose
   * value already equals the current region, so selecting it again never
   * fires `onChange` in the first place; the fallback below is defensive. */
  function handleRegionChange(newRegionId: string) {
    if (newRegionId === REGION_KEEP_CURRENT) return;
    const catalog = providerId ? regionCatalog(providerId) : undefined;
    const region = catalog ? findRegion(catalog, newRegionId) : undefined;
    if (!region) {
      updateField("region", newRegionId);
      return;
    }
    setUnknownEndpoint(false);
    setFieldErrors((e) => (e.endpoint ? { ...e, endpoint: undefined } : e));
    setForm((f) => ({ ...f, region: newRegionId, endpoint: endpointFor(region, regionNetwork) }));
    invalidateTest();
  }

  function handleNetworkChange(newNetwork: Network) {
    setRegionNetwork(newNetwork);
    // A saved connection whose endpoint isn't in the region table keeps that
    // endpoint until the user *picks a region*. Deriving one here would
    // silently overwrite a working hand-typed endpoint on a control the user
    // may have touched just to look around -- and `region` alone can't be
    // trusted to describe it, since a legacy connection can pair a real
    // region id with a custom endpoint.
    if (unknownEndpoint) return;
    const catalog = providerId ? regionCatalog(providerId) : undefined;
    const region = catalog ? findRegion(catalog, form.region) : undefined;
    if (region) {
      setForm((f) => ({ ...f, endpoint: endpointFor(region, newNetwork) }));
      invalidateTest();
    }
  }

  function buildInput(): ConnectionInput {
    return {
      provider: providerId ?? "generic",
      name: form.name.trim(),
      endpoint: form.endpoint.trim(),
      region: form.region.trim(),
      access_key_id: form.access_key_id.trim(),
      secret_access_key: form.secret_access_key.trim(),
      default_bucket: form.default_bucket.trim() ? form.default_bucket.trim() : null,
      // Only ever sent for R2, and only when the user actually supplied one.
      // The backend derives `secret_access_key` from it when that field is
      // blank -- the hash is never computed here, so the derived secret never
      // exists inside the webview.
      api_token: form.api_token.trim() ? form.api_token.trim() : null,
    };
  }

  /** Whether the secret requirement is satisfied without the user typing one.
   *
   * Three ways it can be: an existing connection keeps its stored secret when
   * the field is left blank; an R2 token pasted now derives one server-side;
   * and an R2 connection being edited already has a stored token that will
   * re-derive it. */
  function secretSuppliedIndirectly(): boolean {
    if (isEdit) return true;
    return isR2 && r2CredMode === "token" && form.api_token.trim() !== "";
  }

  function validate(): boolean {
    const errors: FieldErrors = {};
    // In edit mode the secret is optional -- a blank field means "keep the
    // existing secret" (`update_connection`'s contract), so it's excluded
    // from the required-fields check there.
    const requiredFields = secretSuppliedIndirectly()
      ? REQUIRED_FIELDS.filter((key) => key !== "secret_access_key")
      : REQUIRED_FIELDS;
    for (const key of requiredFields) {
      if (!form[key].trim()) errors[key] = t("addConn.required");
    }
    if (isR2) {
      // R2's endpoint and access key are both derived, so the generic
      // "required" copy would point at fields the user cannot type into.
      if (errors.endpoint) errors.endpoint = t("r2.accountRequired");
      if (errors.access_key_id && r2CredMode === "token") {
        errors.access_key_id = t("r2.probeRequired");
      }
    }
    setFieldErrors(errors);
    return Object.keys(errors).length === 0;
  }

  async function handleTest() {
    // Testing with a blank secret in edit mode would send an empty
    // credential to `test_connection` and fail for the wrong reason -- the
    // Test button is disabled in that state (see `secretBlockedForTest`),
    // this is just a defensive backstop.
    if (secretBlockedForTest) return;
    if (!validate()) return;
    const reqId = ++testReqIdRef.current;
    setTestStatus({ kind: "pending" });
    try {
      let input = buildInput();
      let corrected: string | undefined;
      if (isB2) {
        // Ask Backblaze which region this account actually lives in, and let
        // its answer win. The form's own endpoint came from the keyID's
        // cluster prefix -- a convention Backblaze has never documented (see
        // `b2-regions.ts`) -- so this is the step that turns a good guess into
        // a fact. It also covers regions launched after this build shipped,
        // which no table here could know.
        //
        // Testing the *corrected* input rather than the original matters: a
        // wrong B2 endpoint answers `403 InvalidAccessKeyId`, so without this
        // the user would be told their key is invalid when it is fine.
        const probe = await b2ProbeKey(input.access_key_id, input.secret_access_key);
        if (reqId !== testReqIdRef.current) return;
        if (probe.endpoint !== input.endpoint || probe.region !== input.region) {
          corrected = probe.endpoint;
          setForm((f) => ({ ...f, endpoint: probe.endpoint, region: probe.region }));
          setUnknownEndpoint(false);
          input = { ...input, endpoint: probe.endpoint, region: probe.region };
        }
      }
      await testConnection(input);
      if (reqId === testReqIdRef.current) setTestStatus({ kind: "success", corrected });
    } catch (err) {
      if (reqId === testReqIdRef.current) {
        setTestStatus({ kind: "error", error: err as AppError });
      }
    }
  }

  function handleSave() {
    if (!validate()) return;
    if (isEdit && editingConnection) {
      updateMutation.mutate(
        { id: editingConnection.id, input: buildInput() },
        { onSuccess: () => closeEditConnection() },
      );
    } else {
      addMutation.mutate(buildInput(), {
        onSuccess: () => closeAdd(),
      });
    }
  }

  return (
    <Modal onClose={handleClose} className="w-[560px]">
      {step === 1 && (
        <>
          <div className="flex items-start justify-between px-[22px] pt-[22px] pb-1">
            <div>
              <div className="text-[17px] font-bold">{t("addConn.title")}</div>
              <div className="mt-[3px] text-[12.5px] text-muted-foreground">
                {t("addConn.subtitle")}
              </div>
            </div>
            <button
              type="button"
              onClick={handleClose}
              className="flex size-[30px] cursor-pointer items-center justify-center rounded-lg text-muted-foreground hover:bg-hover hover:text-fg2"
            >
              <X className="size-[17px]" />
            </button>
          </div>
          <div className="grid grid-cols-2 gap-2.5 px-[22px] pt-4 pb-6">
            {PROVIDERS.map((p) => (
              <button
                key={p.id}
                type="button"
                onClick={() => chooseProvider(p.id)}
                className="flex cursor-pointer items-center gap-3 rounded-xl border border-border bg-panel p-[13px] text-left hover:border-primary hover:bg-background"
              >
                <ProviderChip meta={p} size="lg" />
                <span className="min-w-0 flex-1">
                  <span className="block text-[13.5px] font-semibold text-foreground">
                    {p.nameKey ? t(p.nameKey) : p.name}
                  </span>
                  <span className="block truncate text-[11.5px] text-muted-foreground">
                    {t(p.descKey)}
                  </span>
                </span>
                <ChevronRight className="size-[15px] text-muted2" />
              </button>
            ))}
          </div>
        </>
      )}
      {step === 2 && provider && (
        <>
          <div className="flex items-center gap-[11px] px-[22px] pt-[18px] pb-1.5">
            {!isEdit && (
              <button
                type="button"
                onClick={backToProviders}
                className="flex size-[30px] shrink-0 cursor-pointer items-center justify-center rounded-lg border border-border bg-background text-fg2 hover:bg-hover"
              >
                <ChevronLeft className="size-4" />
              </button>
            )}
            <ProviderChip meta={provider} size="md" />
            <div className="min-w-0 flex-1">
              <div className="truncate text-[15px] font-bold">
                {isEdit ? t("conn.editTitle") : provider.nameKey ? t(provider.nameKey) : provider.name}
              </div>
              <div className="truncate text-xs text-muted-foreground">
                {isEdit
                  ? provider.nameKey
                    ? t(provider.nameKey)
                    : provider.name
                  : t("addConn.fillCreds")}
              </div>
            </div>
            <button
              type="button"
              onClick={handleClose}
              className="flex size-[30px] shrink-0 cursor-pointer items-center justify-center rounded-lg text-muted-foreground hover:bg-hover hover:text-fg2"
            >
              <X className="size-[17px]" />
            </button>
          </div>
          <div className="px-[22px] pt-3.5 pb-1.5">
            <Field label={t("addConn.name")} error={fieldErrors.name}>
              <input
                value={form.name}
                onChange={(e) => updateField("name", e.target.value)}
                placeholder={t("addConn.namePlaceholder")}
                className={cn(INPUT_CLASS, fieldErrors.name && INPUT_ERROR_CLASS)}
              />
            </Field>
            {layout === "r2" && (
              <R2Fields
                credMode={r2CredMode}
                onCredModeChange={(mode) => {
                  setR2CredMode(mode);
                  invalidateTest();
                }}
                apiToken={form.api_token}
                onApiTokenChange={(v) => updateField("api_token", v)}
                accountId={r2Account}
                onAccountIdChange={handleR2AccountChange}
                jurisdiction={r2Jurisdiction}
                onJurisdictionChange={handleR2JurisdictionChange}
                accessKeyId={form.access_key_id}
                onAccessKeyIdChange={(v) => updateField("access_key_id", v)}
                secretAccessKey={form.secret_access_key}
                onSecretAccessKeyChange={(v) => updateField("secret_access_key", v)}
                isEdit={isEdit}
                hasApiToken={editingConnection?.has_api_token ?? false}
                fieldErrors={fieldErrors}
              />
            )}
            {layout === "b2" && (
              <B2Fields
                keyId={form.access_key_id}
                onKeyIdChange={handleB2KeyIdChange}
                applicationKey={form.secret_access_key}
                onApplicationKeyChange={(v) => updateField("secret_access_key", v)}
                region={form.region}
                endpoint={form.endpoint}
                network={regionNetwork}
                unknownEndpoint={unknownEndpoint}
                onRegionChange={handleRegionChange}
                onNetworkChange={handleNetworkChange}
                isEdit={isEdit}
                fieldErrors={fieldErrors}
              />
            )}
            {layout === "generic" && (
              <>
                {(() => {
                  const catalog = providerId ? regionCatalog(providerId) : undefined;
                  return catalog ? (
                    <RegionPicker
                      catalog={catalog}
                      regionId={form.region}
                      network={regionNetwork}
                      endpoint={form.endpoint}
                      unknownEndpoint={unknownEndpoint}
                      onRegionChange={handleRegionChange}
                      onNetworkChange={handleNetworkChange}
                      endpointError={fieldErrors.endpoint}
                      hintKey={providerId === "rainyun" ? "addConn.rainyunRegionHint" : undefined}
                    />
                  ) : (
                    <div className="mb-3.5 grid grid-cols-[2fr_1fr] gap-3">
                      <Field label={t("addConn.endpoint")} error={fieldErrors.endpoint}>
                        <input
                          value={form.endpoint}
                          onChange={(e) => updateField("endpoint", e.target.value)}
                          placeholder={provider.endpoint}
                          className={cn(
                            INPUT_CLASS,
                            "font-mono",
                            fieldErrors.endpoint && INPUT_ERROR_CLASS,
                          )}
                        />
                      </Field>
                      <Field label={t("addConn.region")}>
                        <input
                          value={form.region}
                          onChange={(e) => updateField("region", e.target.value)}
                          placeholder={provider.region || "—"}
                          className={`${INPUT_CLASS} font-mono`}
                        />
                      </Field>
                    </div>
                  );
                })()}
                <Field label={t("addConn.accessKey")} error={fieldErrors.access_key_id}>
                  {/* Deliberately no placeholder. A masked-looking one
                      (`AKIA••••••••••••`) is indistinguishable from a credential
                      the form already holds, so an empty required field reads as
                      filled -- and the user only finds out at Save. */}
                  <input
                    value={form.access_key_id}
                    onChange={(e) => updateField("access_key_id", e.target.value)}
                    className={cn(
                      INPUT_CLASS,
                      "font-mono",
                      fieldErrors.access_key_id && INPUT_ERROR_CLASS,
                    )}
                  />
                </Field>
                <Field label={t("addConn.secretKey")} error={fieldErrors.secret_access_key}>
                  <div
                    className={cn(
                      "flex h-9 items-center gap-2 rounded-[9px] border border-border bg-panel px-3 focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary-soft",
                      fieldErrors.secret_access_key && INPUT_ERROR_CLASS,
                    )}
                  >
                    <Lock className="size-3.5 text-muted-foreground" />
                    <input
                      type="password"
                      value={form.secret_access_key}
                      onChange={(e) => updateField("secret_access_key", e.target.value)}
                      placeholder={isEdit ? t("addConn.secretKeep") : ""}
                      className="flex-1 border-none bg-transparent font-mono text-[13px] text-foreground outline-none"
                    />
                  </div>
                  {isEdit && (
                    <p className="mt-1 text-[11.5px] text-muted-foreground">
                      {t("addConn.secretKeep")}
                    </p>
                  )}
                </Field>
              </>
            )}
            <div className="mb-1.5">
              <label className="mb-1.5 block text-xs font-medium text-fg2">
                {t("addConn.defaultBucket")}{" "}
                <span className="font-normal text-muted2">{t("addConn.optional")}</span>
              </label>
              <input
                value={form.default_bucket}
                onChange={(e) => updateField("default_bucket", e.target.value)}
                placeholder="my-bucket"
                className={`${INPUT_CLASS} font-mono`}
              />
            </div>
          </div>
          <div className="flex items-center gap-2.5 border-t border-border2 px-[22px] pt-3.5 pb-5">
            <button
              type="button"
              onClick={handleTest}
              disabled={testStatus.kind === "pending" || mutation.isPending || secretBlockedForTest}
              className="inline-flex h-9 shrink-0 cursor-pointer items-center gap-[7px] rounded-[9px] border border-border bg-background px-3.5 text-[13px] font-medium text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60"
            >
              {testStatus.kind === "pending" ? (
                <Loader2 className="size-3.5 animate-spin text-muted-foreground" />
              ) : (
                <Plug className="size-3.5 text-muted-foreground" />
              )}
              {t("addConn.test")}
            </button>
            <div className="min-w-0 flex-1 text-[12.5px]">
              {testStatus.kind === "idle" && secretBlockedForTest && (
                <span className="text-muted-foreground">{t("addConn.testNeedsSecret")}</span>
              )}
              {testStatus.kind === "pending" && (
                <span className="text-muted-foreground">{t("addConn.testing")}</span>
              )}
              {testStatus.kind === "success" && (
                <span
                  className="inline-flex min-w-0 items-center gap-1.5 text-emerald-600 dark:text-emerald-400"
                  title={
                    testStatus.corrected
                      ? t("b2.endpointCorrected", { endpoint: testStatus.corrected })
                      : undefined
                  }
                >
                  <CheckCircle2 className="size-[15px] shrink-0" />
                  <span className="truncate">
                    {testStatus.corrected
                      ? t("b2.endpointCorrected", { endpoint: testStatus.corrected })
                      : t("addConn.testOk")}
                  </span>
                </span>
              )}
              {testStatus.kind === "error" && (
                <span
                  className="inline-flex min-w-0 items-center gap-1.5 text-destructive"
                  title={`${t("addConn.testFail")}: ${errorText(testStatus.error)}`}
                >
                  <XCircle className="size-[15px] shrink-0" />
                  <span className="truncate">
                    {t("addConn.testFail")}: {errorText(testStatus.error)}
                  </span>
                </span>
              )}
            </div>
            <button
              type="button"
              onClick={handleClose}
              className="h-9 shrink-0 cursor-pointer rounded-[9px] border border-border bg-background px-4 text-[13px] font-medium text-fg2 hover:bg-hover"
            >
              {t("addConn.cancel")}
            </button>
            <button
              type="button"
              onClick={handleSave}
              disabled={mutation.isPending || testStatus.kind === "pending"}
              className="inline-flex h-9 shrink-0 cursor-pointer items-center gap-[7px] rounded-[9px] bg-primary px-[18px] text-[13px] font-semibold text-primary-foreground hover:bg-primary-strong disabled:cursor-not-allowed disabled:opacity-70"
            >
              {mutation.isPending && <Loader2 className="size-3.5 animate-spin" />}
              {mutation.isPending
                ? t("addConn.saving")
                : isEdit
                  ? t("conn.saveEdit")
                  : t("addConn.save")}
            </button>
          </div>
          {mutation.isError && (
            <div className="px-[22px] pb-4 text-[12.5px] text-destructive">
              {errorText(mutation.error)}
            </div>
          )}
        </>
      )}
    </Modal>
  );
}
