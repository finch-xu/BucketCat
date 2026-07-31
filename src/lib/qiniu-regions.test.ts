import { describe, expect, it } from "vitest";
import { QINIU_CATALOG, QINIU_REGIONS } from "./qiniu-regions";
import { endpointFor, findRegion, regionCatalog, regionFromEndpoint, isInternalEndpoint } from "./regions";
import { PROVIDERS } from "./providers";

describe("QINIU_REGIONS", () => {
  it("has exactly the eight regions Qiniu documents S3 endpoints for", () => {
    expect(QINIU_REGIONS.map((r) => r.id)).toEqual([
      "cn-east-1",
      "cn-east-2",
      "cn-north-1",
      "cn-south-1",
      "us-north-1",
      "ap-southeast-1",
      "ap-southeast-2",
      "ap-southeast-3",
    ]);
  });

  it("stores endpoints without a scheme", () => {
    for (const region of QINIU_REGIONS) {
      expect(region.endpoint).not.toContain("://");
      expect(region.endpoint.length).toBeGreaterThan(0);
    }
  });

  // 七牛的命名规律统一是 `s3.{id}.qiniucs.com`（DNS 全量核对过），这条测试
  // 锁住这个事实。它并不授权用拼接代替查表 —— 表仍是唯一事实来源；这里只是
  // 让「某一条被手抄错」暴露出来。
  it("follows the s3.{id}.qiniucs.com naming pattern for every region", () => {
    for (const region of QINIU_REGIONS) {
      expect(region.endpoint).toBe(`s3.${region.id}.qiniucs.com`);
    }
  });

  it("has no internal endpoints and no groups", () => {
    expect(QINIU_CATALOG.hasInternalNetwork).toBe(false);
    expect(QINIU_CATALOG.groups).toBeUndefined();
    for (const region of QINIU_REGIONS) {
      expect(region.internal).toBeUndefined();
      expect(region.group).toBeUndefined();
    }
  });

  it("round-trips each region through endpointFor/regionFromEndpoint", () => {
    for (const region of QINIU_REGIONS) {
      const url = endpointFor(region, "public");
      expect(url).toBe(`https://${region.endpoint}`);
      expect(regionFromEndpoint(QINIU_CATALOG, url)?.id).toBe(region.id);
    }
  });

  // 没有内网端点，所以 "internal" 必须回落到公网端点而不是产出空串/undefined
  // （`RegionPicker` 也因此不渲染公网/内网切换）。
  it("falls back to the public endpoint when the internal network is requested", () => {
    for (const region of QINIU_REGIONS) {
      expect(endpointFor(region, "internal")).toBe(`https://${region.endpoint}`);
      expect(isInternalEndpoint(QINIU_CATALOG, region.endpoint)).toBe(false);
    }
  });

  it("is reachable through regionCatalog under the 'qiniu' provider id", () => {
    expect(regionCatalog("qiniu")).toBe(QINIU_CATALOG);
  });
});

describe("qiniu provider preset", () => {
  it("is registered with a default endpoint and region that resolve in the catalog", () => {
    const preset = PROVIDERS.find((p) => p.id === "qiniu");
    expect(preset).toBeDefined();
    expect(findRegion(QINIU_CATALOG, preset!.region)).toBeDefined();
    expect(regionFromEndpoint(QINIU_CATALOG, preset!.endpoint)?.id).toBe(preset!.region);
  });

  // 七牛官方品牌色。图标本身是中性的 lucide 线性图标（商标合规，见
  // `providers.ts`），所以这个色值是七牛在向导里**唯一**的品牌识别，改错了
  // 没有别的东西能兜住。
  it("uses the official brand color", () => {
    const preset = PROVIDERS.find((p) => p.id === "qiniu");
    expect(preset!.color).toBe("#06AEEF");
  });
});
