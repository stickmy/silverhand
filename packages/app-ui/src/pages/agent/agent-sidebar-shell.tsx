import { SidebarRowsSkeleton } from "@/components/layout/sidebar-skeleton";
import { DirectorySidebar } from "@/components/layout/directory-shell";
import { ComputerIcon, MembersIcon, PlusIcon, UserIcon } from "@/icons";
import { Fragment, useMemo, useState } from "react";
import type { AgentDto, HostDto } from "@silverhand/shared";
import { useAgents, useAgentPresence } from "@/features/agents/hooks/hooks";
import { projectPresence } from "@/features/agents/model/presence-projection";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { CreateAgentDialog } from "@/features/agents/components/create-agent-dialog";
import { useHostsQuery } from "@/features/computers/hooks/hooks";
import { hostDisplayName, unknownComputerLabel } from "@/features/computers/model/host-display";
import {
  isBackendUnreachable,
  isRegistrationRequired,
  isUpdateRequired,
  useViewerAvatarObjectUrl,
  useViewerUser,
} from "@/features/session/hooks";
import { useRelatedUsers } from "@/features/users/related-users";
import { usePresenceById, presenceDotStatus } from "@/features/users/presence";
import { useUserAvatarObjectUrls } from "@/features/users/avatar-hooks";
import { AgentAvatar } from "@/components/agent-avatar";
import { UserAvatar } from "@/components/user-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { Button } from "@/components/ui/button";
import { currentUserSubtitle } from "@/features/session/current-user";
import { SidebarRow, SidebarSection } from "@/components/layout/sidebar-shared";
import { t } from "@/lib/i18n";

/** HUMANS caps its list here (not counting the pinned viewer row) before the
 *  "Show all" toggle takes over. Above this the roster buries AGENTS; at or
 *  below it the whole list is scannable at a glance. */
const HUMANS_VISIBLE_CAP = 8;

interface AgentSidebarShellProps {
  selectedAgentId?: string | null;
  onSelectAgent?: (id: string) => void;
  selectedUserId?: string | null;
  onSelectUser?: (id: string) => void;
}

