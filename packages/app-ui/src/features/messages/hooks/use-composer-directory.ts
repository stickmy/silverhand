import { useMemo } from "react";
import type { AgentDto } from "@silverhand/shared";
import type { ShorthandResolver } from "@silverhand/shared/reference-markdown";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { useAgents } from "@/features/agents/hooks/hooks";
import { useUserAvatarObjectUrls } from "@/features/users/avatar-hooks";
import {
  useChannelsAcrossWorkspaces,
  useChannelsForWorkspace,
  useWorkspaceMemberLists,
  useWorkspaces,
  type ChannelListItem,
  type WorkspaceHumanMember,
} from "@/features/workspaces/hooks";
import type { ComposerDirectory, MentionAgent } from "@/features/conversations/pane-mode";
import { useConversationMembers } from "@/features/conversations/use-conversation-members";
import {
  isBackendUnreachable,
  isRegistrationRequired,
  isUpdateRequired,
  useViewerUser,
} from "@/features/session/hooks";
import {
  EMPTY_PICKER,
  buildMentionHumans,
  buildShorthandResolver,
  buildViewerShorthandResolver,
  channelSourceStatusOf,
  filterChannels,
  mergeViewerMentionAgents,
  selectMentionCandidates,
  selectViewerMentionCandidates,
  viewerChannelPickerResult,
  type ComposerChannelSource,
  type ComposerMentionSource,
  type HumanMentionCandidate,
} from "../model/composer-picker";

// Stable empties keep the source memos from re-firing while a query loads.
const EMPTY_AGENTS: AgentDto[] = [];
const EMPTY_HUMANS: WorkspaceHumanMember[] = [];
const EMPTY_CHANNELS: ChannelListItem[] = [];

export interface ComposerDirectorySources {
  /** Read by the picker, the send-time shorthand rewrite and its settle gate,
   *  so a candidate the picker offers is exactly one the send path resolves. */
  mentionSource: ComposerMentionSource;
  channelSource: ComposerChannelSource;
  /** Settles the arm's `@` authority; null when it cannot be had (a failed
   *  refetch, or the withheld arm, which has no source). */
  refetchMentionResolver: () => Promise<ShorthandResolver | null>;
  agentAvatarUrls: Map<string, string | null>;
  humanAvatarUrls: Map<string, string | null>;
}

/**
 * The `@` and `#` candidate sources for one composer, by directory arm. Each
 * arm's queries are `enabled`-gated on the arm, so the other arms start none
 * and leak no candidates; while a query loads, its arm fails closed to empty,
 * never to a global directory.
 */
