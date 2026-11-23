import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  apiPaths,
  RuntimeCatalogResponseSchema,
  runtimeCatalogEntry,
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

/** Discovered per-model efforts win; otherwise the runtime's static vocabulary. */
export function modelReasoningOptions(
  runtime: RuntimeKind,
  model: string | null,
  discovery: RuntimeModelDiscovery | null | undefined,
) {
  const id = model?.trim() || discovery?.default_model;
  const details = discovery?.status === "ok" && id ? discovery.reasoning_by_model?.[id] : undefined;
  return {
    options: details?.supported_efforts ?? runtimeCatalogEntry(runtime).reasoningEffortOptions,
    defaultEffort: details?.default_effort,
  };
}

/** Selectable tiers: Standard, then the CLI's per-model tiers or the static set. */
export function modelServiceTierOptions(
  runtime: RuntimeKind,
  model: string | null,
  discovery: RuntimeModelDiscovery | null | undefined,
): { id: string; name?: string }[] {
  const id = model?.trim() || discovery?.default_model;
  const discovered =
    discovery?.status === "ok" && id ? discovery.service_tiers_by_model?.[id] : undefined;
  const offered =
    discovered ??
    runtimeCatalogEntry(runtime)
      .serviceTierOptions.filter((tier) => tier !== "default")
      .map((tier) => ({ id: tier }));
  return runtimeCatalogEntry(runtime).serviceTierOptions.length > 0
    ? [{ id: "default" }, ...offered]
    : [];
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
  // `unsupported` keeps the static catalog without a refresh control.
  const status = !options.enabled
    ? "idle"
    : query.isFetching
      ? "loading"
      : query.isError || data?.status === "error"
        ? "error"
        : !data || data.status === "unsupported"
          ? "idle"
          : "ok";

  return {
    status,
    reasoningForModel: (model: string | null) => modelReasoningOptions(runtime, model, data),
    serviceTiersForModel: (model: string | null) => modelServiceTierOptions(runtime, model, data),
    suggestions:
      data?.status === "ok" ? data.models : runtimeCatalogEntry(runtime).modelSuggestions,
    modelName: (model: string) => (data?.status === "ok" ? data.model_names?.[model] : undefined),
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
