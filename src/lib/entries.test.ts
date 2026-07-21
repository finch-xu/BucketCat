import { describe, expect, it } from "vitest";
import { filterByPrefix, sortEntries } from "./entries";
import type { ObjectEntry } from "./mock-data";

const e = (name: string, kind: "folder" | "file"): ObjectEntry => ({
  name,
  kind,
  ext: "",
  size: "—",
  modified: "—",
});

describe("sortEntries", () => {
  it("puts folders before files, each group alphabetical", () => {
    const sorted = sortEntries([
      e("b.txt", "file"),
      e("zeta", "folder"),
      e("a.txt", "file"),
      e("alpha", "folder"),
    ]);
    expect(sorted.map((x) => x.name)).toEqual(["alpha", "zeta", "a.txt", "b.txt"]);
  });

  it("does not mutate the input", () => {
    const input = [e("b", "file"), e("a", "file")];
    sortEntries(input);
    expect(input.map((x) => x.name)).toEqual(["b", "a"]);
  });
});

describe("filterByPrefix", () => {
  const entries = [e("hero-banner.png", "file"), e("Hero-sm.png", "file"), e("og-cover.jpg", "file")];

  it("matches name prefixes case-insensitively", () => {
    expect(filterByPrefix(entries, "hero").map((x) => x.name)).toEqual([
      "hero-banner.png",
      "Hero-sm.png",
    ]);
  });

  it("returns everything for a blank query", () => {
    expect(filterByPrefix(entries, "  ")).toHaveLength(3);
  });
});
