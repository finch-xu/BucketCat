import { describe, expect, it } from "vitest";
import {
  endpointFor,
  findRegion,
  isInternalEndpoint,
  regionFormState,
  regionFromEndpoint,
  type RegionCatalog,
} from "./regions";
import { OSS_CATALOG } from "./oss-regions";

/** 一个没有内网端点、没有分组的最小目录，用来验证 optional 字段的处理。 */
const FLAT_CATALOG: RegionCatalog = {
  regions: [
    { id: "a1", label: "地域甲", endpoint: "a1.example.com" },
    { id: "b2", label: "地域乙", endpoint: "b2.example.com" },
  ],
  hasInternalNetwork: false,
};

describe("findRegion", () => {
  it("finds a known region by id", () => {
    const region = findRegion(OSS_CATALOG, "cn-beijing");
    expect(region?.id).toBe("cn-beijing");
    expect(region?.label).toBe("华北2（北京）");
  });

  it("returns undefined for an unknown id", () => {
    expect(findRegion(OSS_CATALOG, "mars-1")).toBeUndefined();
    expect(findRegion(OSS_CATALOG, "")).toBeUndefined();
  });
});

describe("regionFromEndpoint", () => {
  it("finds a region from its public endpoint", () => {
    expect(regionFromEndpoint(OSS_CATALOG, "oss-cn-beijing.aliyuncs.com")?.id).toBe("cn-beijing");
  });

  it("finds a region from its internal endpoint", () => {
    expect(regionFromEndpoint(OSS_CATALOG, "oss-cn-beijing-internal.aliyuncs.com")?.id).toBe(
      "cn-beijing",
    );
  });

  it("finds a region when the endpoint carries an https:// prefix", () => {
    expect(regionFromEndpoint(OSS_CATALOG, "https://oss-cn-beijing.aliyuncs.com")?.id).toBe(
      "cn-beijing",
    );
    expect(
      regionFromEndpoint(OSS_CATALOG, "https://oss-cn-beijing-internal.aliyuncs.com")?.id,
    ).toBe("cn-beijing");
  });

  it("finds finance-cloud regions whose endpoint doesn't follow the normal pattern", () => {
    expect(regionFromEndpoint(OSS_CATALOG, "https://oss-cn-hzfinance.aliyuncs.com")?.id).toBe(
      "cn-hangzhou-finance",
    );
    expect(regionFromEndpoint(OSS_CATALOG, "oss-cn-hzfinance-internal.aliyuncs.com")?.id).toBe(
      "cn-hangzhou-finance",
    );
  });

  it("returns undefined for a custom domain", () => {
    expect(regionFromEndpoint(OSS_CATALOG, "https://cdn.example.com")).toBeUndefined();
    expect(regionFromEndpoint(OSS_CATALOG, "")).toBeUndefined();
  });

  it("works on a catalog whose regions have no internal endpoint", () => {
    expect(regionFromEndpoint(FLAT_CATALOG, "https://a1.example.com")?.id).toBe("a1");
    expect(regionFromEndpoint(FLAT_CATALOG, "nope.example.com")).toBeUndefined();
  });
});

describe("isInternalEndpoint", () => {
  it("is true for internal endpoints", () => {
    expect(isInternalEndpoint(OSS_CATALOG, "oss-cn-beijing-internal.aliyuncs.com")).toBe(true);
    expect(isInternalEndpoint(OSS_CATALOG, "https://oss-cn-beijing-internal.aliyuncs.com")).toBe(
      true,
    );
  });

  it("is false for public endpoints and custom domains", () => {
    expect(isInternalEndpoint(OSS_CATALOG, "oss-cn-beijing.aliyuncs.com")).toBe(false);
    expect(isInternalEndpoint(OSS_CATALOG, "https://cdn.example.com")).toBe(false);
    expect(isInternalEndpoint(OSS_CATALOG, "")).toBe(false);
  });

  it("is always false for a catalog with no internal endpoints at all", () => {
    expect(isInternalEndpoint(FLAT_CATALOG, "a1.example.com")).toBe(false);
  });
});

describe("endpointFor", () => {
  it("builds the public endpoint with an https:// prefix", () => {
    const region = findRegion(OSS_CATALOG, "cn-beijing")!;
    expect(endpointFor(region, "public")).toBe("https://oss-cn-beijing.aliyuncs.com");
  });

  it("builds the internal endpoint with an https:// prefix", () => {
    const region = findRegion(OSS_CATALOG, "cn-beijing")!;
    expect(endpointFor(region, "internal")).toBe("https://oss-cn-beijing-internal.aliyuncs.com");
  });

  it("falls back to the public endpoint when the region has no internal one", () => {
    expect(endpointFor(FLAT_CATALOG.regions[0], "internal")).toBe("https://a1.example.com");
  });
});

describe("regionFormState", () => {
  it("recognizes a known public endpoint", () => {
    expect(
      regionFormState(OSS_CATALOG, "https://oss-cn-beijing.aliyuncs.com", "cn-beijing"),
    ).toEqual({
      regionId: "cn-beijing",
      network: "public",
      endpoint: "https://oss-cn-beijing.aliyuncs.com",
      unknownEndpoint: false,
    });
  });

  it("recognizes a known internal endpoint", () => {
    expect(
      regionFormState(OSS_CATALOG, "https://oss-cn-beijing-internal.aliyuncs.com", "cn-beijing"),
    ).toEqual({
      regionId: "cn-beijing",
      network: "internal",
      endpoint: "https://oss-cn-beijing-internal.aliyuncs.com",
      unknownEndpoint: false,
    });
  });

  it("derives the region from the endpoint's table lookup, not the stored region string", () => {
    const result = regionFormState(OSS_CATALOG, "https://oss-cn-shanghai.aliyuncs.com", "cn-beijing");
    expect(result.regionId).toBe("cn-shanghai");
    expect(result.unknownEndpoint).toBe(false);
  });

  it("preserves a fully custom endpoint byte-identical and flags it as unknown", () => {
    const customEndpoint = "https://oss.my-custom-domain.example.com:1234/weird//path/";
    const result = regionFormState(OSS_CATALOG, customEndpoint, "legacy-region-id");
    expect(result.endpoint).toBe(customEndpoint);
    expect(result.unknownEndpoint).toBe(true);
    expect(result.regionId).toBe("legacy-region-id");
    expect(result.network).toBe("public");
  });

  it("never rewrites the endpoint even for a known region (identity, not just equality)", () => {
    const withWhitespace = "  https://oss-cn-beijing.aliyuncs.com  ";
    const result = regionFormState(OSS_CATALOG, withWhitespace, "cn-beijing");
    expect(result.endpoint).toBe(withWhitespace);
  });
});
