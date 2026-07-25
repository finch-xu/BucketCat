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
import { RegionPicker, REGION_KEEP_CURRENT } from "@/components/modals/region-picker";
import { Modal } from "@/components/ui/modal";
import { useAddConnection, useUpdateConnection } from "@/hooks/use-connections";
import { useErrorText } from "@/hooks/use-error-text";
import type { AppError, ConnectionInput } from "@/lib/api";
import { testConnection } from "@/lib/api";
import { cn } from "@/lib/utils";
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
}

const EMPTY_FORM: FormState = {
  name: "",
  endpoint: "",
  region: "",
  access_key_id: "",
  secret_access_key: "",
  default_bucket: "",
};

type RequiredField = "name" | "endpoint" | "access_key_id" | "secret_access_key";
const REQUIRED_FIELDS: RequiredField[] = ["name", "endpoint", "access_key_id", "secret_access_key"];

type FieldErrors = Partial<Record<RequiredField, string>>;

type TestStatus =
  | { kind: "idle" }
  | { kind: "pending" }
  | { kind: "success" }
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
  // Guards against a stale in-flight `testConnection` result landing after
  // the user has since edited a field or fired off a newer test -- only the
  // most recent request id's resolution/rejection is allowed to update
  // `testStatus`.
  const testReqIdRef = useRef(0);

  if (!isOpen) return null;

  const provider = providerId ? providerMeta(providerId) : undefined;
  const secretBlockedForTest = isEdit && form.secret_access_key.trim() === "";

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
    setForm((f) => ({ ...f, endpoint: meta.endpoint, region: meta.region }));
    const derived = initialRegionState(id, meta.endpoint, meta.region);
    setRegionNetwork(derived.network);
    setUnknownEndpoint(derived.unknownEndpoint);
    setFieldErrors({});
    setTestStatus({ kind: "idle" });
    testReqIdRef.current++;
    setStep(2);
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
    };
  }

  function validate(): boolean {
    const errors: FieldErrors = {};
    // In edit mode the secret is optional -- a blank field means "keep the
    // existing secret" (`update_connection`'s contract), so it's excluded
    // from the required-fields check there.
    const requiredFields = isEdit
      ? REQUIRED_FIELDS.filter((key) => key !== "secret_access_key")
      : REQUIRED_FIELDS;
    for (const key of requiredFields) {
      if (!form[key].trim()) errors[key] = t("addConn.required");
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
      await testConnection(buildInput());
      if (reqId === testReqIdRef.current) setTestStatus({ kind: "success" });
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
            {PROVIDERS.map((p) => {
              const Icon = p.icon;
              return (
                <button
                  key={p.id}
                  type="button"
                  onClick={() => chooseProvider(p.id)}
                  className="flex cursor-pointer items-center gap-3 rounded-xl border border-border bg-panel p-[13px] text-left hover:border-primary hover:bg-background"
                >
                  <span
                    className="flex size-[38px] shrink-0 items-center justify-center rounded-[10px] shadow-[0_2px_5px_rgba(0,0,0,0.2)]"
                    style={{ background: p.color }}
                  >
                    <Icon className="size-5 text-white" />
                  </span>
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
              );
            })}
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
            <span
              className="flex size-[34px] shrink-0 items-center justify-center rounded-[9px] shadow-[0_2px_5px_rgba(0,0,0,0.2)]"
              style={{ background: provider.color }}
            >
              <provider.icon className="size-5 text-white" />
            </span>
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
              <input
                value={form.access_key_id}
                onChange={(e) => updateField("access_key_id", e.target.value)}
                placeholder="AKIA••••••••••••"
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
                  placeholder={isEdit ? t("addConn.secretKeep") : "••••••••••••••••••••"}
                  className="flex-1 border-none bg-transparent font-mono text-[13px] text-foreground outline-none"
                />
              </div>
              {isEdit && (
                <p className="mt-1 text-[11.5px] text-muted-foreground">
                  {t("addConn.secretKeep")}
                </p>
              )}
            </Field>
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
                <span className="inline-flex items-center gap-1.5 text-emerald-600 dark:text-emerald-400">
                  <CheckCircle2 className="size-[15px]" />
                  {t("addConn.testOk")}
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
