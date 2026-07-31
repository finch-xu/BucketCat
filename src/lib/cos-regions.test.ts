import { describe, expect, it } from "vitest";
import { COS_CATALOG, COS_REGIONS } from "./cos-regions";
import { endpointFor, findRegion, regionCatalog, regionFromEndpoint, isInternalEndpoint } from "./regions";
import { PROVIDERS, SELECTABLE_PROVIDERS } from "./providers";

describe("COS_REGIONS", () => {
  // 顺序也钉住：下拉框按数组顺序渲染，中国大陆在前、港澳台及海外在后，是刻意
  // 的排布（本项目用户主要在中国大陆）。金融云三个地域**不在**表里，理由见
  // `cos-regions.ts` 的 TODO(金融云)。
  it("has exactly the seventeen public-cloud regions, in display order", () => {
    expect(COS_REGIONS.map((r) => r.id)).toEqual([
      "ap-beijing",
      "ap-nanjing",
      "ap-shanghai",
      "ap-guangzhou",
      "ap-chengdu",
      "ap-chongqing",
      "ap-hongkong",
      "ap-singapore",
      "ap-jakarta",
      "ap-seoul",
      "ap-bangkok",
      "ap-tokyo",
      "me-saudi-arabia",
      "na-siliconvalley",
      "na-ashburn",
      "sa-saopaulo",
      "eu-frankfurt",
    ]);
  });

  // 金融云是独立开通的产品，普通账号选中必然连不上。这条断言不是说金融云永远
  // 不能进表，而是说进表必须是个**有意识的决定** —— 谁把它加回来，就会看到这
  // 条测试红掉，然后被引导去读 `cos-regions.ts` 里那三条路的取舍。
  it("leaves the finance-cloud regions out of the picker", () => {
    for (const id of COS_REGIONS.map((r) => r.id)) {
      expect(id.endsWith("-fsi"), `${id} is a finance-cloud region`).toBe(false);
    }
  });

  it("stores endpoints without a scheme", () => {
    for (const region of COS_REGIONS) {
      expect(region.endpoint).not.toContain("://");
      expect(region.endpoint.length).toBeGreaterThan(0);
    }
  });

  // 腾讯云的命名规律统一是 `cos.{id}.myqcloud.com`（20 个域名全部发过未鉴权
  // HTTPS 请求核对，见 `cos-regions.ts`）。这条测试锁住这个事实，但**不**授权
  // 用拼接代替查表 —— 表仍是唯一事实来源；这里只是让「某一条被手抄错」暴露。
  it("follows the cos.{id}.myqcloud.com naming pattern for every region", () => {
    for (const region of COS_REGIONS) {
      expect(region.endpoint).toBe(`cos.${region.id}.myqcloud.com`);
    }
  });

  // COS 确实有内网域名（`cos-internal.{id}.tencentcos.cn`），但它只在腾讯云
  // VPC 内可达，而 BucketCat 是跑在用户自己机器上的桌面客户端。
  it("has no internal endpoints and no groups", () => {
    expect(COS_CATALOG.hasInternalNetwork).toBe(false);
    expect(COS_CATALOG.groups).toBeUndefined();
    for (const region of COS_REGIONS) {
      expect(region.internal).toBeUndefined();
      expect(region.group).toBeUndefined();
    }
  });

  it("round-trips each region through endpointFor/regionFromEndpoint", () => {
    for (const region of COS_REGIONS) {
      const url = endpointFor(region, "public");
      expect(url).toBe(`https://${region.endpoint}`);
      expect(regionFromEndpoint(COS_CATALOG, url)?.id).toBe(region.id);
    }
  });

  // 没有内网端点，所以 "internal" 必须回落到公网端点而不是产出空串/undefined
  // （`RegionPicker` 也因此不渲染公网/内网切换）。
  it("falls back to the public endpoint when the internal network is requested", () => {
    for (const region of COS_REGIONS) {
      expect(endpointFor(region, "internal")).toBe(`https://${region.endpoint}`);
      expect(isInternalEndpoint(COS_CATALOG, region.endpoint)).toBe(false);
    }
  });

  it("is reachable through regionCatalog under the 'cos' provider id", () => {
    expect(regionCatalog("cos")).toBe(COS_CATALOG);
  });
});

describe("cos provider preset", () => {
  it("is registered with a default endpoint and region that resolve in the catalog", () => {
    const preset = PROVIDERS.find((p) => p.id === "cos");
    expect(preset).toBeDefined();
    expect(findRegion(COS_CATALOG, preset!.region)).toBeDefined();
    expect(regionFromEndpoint(COS_CATALOG, preset!.endpoint)?.id).toBe(preset!.region);
  });

  // COS 在 `tests/cos_e2e.rs` 跑绿之前一直挂着 `hidden: true`（见 commit
  // 8dcc037），所以它出现在向导里这件事本身值得钉一下 —— 解禁是有条件的，
  // 条件是那套 e2e 存在且通过。
  it("is selectable in the wizard", () => {
    expect(SELECTABLE_PROVIDERS.some((p) => p.id === "cos")).toBe(true);
  });

  // 腾讯云官方品牌色。图标本身是中性的 lucide 线性图标（商标合规，见
  // `providers.ts`），所以这个色值是腾讯云在向导里**唯一**的品牌识别，改错了
  // 没有别的东西能兜住。
  it("uses the official brand color", () => {
    const preset = PROVIDERS.find((p) => p.id === "cos");
    expect(preset!.color).toBe("#006EFF");
  });
});
