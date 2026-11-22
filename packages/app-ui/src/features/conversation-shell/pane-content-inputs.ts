import type { WorkspaceChannelReference } from "@/features/messages/components/conversation-ref";
import type { MessageJumpTarget } from "@/features/channels/use-message-jump-controller";
import type { ConversationPaneDescriptor } from "@/features/conversations/pane-descriptor";
import type { AdvancedSearchSeed } from "@/features/search/advanced-search-seed";
import type { RouteState } from "@/router";
import type { ConversationColumn } from "./conversation-column";
import type { PaneJumpModel } from "./conversation-shell-models";

export interface PaneContentRead {
  workspaceNameById: (wid: string | null) => string | null;
  directPairLoadFailed: boolean;
  directPairNotFound: boolean;
  parentChannelResolving: boolean;
  conversationRefsById: Map<string, WorkspaceChannelReference>;
  /** What the route's own view shows: deferred across a channel switch so the
   *  previous channel stays until the next one is ready. */
  primaryPaneDescriptor: ConversationPaneDescriptor | null;
  /** The route's conversation; null with none open. */
  primary: ConversationColumn | null;
}

export interface PaneContentState {
  channelIndexesAvailable: boolean;
  taskPaneId: string | null;
  /** The route conversation's main-stream jump. */
  parentJumpTarget: { channelId: string; messageId: string } | null;
  parentJumpPrimePending: boolean;
  jump: Pick<PaneJumpModel, "jumpTarget" | "isPrimePendingFor" | "primePhaseFor">;
}

export interface PaneContentCommands {
  departPrimaryPane: (channelId: string) => void;
  requestJump: (target: MessageJumpTarget) => void;
  handleOpenAdvancedSearch: (seed: AdvancedSearchSeed) => void;
  openTaskInPane: (taskId: string) => void;
  setTaskPaneEditing: (editing: boolean) => void;
  requestCloseTaskPane: () => void;
  handleOpenChannelByName: (workspaceId: string, channelId: string) => void;
  handleJumpToMessage: (target: MessageJumpTarget) => boolean;
  handleMessageAgent: (agentId: string) => void;
  handleDraftOpenedPair: (conversationId: string) => void;
  handleJumped: () => void;
  /** Open a thread of conversation `of` from its stream. */
  handleOpenThreadFromChannel: (
    rootMessageId: string,
    jumpToMessageId: string | undefined,
    of: string,
  ) => void;
  /** Open a `#conv:` reference's conversation. */
  openConversationRef: (id: string) => void;
  openThread: (rootMessageId: string, of: string) => void;
  toggleThreadsList: (of: string) => void;
  toggleFilesList: (of: string) => void;
  openAttachmentPanel: (attachmentId: string, of: string) => void;
  openChannelMembers: (of: string, add?: boolean) => void;
  /** Close the pane with `key`. */
  closePane: (key: string) => void;
  /** Move the pane with `key` to `index` in the row. */
  movePane: (key: string, index: number) => void;
  /** Step the pane with `key` back one page. */
  backPane: (key: string) => void;
}

export type PaneContentRoute = Pick<
  RouteState,
  "workspaceId" | "channelId" | "directDraftTarget" | "panes"
>;
