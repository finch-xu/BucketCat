import { describe, expect, it } from "vitest";
import {
  basename,
  dedupeBatchNames,
  isValidObjectName,
  keyToPath,
  listPrefix,
  listingGuard,
  nameCollides,
  PARENT_ENTRY,
  parentPrefix,
  pathToPrefix,
  renameKey,
  sortEntries,
  uploadBaseName,
  uploadKey,
  withParentRow,
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

describe("PARENT_ENTRY", () => {
  it("is a folder row, so orderedFileKeys' !is_prefix filter excludes it", () => {
    expect(PARENT_ENTRY.is_prefix).toBe(true);
  });

  it("carries the NUL sentinel key, so it can never collide with a real S3 key", () => {
    expect(PARENT_ENTRY.key.startsWith("\u0000")).toBe(true);
  });
});

describe("withParentRow", () => {
  it("unshifts PARENT_ENTRY by reference and does not mutate the input", () => {
    const input = [e("a.txt", false), e("photos", true)];
    const result = withParentRow(input, true);
    expect(result[0]).toBe(PARENT_ENTRY);
    expect(result.slice(1)).toEqual(input);
    expect(input).toEqual([e("a.txt", false), e("photos", true)]);
  });

  it("returns the entries unchanged when show is false", () => {
    const input = [e("a.txt", false), e("photos", true)];
    expect(withParentRow(input, false)).toEqual(input);
  });

  it("puts .. first even ahead of an entry that would sort before it (explicit unshift, not sort)", () => {
    // "_x" sorts BEFORE ".." under localeCompare -- relying on sortEntries
    // to place the sentinel would bury it here instead of pinning it to the
    // top, which is exactly the trap withParentRow's unshift avoids.
    const input = [e("_x", true)];
    const result = withParentRow(input, true);
    expect(result[0]).toBe(PARENT_ENTRY);
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

  it("leaves a file key unaffected by folder handling", () => {
    expect(renameKey("docs/a.txt", "b.txt")).toBe("docs/b.txt");
  });

  it("preserves the trailing slash on a folder key", () => {
    expect(renameKey("docs/sub/", "new")).toBe("docs/new/");
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

  it("returns the folder's own parent for a nested folder key", () => {
    expect(parentPrefix("docs/sub/")).toBe("docs/");
  });

  it("returns the bucket root for a top-level folder key", () => {
    expect(parentPrefix("docs/")).toBe("");
  });

  it("returns the full parent path for a deeply nested folder key", () => {
    expect(parentPrefix("a/b/c/")).toBe("a/b/");
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

describe("uploadKey", () => {
  it("joins the browsed prefix with the file name", () => {
    expect(uploadKey("docs/", "a.txt")).toBe("docs/a.txt");
    expect(uploadKey("docs/sub/", "a.txt")).toBe("docs/sub/a.txt");
  });

  it("uploads to the bucket root without a leading slash", () => {
    expect(uploadKey("", "a.txt")).toBe("a.txt");
  });

  it("adds a missing separator", () => {
    expect(uploadKey("docs", "a.txt")).toBe("docs/a.txt");
  });

  it("keeps only the basename so a name cannot escape the prefix", () => {
    expect(uploadKey("docs/", "../secrets/a.txt")).toBe("docs/a.txt");
    expect(uploadKey("docs/", "sub/a.txt")).toBe("docs/a.txt");
    expect(uploadKey("docs/", "sub\\a.txt")).toBe("docs/a.txt");
  });

  // Pinned deliberately, not incidentally: the backend's `upload_key` is
  // written to match, and the collision guard checks the *trimmed* name via
  // `nameCollides`. See `uploadBaseName`'s doc comment for the full
  // rationale -- dropping this trim silently destroys a remote object whose
  // name really does carry trailing whitespace.
  it("trims whitespace around the basename", () => {
    expect(uploadKey("docs/", " a.txt ")).toBe("docs/a.txt");
    expect(uploadKey("", "\ta.txt\n")).toBe("a.txt");
    expect(uploadKey("docs/", "sub/ a.txt ")).toBe("docs/a.txt");
  });

  it("keeps whitespace that is INSIDE the name", () => {
    expect(uploadKey("docs/", "my file.txt")).toBe("docs/my file.txt");
  });

  // Never `prefix` itself: that is the browsed folder's own marker key, and
  // handing it back as an upload target is exactly the silent overwrite this
  // module exists to prevent.
  it("has no target for an empty basename", () => {
    expect(uploadKey("docs/", "")).toBe("");
    expect(uploadKey("docs/", "   ")).toBe("");
    expect(uploadKey("docs/", "sub/")).toBe("");
    expect(uploadKey("", "")).toBe("");
  });
});

describe("uploadBaseName", () => {
  it("keeps only the final path segment, on either separator", () => {
    expect(uploadBaseName("a.txt")).toBe("a.txt");
    expect(uploadBaseName("/Users/me/docs/a.txt")).toBe("a.txt");
    expect(uploadBaseName("C:\\Users\\me\\a.txt")).toBe("a.txt");
    expect(uploadBaseName("../secrets/a.txt")).toBe("a.txt");
  });

  it("trims leading/trailing whitespace, matching nameCollides' candidate", () => {
    expect(uploadBaseName(" a.txt ")).toBe("a.txt");
    expect(uploadBaseName("/tmp/ a.txt\t")).toBe("a.txt");
    expect(nameCollides([e("a.txt", false)], uploadBaseName("/tmp/ a.txt "))).toBe(true);
  });

  it("returns empty for a name that is only whitespace or a bare directory path", () => {
    expect(uploadBaseName("")).toBe("");
    expect(uploadBaseName("   ")).toBe("");
    expect(uploadBaseName("/tmp/dir/")).toBe("");
  });
});

describe("listingGuard", () => {
  const page = (...names: string[]) => ({ entries: names.map((n) => e(n, false)) });

  it("is ready and flattens every loaded page once real data has resolved", () => {
    const guard = listingGuard({ pages: [page("a.txt"), page("b.txt")] }, false);
    expect(guard.ready).toBe(true);
    expect(guard.entries.map((x) => x.name)).toEqual(["a.txt", "b.txt"]);
  });

  it("fails closed while the listing has never resolved", () => {
    const guard = listingGuard(undefined, false);
    expect(guard.ready).toBe(false);
    expect(guard.entries).toEqual([]);
  });

  // The C1 regression: `keepPreviousData` leaves `data` defined but holding
  // the PREVIOUS location's listing during every folder/bucket navigation. A
  // guard that only checked `data !== undefined` would clear an upload into
  // `photos/` against `docs/`'s entries.
  it("fails closed while data is the previous location's placeholder", () => {
    const guard = listingGuard({ pages: [page("hero.png")] }, true);
    expect(guard.ready).toBe(false);
    expect(guard.entries).toEqual([]);
  });

  it("cannot report a phantom collision from a stale listing", () => {
    const stale = listingGuard({ pages: [page("hero.png")] }, true);
    expect(nameCollides(stale.entries, "hero.png")).toBe(false);
  });

  it("still reports a real collision once the listing is trustworthy", () => {
    const fresh = listingGuard({ pages: [page("hero.png")] }, false);
    expect(nameCollides(fresh.entries, "hero.png")).toBe(true);
  });
});

describe("basename", () => {
  it("returns the last segment of a file key", () => {
    expect(basename("a/b/c.txt")).toBe("c.txt");
    expect(basename("a.txt")).toBe("a.txt");
  });

  it("yields the folder's own name for a folder key", () => {
    expect(basename("photos/")).toBe("photos");
    expect(basename("a/b/sub/")).toBe("sub");
  });

  it("is empty for the bucket root", () => {
    expect(basename("")).toBe("");
    expect(basename("/")).toBe("");
  });
});

describe("dedupeBatchNames", () => {
  it("passes distinct basenames through untouched, keeping key and order", () => {
    const out = dedupeBatchNames([
      { key: "a/one.txt", name: "one.txt" },
      { key: "b/two.txt", name: "two.txt" },
    ]);
    expect(out).toEqual([
      { key: "a/one.txt", name: "one.txt" },
      { key: "b/two.txt", name: "two.txt" },
    ]);
  });

  it("de-duplicates a shared basename by suffixing before the extension", () => {
    // The M9.1 case: two objects from different prefixes share a basename and
    // would otherwise collide on the same local path (and the same .bcpart).
    const out = dedupeBatchNames([
      { key: "a/report.pdf", name: "report.pdf" },
      { key: "b/report.pdf", name: "report.pdf" },
      { key: "c/report.pdf", name: "report.pdf" },
    ]);
    expect(out.map((i) => i.name)).toEqual(["report.pdf", "report (2).pdf", "report (3).pdf"]);
  });

  it("de-duplicates case-insensitively (macOS/Windows filesystems)", () => {
    const out = dedupeBatchNames([
      { key: "a/Report.PDF", name: "Report.PDF" },
      { key: "b/report.pdf", name: "report.pdf" },
    ]);
    expect(out.map((i) => i.name)).toEqual(["Report.PDF", "report (2).pdf"]);
  });

  it("suffixes an extensionless name at the end", () => {
    const out = dedupeBatchNames([
      { key: "a/README", name: "README" },
      { key: "b/README", name: "README" },
    ]);
    expect(out.map((i) => i.name)).toEqual(["README", "README (2)"]);
  });

  it("strips a path fragment out of a remote-derived name so it can't escape the dir", () => {
    // A crafted or search-derived name carrying separators or `..` must not
    // reach `join(dir, name)` as a traversal.
    expect(dedupeBatchNames([{ key: "k", name: "../../etc/passwd" }])[0].name).toBe("passwd");
    expect(dedupeBatchNames([{ key: "k", name: "sub\\evil.txt" }])[0].name).toBe("evil.txt");
  });

  it("falls back to the key's basename, then to 'download', when the name is unusable", () => {
    expect(dedupeBatchNames([{ key: "a/real.txt", name: ".." }])[0].name).toBe("real.txt");
    expect(dedupeBatchNames([{ key: "..", name: "." }])[0].name).toBe("download");
  });
});
