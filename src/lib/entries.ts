import type { ObjectEntry } from "./api";

/** Folders (prefixes) first, then files; each group locale-alphabetical.
 * Applied client-side to the flattened window of all loaded pages — the
 * server returns prefixes-then-objects *within* a page only, so a folder
 * that first appears on page 3 has to sort up into the already-rendered
 * rows. That row shuffle on page-in is inherent to combining
 * continuation-token paging with a global folders-first order, and is
 * preferred over the alternative (folders drifting into the middle of the
 * list wherever their page happened to land). */
export function sortEntries(entries: ObjectEntry[]): ObjectEntry[] {
  return entries.slice().sort((a, b) => {
    if (a.is_prefix !== b.is_prefix) return a.is_prefix ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
}

/** `["docs","2026"]` → `"docs/2026/"`; `[]` → `""` (bucket root). */
export function pathToPrefix(path: string[]): string {
  return path.map((segment) => `${segment}/`).join("");
}

/** The server-side ListObjectsV2 prefix for the current location: current
 * path + the search box's text (design §6 — search is *prefix* search on
 * the current path, not full-text). */
export function listPrefix(path: string[], search: string): string {
  return pathToPrefix(path) + search.trim();
}

/** Rename target key: `key` with its final path segment replaced. Handles
 * folder keys (trailing "/") by stripping that one trailing slash before
 * locating the final segment, then re-appending it to the result -- so a
 * renamed folder key stays a folder key instead of turning into a bare
 * (non-prefix) key: `renameKey("docs/sub/", "new")` -> `"docs/new/"`, not
 * `"docs/new"`. File keys are unaffected: `renameKey("docs/a.txt", "b.txt")`
 * -> `"docs/b.txt"`. */
export function renameKey(key: string, newName: string): string {
  const isFolder = key.endsWith("/");
  const stripped = isFolder ? key.slice(0, -1) : key;
  const idx = stripped.lastIndexOf("/");
  const base = idx === -1 ? newName : stripped.slice(0, idx + 1) + newName;
  return isFolder ? `${base}/` : base;
}

/** The prefix `key` itself lives under: `"a.txt"` -> `""` (bucket root),
 * `"sub/a.txt"` -> `"sub/"`, `"a/b/c.txt"` -> `"a/b/"`.
 *
 * Handles folder keys (trailing "/") by stripping exactly one trailing
 * slash before locating the parent, so the result is the folder's OWN
 * parent -- the prefix its siblings live under -- not the folder's own key:
 * `"docs/sub/"` -> `"docs/"`, `"docs/"` -> `""` (bucket root), `"a/b/c/"` ->
 * `"a/b/"`. Without this, a folder key's trailing "/" would itself be the
 * rightmost "/", so `lastIndexOf` would return the key unchanged instead of
 * its parent.
 *
 * Used to derive the rename collision guard's listing prefix from the
 * *target's own key*, not from the store's browsed `path`. Those two can
 * diverge: the browsed listing prefix is `pathToPrefix(path) + search`
 * (design §6 -- search is a prefix search scoped to the current path), so
 * typing a search term containing "/" lists rows that live under a deeper
 * prefix than `pathToPrefix(path)` alone. A guard built from `path` would
 * then check the wrong (shallower) listing and miss a real collision. This
 * folder-key handling matters for the same reason once folder rename lands
 * (M4/M6): the guard must list the folder's SIBLINGS, not its own contents. */
export function parentPrefix(key: string): string {
  const stripped = key.endsWith("/") ? key.slice(0, -1) : key;
  const idx = stripped.lastIndexOf("/");
  return idx === -1 ? "" : stripped.slice(0, idx + 1);
}

/** Derives the browsed `path` segments from an entry's own key rather than
 * its display name: `"sub/img/"` -> `["sub", "img"]`. Navigating by key
 * (instead of appending the display name to the current `path`) keeps
 * double-click-to-open correct when the listing was reached via a search
 * term that lists rows under a deeper prefix than `pathToPrefix(path)`
 * (see `parentPrefix`). */
export function keyToPath(key: string): string[] {
  const trimmed = key.endsWith("/") ? key.slice(0, -1) : key;
  return trimmed.length === 0 ? [] : trimmed.split("/");
}

/** The final path segment of a key: `"a/b/c.txt"` -> `"c.txt"`, `"photos/"` ->
 * `"photos"` (one trailing slash is stripped first, so a folder key yields the
 * folder's own name), `"a.txt"` -> `"a.txt"`. Splits on `"/"`, the object-key
 * separator. */
export function basename(key: string): string {
  const stripped = key.endsWith("/") ? key.slice(0, -1) : key;
  const idx = stripped.lastIndexOf("/");
  return idx === -1 ? stripped : stripped.slice(idx + 1);
}

/** A bare, filesystem-safe filename derived from `name`: its last segment after
 * ANY separator (`/` or `\` -- a remote key can carry either), or `null` when
 * nothing usable survives (empty, `"."`, `".."`). Used to keep a remote-derived
 * name from smuggling a path fragment into a local `join`. */
function safeLocalName(name: string): string | null {
  const bare = name.split(/[\\/]/).pop() ?? "";
  return bare === "" || bare === "." || bare === ".." ? null : bare;
}

/** Inserts ` (n)` before the extension: `withCounter("a.txt", 2)` ->
 * `"a (2).txt"`, `withCounter("README", 2)` -> `"README (2)"`. A leading dot
 * (dotfile) is not treated as an extension. */
function withCounter(base: string, n: number): string {
  const dot = base.lastIndexOf(".");
  const suffix = ` (${n})`;
  return dot > 0 ? `${base.slice(0, dot)}${suffix}${base.slice(dot)}` : `${base}${suffix}`;
}

/** Assigns each item a safe, collision-free local filename for a batch download
 * into ONE directory. Two problems this closes, both reachable when a search
 * listing spanning prefixes is multi-selected:
 *   1. Safety -- a remote-derived name is reduced to a bare basename with no
 *      path separators and never `"."`/`".."`, so it can't escape the chosen
 *      directory via `join`.
 *   2. Uniqueness -- objects that share a basename (`a/report.pdf` and
 *      `b/report.pdf`) would otherwise land on the same local path and the same
 *      `.bcpart`, corrupting each other; duplicates get ` (2)`, ` (3)` ...
 *      inserted before the extension instead. Dedup is case-insensitive to
 *      match case-insensitive filesystems (macOS/Windows).
 * Order is preserved; `key` (the remote identity) is passed through untouched. */
export function dedupeBatchNames(
  items: { key: string; name: string }[],
): { key: string; name: string }[] {
  const used = new Set<string>();
  return items.map(({ key, name }) => {
    const base = safeLocalName(name) ?? safeLocalName(key) ?? "download";
    let candidate = base;
    let n = 2;
    while (used.has(candidate.toLowerCase())) {
      candidate = withCounter(base, n);
      n += 1;
    }
    used.add(candidate.toLowerCase());
    return { key, name: candidate };
  });
}

/** A single object/folder display name: non-empty after trimming and free
 * of "/" (path separators are navigation, not names). */
export function isValidObjectName(name: string): boolean {
  const trimmed = name.trim();
  return trimmed.length > 0 && !trimmed.includes("/");
}

/** Best-effort client-side collision guard for new-folder / rename: does
 * `name` already match an existing entry's display name in the given
 * listing? Checked against *both* files and folders -- a file "photos" and
 * a folder "photos" have different keys ("photos" vs "photos/") so this
 * isn't only about the copy-then-delete overwrite risk (that's the actual
 * data-loss case, when the collision is with another file) but also the
 * confusing case of two same-named rows of different kinds. `excludeKey`
 * lets a rename ignore the target's own current entry.
 *
 * This is necessarily a check against whatever page(s) of the current
 * listing are already loaded client-side, not an atomic server-side
 * guarantee: a key created concurrently, or one that lives on a page not
 * yet fetched, won't be caught here. The backend still does copy-then-
 * delete (rename) / a plain PUT (new folder) with no server-side
 * existence check. */
export function nameCollides(entries: ObjectEntry[], name: string, excludeKey?: string): boolean {
  const trimmed = name.trim();
  if (trimmed.length === 0) return false;
  return entries.some((entry) => entry.key !== excludeKey && entry.name === trimmed);
}

/** The display/key name an upload of `fileName` will land under: its last
 * path segment (split on BOTH separators, so a Windows-style path is handled
 * on any host), with leading/trailing whitespace trimmed.
 *
 * The single definition of that rule — `uploadKey` builds on it and
 * `useStartUploads` uses it for the collision check, so the name that is
 * *checked* and the name that is *written* can never drift apart.
 *
 * WHY TRIM (deliberate, load-bearing — the backend's `upload_key` in
 * `src-tauri/src/commands/transfer.rs` must match):
 *  - `nameCollides` trims its candidate before comparing against a listing's
 *    display names. Trimming here keeps the guard and the target key derived
 *    from exactly the same string. Dropping the trim would open a real
 *    data-loss hole rather than a cosmetic one: a local file named `"a.txt "`
 *    would be checked as `"a.txt"` (nameCollides trims) but written to
 *    `"a.txt "` — so an existing remote object literally named `"a.txt "`
 *    would be reported as no-collision and then silently destroyed by the
 *    `PutObject`, while an existing `"a.txt"` would raise a phantom conflict
 *    prompt for a key nothing was going to touch.
 *  - Leading/trailing whitespace in an S3 key is invisible in every listing
 *    UI and awkward in URLs/signatures; normalizing at the boundary is the
 *    conservative choice.
 *  - The cost is bounded and visible: a local `"a.txt "` uploads as
 *    `"a.txt"`, and if that name is taken the conflict dialog shows the full
 *    target key before anything is overwritten.
 *
 * Trim semantics are JS `String.prototype.trim()`. For an exact port, that is
 * Unicode `White_Space` minus U+0085 (NEL) plus U+FEFF (ZWNBSP) — in Rust,
 * `trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c ==
 * '\u{feff}')`, NOT the bare `str::trim()` (which trims NEL and keeps
 * U+FEFF).
 *
 * Returns `""` when nothing survives (`""`, `"   "`, `"docs/"`): there is no
 * valid upload target for such an input — see `uploadKey`. */
export function uploadBaseName(fileName: string): string {
  return (fileName.split(/[\\/]/).pop() ?? fileName).trim();
}

/** Remote key a local file will land on when uploaded into `prefix`.
 *
 * The backend's `upload_key` (in `src-tauri/src/commands/transfer.rs`) is the
 * authoritative implementation and will be written to the same cases; this
 * copy exists so the upload flow can ask "what will this overwrite?" *before*
 * anything is queued. Object storage has no rename and no overwrite warning —
 * a `PutObject` onto an existing key silently replaces it — so the answer has
 * to be known client-side, in advance.
 *
 * Only the last path segment of `fileName` is used (see `uploadBaseName`,
 * which also documents the trim), so a value carrying separators cannot
 * redirect the upload out of the browsed prefix.
 *
 * An empty basename has no valid target and yields `""` — never `prefix`
 * itself. Returning `prefix` would name the browsed folder's own marker
 * object (`"docs/"`), i.e. a key a `PutObject` could overwrite, which is the
 * one thing this function exists to prevent. Callers must treat `""` as "not
 * uploadable" rather than as a key. */
export function uploadKey(prefix: string, fileName: string): string {
  const base = uploadBaseName(fileName);
  if (base.length === 0) return "";
  if (!prefix) return base;
  return prefix.endsWith("/") ? `${prefix}${base}` : `${prefix}/${base}`;
}

/** Just enough of `useObjects`'s infinite-query data for a collision guard.
 * Structural on purpose, so this module stays free of a react-query import
 * and stays unit-testable without one. */
export interface ListingPages {
  pages: { entries: ObjectEntry[] }[];
}

/** What a collision guard is allowed to conclude from a `useObjects` query.
 *
 * `ready` is the ONLY thing a destructive action may gate on, and `entries`
 * is the only listing it may check against — both are empty/false together,
 * so a not-yet-trustworthy listing can neither wave an overwrite through nor
 * invent a phantom collision.
 *
 * Both conditions matter, and each has already been the subject of a
 * silent-data-loss bug in this project:
 *  - `data === undefined`: the listing has never resolved, so `entries` is
 *    `[]` and every name looks free — indistinguishable from an actually
 *    empty folder. Fail CLOSED. (Deliberately not `isFetching`: a background
 *    refetch of already-usable data must not re-block a guard that is
 *    already trustworthy.)
 *  - `isPlaceholderData`: `useObjects` sets `placeholderData:
 *    keepPreviousData`, so during ANY navigation (folder → folder, or bucket
 *    → bucket) `data` is defined but belongs to the PREVIOUS location. A
 *    guard that only checked `data !== undefined` would happily clear an
 *    upload into `photos/` against `docs/`'s entries — and would report a
 *    phantom conflict in the other direction. */
export function listingGuard(
  data: ListingPages | undefined,
  isPlaceholderData: boolean,
): { ready: boolean; entries: ObjectEntry[] } {
  if (data === undefined || isPlaceholderData) return { ready: false, entries: [] };
  return { ready: true, entries: data.pages.flatMap((page) => page.entries) };
}
