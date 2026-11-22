import { useMemo } from "react";
import type { DirectPairListItem } from "@silverhand/shared";
import type { ResolvedChannelMembership } from "@/features/channels/use-channel-membership";
import type { ConversationIdentityStatus } from "@/features/conversations/conversation-identity";
import type { ConversationPaneDescriptor } from "@/features/conversations/pane-descriptor";
import {
  inheritedComposerDirectory,
  inheritedPaneCapabilities,
  type ComposerDirectory,
  type ConversationPaneCapabilities,
} from "@/features/conversations/pane-mode";

/**
 * One conversation on the stage, resolved: everything its own view and the
 * panes it owns (Threads, Files, Settings) read about it. The route's
 * conversation has this shape, so a pane never asks where its conversation
 * stands — it reads the owning one.
 */
export interface ConversationColumn {
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
