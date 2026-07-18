import { describe, expect, it } from "vitest";
import { resolveLocale } from "./resolve-locale";

describe("resolveLocale", () => {
  it("maps all Chinese variants to zh-CN", () => {
    expect(resolveLocale("zh")).toBe("zh-CN");
    expect(resolveLocale("zh-CN")).toBe("zh-CN");
    expect(resolveLocale("zh-TW")).toBe("zh-CN");
    expect(resolveLocale("zh-HK")).toBe("zh-CN");
    expect(resolveLocale("zh-Hant-TW")).toBe("zh-CN");
    expect(resolveLocale("ZH-CN")).toBe("zh-CN");
  });

  it("maps English variants to en", () => {
    expect(resolveLocale("en")).toBe("en");
    expect(resolveLocale("en-US")).toBe("en");
    expect(resolveLocale("en-GB")).toBe("en");
  });

  it("falls back to en for unmatched locales", () => {
    expect(resolveLocale("ja-JP")).toBe("en");
    expect(resolveLocale("fr")).toBe("en");
    expect(resolveLocale("de-DE")).toBe("en");
  });

  it("falls back to en for empty input", () => {
    expect(resolveLocale("")).toBe("en");
    expect(resolveLocale(undefined)).toBe("en");
    expect(resolveLocale(null)).toBe("en");
  });
});
