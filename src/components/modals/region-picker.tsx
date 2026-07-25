import { useTranslation } from "react-i18next";
import { Segmented } from "@/components/ui/segmented";
import { Select, type SelectOption } from "@/components/ui/select";
import { cn } from "@/lib/utils";
import type { Network, RegionCatalog } from "@/lib/regions";

/** Value of the "keep this connection's current endpoint" option shown when a
 * saved connection's endpoint isn't in the official region table.
 *
 * Deliberately not the connection's own region id: a legacy connection can
 * store a real region id (`cn-beijing`) alongside a hand-typed endpoint, and
 * two options sharing a value make the real one unselectable -- picking it
 * leaves the value unchanged, so `change` never fires and that one region
 * becomes unreachable. A sentinel no region id can collide with keeps every
 * real option selectable.
 *
 * Written as an escape sequence, never as a raw NUL byte in the source: a raw
 * NUL inside the first 8000 bytes makes git treat this whole file as binary
 * (`git diff` degrades to "Binary files differ") and makes ripgrep skip it.
 * The runtime string is identical either way. */
export const REGION_KEEP_CURRENT = "\u0000keep-current";

const READONLY_INPUT_CLASS =
  "h-9 w-full cursor-default rounded-[9px] border border-border bg-hover px-3 text-[13px] text-fg2 outline-none focus:border-primary focus:ring-[3px] focus:ring-primary-soft";

/**
 * Region + network + derived-endpoint block of the connection form, for any
 * provider that ships a region catalog (`regionCatalog(provider)`).
 *
 * Purely presentational: every piece of state lives in the parent form, so
 * this component never has to reconcile with it. The network segmented
 * control renders only when the catalog has internal endpoints (Rainyun has
 * none), and group headings render only when the catalog defines groups.
 */
export function RegionPicker({
  catalog,
  regionId,
  network,
  endpoint,
  unknownEndpoint,
  onRegionChange,
  onNetworkChange,
  endpointError,
  hintKey,
}: {
  catalog: RegionCatalog;
  regionId: string;
  network: Network;
  endpoint: string;
  unknownEndpoint: boolean;
  onRegionChange: (regionId: string) => void;
  onNetworkChange: (network: Network) => void;
  endpointError?: string;
  /** Extra provider-specific i18n key rendered under the region select. */
  hintKey?: string;
}) {
  const { t } = useTranslation();

  const options: SelectOption[] = [
    ...(unknownEndpoint
      ? [
          {
            value: REGION_KEEP_CURRENT,
            label: t("addConn.regionCurrentValue", { region: regionId || "—" }),
          },
        ]
      : []),
    ...catalog.regions.map((r) => ({
      value: r.id,
      label: `${r.label} · ${r.id}`,
      group: r.group,
    })),
  ];

  const groups = catalog.groups?.map((g) => ({ key: g.key, label: t(g.labelKey) }));

  return (
    <>
      <div className="mb-3.5">
        <label className="mb-1.5 block text-xs font-medium text-fg2">
          {t("addConn.region")}
        </label>
        <Select
          value={unknownEndpoint ? REGION_KEEP_CURRENT : regionId}
          onChange={onRegionChange}
          options={options}
          groups={groups}
        />
        {hintKey && (
          <p className="mt-1 text-[11.5px] text-muted-foreground">{t(hintKey)}</p>
        )}
      </div>

      {catalog.hasInternalNetwork && (
        <div className="mb-3.5">
          <label className="mb-1.5 block text-xs font-medium text-fg2">
            {t("addConn.network")}
          </label>
          <Segmented<Network>
            value={network}
            onChange={onNetworkChange}
            options={[
              { value: "public", label: t("addConn.networkPublic") },
              { value: "internal", label: t("addConn.networkInternal") },
            ]}
          />
        </div>
      )}

      <div className="mb-3.5">
        <label className="mb-1.5 block text-xs font-medium text-fg2">
          {t("addConn.endpoint")}
        </label>
        <input value={endpoint} readOnly className={cn(READONLY_INPUT_CLASS, "font-mono")} />
        {endpointError && <p className="mt-1 text-[11.5px] text-destructive">{endpointError}</p>}
        <div className="mt-1 text-[11.5px] text-muted-foreground">
          {unknownEndpoint && <div>{t("addConn.endpointCustomWarning")}</div>}
          <div>{t("addConn.endpointGenericHint")}</div>
        </div>
      </div>
    </>
  );
}
