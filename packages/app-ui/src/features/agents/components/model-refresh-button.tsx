import type { useRuntimeModels } from "../hooks/use-runtime-models";
import { RefreshIcon } from "@/icons";
import { t } from "@/lib/i18n";
import { cn } from "@/lib/cn";

/** Sits in the Model label row; a failed lookup tints the icon instead of adding a line. */
export function ModelRefreshButton({
  discovery,
}: {
  discovery: ReturnType<typeof useRuntimeModels>;
}) {
  if (discovery.status === "idle") return null;
  const loading = discovery.status === "loading";
  const failed = discovery.status === "error";
  const label = failed ? t("agents.modelsFailed") : t("agents.modelsRefresh");
  return (
    <button
      type="button"
      disabled={loading}
      onClick={() => void discovery.refresh()}
      aria-label={label}
      title={label}
      className={cn(
        "inline-flex size-5 items-center justify-center p-0 transition-colors hover:text-text-primary disabled:cursor-default disabled:opacity-60",
        failed ? "text-error" : "text-text-muted",
      )}
    >
      <RefreshIcon className={cn("size-3.5", loading && "animate-spin")} />
    </button>
  );
}
