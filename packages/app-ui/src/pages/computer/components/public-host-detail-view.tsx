import type { HostPublicDto } from "@silverhand/shared";
import { useMemo } from "react";
import { cn } from "@/lib/cn";
import { t } from "@/lib/i18n";
import { paneChrome } from "@/components/layout/pane-chrome";
import { useHostStatusReadQuery } from "@/features/computers/hooks/hooks";
import { describeHostStatus } from "../model/host-status-presentation";
import { formatOsShort } from "../model/format-os";
import { ComputerStatusSkeleton } from "./computer-skeletons";
import {
  CreatedField,
  formatCreatedAt,
  HostHero,
  RegisteredAgentsSection,
  SystemField,
} from "./host-detail-view";
import { Banner, HostSection } from "./host-section";

/**
 * Someone else's computer, read at the public tier: who made it, whether it is
 * on, its OS, and the agents on it. Nothing here acts on the computer, and the
 * page has no realtime frames, so Refresh and window focus re-read it.
 */
export function PublicHostDetailView({ host }: { host: HostPublicDto }): JSX.Element {
  const statusQuery = useHostStatusReadQuery(host.id);
  const status = statusQuery.data;
  const statusView = useMemo(() => (status ? describeHostStatus(status) : null), [status]);
  const trimmedLabel = host.display_label?.trim() ?? "";
  const snapshot = status?.availability === "available" ? status.snapshot : null;

  return (
    <main
      data-panel="detail"
      className="flex h-full min-w-0 flex-1 flex-col overflow-y-auto"
      style={paneChrome}
    >
      <HostHero
        host={host}
        showStableName={trimmedLabel.length > 0 && trimmedLabel !== host.name}
        statusLabel={statusView?.label ?? null}
        statusTone={statusView?.tone ?? null}
        statusLoading={statusQuery.isLoading}
        refreshing={statusQuery.isFetching}
        onRefresh={() => void statusQuery.refetch()}
        appManaged={false}
      />

      <div className="mx-auto flex w-full max-w-2xl flex-col px-6">
        <HostSection title={t("computers.computer")}>
          {snapshot ? (
            <dl data-host-system-list className="flex flex-col gap-2.5">
              <SystemField label={t("computers.os")}>
                <span className="text-text-primary">{formatOsShort(snapshot.os)}</span>
                {snapshot.os_arch ? (
                  <span className="ml-2 text-text-muted">{snapshot.os_arch}</span>
                ) : null}
              </SystemField>
            </dl>
          ) : null}
          <dl
            data-host-system-list
            className={cn(
              "flex flex-col gap-1.5 text-xs text-text-muted",
              snapshot && "mt-4 border-t border-dashed border-border-subtle pt-4",
            )}
          >
            <CreatedField createdAt={host.created_at} creatorUserId={host.creator_user_id} />
          </dl>
        </HostSection>

        {statusQuery.isLoading ? (
          <div className="py-6">
            <ComputerStatusSkeleton />
          </div>
        ) : statusQuery.isError ? (
          <div className="py-6">
            <Banner tone="error">{t("computers.couldNotLoadThis")}</Banner>
          </div>
        ) : status ? (
          <RegisteredAgentsSection hostId={host.id} agents={status.agents} live={false} />
        ) : null}
      </div>
    </main>
  );
}
