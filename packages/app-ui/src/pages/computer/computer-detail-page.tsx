import { ComputerDetailSkeleton } from "./components/computer-skeletons";
import type { HostGetResponse } from "@silverhand/shared";
import { HostDetailView } from "@/pages/computer/components/host-detail-view";
import { PublicHostDetailView } from "@/pages/computer/components/public-host-detail-view";
import { DirectoryPlaceholder } from "@/components/layout/directory-shell";
import { Button } from "@/components/ui/button";
import { t } from "@/lib/i18n";

export function ComputerDetailPage({
  read,
  loading,
  error,
  onRetry,
}: {
  read: HostGetResponse | null;
  loading: boolean;
  /** A failed read of a computer outside the viewer's list, other than 404. */
  error: unknown;
  onRetry: () => void;
}): JSX.Element {
  if (read?.view === "privileged") {
    return <HostDetailView key={read.host.id} host={read.host} />;
  }

  if (read?.view === "public") {
    return <PublicHostDetailView key={read.host.id} host={read.host} />;
  }

  if (error) {
    return (
      <DirectoryPlaceholder>
        <div className="flex flex-col items-center gap-3">
          <span>{t("computers.couldNotLoadComputer")}</span>
          <Button variant="neutral" size="sm" onClick={onRetry}>
            {t("common.retry")}
          </Button>
        </div>
      </DirectoryPlaceholder>
    );
  }

  return loading ? (
    <ComputerDetailSkeleton />
  ) : (
    <DirectoryPlaceholder>{t("computers.selectAComputer")}</DirectoryPlaceholder>
  );
}
