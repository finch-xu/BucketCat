import { describe, expect, it } from "vitest";
import { PROVIDERS, providerMeta } from "./providers";
import { regionCatalog, findRegion, regionFromEndpoint } from "./regions";

/** 只有这些 provider 没有官方品牌 logo，用 lucide 线性图标 + 品牌色顶上
 * （见 `providers.ts` 的图标来源规则）。`minio` / `b2` 用的是 simple-icons 的
 * 单色 logo，同样需要着色，所以也在这里。改这张表时请一并更新这个集合。 */
const MONOCHROME_ICON_PROVIDERS = new Set(["minio", "rainyun", "b2", "generic"]);

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

  // 真正要防的失败模式是「图标名写错 / 导入被删」导致 `icon` 成了
  // `undefined`，渲染时才炸。这里不能断言 `typeof === "function"`：内联的品牌
  // SVG 是普通函数组件，而 lucide 的图标是 `forwardRef` 出来的**对象**
  // （`{$$typeof, render}`），两种都是合法的 React 组件类型。
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

  // `colorLogo` 驱动 `ProviderGlyph` 要不要着色，两边不能各写一套：原色 logo
  // 着色无效（fill 优先），单色 logo 不着色就丢品牌色。
  it("marks exactly the monochrome-icon providers as colorLogo: false", () => {
    for (const p of PROVIDERS) {
      expect(p.colorLogo).toBe(!MONOCHROME_ICON_PROVIDERS.has(p.id));
    }
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
});
