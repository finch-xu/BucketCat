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
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Modal } from "@/components/ui/modal";
import { useAddConnection } from "@/hooks/use-connections";
import { useErrorText } from "@/hooks/use-error-text";
import type { AppError, ConnectionInput } from "@/lib/api";
import { testConnection } from "@/lib/api";
import { cn } from "@/lib/utils";
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

export function AddConnectionModal() {
  const { t } = useTranslation();
  const errorText = useErrorText();
  const { showAdd, closeAdd } = useApp();
  const addMutation = useAddConnection();

  // Wizard step + form state are ephemeral to this modal instance, so they
  // live in component-local state rather than the global app store.
  const [step, setStep] = useState<1 | 2>(1);
  const [providerId, setProviderId] = useState<string | null>(null);
  const [form, setForm] = useState<FormState>(EMPTY_FORM);
  const [fieldErrors, setFieldErrors] = useState<FieldErrors>({});
  const [testStatus, setTestStatus] = useState<TestStatus>({ kind: "idle" });

  // Reset the whole wizard every time it's (re)opened, so a previous run's
  // step/provider/values/test result never leak into the next one.
  useEffect(() => {
    if (showAdd) {
      setStep(1);
      setProviderId(null);
      setForm(EMPTY_FORM);
      setFieldErrors({});
      setTestStatus({ kind: "idle" });
      addMutation.reset();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [showAdd]);

  if (!showAdd) return null;

  const provider = PROVIDERS.find((p) => p.id === providerId);

  function chooseProvider(id: string) {
    const meta = providerMeta(id);
    setProviderId(id);
    setForm({ ...EMPTY_FORM, endpoint: meta.endpoint, region: meta.region });
    setFieldErrors({});
    setTestStatus({ kind: "idle" });
    setStep(2);
  }

  function backToProviders() {
    setStep(1);
    setProviderId(null);
  }

  function updateField(key: keyof FormState, value: string) {
    setForm((f) => ({ ...f, [key]: value }));
    if (fieldErrors[key as RequiredField]) {
      setFieldErrors((e) => ({ ...e, [key]: undefined }));
    }
    if (testStatus.kind !== "idle") setTestStatus({ kind: "idle" });
  }

  function buildInput(): ConnectionInput {
    return {
      provider: providerId ?? "generic",
      name: form.name.trim(),
      endpoint: form.endpoint.trim(),
      region: form.region.trim(),
      access_key_id: form.access_key_id.trim(),
      secret_access_key: form.secret_access_key,
      default_bucket: form.default_bucket.trim() ? form.default_bucket.trim() : null,
    };
  }

  function validate(): boolean {
    const errors: FieldErrors = {};
    for (const key of REQUIRED_FIELDS) {
      if (!form[key].trim()) errors[key] = t("addConn.required");
    }
    setFieldErrors(errors);
    return Object.keys(errors).length === 0;
  }

  async function handleTest() {
    if (!validate()) return;
    setTestStatus({ kind: "pending" });
    try {
      await testConnection(buildInput());
      setTestStatus({ kind: "success" });
    } catch (err) {
      setTestStatus({ kind: "error", error: err as AppError });
    }
  }

  function handleSave() {
    if (!validate()) return;
    addMutation.mutate(buildInput(), {
      onSuccess: () => closeAdd(),
    });
  }

  return (
    <Modal onClose={closeAdd} className="w-[560px]">
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
              onClick={closeAdd}
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
            <button
              type="button"
              onClick={backToProviders}
              className="flex size-[30px] shrink-0 cursor-pointer items-center justify-center rounded-lg border border-border bg-background text-fg2 hover:bg-hover"
            >
              <ChevronLeft className="size-4" />
            </button>
            <span
              className="flex size-[34px] items-center justify-center rounded-[9px] shadow-[0_2px_5px_rgba(0,0,0,0.2)]"
              style={{ background: provider.color }}
            >
              <provider.icon className="size-5 text-white" />
            </span>
            <div className="flex-1">
              <div className="text-[15px] font-bold">
                {provider.nameKey ? t(provider.nameKey) : provider.name}
              </div>
              <div className="text-xs text-muted-foreground">{t("addConn.fillCreds")}</div>
            </div>
            <button
              type="button"
              onClick={closeAdd}
              className="flex size-[30px] cursor-pointer items-center justify-center rounded-lg text-muted-foreground hover:bg-hover hover:text-fg2"
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
                  placeholder="••••••••••••••••••••"
                  className="flex-1 border-none bg-transparent font-mono text-[13px] text-foreground outline-none"
                />
              </div>
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
              disabled={testStatus.kind === "pending"}
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
              onClick={closeAdd}
              className="h-9 shrink-0 cursor-pointer rounded-[9px] border border-border bg-background px-4 text-[13px] font-medium text-fg2 hover:bg-hover"
            >
              {t("addConn.cancel")}
            </button>
            <button
              type="button"
              onClick={handleSave}
              disabled={addMutation.isPending}
              className="inline-flex h-9 shrink-0 cursor-pointer items-center gap-[7px] rounded-[9px] bg-primary px-[18px] text-[13px] font-semibold text-primary-foreground hover:bg-primary-strong disabled:cursor-not-allowed disabled:opacity-70"
            >
              {addMutation.isPending && <Loader2 className="size-3.5 animate-spin" />}
              {addMutation.isPending ? t("addConn.saving") : t("addConn.save")}
            </button>
          </div>
          {addMutation.isError && (
            <div className="px-[22px] pb-4 text-[12.5px] text-destructive">
              {errorText(addMutation.error)}
            </div>
          )}
        </>
      )}
    </Modal>
  );
}
