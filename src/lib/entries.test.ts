import { describe, expect, it } from "vitest";
import {
  isValidObjectName,
  keyToPath,
  listPrefix,
  nameCollides,
  parentPrefix,
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

describe("parentPrefix", () => {
  it("returns the bucket root for a key with no slash", () => {
    expect(parentPrefix("a.txt")).toBe("");
  });

  it("returns the immediate parent prefix for a nested key", () => {
    expect(parentPrefix("sub/a.txt")).toBe("sub/");
  });

  it("returns the full parent path for a deeply nested key", () => {
    expect(parentPrefix("a/b/c.txt")).toBe("a/b/");
  });
});

describe("keyToPath", () => {
  it("splits a folder key into its path segments", () => {
    expect(keyToPath("sub/img/")).toEqual(["sub", "img"]);
  });

  it("splits a single-segment folder key", () => {
    expect(keyToPath("docs/")).toEqual(["docs"]);
  });

  it("treats the bucket root key as an empty path", () => {
    expect(keyToPath("")).toEqual([]);
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

describe("nameCollides", () => {
  const entries = [e("photos", true), e("readme.md", false)];

  it("blocks a name matching an existing file", () => {
    expect(nameCollides(entries, "readme.md")).toBe(true);
  });

  it("blocks a name matching an existing folder", () => {
    expect(nameCollides(entries, "photos")).toBe(true);
  });

  it("blocks a folder name colliding with a same-named file, and vice versa", () => {
    // A new folder named after an existing file (different key: "x" vs
    // "x/") wouldn't overwrite anything server-side, but it would still
    // produce two rows with an identical displayed name -- rejected too.
    expect(nameCollides([e("x", false)], "x")).toBe(true);
    expect(nameCollides([e("x", true)], "x")).toBe(true);
  });

  it("trims before comparing", () => {
    expect(nameCollides(entries, "  photos  ")).toBe(true);
  });

  it("allows a name that isn't taken", () => {
    expect(nameCollides(entries, "new-name")).toBe(false);
  });

  it("ignores the excluded key (renaming a target back to its own name)", () => {
    expect(nameCollides(entries, "photos", "photos/")).toBe(false);
    // Excluding one entry doesn't hide a collision with a *different* entry.
    expect(nameCollides(entries, "readme.md", "photos/")).toBe(true);
  });

  it("treats an empty/blank candidate as non-colliding (isValidObjectName already rejects it)", () => {
    expect(nameCollides(entries, "   ")).toBe(false);
  });

  it("is case-sensitive, matching S3 key semantics", () => {
    expect(nameCollides([e("A.txt", false)], "a.txt")).toBe(false);
    expect(nameCollides([e("a.txt", false)], "a.txt")).toBe(true);
  });
});
