import { Check, ChevronLeft, ChevronRight, Lock, X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Modal } from "@/components/ui/modal";
import { PROVIDERS } from "@/lib/providers";
import { useApp } from "@/store/app-store";

const INPUT_CLASS =
  "h-9 w-full rounded-[9px] border border-border bg-panel px-3 text-[13px] text-foreground outline-none focus:border-primary focus:ring-[3px] focus:ring-primary-soft";

function Field({ label, children }: { label: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="mb-3.5">
      <label className="mb-1.5 block text-xs font-medium text-fg2">{label}</label>
      {children}
    </div>
  );
}

export function AddConnectionModal() {
  const { t } = useTranslation();
  const { showAdd, addStep, addProvider, closeAdd, chooseProvider, backToProviders } = useApp();

  if (!showAdd) return null;

  const provider = PROVIDERS.find((p) => p.id === addProvider);

  return (
    <Modal onClose={closeAdd} className="w-[560px]">
      {addStep === 1 && (
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
      {addStep === 2 && provider && (
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
            <Field label={t("addConn.name")}>
              <input placeholder={t("addConn.namePlaceholder")} className={INPUT_CLASS} />
            </Field>
            <div className="mb-3.5 grid grid-cols-[2fr_1fr] gap-3">
              <div>
                <label className="mb-1.5 block text-xs font-medium text-fg2">
                  {t("addConn.endpoint")}
                </label>
                <input placeholder={provider.endpoint} className={`${INPUT_CLASS} font-mono`} />
              </div>
              <div>
                <label className="mb-1.5 block text-xs font-medium text-fg2">
                  {t("addConn.region")}
                </label>
                <input placeholder={provider.region || "—"} className={`${INPUT_CLASS} font-mono`} />
              </div>
            </div>
            <Field label={t("addConn.accessKey")}>
              <input placeholder="AKIA••••••••••••" className={`${INPUT_CLASS} font-mono`} />
            </Field>
            <Field label={t("addConn.secretKey")}>
              <div className="flex h-9 items-center gap-2 rounded-[9px] border border-border bg-panel px-3 focus-within:border-primary focus-within:ring-[3px] focus-within:ring-primary-soft">
                <Lock className="size-3.5 text-muted-foreground" />
                <input
                  type="password"
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
              <input placeholder="my-bucket" className={`${INPUT_CLASS} font-mono`} />
            </div>
          </div>
          <div className="mt-2 flex items-center gap-2.5 border-t border-border2 px-[22px] pt-3.5 pb-5">
            <button
              type="button"
              className="inline-flex h-9 cursor-pointer items-center gap-[7px] rounded-[9px] border border-border bg-background px-3.5 text-[13px] font-medium text-fg2 hover:bg-hover"
            >
              <Check className="size-3.5 text-sky" />
              {t("addConn.test")}
            </button>
            <div className="flex-1" />
            <button
              type="button"
              onClick={closeAdd}
              className="h-9 cursor-pointer rounded-[9px] border border-border bg-background px-4 text-[13px] font-medium text-fg2 hover:bg-hover"
            >
              {t("addConn.cancel")}
            </button>
            <button
              type="button"
              onClick={closeAdd}
              className="h-9 cursor-pointer rounded-[9px] bg-primary px-[18px] text-[13px] font-semibold text-primary-foreground hover:bg-primary-strong"
            >
              {t("addConn.save")}
            </button>
          </div>
        </>
      )}
    </Modal>
  );
}
