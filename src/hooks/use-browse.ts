import { useMemo } from "react";
import type { ObjectEntry } from "@/lib/api";
import { listPrefix, sortEntries } from "@/lib/entries";
import { useDebounced } from "@/hooks/use-debounced";
import { useObjects } from "@/hooks/use-objects";
import { useApp } from "@/store/app-store";

/** The current browse location's listing: store state (conn/bucket/path/
 * search) → debounced server prefix → infinite query → all loaded pages
 * flattened and sorted (folders first). Both `FileBrowser` and
 * `DetailsPanel` call this; TanStack Query dedupes the underlying IPC by
 * query key. */
export function useBrowse() {
  const { activeConn, activeBucket, path, search } = useApp();
  const debouncedSearch = useDebounced(search, 300);
  const prefix = listPrefix(path, debouncedSearch);
  const query = useObjects(activeConn, activeBucket, prefix);
  const entries: ObjectEntry[] = useMemo(
    () => sortEntries((query.data?.pages ?? []).flatMap((p) => p.entries)),
    [query.data],
  );
  return { query, entries, prefix, searching: debouncedSearch.trim().length > 0 };
}

export type BrowseQuery = ReturnType<typeof useBrowse>["query"];