export function AgentSidebarShell({
  selectedAgentId,
  onSelectAgent,
  selectedUserId,
  onSelectUser,
}: AgentSidebarShellProps): JSX.Element {
  const [open, setOpen] = useState(false);
  // AGENTS lists only the agents the login user CREATED (#task:1256, shiki
  // option A). The narrowing is defined by the backend `?scope=owned` contract
  // — we pass the flag and never re-filter here, so "my agents" has one
  // definition shared with the CLI and every other consumer.
  const { data: agents = [], isLoading: agentsLoading } = useAgents({ scope: "owned" });
  const agentAvatarUrls = useAgentAvatarObjectUrls(agents);
  // Rail groups agents by the computer they run on (`agent.host_id`). The
  // name comes from the host DTO via the shared resolver — never from `kind`
  // or provisioning. Computers the viewer no longer owns (deleted / revoked)
  // collapse into one restrained "Unknown computer" group at the end.
  const hostsQuery = useHostsQuery();
  const hostsById = useMemo(
    () => new Map((hostsQuery.data?.hosts ?? []).map((h) => [h.id, h] as const)),
    [hostsQuery.data],
  );
  const agentGroups = useMemo(() => {
    const known = new Map<string, { host: HostDto; agents: AgentDto[] }>();
    const unknown: AgentDto[] = [];
    for (const a of agents) {
      const host = hostsById.get(a.host_id);
      if (host) {
        const group = known.get(host.id);
        if (group) {
          group.agents.push(a);
        } else {
          known.set(host.id, { host, agents: [a] });
        }
      } else {
        unknown.push(a);
      }
    }
    const groups = [...known.values()]
      .map((g) => ({ key: g.host.id, name: hostDisplayName(g.host), agents: g.agents }))
      .sort((a, b) => a.name.localeCompare(b.name));
    if (unknown.length > 0) {
      groups.push({ key: "__unknown__", name: unknownComputerLabel(), agents: unknown });
    }
    return groups;
  }, [agents, hostsById]);
  // Only group once it's meaningful (≥2 computers). A single computer would
  // just add a redundant header, so those agents render flat as before.
  const showGroups = agentGroups.length >= 2;

  const renderAgentRow = (a: AgentDto): JSX.Element => (
    <AgentRow
      key={a.id}
      agent={a}
      avatarObjectUrl={agentAvatarUrls.get(a.id) ?? null}
      selected={selectedAgentId === a.id}
      onSelect={onSelectAgent}
    />
  );
  const viewerQuery = useViewerUser();
  const viewerAvatarObjectUrl = useViewerAvatarObjectUrl();
  const viewer =
    viewerQuery.data &&
    !isRegistrationRequired(viewerQuery.data) &&
    !isBackendUnreachable(viewerQuery.data) &&
    !isUpdateRequired(viewerQuery.data)
      ? viewerQuery.data
      : null;

  const { data: relatedUsers = [], isLoading: humansLoading } = useRelatedUsers(viewer?.id ?? null);

  const otherHumans = viewer ? relatedUsers.filter((u) => u.id !== viewer.id) : [];
  const otherAvatarUrls = useUserAvatarObjectUrls(otherHumans);
  const humansCount = (viewer ? 1 : 0) + otherHumans.length;

  const presenceById = usePresenceById();

  // Cap HUMANS at HUMANS_VISIBLE_CAP so a long list can't bury AGENTS; the
  // viewer's own row is pinned first, so the cap never hides "you". Local view
  // state, not a persisted preference.
  const [showAllHumans, setShowAllHumans] = useState(false);
  const humansOverflow = otherHumans.length > HUMANS_VISIBLE_CAP;
  const visibleOtherHumans =
    showAllHumans || !humansOverflow ? otherHumans : otherHumans.slice(0, HUMANS_VISIBLE_CAP);

  return (
    <DirectorySidebar>
      <SidebarSection
        label={t("agentPage.agents")}
        icon={<MembersIcon className="size-3.5" />}
        count={agents.length}
        collapsible
        defaultExpanded
        persistId="agent-directory-agents"
        actionAlwaysVisible
        action={
          <Button
            variant="ghost"
            size="icon"
            className="h-6 w-6"
            onClick={() => setOpen(true)}
            aria-label={t("agentPage.newAgent")}
          >
            <PlusIcon className="size-3.5" />
          </Button>
        }
      >
        {agentsLoading && agents.length === 0 ? (
          <li>
            <SidebarRowsSkeleton label={t("agentPage.loadingAgents")} members />
          </li>
        ) : agents.length === 0 ? (
          <li className="px-2 py-2 text-xs text-text-muted">{t("agentPage.noAgents")}</li>
        ) : showGroups ? (
          agentGroups.map((group) => (
            <Fragment key={group.key}>
              <li
                data-agent-group-header
                className="flex items-center gap-1.5 px-2 pb-0.5 pt-2 text-(length:--text-meta) font-medium text-text-muted"
              >
                <ComputerIcon className="size-3 shrink-0" />
                <span className="truncate">{group.name}</span>
              </li>
              {group.agents.map(renderAgentRow)}
            </Fragment>
          ))
        ) : (
          agents.map(renderAgentRow)
        )}
      </SidebarSection>

      <SidebarSection
        label={t("agentPage.humans")}
        icon={<UserIcon className="size-3.5" />}
        count={humansCount}
        collapsible
        defaultExpanded
        persistId="agent-directory-humans"
      >
        {viewer ? (
          <SidebarRow
            key={viewer.id}
            className="h-8"
            icon={
              <HumanRowIcon
                avatarObjectUrl={viewerAvatarObjectUrl}
                alt={viewer.display_name || viewer.name}
              />
            }
            title={viewer.display_name || viewer.name}
            subtitle={currentUserSubtitle()}
            status={presenceDotStatus(presenceById.get(viewer.id)?.presence)}
            selected={selectedUserId === viewer.id}
            onClick={onSelectUser ? () => onSelectUser(viewer.id) : undefined}
          />
        ) : null}
        {(viewerQuery.isLoading || humansLoading) && otherHumans.length === 0 ? (
          <li>
            <SidebarRowsSkeleton label={t("agentPage.loadingHumans")} members />
          </li>
        ) : null}
        {visibleOtherHumans.map((u) => {
          const avatarObjectUrl = otherAvatarUrls.get(u.id) ?? null;
          return (
            <SidebarRow
              key={u.id}
              className="h-8"
              icon={
                <HumanRowIcon avatarObjectUrl={avatarObjectUrl} alt={u.display_name || u.name} />
              }
              title={u.display_name || u.name}
              subtitle={<span className="font-mono">@{u.name}</span>}
              status={presenceDotStatus(presenceById.get(u.id)?.presence)}
              selected={selectedUserId === u.id}
              onClick={onSelectUser ? () => onSelectUser(u.id) : undefined}
            />
          );
        })}
        {humansOverflow ? (
          <li>
            <button
              type="button"
              onClick={() => setShowAllHumans((prev) => !prev)}
              className="flex w-full items-center px-2 py-1 text-xs font-medium text-text-muted transition-colors hover:text-text-primary"
            >
              {showAllHumans
                ? t("agentPage.showFewer")
                : t("agentPage.showAll", { count: humansCount })}
            </button>
          </li>
        ) : null}
        {!viewerQuery.isLoading && !humansLoading && !viewer && otherHumans.length === 0 ? (
          <li className="px-2 py-2 text-xs text-text-muted">{t("agentPage.noHumans")}</li>
        ) : null}
      </SidebarSection>

      <CreateAgentDialog open={open} onOpenChange={setOpen} />
    </DirectorySidebar>
  );
}

