import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  apiPaths,
  RuntimeCatalogResponseSchema,
  runtimeCatalogEntry,
  reasoningEffortOptionsForRuntime,
  type RuntimeModelDiscovery,
  type RuntimeKind,
} from "@silverhand/shared";
import { api } from "@/lib/api";

export const runtimeModelsKey = (hostId: string | null | undefined, runtime: RuntimeKind) =>
  ["runtime-models", hostId, runtime] as const;

export function runtimeModelsQueryOptions(
  hostId: string | null | undefined,
  runtime: RuntimeKind,
  enabled = true,
  force = false,
) {
  return {
    queryKey: runtimeModelsKey(hostId, runtime),
    enabled: enabled && Boolean(hostId) && runtime !== "qoder",
    staleTime: force ? 0 : 60_000,
    retry: false,
    queryFn: async ({ signal }: { signal: AbortSignal }) => {
      const raw = await api<unknown>(
        `${apiPaths.hostRuntimeCatalog(hostId!)}?models_for=${runtime}${force ? "&force=1" : ""}`,
        { signal },
      );
      const result = RuntimeCatalogResponseSchema.parse(raw).model_discovery;
      if (result && result.runtime !== runtime) throw new Error("Model discovery runtime mismatch");
      return result ?? null;
    },
  };
}

/** Missing metadata uses fallback suggestions; an explicit empty set does not. */
export function modelReasoningOptions(
  runtime: RuntimeKind,
  model: string | null,
  discovery: RuntimeModelDiscovery | null | undefined,
) {
  const native = runtime === "claude" || runtime === "codex";
  const id = model?.trim() || (runtime === "claude" ? "opus" : discovery?.default_model);
  const details =
    native && discovery?.status === "ok" && id ? discovery.reasoning_by_model?.[id] : undefined;
  return {
    options: details?.supported_efforts ?? reasoningEffortOptionsForRuntime(runtime, model),
    defaultEffort: details?.default_effort,
  };
}

export function useRuntimeModels(
  hostId: string | null | undefined,
  runtime: RuntimeKind,
  enabled: boolean,
) {
  const client = useQueryClient();
  const options = runtimeModelsQueryOptions(hostId, runtime, enabled);
  const query = useQuery(options);
  const data = query.data;
  const status = !options.enabled
    ? "idle"
    : query.isFetching
      ? "loading"
      : query.isError || data?.status === "error"
        ? "error"
        : !data || data.status === "unsupported"
          ? "unsupported"
          : data.models.length === 0
            ? "empty"
            : data.cached
              ? "cached"
              : "ok";

  return {
    status,
    reasoningForModel: (model: string | null) => modelReasoningOptions(runtime, model, data),
    suggestions:
      data?.status === "ok" ? data.models : runtimeCatalogEntry(runtime).modelSuggestions,
    async refresh() {
      if (!options.enabled || query.isFetching) return;
      await client.cancelQueries({ queryKey: options.queryKey, exact: true });
      // Query state carries refresh failures too; the model input remains independent.
      await client
        .fetchQuery(runtimeModelsQueryOptions(hostId, runtime, true, true))
        .catch(() => {});
    },
  };
}
