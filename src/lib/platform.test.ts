import { describe, expect, it } from "vitest";
import { isMacUserAgent } from "./platform";

describe("isMacUserAgent", () => {
  it("detects the macOS WKWebView Tauri runs in", () => {
    expect(
      isMacUserAgent(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15",
      ),
    ).toBe(true);
  });

  it("rejects the Windows WebView2 user agent", () => {
    expect(
      isMacUserAgent(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0",
      ),
    ).toBe(false);
  });

  it("rejects the Linux WebKitGTK user agent", () => {
    expect(
      isMacUserAgent(
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15",
      ),
    ).toBe(false);
  });
});
