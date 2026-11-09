import { useCompactLayout } from "@/navigation/layout";
import { useEffect, useRef } from "react";
import { routes, useNav, useRouteState } from "@/router";
import { useAgent, useAgents } from "@/features/agents/hooks/hooks";
import { DirectoryShell, type DirectoryLifecycle } from "@/components/layout/directory-shell";
import { Sidebar } from "@/components/layout/sidebar";
import { AgentSidebarShell } from "./agent-sidebar-shell";
import { AgentDetailPage, type AgentNavigation } from "./agent-detail-page";
import { resolveDefaultAgent } from "./selection";
import { byIdSelection } from "@/lib/by-id-selection";

export function AgentShell({
  mounted,
  visible,
  onMessageAgent,
  onOpenConversation,
}: DirectoryLifecycle & AgentNavigation): JSX.Element {
  const compact = useCompactLayout();
  const route = useRouteState();
  const nav = useNav();
  const { data: agents = [] } = useAgents();
  const lastSelectedAgentId = useRef<string | null>(null);
  // An agent outside the viewer's list (opened from a link) is read by id;
  // only a definite 404 falls back to the list.
  const listed = route.agentId !== null && agents.some((agent) => agent.id === route.agentId);
  const selectedQuery = useAgent(listed ? null : route.agentId);
  const unlisted = !listed && route.agentId !== null ? byIdSelection(selectedQuery) : null;
  const selectedPending = unlisted === "pending";
  const selectableIds = agents.map((agent) => agent.id);

  if (unlisted === "kept" && route.agentId) {
    selectableIds.push(route.agentId);
  }

  useEffect(() => {
    if (route.agentId && agents.some((agent) => agent.id === route.agentId)) {
      lastSelectedAgentId.current = route.agentId;
    }
  }, [agents, route.agentId]);

  useEffect(() => {
    if (compact || route.section !== "members" || selectedPending) {
      return;
    }

    const target = resolveDefaultAgent(
      selectableIds,
      route.agentId,
      route.userId,
      lastSelectedAgentId.current,
    );

    if (target) {
      nav.go(routes.membersAgent(target), { replace: true });
    }
  }, [compact, agents, unlisted, route.section, route.agentId, route.userId, nav]);

  return (
    <DirectoryShell
      name="members"
      selected={Boolean(route.agentId || route.userId)}
      mounted={mounted}
      visible={visible}
      sidebar={
        <AgentSidebarShell
          selectedAgentId={route.agentId}
          selectedUserId={route.userId}
          onSelectAgent={(id) => nav.toMembers(id)}
          onSelectUser={(id) => nav.toMembersUser(id)}
        />
      }
    >
      <AgentDetailPage
        agentId={route.agentId}
        userId={route.userId}
        onExit={() => nav.toMembers()}
        onMessageAgent={onMessageAgent}
        onOpenConversation={onOpenConversation}
      />
    </DirectoryShell>
  );
}

/** The Members sidebar as the rail's hover peek. */
export function MembersPeekSidebar(): JSX.Element {
  const route = useRouteState();
  const nav = useNav();

  return (
    <Sidebar
      name="members"
      width="var(--sidebar-width)"
      sidebar={
        <AgentSidebarShell
          selectedAgentId={route.section === "members" ? route.agentId : null}
          selectedUserId={route.section === "members" ? route.userId : null}
          onSelectAgent={(id) => nav.toMembers(id)}
          onSelectUser={(id) => nav.toMembersUser(id)}
        />
      }
    />
  );
}
