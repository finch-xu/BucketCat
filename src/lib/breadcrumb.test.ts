import { describe, expect, it } from "vitest";
import { ANCESTOR_BUDGET, displayWidth, middleTruncate, planCrumbs } from "./breadcrumb";

describe("displayWidth", () => {
  it("counts an empty string as zero", () => {
    expect(displayWidth("")).toBe(0);
  });

  it("counts latin characters as one each", () => {
    expect(displayWidth("abc")).toBe(3);
    expect(displayWidth("backup-production")).toBe(17);
  });

  it("counts CJK characters as two each", () => {
    expect(displayWidth("中文")).toBe(4);
    expect(displayWidth("備份")).toBe(4);
    expect(displayWidth("ひらがな")).toBe(8);
    expect(displayWidth("한글")).toBe(4);
  });

  it("counts fullwidth punctuation as two", () => {
    expect(displayWidth("，")).toBe(2);
    expect(displayWidth("Ａ")).toBe(2);
  });

  it("counts astral-plane emoji as two, not as two UTF-16 code units", () => {
    // "😀".length is 2 (a surrogate pair) but it renders as one wide glyph.
    expect(displayWidth("😀")).toBe(2);
    expect(displayWidth("😀😀")).toBe(4);
  });

  it("handles mixed scripts", () => {
    expect(displayWidth("a中")).toBe(3);
    // The comparison that motivates width-over-length: 11 CJK chars are
    // visually wider than 17 latin ones.
    expect(displayWidth("备份-生产库-全量归档")).toBeGreaterThan(
      displayWidth("backup-production"),
    );
  });
});

describe("middleTruncate", () => {
  it("returns the input untouched when it already fits", () => {
    expect(middleTruncate("abc", 10)).toBe("abc");
    expect(middleTruncate("", 5)).toBe("");
  });

  it("returns the input untouched when it exactly hits the limit", () => {
    expect(middleTruncate("abcdefghij", 10)).toBe("abcdefghij");
  });

  it("keeps both ends when it does not fit", () => {
    const out = middleTruncate("abcdefghijk", 10);
    expect(out).toBe("abcde…hijk");
    expect(displayWidth(out)).toBeLessThanOrEqual(10);
  });

  it("keeps sibling directories distinguishable by their suffix", () => {
    const full = middleTruncate("2026-07-29-backup-of-production-db-full", 24);
    const incr = middleTruncate("2026-07-29-backup-of-production-db-incr", 24);
    expect(full).not.toBe(incr);
    expect(full.startsWith("2026-07-29")).toBe(true);
    expect(full.endsWith("full")).toBe(true);
    expect(incr.endsWith("incr")).toBe(true);
  });

  it("never exceeds the budget, for any prefix of a long mixed string", () => {
    const s = "备份-2026-生产库😀-full-归档";
    for (let max = 1; max <= displayWidth(s) + 2; max += 1) {
      expect(displayWidth(middleTruncate(s, max))).toBeLessThanOrEqual(max);
    }
  });

  it("never returns something wider than the input", () => {
    const s = "abcdefghijk";
    for (let max = 1; max <= 20; max += 1) {
      expect(displayWidth(middleTruncate(s, max))).toBeLessThanOrEqual(displayWidth(s));
    }
  });

  it("does not split an astral-plane emoji into a lone surrogate", () => {
    const out = middleTruncate("😀😀😀😀😀", 6);
    expect(out).toBe("😀…😀");
    // A half-surrogate would render as U+FFFD; assert none survived.
    expect(out).not.toContain("�");
    for (const ch of out) expect(ch.codePointAt(0)).not.toBeUndefined();
    expect(Array.from(out).every((c) => !/[\uD800-\uDFFF]/.test(c) || c.length === 2)).toBe(true);
  });

  it("degrades to just the ellipsis at width 1", () => {
    expect(middleTruncate("abcdef", 1)).toBe("…");
  });

  it("keeps one leading character at width 2", () => {
    expect(middleTruncate("abcdef", 2)).toBe("a…");
  });

  it("returns an empty string when there is no room at all", () => {
    expect(middleTruncate("abcdef", 0)).toBe("");
    expect(middleTruncate("abcdef", -3)).toBe("");
  });

  it("fills the budget rather than leaving wide-character slack", () => {
    // Naive head/tail halving would yield "一…六" (width 5) here; the greedy
    // fill should use the whole budget.
    expect(middleTruncate("一二三四五六", 7)).toBe("一二…六");
    expect(displayWidth("一二…六")).toBe(7);
  });
});


