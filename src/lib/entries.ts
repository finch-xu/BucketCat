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
