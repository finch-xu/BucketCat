import { describe, expect, it } from "vitest";
import { extFromName, formatDate, formatEta, formatSize, formatSpeed } from "./format";

describe("formatSize", () => {
  it("renders null/invalid as an em dash", () => {
    expect(formatSize(null)).toBe("—");
    expect(formatSize(-1)).toBe("—");
    expect(formatSize(Number.NaN)).toBe("—");
  });

  it("renders sub-KB byte counts verbatim", () => {
    expect(formatSize(0)).toBe("0 B");
    expect(formatSize(312)).toBe("312 B");
    expect(formatSize(1023)).toBe("1023 B");
  });

  it("uses 1024-based units with one trimmed decimal", () => {
    expect(formatSize(1024)).toBe("1 KB");
    expect(formatSize(1536)).toBe("1.5 KB");
    expect(formatSize(48 * 1024)).toBe("48 KB");
    expect(formatSize(1024 * 1024)).toBe("1 MB");
    expect(formatSize(Math.round(3.5 * 1024 * 1024))).toBe("3.5 MB");
    expect(formatSize(318 * 1024 * 1024)).toBe("318 MB");
    expect(formatSize(1024 ** 4)).toBe("1 TB");
  });

  it("drops the decimal for values >= 100 in-unit", () => {
    expect(formatSize(Math.round(204.4 * 1024))).toBe("204 KB");
  });
});

describe("formatDate", () => {
  it("renders null/unparseable as an em dash", () => {
    expect(formatDate(null)).toBe("—");
    expect(formatDate("not-a-date")).toBe("—");
  });

  it("renders RFC 3339 input as local YYYY-MM-DD HH:mm", () => {
    const iso = "2026-07-18T09:22:33Z";
    const rendered = formatDate(iso);
    expect(rendered).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/);
    // Timezone-safe expectation: rebuild from the same Date the impl uses.
    const d = new Date(iso);
    const pad = (n: number) => String(n).padStart(2, "0");
    expect(rendered).toBe(
      `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`,
    );
  });
});

describe("extFromName", () => {
  it("returns the lowercased final extension", () => {
    expect(extFromName("photo.PNG")).toBe("png");
    expect(extFromName("archive.tar.gz")).toBe("gz");
  });

  it("returns empty for no extension, leading-dot names, and trailing dots", () => {
    expect(extFromName("Makefile")).toBe("");
    expect(extFromName(".gitignore")).toBe("");
    expect(extFromName("weird.")).toBe("");
  });
});

describe("formatSpeed", () => {
  it("renders per-second units", () => {
    expect(formatSpeed(0)).toBe("0 B/s");
    expect(formatSpeed(2_200_000)).toBe("2.1 MB/s");
  });

  it("never renders a negative or fractional byte count", () => {
    expect(formatSpeed(-5)).toBe("0 B/s");
    expect(formatSpeed(1.6)).toBe("2 B/s");
  });
});

describe("formatEta", () => {
  it("renders seconds, minutes and hours", () => {
    expect(formatEta(45)).toBe("45s");
    expect(formatEta(125)).toBe("2m 5s");
    expect(formatEta(3_700)).toBe("1h 1m");
  });

  it("renders nothing when the eta is unknown", () => {
    expect(formatEta(null)).toBe("");
    expect(formatEta(-1)).toBe("");
  });
});