describe("planCrumbs", () => {
  /** Path bar inside the default 1100px window: 1100 - 248 sidebar - 32 padding. */
  const WIDE = 820;
  const LONG = "2026-07-29-backup-of-production-db-full";
  const filler = ["logs", "2026", "app", "data", "runs", "temp", "arch", "prod"];

  const visible = (items: ReturnType<typeof planCrumbs>) =>
    items.filter((i) => i.kind === "crumb");
  const levels = (items: ReturnType<typeof planCrumbs>) =>
    items.flatMap((i) => (i.kind === "ellipsis" ? i.hidden : [i])).map((c) => c.full);

  it("returns nothing for an empty path", () => {
    expect(planCrumbs([], WIDE)).toEqual([]);
  });

  it("renders a lone bucket as a single crumb at index 0, never an ellipsis", () => {
    expect(planCrumbs(["my-bucket"], WIDE)).toEqual([
      { kind: "crumb", label: "my-bucket", full: "my-bucket", index: 0 },
    ]);
    expect(planCrumbs(["my-bucket"], 0)).toHaveLength(1);
  });

  it("expands fully when everything fits", () => {
    const items = planCrumbs(["my-bucket", "2026", "photos", "day-3"], WIDE);
    expect(items.every((i) => i.kind === "crumb")).toBe(true);
    expect(levels(items)).toEqual(["my-bucket", "2026", "photos", "day-3"]);
  });

  it("collapses once the path outgrows the bar", () => {
    const items = planCrumbs(["my-bucket", ...filler, ...filler, "day-3"], WIDE);
    expect(items.filter((i) => i.kind === "ellipsis")).toHaveLength(1);
  });

  it("budgets by each segment's real width, not by a per-segment average", () => {
    // The regression this guards: dividing the width by a fixed ~82px average
    // let three long names blow the budget while the planner still believed
    // ten crumbs fit, so the row overflowed and flex squeezed every short
    // segment down to "a…". Same depth, longer names => fewer crumbs shown.
    const short = planCrumbs(["my-bucket", ...filler, ...filler, "day-3"], WIDE);
    const long = planCrumbs(
      ["my-bucket", ...filler, LONG, LONG, LONG, ...filler.slice(0, 5), "day-3"],
      WIDE,
    );
    expect(visible(long).length).toBeLessThan(visible(short).length);
    // A width/82 average would have claimed 10 crumbs fit regardless of length.
    expect(visible(long).length).toBeLessThan(10);
  });

  it("always keeps the bucket and the current folder", () => {
    for (const width of [0, 40, 120, 300, WIDE]) {
      const items = visible(planCrumbs(["my-bucket", ...filler, ...filler, "day-3"], width));
      expect(items[0].index).toBe(0);
      expect(items[0].full).toBe("my-bucket");
      expect(items[items.length - 1].full).toBe("day-3");
    }
  });

  it("keeps the current folder even when it alone overflows the bar", () => {
    const items = visible(planCrumbs(["my-bucket", "a", "b", LONG], 10));
    expect(items[items.length - 1].full).toBe(LONG);
  });

  it("never loses a level, at any width and depth", () => {
    const crumbs = ["my-bucket", ...filler, LONG, ...filler, "day-3"];
    for (let w = 0; w <= 1200; w += 37) {
      expect(levels(planCrumbs(crumbs, w))).toEqual(crumbs);
    }
  });

  it("emits at most one ellipsis, at any width", () => {
    const crumbs = ["my-bucket", ...filler, ...filler, "day-3"];
    for (let w = 0; w <= 1200; w += 37) {
      expect(planCrumbs(crumbs, w).filter((i) => i.kind === "ellipsis").length).toBeLessThanOrEqual(1);
    }
  });

  it("maps every level to its original index, collapsed ones included", () => {
    // `index` drives gotoCrumb(index - 1), so an off-by-one navigates to the
    // wrong folder. The bucket must stay 0 so it maps to gotoCrumb(-1).
    const crumbs = ["my-bucket", ...filler, ...filler, "day-3"];
    const flat = planCrumbs(crumbs, 300).flatMap((i) => (i.kind === "ellipsis" ? i.hidden : [i]));
    expect(flat.map((c) => c.index)).toEqual(crumbs.map((_, i) => i));
  });

  it("middle-truncates ancestor labels to their budget, keeping `full` intact", () => {
    const items = visible(planCrumbs(["my-bucket", LONG, "day-3"], WIDE));
    const ancestor = items.find((c) => c.full === LONG);
    expect(ancestor).toBeDefined();
    expect(ancestor!.label).not.toBe(LONG);
    expect(displayWidth(ancestor!.label)).toBeLessThanOrEqual(ANCESTOR_BUDGET);
    expect(ancestor!.label).toContain("…");
  });
});
