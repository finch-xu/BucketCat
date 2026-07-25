import { describe, expect, it } from "vitest";
import { RAINYUN_CATALOG, RAINYUN_REGIONS } from "./rainyun-regions";
import { endpointFor, findRegion, regionCatalog, regionFromEndpoint } from "./regions";
import { PROVIDERS } from "./providers";
import { OSS_CATALOG } from "./oss-regions";

describe("RAINYUN_REGIONS", () => {
  it("has exactly the two regions Rainyun operates", () => {
    expect(RAINYUN_REGIONS.map((r) => r.id)).toEqual(["cn-nb1", "cn-sy1"]);
  });

  it("stores endpoints without a scheme", () => {
    for (const region of RAINYUN_REGIONS) {
      expect(region.endpoint).not.toContain("://");
      expect(region.endpoint.length).toBeGreaterThan(0);
    }
  });

  // 实测确认：雨云无 VPC 端点，两个区域也不分组。
  it("has no internal endpoints and no groups", () => {
    expect(RAINYUN_CATALOG.hasInternalNetwork).toBe(false);
    expect(RAINYUN_CATALOG.groups).toBeUndefined();
    for (const region of RAINYUN_REGIONS) {
      expect(region.internal).toBeUndefined();
      expect(region.group).toBeUndefined();
    }
  });

  it("round-trips each region through endpointFor/regionFromEndpoint", () => {
    for (const region of RAINYUN_REGIONS) {
      const url = endpointFor(region, "public");
      expect(url).toBe(`https://${region.endpoint}`);
      expect(regionFromEndpoint(RAINYUN_CATALOG, url)?.id).toBe(region.id);
    }
  });

  it("is reachable through regionCatalog under the 'rainyun' provider id", () => {
    expect(regionCatalog("rainyun")).toBe(RAINYUN_CATALOG);
  });

  // regionCatalog 的另外两个分支：OSS 有目录，未知 provider 必须返回
  // undefined（连接表单据此回退到手填 endpoint/region）。
  it("resolves the OSS catalog and returns undefined for providers without one", () => {
    expect(regionCatalog("oss")).toBe(OSS_CATALOG);
    expect(regionCatalog("minio")).toBeUndefined();
    expect(regionCatalog("")).toBeUndefined();
  });
});

describe("rainyun provider preset", () => {
  it("is registered with a default endpoint and region that resolve in the catalog", () => {
    const preset = PROVIDERS.find((p) => p.id === "rainyun");
    expect(preset).toBeDefined();
    expect(findRegion(RAINYUN_CATALOG, preset!.region)).toBeDefined();
    expect(regionFromEndpoint(RAINYUN_CATALOG, preset!.endpoint)?.id).toBe(preset!.region);
  });
});
