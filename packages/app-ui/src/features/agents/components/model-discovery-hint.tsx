import type { useRuntimeModels } from "../hooks/use-runtime-models";
import { t } from "@/lib/i18n";

export function ModelDiscoveryHint({
  discovery,
}: {
  discovery: ReturnType<typeof useRuntimeModels>;
}) {
  if (discovery.status === "idle") return null;
  const message =
    discovery.status === "loading"
      ? t("agents.modelsDetecting")
      : discovery.status === "error"
        ? t("agents.modelsFailed")
        : discovery.status === "empty"
          ? t("agents.modelsEmpty")
          : discovery.status === "cached"
            ? t("agents.modelsCached")
            : t("agents.modelsDetected");
  return (
    <div className="flex items-center gap-2 text-xs text-text-muted">
      <span role="status">{message}</span>
      <button
        type="button"
        disabled={discovery.status === "loading"}
        onClick={() => void discovery.refresh()}
        className="shrink-0 hover:text-text-primary disabled:opacity-50"
      >
        {t("agents.modelsRefresh")}
      </button>
    </div>
  );
}
