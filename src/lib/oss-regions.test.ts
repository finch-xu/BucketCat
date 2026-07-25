import { describe, expect, it } from "vitest";
import {
  findOssRegion,
  isInternalOssEndpoint,
  ossEndpointFor,
  ossRegionFromEndpoint,
  OSS_REGIONS,
} from "./oss-regions";

describe("findOssRegion", () => {
  it("finds a known region by id", () => {
    const region = findOssRegion("cn-beijing");
    expect(region?.id).toBe("cn-beijing");
    expect(region?.label).toBe("华北2（北京）");
  });

  it("returns undefined for an unknown id", () => {
    expect(findOssRegion("mars-1")).toBeUndefined();
    expect(findOssRegion("")).toBeUndefined();
  });
});

describe("ossRegionFromEndpoint", () => {
  it("finds a region from its public endpoint", () => {
    expect(ossRegionFromEndpoint("oss-cn-beijing.aliyuncs.com")?.id).toBe("cn-beijing");
  });

  it("finds a region from its internal endpoint", () => {
    expect(ossRegionFromEndpoint("oss-cn-beijing-internal.aliyuncs.com")?.id).toBe("cn-beijing");
  });

  it("finds a region when the endpoint carries an https:// prefix", () => {
    expect(ossRegionFromEndpoint("https://oss-cn-beijing.aliyuncs.com")?.id).toBe("cn-beijing");
    expect(ossRegionFromEndpoint("https://oss-cn-beijing-internal.aliyuncs.com")?.id).toBe(
      "cn-beijing",
    );
  });

  it("finds finance-cloud regions whose endpoint doesn't follow the normal pattern", () => {
    expect(ossRegionFromEndpoint("https://oss-cn-hzfinance.aliyuncs.com")?.id).toBe(
      "cn-hangzhou-finance",
    );
    expect(ossRegionFromEndpoint("oss-cn-hzfinance-internal.aliyuncs.com")?.id).toBe(
      "cn-hangzhou-finance",
    );
  });

  it("returns undefined for a custom domain", () => {
    expect(ossRegionFromEndpoint("https://cdn.example.com")).toBeUndefined();
    expect(ossRegionFromEndpoint("")).toBeUndefined();
  });
});

describe("isInternalOssEndpoint", () => {
  it("is true for internal endpoints", () => {
    expect(isInternalOssEndpoint("oss-cn-beijing-internal.aliyuncs.com")).toBe(true);
    expect(isInternalOssEndpoint("https://oss-cn-beijing-internal.aliyuncs.com")).toBe(true);
  });

  it("is false for public endpoints and custom domains", () => {
    expect(isInternalOssEndpoint("oss-cn-beijing.aliyuncs.com")).toBe(false);
    expect(isInternalOssEndpoint("https://cdn.example.com")).toBe(false);
    expect(isInternalOssEndpoint("")).toBe(false);
  });
});

describe("ossEndpointFor", () => {
  it("builds the public endpoint with an https:// prefix", () => {
    const region = findOssRegion("cn-beijing")!;
    expect(ossEndpointFor(region, "public")).toBe("https://oss-cn-beijing.aliyuncs.com");
  });

  it("builds the internal endpoint with an https:// prefix", () => {
    const region = findOssRegion("cn-beijing")!;
    expect(ossEndpointFor(region, "internal")).toBe(
      "https://oss-cn-beijing-internal.aliyuncs.com",
    );
  });
});

describe("data integrity", () => {
  it("has globally unique ids", () => {
    const ids = OSS_REGIONS.map((r) => r.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("has globally unique endpoints (public and internal don't collide either)", () => {
    const endpoints = OSS_REGIONS.flatMap((r) => [r.endpoint, r.internal]);
    expect(new Set(endpoints).size).toBe(endpoints.length);
  });

  it("has non-empty endpoint/internal values with no scheme prefix", () => {
    for (const region of OSS_REGIONS) {
      expect(region.endpoint.length).toBeGreaterThan(0);
      expect(region.internal.length).toBeGreaterThan(0);
      expect(region.endpoint).not.toContain("://");
      expect(region.internal).not.toContain("://");
    }
  });

  it("locks in that finance-cloud endpoints do NOT follow the oss-{id}.aliyuncs.com pattern", () => {
    const financeRegions = OSS_REGIONS.filter((r) => r.group === "finance");
    expect(financeRegions.length).toBeGreaterThan(0);
    for (const region of financeRegions) {
      expect(region.endpoint).not.toBe(`oss-${region.id}.aliyuncs.com`);
    }
    // Spot-check the specific example called out in the brief.
    const hangzhouFinance = findOssRegion("cn-hangzhou-finance");
    expect(hangzhouFinance?.endpoint).toBe("oss-cn-hzfinance.aliyuncs.com");
  });
});
