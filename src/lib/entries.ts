import type { ObjectEntry } from "./mock-data";

export function sortEntries(entries: ObjectEntry[]): ObjectEntry[] {
  return entries.slice().sort((a, b) => {
    if (a.kind !== b.kind) return a.kind === "folder" ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
}

export function filterByPrefix(entries: ObjectEntry[], prefix: string): ObjectEntry[] {
  const q = prefix.trim().toLowerCase();
  if (!q) return entries;
  return entries.filter((e) => e.name.toLowerCase().startsWith(q));
}
