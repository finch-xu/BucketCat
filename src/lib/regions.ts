/**
 * Provider-agnostic region catalog layer.
 *
 * Two providers ship a built-in region table today (Aliyun OSS with 35
 * regions across 3 groups plus internal/public endpoints, Rainyun ROS with 2
 * flat regions). Everything the connection form needs to do with such a
 * table -- look a region up, map an endpoint back to a region, build an
 * endpoint, derive the edit form's initial state -- is identical for both,
 * so it lives here once and takes the catalog as a parameter.
 *
 * The per-provider *data* (and only the data) lives in `oss-regions.ts` /
 * `rainyun-regions.ts`, which import types from this module with
 * `import type` so there is no runtime import cycle.
 */
import { OSS_CATALOG } from "./oss-regions";
import { RAINYUN_CATALOG } from "./rainyun-regions";

/** Which network a region's endpoint targets. Only meaningful for catalogs
 * whose `hasInternalNetwork` is true; everything else is always "public". */
export type Network = "public" | "internal";

/** One selectable region. `internal` and `group` are optional because not
 * every provider has VPC endpoints or region groupings -- Rainyun has
 * neither. */
export interface RegionOption {
  /** Region id. This is also the value written to `connection.region`. */
  id: string;
  /** Human label, e.g. "华北2（北京）". */
  label: string;
  /** Public (internet) endpoint, no scheme. */
  endpoint: string;
  /** Internal (VPC) endpoint, no scheme. Absent when the provider has none. */
  internal?: string;
  /** Grouping key, matched against `RegionCatalog.groups[].key`. */
  group?: string;
}

/** An `<optgroup>`-style heading. `labelKey` is an i18n key, not a label. */
export interface RegionGroup {
  key: string;
  labelKey: string;
}

export interface RegionCatalog {
  regions: RegionOption[];
  /** When present, the picker renders grouped headings in this order. */
  groups?: RegionGroup[];
  /** Whether to offer a public/internal network toggle. */
  hasInternalNetwork: boolean;
}

/** Looks up a provider's region catalog, or undefined for providers that
 * don't ship one (the form then falls back to free-text endpoint/region). */
export function regionCatalog(provider: string): RegionCatalog | undefined {
  if (provider === "oss") return OSS_CATALOG;
  if (provider === "rainyun") return RAINYUN_CATALOG;
  return undefined;
}

export function findRegion(catalog: RegionCatalog, id: string): RegionOption | undefined {
  return catalog.regions.find((r) => r.id === id);
}

/** Strips an optional `https://`/`http://` prefix and trailing slashes so
 * endpoint comparisons are scheme-agnostic. */
function stripScheme(endpoint: string): string {
  return endpoint.trim().replace(/^https?:\/\//i, "").replace(/\/+$/, "");
}

/** Which region an endpoint belongs to (public or internal endpoint, with or
 * without a scheme). Undefined for custom domains not in the table. */
export function regionFromEndpoint(
  catalog: RegionCatalog,
  endpoint: string,
): RegionOption | undefined {
  const bare = stripScheme(endpoint).toLowerCase();
  if (!bare) return undefined;
  return catalog.regions.find(
    (r) => r.endpoint.toLowerCase() === bare || r.internal?.toLowerCase() === bare,
  );
}

/** Whether the given endpoint is some region's internal (VPC) endpoint. */
export function isInternalEndpoint(catalog: RegionCatalog, endpoint: string): boolean {
  const bare = stripScheme(endpoint).toLowerCase();
  if (!bare) return false;
  return catalog.regions.some((r) => r.internal?.toLowerCase() === bare);
}

/** Assembles the endpoint to put in the form, with an `https://` prefix,
 * always by table lookup -- never string concatenation. Falls back to the
 * public endpoint when `internal` is requested but the region has none. */
export function endpointFor(region: RegionOption, network: Network): string {
  const host = network === "internal" && region.internal ? region.internal : region.endpoint;
  return `https://${host}`;
}

/** Derives the region picker's initial UI state from an *already saved*
 * connection's endpoint/region, for prefilling the edit form.
 *
 * `endpoint` is always returned exactly as passed in -- byte-identical, no
 * trimming/normalizing -- because opening the edit dialog must never change
 * a connection's stored endpoint. Some connections were saved by hand before
 * the region table existed (or point at a custom domain the table doesn't
 * know about); for those `regionFromEndpoint` can't find a match, so
 * `unknownEndpoint` comes back `true` and `regionId` falls back to whatever
 * region string the connection already had. When the endpoint IS recognized,
 * `regionId` is the table's canonical id for it (source of truth is the
 * endpoint, not the possibly-stale `region` field). */
export function regionFormState(
  catalog: RegionCatalog,
  endpoint: string,
  region: string,
): { regionId: string; network: Network; endpoint: string; unknownEndpoint: boolean } {
  const found = regionFromEndpoint(catalog, endpoint);
  return {
    regionId: found ? found.id : region,
    network: isInternalEndpoint(catalog, endpoint) ? "internal" : "public",
    endpoint,
    unknownEndpoint: !found,
  };
}
