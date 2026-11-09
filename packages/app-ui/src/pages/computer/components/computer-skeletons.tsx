import { SkeletonBlock as Block, SkeletonRegion as Region } from "@/components/layout/skeleton";
import { t } from "@/lib/i18n";
export function ComputerRowsSkeleton(): JSX.Element {
  return (
    <Region label={t("computers.loadingComputers")}>
      {["w-24", "w-20", "w-28", "w-16"].map((width) => (
        <div
          key={width}
          className="sidebar-row-border flex items-center gap-2 border border-transparent px-2 py-2"
        >
          <Block className="size-4" />
          <div className="min-w-0 flex-1 space-y-0.5">
            <div className="flex h-4 items-center">
              <Block className={`h-3 ${width}`} />
            </div>
            <div className="flex h-4 items-center">
              <Block className="h-2.5 w-16" />
            </div>
          </div>
          <Block className="ml-auto size-1.5 rounded-full" />
        </div>
      ))}
    </Region>
  );
}

export function ComputerStatusSkeleton(): JSX.Element {
  return (
    <Region label={t("computers.loadingHostStatus")}>
      <h3 className="mb-4 text-xs font-semibold text-text-muted">{t("computers.computer")}</h3>
      <div className="space-y-2.5">
        {[
          t("computers.hostname"),
          t("computers.os"),
          t("computers.ipAddress"),
          t("computers.memory"),
          t("computers.disk"),
          t("computers.uptime"),
        ].map((label) => (
          <div key={label} className="flex items-center gap-4">
            <span className="w-28 shrink-0 text-xs text-text-muted">{label}</span>
            <Block className="h-3 w-2/5" />
          </div>
        ))}
      </div>
      <div className="mt-6 space-y-3">
        <Block className="h-3 w-36" />
        {[0, 1].map((i) => (
          <div key={i} className="flex items-center gap-2 py-1">
            <Block className="size-5" />
            <Block className="h-3 w-24" />
          </div>
        ))}
      </div>
    </Region>
  );
}

export function ComputerDetailSkeleton(): JSX.Element {
  return (
    <div className="mx-auto w-full max-w-2xl px-6 py-6">
      <Region label={t("computers.loadingComputer")}>
        <div className="mb-8 flex items-center gap-4">
          <Block className="size-20" />
          <div className="flex-1 space-y-3">
            <Block className="h-6 w-3/5" />
            <Block className="h-3 w-24" />
          </div>
        </div>
      </Region>
      <ComputerStatusSkeleton />
    </div>
  );
}

export function ProxySkeleton(): JSX.Element {
  return (
    <Region label={t("computers.loadingProxySettings")}>
      <div className="grid gap-5">
        <div className="space-y-2">
          <p className="text-xs text-text-muted">{t("computers.proxyUrl")}</p>
          <Block className="h-8 w-full" />
        </div>
        <div className="space-y-3">
          <p className="text-xs text-text-muted">{t("computers.enabledRuntimes")}</p>
          {[0, 1, 2].map((i) => (
            <div key={i} className="flex gap-2">
              <Block className="size-3.5" />
              <Block className="h-3 w-24" />
            </div>
          ))}
        </div>
      </div>
    </Region>
  );
}
