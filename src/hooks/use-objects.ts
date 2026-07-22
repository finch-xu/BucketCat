import {
  useInfiniteQuery,
  useMutation,
  useQueryClient,
  type InfiniteData,
  type UseInfiniteQueryResult,
  type UseMutationResult,
} from "@tanstack/react-query";
import {
  createFolder,
  deleteObjects,
  listObjects,
  renameObject,
  type AppError,
  type BatchResult,
  type ListPage,
} from "@/lib/api";

/** Key prefix for every listing of `bucket` (all prefixes/pages).
 * Mutations and the toolbar's refresh invalidate at this level, so a
 * delete inside `docs/` also refreshes the parent listing's folder row
 * counts and any sibling views. */
export const objectsRootKey = (connectionId: string, bucket: string) =>
  ["objects", connectionId, bucket] as const;

const objectsKey = (connectionId: string, bucket: string, prefix: string) =>
  ["objects", connectionId, bucket, prefix] as const;

/** Infinite listing of one browse location. Pages chain through the
 * backend's continuation token (`ListPage.next_token`; null = done). */
export function useObjects(
  connectionId: string,
  bucket: string,
  prefix: string,
): UseInfiniteQueryResult<InfiniteData<ListPage, string | null>, AppError> {
  return useInfiniteQuery({
    queryKey: objectsKey(connectionId, bucket, prefix),
    queryFn: ({ pageParam }) => listObjects(connectionId, bucket, prefix, pageParam),
    initialPageParam: null as string | null,
    getNextPageParam: (lastPage) => lastPage.next_token,
    enabled: connectionId.length > 0 && bucket.length > 0,
  });
}

export function useDeleteObjects(
  connectionId: string,
  bucket: string,
): UseMutationResult<BatchResult, AppError, string[]> {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (keys: string[]) => deleteObjects(connectionId, bucket, keys),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: objectsRootKey(connectionId, bucket) });
    },
  });
}

export function useRenameObject(
  connectionId: string,
  bucket: string,
): UseMutationResult<void, AppError, { fromKey: string; toKey: string }> {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ fromKey, toKey }: { fromKey: string; toKey: string }) =>
      renameObject(connectionId, bucket, fromKey, toKey),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: objectsRootKey(connectionId, bucket) });
    },
  });
}

export function useCreateFolder(
  connectionId: string,
  bucket: string,
): UseMutationResult<void, AppError, string> {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (prefix: string) => createFolder(connectionId, bucket, prefix),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: objectsRootKey(connectionId, bucket) });
    },
  });
}
