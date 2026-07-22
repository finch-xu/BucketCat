import {
  useMutation,
  useQuery,
  useQueryClient,
  type UseMutationResult,
  type UseQueryResult,
} from "@tanstack/react-query";
import {
  addConnection,
  deleteConnection,
  listBuckets,
  listConnections,
  updateConnection,
  type AppError,
  type Bucket,
  type ConnectionDto,
  type ConnectionInput,
} from "@/lib/api";

const connectionsKey = ["connections"] as const;
const bucketsKey = (connectionId: string) => ["buckets", connectionId] as const;

export function useConnections(): UseQueryResult<ConnectionDto[], AppError> {
  return useQuery({
    queryKey: connectionsKey,
    queryFn: listConnections,
  });
}

export function useBuckets(
  connectionId: string,
  enabled = true,
): UseQueryResult<Bucket[], AppError> {
  return useQuery({
    queryKey: bucketsKey(connectionId),
    queryFn: () => listBuckets(connectionId),
    enabled: enabled && connectionId.length > 0,
  });
}

export function useAddConnection(): UseMutationResult<
  ConnectionDto,
  AppError,
  ConnectionInput
> {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: ConnectionInput) => addConnection(input),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: connectionsKey });
    },
  });
}

export function useUpdateConnection(): UseMutationResult<
  ConnectionDto,
  AppError,
  { id: string; input: ConnectionInput }
> {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ id, input }: { id: string; input: ConnectionInput }) =>
      updateConnection(id, input),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: connectionsKey });
    },
  });
}

export function useDeleteConnection(): UseMutationResult<void, AppError, string> {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => deleteConnection(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: connectionsKey });
    },
  });
}
