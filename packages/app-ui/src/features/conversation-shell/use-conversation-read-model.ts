import type { WorkspaceChannelReference } from "@/features/messages/components/conversation-ref";
import { isMemberChannel } from "@/features/channels/membership";
import { useChannelMembership } from "@/features/channels/use-channel-membership";
import { locatorWorkspaceName } from "@/features/conversations/locator-source";
import {
  directPairPaneDescriptor,
  type ConversationPaneDescriptor,
} from "@/features/conversations/pane-descriptor";
import { useColumnFacts, useConversationColumn } from "./conversation-column";
import { presentCounterpart } from "@/features/direct-pairs/counterpart-presentation";
import {
  directPairPaneProps,
  useDirectPairPane,
  useDirectPairsList,
} from "@/features/direct-pairs/hooks";
import {
  useArchivedChannelsForWorkspace,
  useChannelsForWorkspace,
  useWorkspaces,
  type ChannelListItem,
  type WorkspaceListPayload,
} from "@/features/workspaces/hooks";
import { isAgentSpace } from "@/features/workspaces/workspace-ownership";
import {
  directPairAuthorityFromQuery,
  type DirectPairAuthority,
} from "@/features/app-shell/chat-place-restore";
import type { ConversationPointer } from "./conversation-shell-models";
import { useRouteState } from "@/router";
import { conversationPanes } from "@/navigation/panes";
import { isWorkspacePlaced } from "@silverhand/shared";
import { useCallback, useDeferredValue, useMemo } from "react";
import { useCompactLayout } from "@/navigation/layout";

// Shared fallbacks for reads with no data (a DM has no workspace, so its channel
// reads stay disabled): a fresh `[]` per render would recompute every memo below.
const NO_WORKSPACES: WorkspaceListPayload["items"] = [];
const NO_CHANNELS: ChannelListItem[] = [];

