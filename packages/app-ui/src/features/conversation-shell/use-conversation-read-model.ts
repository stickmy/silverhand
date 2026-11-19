import type { WorkspaceChannelReference } from "@/features/messages/components/conversation-ref";
import { isMemberChannel } from "@/features/channels/membership";
import { useChannelMembership } from "@/features/channels/use-channel-membership";
import { locatorWorkspaceName } from "@/features/conversations/locator-source";
import {
  directPairPaneDescriptor,
  type ConversationPaneDescriptor,
} from "@/features/conversations/pane-descriptor";
import {
  conversationPaneDescriptorFromIdentity,
  useConversationIdentity,
  type ConversationIdentityStatus,
} from "@/features/conversations/conversation-identity";
import {
  inheritedComposerDirectory,
  inheritedPaneCapabilities,
} from "@/features/conversations/pane-mode";
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
} from "@/features/workspaces/hooks";
import { isAgentSpace } from "@/features/workspaces/workspace-ownership";
import {
  directPairAuthorityFromQuery,
  type DirectPairAuthority,
} from "@/features/app-shell/chat-place-restore";
import { type ConversationPointer } from "@/features/conversation-shell/model/secondary-pane";
import { useRouteState } from "@/router";
import { isWorkspacePlaced } from "@silverhand/shared";
import { useCallback, useDeferredValue, useMemo } from "react";
import { useCompactLayout } from "@/navigation/layout";

/** Carrier authority only; navigation and read-cursor writes belong to the coordinators. */
export function useConversationReadModel() {
  const {
    section,
    workspaceId: selectedWorkspaceId,
    channelId: selectedChannelId,
    directConversationId,
    splitConversationId,
    paneColumn,
  } = useRouteState();

  const { data: workspaces = [], isLoading: workspacesLoading } = useWorkspaces();

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
    data: activeChannels = [] as ChannelListItem[],
    isLoading: activeChannelsLoading,
    status: activeChannelsStatus,
    fetchStatus: activeChannelsFetchStatus,
  } = useChannelsForWorkspace(selectedWorkspaceId, workspaceResolved);

  const {
    data: archivedChannels = [] as ChannelListItem[],
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

  const selectedChannelDescriptor = useMemo(
    () => buildChannelDescriptor(selectedChannel),
    [buildChannelDescriptor, selectedChannel],
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

  const primaryPaneCapabilities = useMemo(
    () => inheritedPaneCapabilities(primaryPaneDescriptor),
    [primaryPaneDescriptor],
  );

  const hasChannelNamespace = workspaceResolved;

  const primaryPaneHasChannelNamespace = directPairDescriptor ? false : hasChannelNamespace;

  // The second column of a split view (`?split`). A channel of this workspace
  // reads from the list already in hand (it carries `is_member`); anything else
  // — another workspace's channel, a direct pair — resolves by identity.
  const splitId = section === "chat" && selectedChannelId ? splitConversationId : null;
  const splitChannel = splitId ? (channels.find((c) => c.id === splitId) ?? null) : null;
  const splitIdentity = useConversationIdentity(splitChannel ? null : splitId);
  const splitDescriptor = useMemo<ConversationPaneDescriptor | null>(() => {
    if (splitChannel) return buildChannelDescriptor(splitChannel);
    if (splitId && splitIdentity.status === "resolved") {
      return conversationPaneDescriptorFromIdentity(splitIdentity.identity);
    }
    return null;
  }, [splitChannel, splitId, splitIdentity, buildChannelDescriptor]);
  const splitStatus: ConversationIdentityStatus | null =
    splitId === null ? null : splitDescriptor ? "resolved" : splitIdentity.status;
  const splitIsDirect = splitDescriptor?.placement_kind === "direct_pair";
  const splitMembership = useChannelMembership(
    splitChannel ?? splitDescriptor,
    splitDescriptor ? splitId : null,
    splitIsDirect ? { readOnly: splitDescriptor?.read_only === true } : null,
  );
  const splitHasChannelNamespace =
    splitDescriptor !== null && splitDescriptor.workspace_id !== null;
  const splitCapabilities = useMemo(
    () => inheritedPaneCapabilities(splitDescriptor),
    [splitDescriptor],
  );
  const splitThreadComposerDirectory = useMemo(
    () =>
      inheritedComposerDirectory(splitDescriptor, {
        workspaceName: workspaceNameById(splitDescriptor?.workspace_id ?? null),
        hasChannelNamespace: splitHasChannelNamespace,
      }),
    [splitDescriptor, workspaceNameById, splitHasChannelNamespace],
  );
  const splitPair =
    splitIdentity.status === "resolved" && splitIdentity.identity.placement === "direct_pair"
      ? splitIdentity.identity.pair
      : null;
  // The column that owns the channel-scoped right panes. Falls back to the
  // first column until the second one resolves.
  const contextIsSplit = paneColumn === "split" && splitDescriptor !== null;

  const primaryThreadComposerDirectory = useMemo(
    () =>
      inheritedComposerDirectory(primaryPaneDescriptor, {
        workspaceName: workspaceNameById(primaryPaneDescriptor?.workspace_id ?? null),
        hasChannelNamespace: primaryPaneHasChannelNamespace,
      }),
    [primaryPaneDescriptor, workspaceNameById, primaryPaneHasChannelNamespace],
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
    selectedChannelDescriptor,
    primaryPaneDescriptor,
    primaryPaneCapabilities,
    hasChannelNamespace,
    primaryPaneHasChannelNamespace,
    primaryThreadComposerDirectory,
    split: {
      id: splitId,
      status: splitStatus,
      channel: splitChannel,
      descriptor: splitDescriptor,
      pair: splitPair,
      membership: splitMembership,
      hasChannelNamespace: splitHasChannelNamespace,
      capabilities: splitCapabilities,
      threadComposerDirectory: splitThreadComposerDirectory,
    },
    paneContext: contextIsSplit
      ? {
          column: "split" as const,
          channelId: splitId,
          workspaceId: splitDescriptor?.workspace_id ?? null,
          membership: splitMembership.membership,
        }
      : {
          column: "primary" as const,
          channelId: selectedChannelId,
          workspaceId: selectedWorkspaceId,
          membership: parentChannelMembership,
        },
    resolveNavWorkspaceId,
    activeSettled: activeChannelsStatus === "success" && activeChannelsFetchStatus === "idle",
    archivedSettled: archivedChannelsStatus === "success" && archivedChannelsFetchStatus === "idle",
  };
}
