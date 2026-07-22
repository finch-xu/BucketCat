import { describe, expect, it } from "vitest";
import {
  isValidObjectName,
  listPrefix,
  pathToPrefix,
  renameKey,
  sortEntries,
} from "./entries";
import type { ObjectEntry } from "./api";

const e = (name: string, is_prefix: boolean): ObjectEntry => ({
  key: is_prefix ? `${name}/` : name,
  name,
  size: is_prefix ? null : 1,
  last_modified: null,
  storage_class: null,
  is_prefix,
});

describe("sortEntries", () => {
  it("puts folders before files, each group alphabetical", () => {
    const sorted = sortEntries([
      e("b.txt", false),
      e("zeta", true),
      e("a.txt", false),
      e("alpha", true),
    ]);
    expect(sorted.map((x) => x.name)).toEqual(["alpha", "zeta", "a.txt", "b.txt"]);
  });

  it("does not mutate the input", () => {
    const input = [e("b", false), e("a", false)];
    sortEntries(input);
    expect(input.map((x) => x.name)).toEqual(["b", "a"]);
  });
});

describe("pathToPrefix / listPrefix", () => {
  it("joins path segments with trailing slashes", () => {
    expect(pathToPrefix([])).toBe("");
    expect(pathToPrefix(["docs"])).toBe("docs/");
    expect(pathToPrefix(["docs", "2026"])).toBe("docs/2026/");
  });

  it("appends the trimmed search text as a key prefix", () => {
    expect(listPrefix([], "")).toBe("");
    expect(listPrefix(["docs"], "")).toBe("docs/");
    expect(listPrefix(["docs"], " gu ")).toBe("docs/gu");
  });
});

describe("renameKey", () => {
  it("replaces the final segment", () => {
    expect(renameKey("docs/old.md", "new.md")).toBe("docs/new.md");
    expect(renameKey("root.txt", "renamed.txt")).toBe("renamed.txt");
  });
});

describe("isValidObjectName", () => {
  it("accepts plain names", () => {
    expect(isValidObjectName("photo.png")).toBe(true);
    expect(isValidObjectName("猫.jpg")).toBe(true);
  });

  it("rejects empty, blank, and slash-containing names", () => {
    expect(isValidObjectName("")).toBe(false);
    expect(isValidObjectName("   ")).toBe(false);
    expect(isValidObjectName("a/b")).toBe(false);
  });
});