export function useComposerDirectory(
  directory: ComposerDirectory,
  conversationId: string,
): ComposerDirectorySources {
  const isWorkspaceArm = directory.kind === "workspace";
  const isViewerArm = directory.kind === "viewer";
  const counterpartAgent = directory.kind === "viewer" ? directory.counterpartAgent : null;
  const armWorkspaceId = directory.kind === "workspace" ? directory.workspaceId : null;
  const armWorkspaceName = directory.kind === "workspace" ? directory.workspaceName : null;
  const armHasChannelNamespace =
    directory.kind === "workspace" ? directory.hasChannelNamespace : false;

  const memberLists = useWorkspaceMemberLists(armWorkspaceId);
  const agents = memberLists.data?.agents ?? EMPTY_AGENTS;
  const workspaceHumans = memberLists.data?.humans ?? EMPTY_HUMANS;
  const conversationMembers = useConversationMembers(isWorkspaceArm ? conversationId : null);
  const { data: workspaceChannels = EMPTY_CHANNELS, status: workspaceChannelsStatus } =
    useChannelsForWorkspace(
      armWorkspaceId,
      // A channel namespace only exists for a resolved workspace placement.
      armHasChannelNamespace,
    );

  // A DM offers the counterpart agent, even one the viewer does not own, with
  // the viewer's owned agents. The backend narrows ownership.
  const ownedAgentsQuery = useAgents({ scope: "owned", enabled: isViewerArm });
  const ownedAgents = ownedAgentsQuery.data ?? EMPTY_AGENTS;
  const workspacesQuery = useWorkspaces({ enabled: isViewerArm });
  const viewerWorkspaceIds = useMemo(
    () => (isViewerArm ? (workspacesQuery.data ?? []).map((w) => w.id) : []),
    [isViewerArm, workspacesQuery.data],
  );
  const viewerChannelSlices = useChannelsAcrossWorkspaces(viewerWorkspaceIds);

  const viewerQuery = useViewerUser();

  const viewerId =
    viewerQuery.data &&
    !isRegistrationRequired(viewerQuery.data) &&
    !isBackendUnreachable(viewerQuery.data) &&
    !isUpdateRequired(viewerQuery.data)
      ? viewerQuery.data.id
      : null;
  const viewerMentionAgents = useMemo<MentionAgent[]>(
    () => (isViewerArm ? mergeViewerMentionAgents(counterpartAgent, ownedAgents) : EMPTY_AGENTS),
    [isViewerArm, counterpartAgent, ownedAgents],
  );
  // Cross-workspace channel candidates name their own workspace.
  const workspaceNameById = useMemo(() => {
    const names = new Map<string, string>();
    for (const workspace of workspacesQuery.data ?? []) {
      names.set(workspace.id, workspace.name);
    }
    return names;
  }, [workspacesQuery.data]);
  const rosterResolved = conversationMembers.status === "resolved";
  const { agentMemberIds, humanMemberIds } = useMemo(() => {
    const agentIds = new Set<string>();
    const humanIds = new Set<string>();
    if (conversationMembers.status === "resolved") {
      for (const member of conversationMembers.members) {
        if (member.kind === "agent") {
          agentIds.add(member.agent_id);
        } else if (member.kind === "human") {
          humanIds.add(member.user_id);
        }
      }
    }
    return { agentMemberIds: agentIds, humanMemberIds: humanIds };
  }, [conversationMembers.status, conversationMembers.members]);
  // Avatars warm before a row renders; prefetch is not gated on the roster.
  const mentionHumans = useMemo<HumanMentionCandidate[]>(
    () => buildMentionHumans(workspaceHumans, viewerId),
    [workspaceHumans, viewerId],
  );
  const agentAvatarUrls = useAgentAvatarObjectUrls(isViewerArm ? viewerMentionAgents : agents);
  const humanAvatarUrls = useUserAvatarObjectUrls(mentionHumans);

  // `sourceKnown`: withheld is undetermined, so a typed `@handle` stays pending;
  // a workspace roster that has answered once is authoritative; a settled-empty
  // owned-agents list is still known.
  const mentionSource = useMemo<ComposerMentionSource>(() => {
    switch (directory.kind) {
      case "workspace":
        return {
          sourceKnown: memberLists.data !== undefined,
          select: (query) =>
            selectMentionCandidates(
              memberLists.data,
              rosterResolved,
              agentMemberIds,
              humanMemberIds,
              viewerId,
              query,
            ),
          resolveShorthand: buildShorthandResolver(agents, workspaceHumans),
        };
      case "viewer":
        return {
          sourceKnown: ownedAgentsQuery.isSuccess,
          select: (query) =>
            selectViewerMentionCandidates(viewerMentionAgents, counterpartAgent?.id ?? null, query),
          resolveShorthand: buildViewerShorthandResolver(viewerMentionAgents),
        };
      case "withheld":
        return {
          sourceKnown: false,
          select: () => EMPTY_PICKER,
          resolveShorthand: () => null,
        };
    }
  }, [
    directory.kind,
    memberLists.data,
    rosterResolved,
    agentMemberIds,
    humanMemberIds,
    viewerId,
    agents,
    workspaceHumans,
    viewerMentionAgents,
    counterpartAgent,
    ownedAgentsQuery.isSuccess,
  ]);
  const channelSource = useMemo<ComposerChannelSource>(() => {
    switch (directory.kind) {
      case "workspace":
        return {
          offersChannels: armHasChannelNamespace,
          select: (query) => ({
            ...filterChannels(workspaceChannels, armWorkspaceName, query),
            status: channelSourceStatusOf(workspaceChannelsStatus),
          }),
        };
      case "viewer":
        return {
          offersChannels: true,
          select: (query) =>
            viewerChannelPickerResult(
              viewerChannelSlices,
              workspacesQuery.status,
              workspaceNameById,
              query,
            ),
        };
      case "withheld":
        return { offersChannels: false, select: () => ({ ...EMPTY_PICKER, status: "complete" }) };
    }
  }, [
    directory.kind,
    armHasChannelNamespace,
    armWorkspaceName,
    workspaceChannels,
    workspaceChannelsStatus,
    viewerChannelSlices,
    workspacesQuery.status,
    workspaceNameById,
  ]);

  async function refetchMentionResolver(): Promise<ShorthandResolver | null> {
    if (directory.kind === "workspace") {
      const roster = (await memberLists.refetch()).data;
      return roster ? buildShorthandResolver(roster.agents, roster.humans) : null;
    }
    if (directory.kind === "viewer") {
      const owned = (await ownedAgentsQuery.refetch()).data;
      if (!owned) {
        return null;
      }
      return buildViewerShorthandResolver(
        mergeViewerMentionAgents(directory.counterpartAgent, owned),
      );
    }
    return null;
  }

  return {
    mentionSource,
    channelSource,
    refetchMentionResolver,
    agentAvatarUrls,
    humanAvatarUrls,
  };
}
