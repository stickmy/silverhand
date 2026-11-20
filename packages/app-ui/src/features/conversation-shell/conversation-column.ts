import { useMemo } from "react";
import type { DirectPairListItem } from "@silverhand/shared";
import {
  useChannelMembership,
  type ResolvedChannelMembership,
} from "@/features/channels/use-channel-membership";
import {
  conversationPaneDescriptorFromIdentity,
  useConversationIdentity,
  type ConversationIdentityStatus,
} from "@/features/conversations/conversation-identity";
import type { ConversationPaneDescriptor } from "@/features/conversations/pane-descriptor";
import {
  inheritedComposerDirectory,
  inheritedPaneCapabilities,
  type ComposerDirectory,
  type ConversationPaneCapabilities,
} from "@/features/conversations/pane-mode";
import type { ChannelListItem } from "@/features/workspaces/hooks";
import type { PaneColumn } from "@/navigation/pane-query";

/**
 * One conversation column of the chat stage, resolved: everything a column's
 * own view and the right panes it owns (thread, threads, files, attachment)
 * read about its conversation. Both columns have this shape, so a pane never
 * asks which column it belongs to — it reads the owning column.
 */
export interface ConversationColumn {
  column: PaneColumn;
  /** The conversation id the route names. */
  id: string;
  /** Null while the descriptor is still resolving, or when it never will. */
  descriptor: ConversationPaneDescriptor | null;
  status: ConversationIdentityStatus;
  workspaceId: string | null;
  /** The name a channel-scoped pane shows as its parent; a direct pair has no
   *  `#name`, so it is null there. */
  paneTitle: string | null;
  membership: ResolvedChannelMembership;
  hasChannelNamespace: boolean;
  capabilities: ConversationPaneCapabilities;
  threadComposerDirectory: ComposerDirectory;
  pair: DirectPairListItem | null;
}

/** The derived facts of a column, from its descriptor and membership. */
export function useColumnFacts(input: {
  column: PaneColumn;
  id: string;
  descriptor: ConversationPaneDescriptor | null;
  status: ConversationIdentityStatus;
  membership: ResolvedChannelMembership;
  hasChannelNamespace: boolean;
  pair: DirectPairListItem | null;
  /** The workspace the route names, known before the descriptor resolves. */
  workspaceId?: string | null;
  workspaceNameById: (id: string | null) => string | null;
}): ConversationColumn {
  const { descriptor, hasChannelNamespace, workspaceNameById, workspaceId, ...facts } = input;
  const capabilities = useMemo(() => inheritedPaneCapabilities(descriptor), [descriptor]);
  const threadComposerDirectory = useMemo(
    () =>
      inheritedComposerDirectory(descriptor, {
        workspaceName: workspaceNameById(descriptor?.workspace_id ?? null),
        hasChannelNamespace,
      }),
    [descriptor, workspaceNameById, hasChannelNamespace],
  );

  return {
    ...facts,
    descriptor,
    hasChannelNamespace,
    workspaceId: workspaceId !== undefined ? workspaceId : (descriptor?.workspace_id ?? null),
    paneTitle: descriptor && descriptor.placement_kind !== "direct_pair" ? descriptor.title : null,
    capabilities,
    threadComposerDirectory,
  };
}

/**
 * The second column of a split view. A channel of the open workspace reads
 * from the list already in hand (it carries `is_member`); anything else —
 * another workspace's channel, a direct pair — resolves by identity.
 */
export function useSplitColumn(
  id: string | null,
  channels: readonly ChannelListItem[],
  toDescriptor: (channel: ChannelListItem) => ConversationPaneDescriptor | null,
  workspaceNameById: (id: string | null) => string | null,
): ConversationColumn | null {
  const channel = id ? (channels.find((c) => c.id === id) ?? null) : null;
  const identity = useConversationIdentity(channel ? null : id);
  const descriptor = useMemo<ConversationPaneDescriptor | null>(() => {
    if (channel) return toDescriptor(channel);
    if (id && identity.status === "resolved") {
      return conversationPaneDescriptorFromIdentity(identity.identity);
    }
    return null;
  }, [channel, id, identity, toDescriptor]);
  const isDirect = descriptor?.placement_kind === "direct_pair";
  const membership = useChannelMembership(
    channel ?? descriptor,
    descriptor ? id : null,
    isDirect ? { readOnly: descriptor?.read_only === true } : null,
  );
  const column = useColumnFacts({
    column: "split",
    id: id ?? "",
    descriptor,
    status: descriptor ? "resolved" : identity.status,
    membership,
    hasChannelNamespace: descriptor !== null && descriptor.workspace_id !== null,
    pair:
      identity.status === "resolved" && identity.identity.placement === "direct_pair"
        ? identity.identity.pair
        : null,
    workspaceNameById,
  });

  return id ? column : null;
}
