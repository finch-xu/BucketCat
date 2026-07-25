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
import { Modal } from "@/components/ui/modal";
import { Segmented } from "@/components/ui/segmented";
import { useAddConnection, useUpdateConnection } from "@/hooks/use-connections";
import { useErrorText } from "@/hooks/use-error-text";
import type { AppError, ConnectionInput } from "@/lib/api";
import { testConnection } from "@/lib/api";
import { cn } from "@/lib/utils";
import {
  findOssRegion,
  ossEndpointFor,
  ossFormStateFromConnection,
  OSS_REGIONS,
} from "@/lib/oss-regions";
import { PROVIDERS, providerMeta } from "@/lib/providers";
import { useApp } from "@/store/app-store";

/** Region `<select>` groups, in display order -- each maps to an
 * `<optgroup>` over `OSS_REGIONS` filtered by `group`. */
const OSS_REGION_GROUPS: { key: "public" | "finance" | "gov"; labelKey: string }[] = [
  { key: "public", labelKey: "addConn.regionGroupPublic" },
  { key: "finance", labelKey: "addConn.regionGroupFinance" },
  { key: "gov", labelKey: "addConn.regionGroupGov" },
];

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
/** The region `<select>` and the read-only endpoint display share the same
 * sizing/rounding cadence as `INPUT_CLASS` (so all fields in this form line
 * up), but borrow `bg-background`/`text-fg2` from the `<select>` already
 * used in `settings-modal.tsx` -- the token pairing that project already
 * uses to visually mark a control as "not a free-text input". */
const SELECT_CLASS =
  "h-9 w-full rounded-[9px] border border-border bg-background px-3 text-[13px] text-fg2 outline-none focus:border-primary focus:ring-[3px] focus:ring-primary-soft";
/** Read-only endpoint display: same footprint as `INPUT_CLASS` but
 * `bg-hover`/`text-fg2` (never `bg-panel`/`text-foreground`) so it reads as
 * non-editable even before a user notices the cursor, per the brief's "not
 * just gray text" requirement. */
const READONLY_INPUT_CLASS =
  "h-9 w-full cursor-default rounded-[9px] border border-border bg-hover px-3 text-[13px] text-fg2 outline-none focus:border-primary focus:ring-[3px] focus:ring-primary-soft";

type OssNetwork = "public" | "internal";

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
  // OSS-only: which network the shown (read-only) endpoint targets, and
  // whether the current endpoint is one this build can't map back to a
  // region -- a connection saved by hand before the region table existed.
  // Derived from the prefilled endpoint on mount (edit mode) or reset
  // whenever the provider picker switches to OSS (`chooseProvider` below);
  // inert for every other provider. `ossFormStateFromConnection` never
  // rewrites `form.endpoint` itself -- see its doc comment -- so opening the
  // edit dialog can't change a saved connection's endpoint.
  const [ossNetwork, setOssNetwork] = useState<OssNetwork>(
    () => ossFormStateFromConnection(editingConnection?.endpoint ?? "", editingConnection?.region ?? "").network,
  );
  const [ossUnknownEndpoint, setOssUnknownEndpoint] = useState<boolean>(
    () => ossFormStateFromConnection(editingConnection?.endpoint ?? "", editingConnection?.region ?? "").unknownEndpoint,
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
    const derived = ossFormStateFromConnection(meta.endpoint, meta.region);
    setOssNetwork(derived.network);
    setOssUnknownEndpoint(derived.unknownEndpoint);
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

  /** OSS region `<select>` changed. Every real `<option>` resolves via
   * `findOssRegion`, so the endpoint (read-only, derived) always gets
   * overwritten from the table -- this IS the "changing the region
   * overwrites a custom endpoint" behavior the UI warns about for
   * `ossUnknownEndpoint` connections. The one value that can't resolve is
   * the temporary "current value" option (see the render below), whose
   * value already equals `form.region`, so selecting it again never fires
   * `onChange` in the first place; the fallback below is defensive only. */
  function handleOssRegionChange(newRegionId: string) {
    const region = findOssRegion(newRegionId);
    if (!region) {
      updateField("region", newRegionId);
      return;
    }
    setOssUnknownEndpoint(false);
    setFieldErrors((e) => (e.endpoint ? { ...e, endpoint: undefined } : e));
    setForm((f) => ({ ...f, region: newRegionId, endpoint: ossEndpointFor(region, ossNetwork) }));
    invalidateTest();
  }

  /** OSS network segmented control changed. No-ops on the endpoint when
   * `form.region` isn't a known region yet (the `ossUnknownEndpoint` case --
   * nothing to derive from until the user picks a real region). */
  function handleOssNetworkChange(newNetwork: OssNetwork) {
    setOssNetwork(newNetwork);
    const region = findOssRegion(form.region);
    if (region) {
      setForm((f) => ({ ...f, endpoint: ossEndpointFor(region, newNetwork) }));
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
            {providerId === "oss" ? (
              <>
                <Field label={t("addConn.region")}>
                  <select
                    value={form.region}
                    onChange={(e) => handleOssRegionChange(e.target.value)}
                    className={SELECT_CLASS}
                  >
                    {ossUnknownEndpoint && (
                      <option value={form.region}>
                        {t("addConn.regionCurrentValue", { region: form.region || "—" })}
                      </option>
                    )}
                    {OSS_REGION_GROUPS.map((g) => (
                      <optgroup key={g.key} label={t(g.labelKey)}>
                        {OSS_REGIONS.filter((r) => r.group === g.key).map((r) => (
                          <option key={r.id} value={r.id}>{`${r.label} · ${r.id}`}</option>
                        ))}
                      </optgroup>
                    ))}
                  </select>
                </Field>
                <Field label={t("addConn.network")}>
                  <Segmented<OssNetwork>
                    value={ossNetwork}
                    onChange={handleOssNetworkChange}
                    options={[
                      { value: "public", label: t("addConn.networkPublic") },
                      { value: "internal", label: t("addConn.networkInternal") },
                    ]}
                  />
                </Field>
                <Field label={t("addConn.endpoint")} error={fieldErrors.endpoint}>
                  <input
                    value={form.endpoint}
                    readOnly
                    className={cn(READONLY_INPUT_CLASS, "font-mono")}
                  />
                  <div className="mt-1 text-[11.5px] text-muted-foreground">
                    {ossUnknownEndpoint && <div>{t("addConn.endpointCustomWarning")}</div>}
                    <div>{t("addConn.endpointGenericHint")}</div>
                  </div>
                </Field>
              </>
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
            )}
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
