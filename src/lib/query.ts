import { QueryClient } from "@tanstack/react-query";

/**
 * Shared TanStack Query client.
 *
 * BucketCat's data (connections, buckets) comes from a local Tauri
 * command, not a flaky network API, so defaults lean away from the
 * library's web-oriented "retry a few times, refetch on every focus"
 * posture:
 * - `retry: false` everywhere: an IPC call either succeeds or rejects with
 *   a structured `AppError` the UI should surface immediately, not mask
 *   behind silent retries.
 * - `refetchOnWindowFocus: false`: this is a desktop app window, not a
 *   browser tab that goes stale in the background.
 * - a moderate `staleTime` on queries so switching panes doesn't
 *   re-invoke the backend for data that was just fetched; mutations
 *   explicitly invalidate the affected query keys instead of relying on
 *   background refetches.
 */
export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: false,
      refetchOnWindowFocus: false,
      staleTime: 30_000,
    },
    mutations: {
      retry: false,
    },
  },
});
