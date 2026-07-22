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

/** Rename target key: `key` with its final path segment replaced. */
export function renameKey(key: string, newName: string): string {
  const idx = key.lastIndexOf("/");
  return idx === -1 ? newName : key.slice(0, idx + 1) + newName;
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
