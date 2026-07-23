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

/** Remote key a local file will land on when uploaded into `prefix`.
 *
 * The backend's `upload_key` (in `src-tauri/src/commands/transfer.rs`) is the
 * authoritative implementation and will be written to the same cases; this
 * copy exists so the upload flow can ask "what will this overwrite?" *before*
 * anything is queued. Object storage has no rename and no overwrite warning —
 * a `PutObject` onto an existing key silently replaces it — so the answer has
 * to be known client-side, in advance.
 *
 * Only the last path segment of `fileName` is used, so a value carrying
 * separators cannot redirect the upload out of the browsed prefix. */
export function uploadKey(prefix: string, fileName: string): string {
  const base = (fileName.split(/[\\/]/).pop() ?? fileName).trim();
  if (!prefix) return base;
  return prefix.endsWith("/") ? `${prefix}${base}` : `${prefix}/${base}`;
}