function AgentRow({
  agent,
  avatarObjectUrl,
  selected,
  onSelect,
}: {
  agent: AgentDto;
  avatarObjectUrl: string | null;
  selected: boolean;
  onSelect?: (id: string) => void;
}): JSX.Element {
  const presence = useAgentPresence(agent.id);

  const display = presence === null ? null : projectPresence({ scope: "global", presence });
  return (
    <SidebarRow
      className="h-8"
      icon={
        <AgentAvatar
          runtime={agent.runtime}
          size={AVATAR_PX.xs}
          avatarObjectUrl={avatarObjectUrl}
          alt={agent.display_name || agent.name}
        />
      }
      title={agent.display_name || agent.name}
      // Runtime as a faint chip (`--chip-bg`), not bare grey text: gives the
      // agent row a shape the human `@handle` row lacks, so the two entity
      // kinds don't read as one list.
      subtitle={
        <span
          data-agent-runtime
          className="rounded-(--chip-radius) bg-(--chip-bg) px-1 py-px text-(length:--text-badge) font-medium uppercase tracking-wide text-text-muted"
        >
          {agent.runtime}
        </span>
      }
      // Always a dot for a consistent column: online green, crashed red, every
      // other state the muted "idle" dot; only an unsettled read draws nothing.
      status={
        display === null
          ? null
          : display.tone === "online"
            ? "active"
            : display.tone === "failed"
              ? "crashed"
              : "idle"
      }
      selected={selected}
      onClick={onSelect ? () => onSelect(agent.id) : undefined}
    />
  );
}

function HumanRowIcon({
  avatarObjectUrl,
  alt,
}: {
  avatarObjectUrl: string | null | undefined;
  alt: string;
}): JSX.Element {
  return <UserAvatar size={AVATAR_PX.xs} avatarObjectUrl={avatarObjectUrl} alt={alt} />;
}
