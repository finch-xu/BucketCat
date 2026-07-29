import { useQuery, type UseQueryResult } from "@tanstack/react-query";
import { getSettings, type AppError, type Settings } from "@/lib/api";

/** The shared query key. Exported so writers can invalidate it without
 * re-typing the literal -- every settings writer in `general-pane.tsx` and
 * `transfers-pane.tsx` does exactly that after a successful persist. */
export const settingsKey = ["settings"] as const;

/**
 * The persisted `Settings` blob, shared by every consumer of the
 * `["settings"]` key.
 *
 * `staleTime: Infinity` on purpose: this is a local JSON file behind an IPC
 * call, not a resource that drifts on its own. It only ever changes when
 * *this* app writes it, and every writer already invalidates the key -- so a
 * time-based refetch could only ever re-read a value we just wrote.
 *
 * Because the cache lives in the `QueryClient` rather than in a component,
 * it survives the Settings modal unmounting: reopening the modal costs zero
 * extra `get_settings` round trips.
 */
export function useSettings(): UseQueryResult<Settings, AppError> {
  return useQuery({
    queryKey: settingsKey,
    // `useQuery`'s `onError` callback was removed in TanStack Query v5, and
    // `retry: false` (the shared client default, see `src/lib/query.ts`)
    // means a failure surfaces exactly once -- so log it here, in the
    // queryFn itself, and re-throw so the query still enters its error
    // state for any consumer that wants it.
    queryFn: () =>
      getSettings().catch((err) => {
        console.error("Failed to load settings", err);
        throw err;
      }),
    staleTime: Infinity,
  });
}
