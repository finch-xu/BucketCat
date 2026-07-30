import { describe, expect, it } from "vitest";
import { B2_CATALOG, B2_REGIONS, b2RegionFromKeyId, looksLikeB2MasterKeyId } from "./b2-regions";
import { endpointFor, findRegion, isInternalEndpoint, regionCatalog, regionFromEndpoint } from "./regions";
import { PROVIDERS } from "./providers";

/** The account id behind the key this feature was developed against.
 *
 * Not a credential: an account id is the `Owner/ID` of every `ListBuckets`
 * response and authenticates nothing on its own. It appears here because it is
 * the only way to test the master-key path against a *real* shape rather than
 * an invented one. */
const REAL_ACCOUNT_ID = "024147e7d076";
/** The real application key **id** (never the secret) it belongs to. */
const REAL_KEY_ID = `004${REAL_ACCOUNT_ID}0000000001`;

describe("B2_REGIONS", () => {
  it("has exactly the seven regions DNS confirms exist", () => {
    expect(B2_REGIONS.map((r) => r.id)).toEqual([
      "us-west-000",
      "us-west-001",
      "us-west-002",
      "eu-central-003",
      "us-west-004",
      "us-east-005",
      "ca-east-006",
    ]);
  });

  it("stores endpoints without a scheme", () => {
    for (const region of B2_REGIONS) {
      expect(region.endpoint).not.toContain("://");
      expect(region.endpoint.length).toBeGreaterThan(0);
    }
  });

  // Backblaze's naming is uniform (DNS-verified 2026-07-31). This does NOT
  // authorize building endpoints by concatenation -- the table stays the
  // single source of truth -- it only makes a mistyped row fail here rather
  // than at a user's first request.
  it("follows the s3.{id}.backblazeb2.com naming pattern for every region", () => {
    for (const region of B2_REGIONS) {
      expect(region.endpoint).toBe(`s3.${region.id}.backblazeb2.com`);
    }
  });

  it("has no internal endpoints and no groups", () => {
    expect(B2_CATALOG.hasInternalNetwork).toBe(false);
    expect(B2_CATALOG.groups).toBeUndefined();
    for (const region of B2_REGIONS) {
      expect(region.internal).toBeUndefined();
      expect(region.group).toBeUndefined();
    }
  });

  it("round-trips each region through endpointFor/regionFromEndpoint", () => {
    for (const region of B2_REGIONS) {
      const url = endpointFor(region, "public");
      expect(url).toBe(`https://${region.endpoint}`);
      expect(regionFromEndpoint(B2_CATALOG, url)?.id).toBe(region.id);
    }
  });

  it("falls back to the public endpoint when the internal network is requested", () => {
    for (const region of B2_REGIONS) {
      expect(endpointFor(region, "internal")).toBe(`https://${region.endpoint}`);
      expect(isInternalEndpoint(B2_CATALOG, region.endpoint)).toBe(false);
    }
  });

  it("is reachable through regionCatalog under the 'b2' provider id", () => {
    expect(regionCatalog("b2")).toBe(B2_CATALOG);
  });

  it("gives every region a unique cluster number matching its id suffix", () => {
    const clusters = B2_REGIONS.map((r) => r.cluster);
    expect(new Set(clusters).size).toBe(clusters.length);
    for (const region of B2_REGIONS) {
      expect(region.id.endsWith(`-${region.cluster}`)).toBe(true);
    }
  });
});

describe("b2RegionFromKeyId", () => {
  it("derives us-west-004 from a real application key id", () => {
    expect(b2RegionFromKeyId(REAL_KEY_ID)?.id).toBe("us-west-004");
  });

  it("derives every cluster in the table", () => {
    for (const region of B2_REGIONS) {
      const keyId = `${region.cluster}${REAL_ACCOUNT_ID}0000000001`;
      expect(b2RegionFromKeyId(keyId)?.id).toBe(region.id);
    }
  });

  it("trims surrounding whitespace from a pasted key id", () => {
    expect(b2RegionFromKeyId(`  ${REAL_KEY_ID}\n`)?.id).toBe("us-west-004");
  });

  // The case the length gate exists for: a 12-hex master key id whose account
  // id happens to start with `004`. Matching on the prefix alone would hand
  // back a plausible, wrong endpoint for a credential B2 rejects anyway.
  it("does not read a region out of a master key id that starts with digits", () => {
    expect(b2RegionFromKeyId("004147e7d076")).toBeUndefined();
    expect(looksLikeB2MasterKeyId("004147e7d076")).toBe(true);
  });

  it("returns undefined for an unknown cluster, so the picker takes over", () => {
    expect(b2RegionFromKeyId(`999${REAL_ACCOUNT_ID}0000000001`)).toBeUndefined();
  });

  it("returns undefined for a non-numeric prefix", () => {
    expect(b2RegionFromKeyId(`abc${REAL_ACCOUNT_ID}0000000001`)).toBeUndefined();
  });

  it("returns undefined until the key id is long enough to not be a master key", () => {
    for (const partial of ["", "0", "004", REAL_KEY_ID.slice(0, 12)]) {
      expect(b2RegionFromKeyId(partial)).toBeUndefined();
    }
  });

  // Deliberate: the cluster number is complete after three characters, so the
  // region appears the moment it is unambiguous rather than waiting for the
  // whole id. The only value the length gate has to exclude is a master key
  // id, which is exactly 12 characters -- one more than that and the prefix
  // can be trusted.
  it("resolves as soon as the id outgrows a master key id, mid-typing", () => {
    expect(b2RegionFromKeyId(REAL_KEY_ID.slice(0, 13))?.id).toBe("us-west-004");
  });
});

describe("looksLikeB2MasterKeyId", () => {
  it("recognizes a bare account id, case-insensitively and trimmed", () => {
    expect(looksLikeB2MasterKeyId(REAL_ACCOUNT_ID)).toBe(true);
    expect(looksLikeB2MasterKeyId(`  ${REAL_ACCOUNT_ID}  `)).toBe(true);
    expect(looksLikeB2MasterKeyId(REAL_ACCOUNT_ID.toUpperCase())).toBe(true);
  });

  it("does not flag a full application key id", () => {
    expect(looksLikeB2MasterKeyId(REAL_KEY_ID)).toBe(false);
  });

  // Conservative by design: unsure means false, so a working credential is
  // never blocked on a guess.
  it("does not flag anything of the wrong length or with non-hex characters", () => {
    for (const id of ["", "024147e7d07", "024147e7d0760", "024147e7d07g", "hello world!"]) {
      expect(looksLikeB2MasterKeyId(id)).toBe(false);
    }
  });
});

describe("b2 provider preset", () => {
  it("is registered with a default endpoint and region that resolve in the catalog", () => {
    const preset = PROVIDERS.find((p) => p.id === "b2");
    expect(preset).toBeDefined();
    expect(findRegion(B2_CATALOG, preset!.region)).toBeDefined();
    expect(regionFromEndpoint(B2_CATALOG, preset!.endpoint)?.id).toBe(preset!.region);
  });

  // Backblaze's official brand red. The icon is simple-icons' monochrome mark,
  // so `colorLogo` must be false -- otherwise `ProviderGlyph` skips the
  // coloring and it renders black.
  it("uses the official brand color and is marked as a monochrome logo", () => {
    const preset = PROVIDERS.find((p) => p.id === "b2");
    expect(preset!.color).toBe("#E21E29");
    expect(preset!.colorLogo).toBe(false);
  });
});
