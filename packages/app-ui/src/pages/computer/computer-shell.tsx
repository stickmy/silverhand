import { useCompactLayout } from "@/navigation/layout";
import { useLocation } from "react-router-dom";
import { routes, useNav } from "@/router";
import { useEffect, useRef, useState } from "react";
import {
  useBackendInfoSnapshot,
  useHostReadQuery,
  useHostsQuery,
} from "@/features/computers/hooks/hooks";
import { NewComputerDialog } from "@/pages/computer/components/new-computer-dialog";
import { DirectoryShell, type DirectoryLifecycle } from "@/components/layout/directory-shell";
import { Sidebar } from "@/components/layout/sidebar";
import { ComputerSidebarShell } from "./computer-sidebar-shell";
import { ComputerDetailPage } from "./computer-detail-page";
import { resolveComputerSelection } from "./selection";
import { byIdSelection } from "@/lib/by-id-selection";

export function ComputerShell({ mounted, visible }: DirectoryLifecycle): JSX.Element {
  const compact = useCompactLayout();
  const location = useLocation();
  const nav = useNav();
  const lastSelectedHostId = useRef<string | null>(null);
  const selectedHostId = new URLSearchParams(location.search).get("computer");
  const setSelectedHostId = (id: string | null, replace = false) =>
    nav.go(id ? routes.computer(id) : routes.computers(), { replace });
  const [newComputerOpen, setNewComputerOpen] = useState(false);

  const hostsQuery = useHostsQuery();
  const backendInfoQuery = useBackendInfoSnapshot();
  const hosts = hostsQuery.data?.hosts ?? [];
  const localHostId = backendInfoQuery.data?.local_host_id ?? null;
  const listedHost = hosts.find((host) => host.id === selectedHostId) ?? null;
  // A computer outside the viewer's list (someone else's, opened from an agent
  // profile) is read by id at whatever tier the backend grants; only a
  // definite 404 falls back to the default selection.
  const byIdQuery = useHostReadQuery(selectedHostId, {
    enabled: hostsQuery.isSuccess && listedHost === null,
  });
  const selectedRead = listedHost
    ? { view: "privileged" as const, host: listedHost }
    : (byIdQuery.data ?? null);
  const unlisted =
    selectedHostId !== null && listedHost === null
      ? hostsQuery.isSuccess
        ? byIdSelection(byIdQuery)
        : "pending"
      : null;
  const selectedPending = unlisted === "pending";

  useEffect(() => {
    if (compact || location.pathname !== "/computers" || selectedPending) return;
    const ids = hosts.map((host) => host.id);

    if (unlisted === "kept" && selectedHostId) {
      ids.push(selectedHostId);
    }

    const next = resolveComputerSelection(
      ids,
      selectedHostId ?? lastSelectedHostId.current,
      localHostId,
    );

    lastSelectedHostId.current = next;
    if (next !== selectedHostId) {
      setSelectedHostId(next, true);
    }
  }, [
    compact,
    location.pathname,
    hosts,
    unlisted,
    selectedPending,
    localHostId,
    selectedHostId,
    nav.go,
  ]);

  return (
    <>
      <DirectoryShell
        name="computers"
        selected={selectedHostId !== null}
        mounted={mounted}
        visible={visible}
        sidebar={
          <ComputerSidebarShell
            hosts={hosts}
            localHostId={localHostId}
            isLoading={hostsQuery.isLoading}
            selectedHostId={selectedHostId}
            onSelectHost={(id) => setSelectedHostId(id)}
            onAddComputer={() => setNewComputerOpen(true)}
          />
        }
      >
        <ComputerDetailPage
          read={selectedRead}
          loading={hostsQuery.isLoading || selectedPending}
          error={unlisted === "kept" && !selectedRead ? byIdQuery.error : null}
          onRetry={() => void byIdQuery.refetch()}
        />
      </DirectoryShell>
      <NewComputerDialog
        open={newComputerOpen}
        onOpenChange={setNewComputerOpen}
        onCreated={(id) => setSelectedHostId(id)}
      />
    </>
  );
}

/** The Computers sidebar as the rail's hover peek, with its add dialog. */
export function ComputersPeekSidebar(): JSX.Element {
  const location = useLocation();
  const nav = useNav();
  const [newComputerOpen, setNewComputerOpen] = useState(false);

  const hostsQuery = useHostsQuery();
  const backendInfoQuery = useBackendInfoSnapshot();
  const selectedHostId =
    location.pathname === "/computers"
      ? new URLSearchParams(location.search).get("computer")
      : null;
  const openHost = (id: string) => nav.toComputer(id);

  return (
    <>
      <Sidebar
        name="computers"
        width="var(--sidebar-width)"
        sidebar={
          <ComputerSidebarShell
            hosts={hostsQuery.data?.hosts ?? []}
            localHostId={backendInfoQuery.data?.local_host_id ?? null}
            isLoading={hostsQuery.isLoading}
            selectedHostId={selectedHostId}
            onSelectHost={openHost}
            onAddComputer={() => setNewComputerOpen(true)}
          />
        }
      />
      <NewComputerDialog
        open={newComputerOpen}
        onOpenChange={setNewComputerOpen}
        onCreated={openHost}
      />
    </>
  );
}
