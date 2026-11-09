import { useCompactLayout } from "@/navigation/layout";
import { useLocation } from "react-router-dom";
import { routes, useNav } from "@/router";
import { useEffect, useRef, useState } from "react";
import { useBackendInfoSnapshot, useHostsQuery } from "@/features/computers/hooks/hooks";
import { NewComputerDialog } from "@/pages/computer/components/new-computer-dialog";
import { DirectoryShell, type DirectoryLifecycle } from "@/components/layout/directory-shell";
import { Sidebar } from "@/components/layout/sidebar";
import { ComputerSidebarShell } from "./computer-sidebar-shell";
import { ComputerDetailPage } from "./computer-detail-page";
import { resolveComputerSelection } from "./selection";

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
  const selectedHost = hosts.find((host) => host.id === selectedHostId) ?? null;

  useEffect(() => {
    if (compact || location.pathname !== "/computers") return;
    const next = resolveComputerSelection(
      hosts.map((host) => host.id),
      selectedHostId ?? lastSelectedHostId.current,
      localHostId,
    );

    lastSelectedHostId.current = next;
    if (next !== selectedHostId) {
      setSelectedHostId(next, true);
    }
  }, [compact, location.pathname, hosts, localHostId, selectedHostId, nav.go]);

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
        <ComputerDetailPage host={selectedHost} loading={hostsQuery.isLoading} />
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
