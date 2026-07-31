import { describe, expect, it } from "vitest";
import { PROVIDERS, SELECTABLE_PROVIDERS, providerMeta } from "./providers";
import { R2_REGION } from "./r2";
import { regionCatalog, findRegion, regionFromEndpoint } from "./regions";

describe("PROVIDERS", () => {
  it("has no duplicate ids", () => {
    const ids = PROVIDERS.map((p) => p.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("keeps a generic entry for providerMeta to fall back to", () => {
    expect(PROVIDERS.some((p) => p.id === "generic")).toBe(true);
    expect(providerMeta("something-a-newer-build-saved").id).toBe("generic");
    expect(providerMeta("qiniu").id).toBe("qiniu");
  });

  // 隐藏一个 provider 必须是「向导里选不到」，而不是「从表里删掉」。两者在
  // 向导上看起来一样，差别只在已保存的连接上才暴露：删掉之后 `providerMeta`
  // 会静默回落到 `generic`，用户什么都没做，他那条腾讯云连接却变成了方块图标
  // 加「通用 S3 兼容」。这条断言就是钉住这个区别的。
  it("hides cos from the wizard while keeping it resolvable for saved connections", () => {
    expect(SELECTABLE_PROVIDERS.some((p) => p.id === "cos")).toBe(false);
    expect(providerMeta("cos").id).toBe("cos");
    expect(providerMeta("cos").name).toBe("Tencent COS");
  });

  // 反过来：可选列表不能把没标 `hidden` 的 provider 漏掉，否则某个厂商会
  // 在无人察觉的情况下从向导里消失。
  it("offers every provider that is not explicitly hidden", () => {
    expect(SELECTABLE_PROVIDERS.map((p) => p.id)).toEqual(
      PROVIDERS.filter((p) => !p.hidden).map((p) => p.id),
    );
    expect(SELECTABLE_PROVIDERS.length).toBe(PROVIDERS.length - 1);
  });

  // 真正要防的失败模式是「图标名写错 / 导入被删」导致 `icon` 成了
  // `undefined`，渲染时才炸。这里不能断言 `typeof === "function"`：lucide 的
  // 图标是 `forwardRef` 出来的**对象**（`{$$typeof, render}`），不是函数。
  it("gives every provider a defined icon and a label", () => {
    for (const p of PROVIDERS) {
      expect(p.icon, `${p.id} icon`).toBeTruthy();
      expect(["function", "object"], `${p.id} icon`).toContain(typeof p.icon);
      // `generic` 走 nameKey（需要翻译），其它厂商用固定的商号。
      expect(p.nameKey ?? p.name).toBeTruthy();
      expect(p.descKey).toBeTruthy();
    }
  });

  it("gives every provider a valid brand hex color", () => {
    for (const p of PROVIDERS) {
      expect(p.color).toMatch(/^#[0-9a-fA-F]{6}$/);
    }
  });

  // 图标必须两两不同：出于商标合规，这里全是中性的 lucide 线性图标（见
  // `providers.ts` 的图标规则），厂商之间**只**靠图形与品牌色区分。两家共用一个
  // 图标不会报错，只会让向导里两行看起来像同一个服务。
  it("gives every provider a distinct icon", () => {
    const icons = PROVIDERS.map((p) => p.icon);
    expect(new Set(icons).size).toBe(icons.length);
  });

  // 有地区目录的 provider，其预填的 endpoint/region 必须能在目录里解析出来
  // —— 否则连接向导一进第二步就落进 `unknownEndpoint` 分支，把预填当成用户
  // 手填的自定义域名。
  it("prefills endpoint/region that resolve in the catalog, for providers that ship one", () => {
    for (const p of PROVIDERS) {
      const catalog = regionCatalog(p.id);
      if (!catalog) continue;
      expect(findRegion(catalog, p.region), `${p.id} region`).toBeDefined();
      expect(regionFromEndpoint(catalog, p.endpoint)?.id, `${p.id} endpoint`).toBe(p.region);
    }
  });

  // R2 的端点由账户 ID 派生（见 `R2Fields` / `src/lib/r2.ts`），所以这里
  // **必须**是空串。曾经预填的是 `https://<account>.r2.cloudflarestorage.com`
  // 这种带占位符的假地址 —— 它会被原样存进连接，用户不手改就连不上，而且
  // 因为看起来像个正常端点，谁也不会怀疑它。
  it("prefills no endpoint for r2, whose endpoint is derived from the account id", () => {
    const r2 = PROVIDERS.find((p) => p.id === "r2");
    expect(r2).toBeDefined();
    expect(r2!.endpoint).toBe("");
    expect(r2!.region).toBe(R2_REGION);
  });
});