/** Carrier authority only; navigation and read-cursor writes belong to the coordinators. */
export function useConversationReadModel() {
  const {
    section,
    workspaceId: selectedWorkspaceId,
    channelId: selectedChannelId,
    directConversationId,
    panes,
  } = useRouteState();

  const { data: workspaces = NO_WORKSPACES, isLoading: workspacesLoading } = useWorkspaces();

  const visibleWorkspaceIds = useMemo(
    () => workspaces.map((workspace) => workspace.id),
    [workspaces],
  );

  const selectedWorkspace = workspaces.find((w) => w.id === selectedWorkspaceId) ?? null;

  const workspaceNameById = useCallback(
    (wid: string | null): string | null =>
      wid === null ? null : locatorWorkspaceName(workspaces.find((w) => w.id === wid)),
    [workspaces],
  );

  const workspaceRefById = useMemo(
    () =>
      new Map(
        workspaces.map(
          (w) =>
            [
              w.id,
              {
                name: w.name,
                ownerAgentId: w.owner_agent_id,
              },
            ] as const,
        ),
      ),
    [workspaces],
  );

  const workspaceResolved = !!selectedWorkspace;

  const {
    data: activeChannels = NO_CHANNELS,
    isLoading: activeChannelsLoading,
    status: activeChannelsStatus,
    fetchStatus: activeChannelsFetchStatus,
  } = useChannelsForWorkspace(selectedWorkspaceId, workspaceResolved);

  const {
    data: archivedChannels = NO_CHANNELS,
    status: archivedChannelsStatus,
    fetchStatus: archivedChannelsFetchStatus,
  } = useArchivedChannelsForWorkspace(selectedWorkspaceId, workspaceResolved);

  const channels = useMemo<ChannelListItem[]>(
    () => [...activeChannels, ...archivedChannels],
    [activeChannels, archivedChannels],
  );

  const memberActiveChannels = useMemo(
    () => activeChannels.filter(isMemberChannel),
    [activeChannels],
  );

  const directPairsQuery = useDirectPairsList();

  const directPairs = directPairsQuery.data?.direct_pairs ?? [];

  const directPairPane = directPairPaneProps(useDirectPairPane(directConversationId));

  const directPair = directPairPane.pair;

  const directPairResolving = directPairPane.resolving;

  const directPairLoadFailed = directPairPane.loadFailed;

  const directPairNotFound = directPairPane.notFound;

  const directPairIds = useMemo(
    () => new Set(directPairs.map((p) => p.conversation_id)),
    [directPairs],
  );

  const primaryPointer = useMemo<ConversationPointer | null>(() => {
    // `roster_policy` is not a lookup here: a `direct_pair` is minted `fixed`
    // (`conversation-repo.ts`) and its two endpoints are the placement itself,
    // so the value is a property of the KIND, not of the row.
    if (directPair) {
      return { id: directPair.conversation_id, rosterPolicy: "fixed" };
    }
    if (!selectedChannelId) {
      return null;
    }
    const hit = channels.find((c) => c.id === selectedChannelId);
    return hit ? { id: hit.id, rosterPolicy: hit.roster_policy } : null;
  }, [directPair, channels, selectedChannelId]);

  const resolveNavWorkspaceId = (
    workspaceId: string | undefined,
    channelId?: string | null,
  ): string | null => {
    if (!workspaceId) {
      return selectedWorkspaceId;
    }
    if (workspaces.some((w) => w.id === workspaceId)) {
      return workspaceId;
    }
    const owningAgentSpace = workspaces.find((w) => {
      if (!isAgentSpace(w)) {
        return false;
      }
      if (w.dm_channel_ids.includes(workspaceId)) {
        return true;
      }
      return !!channelId && w.dm_channel_ids.includes(channelId);
    });
    return owningAgentSpace?.id ?? workspaceId;
  };

  const answeredPairIds = directPairsQuery.data ? directPairIds : undefined;

  const directPairAuthority = useMemo<DirectPairAuthority>(
    () =>
      directPairAuthorityFromQuery({
        answered: answeredPairIds,
        isFetching: directPairsQuery.isFetching,
        isError: directPairsQuery.isError,
      }),
    [answeredPairIds, directPairsQuery.isFetching, directPairsQuery.isError],
  );

  const selectedChannel = channels.find((s) => s.id === selectedChannelId) ?? null;

  const parentMembership = useChannelMembership(
    selectedChannel,
    selectedChannelId,
    directConversationId !== null ? { readOnly: directPair?.read_only === true } : null,
  );

  const parentChannelMembership = parentMembership.membership;

  // A compact page slides in as it first paints, so it renders its channel at
  // once; deferring would slide in a placeholder that then swaps.
  const compact = useCompactLayout();
  const deferred = useDeferredValue(selectedChannelId);
  const deferredChannelId = compact ? selectedChannelId : deferred;

  const deferredSelectedChannel = channels.find((s) => s.id === deferredChannelId) ?? null;

  const parentChannelResolving =
    directPairResolving ||
    (deferredChannelId != null && deferredSelectedChannel == null && activeChannelsLoading);

  const conversationRefsById = useMemo(() => {
    // `channels` is the CURRENT WORKSPACE's channel list — that scope is what
    // entitles every consumer to draw these as channels (#task:2077, see
    // {@link WorkspaceChannelReference}). Feeding this map from any other list
    // would mis-label, not fail.
    const m = new Map<string, WorkspaceChannelReference>();
    for (const item of channels) {
      m.set(item.id, {
        id: item.id,
        title: item.title,
      });
    }
    return m;
  }, [channels]);

  const buildChannelDescriptor = useCallback(
    (s: ChannelListItem | null): ConversationPaneDescriptor | null =>
      s && isWorkspacePlaced(s)
        ? {
            id: s.id,
            title: s.title,
            placement_kind: s.placement_kind,
            roster_policy: s.roster_policy,
            closed_at: s.closed_at,
            archived_at: s.archived_at,
            workspace_id: s.workspace_id,
            is_member: s.is_member,
          }
        : null,
    [],
  );

  const deferredParentDescriptor = useMemo(
    () => buildChannelDescriptor(deferredSelectedChannel),
    [buildChannelDescriptor, deferredSelectedChannel],
  );

  const directPairDescriptor = useMemo(
    () =>
      directPair
        ? directPairPaneDescriptor(directPair, presentCounterpart(directPair.counterpart).label)
        : null,
    [directPair],
  );

  const primaryPaneDescriptor = directPairDescriptor ?? deferredParentDescriptor;

  const hasChannelNamespace = workspaceResolved;

  const primaryPaneHasChannelNamespace = directPairDescriptor ? false : hasChannelNamespace;

  // The column reads the CURRENT channel; only the route conversation's own view
  // defers (`primaryPaneDescriptor`), so the panes it owns never show the
  // previous channel's facts.
  const currentDescriptor = useMemo(
    () => directPairDescriptor ?? buildChannelDescriptor(selectedChannel),
    [directPairDescriptor, buildChannelDescriptor, selectedChannel],
  );
  const primaryColumn = useColumnFacts({
    id: selectedChannelId ?? "",
    descriptor: currentDescriptor,
    status: currentDescriptor ? "resolved" : "loading",
    membership: parentMembership,
    hasChannelNamespace: primaryPaneHasChannelNamespace,
    pair: directPair,
    workspaceId: selectedWorkspaceId,
    workspaceNameById,
  });
  const primary = selectedChannelId ? primaryColumn : null;

  // The first conversation pane: the right half of the sidebar's pair row.
  const [firstConversationPane = null] = conversationPanes(panes);
  const paired = useConversationColumn(
    section === "chat" && selectedChannelId ? firstConversationPane : null,
    channels,
    buildChannelDescriptor,
    workspaceNameById,
  );

  return {
    workspaces,
    workspacesLoading,
    visibleWorkspaceIds,
    selectedWorkspace,
    workspaceNameById,
    workspaceRefById,
    workspaceResolved,
    activeChannels,
    archivedChannels,
    channels,
    memberActiveChannels,
    directPairs,
    directPair,
    directPairLoadFailed,
    directPairNotFound,
    primaryPointer,
    directPairAuthority,
    selectedChannel,
    parentMembership,
    parentChannelMembership,
    deferredChannelId,
    parentChannelResolving,
    conversationRefsById,
    primaryPaneDescriptor,
    hasChannelNamespace,
    primary,
    paired,
    buildChannelDescriptor,
    resolveNavWorkspaceId,
    activeSettled: activeChannelsStatus === "success" && activeChannelsFetchStatus === "idle",
    archivedSettled: archivedChannelsStatus === "success" && archivedChannelsFetchStatus === "idle",
  };
}
