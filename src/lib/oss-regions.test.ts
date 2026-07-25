import { describe, expect, it } from "vitest";
import { findRegion } from "./regions";
import { OSS_CATALOG, OSS_REGIONS } from "./oss-regions";

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
    const hangzhouFinance = findRegion(OSS_CATALOG, "cn-hangzhou-finance");
    expect(hangzhouFinance?.endpoint).toBe("oss-cn-hzfinance.aliyuncs.com");
  });
});
