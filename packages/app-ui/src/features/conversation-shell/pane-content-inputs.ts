import type { WorkspaceChannelReference } from "@/features/messages/components/conversation-ref";
import type { JumpPhase } from "@/features/channels/message-jump-phase";
import type { MessageJumpTarget } from "@/features/channels/use-message-jump-controller";
import type { ConversationPaneDescriptor } from "@/features/conversations/pane-descriptor";
import type { AdvancedSearchSeed } from "@/features/search/advanced-search-seed";
import type {
  SecondaryDescriptor,
  SecondaryViewInputs,
} from "@/features/conversation-shell/model/secondary-pane";
import type { PaneColumn, RouteState } from "@/router";
import type { ConversationColumn } from "./conversation-column";

export interface PaneContentRead {
  workspaceNameById: (wid: string | null) => string | null;
  directPairLoadFailed: boolean;
  directPairNotFound: boolean;
  parentChannelResolving: boolean;
  conversationRefsById: Map<string, WorkspaceChannelReference>;
  /** What the first column's own view shows: deferred across a channel switch
   *  so the previous channel stays until the next one is ready. */
  primaryPaneDescriptor: ConversationPaneDescriptor | null;
  primary: ConversationColumn | null;
  split: ConversationColumn | null;
  /** The column the right panes resolve against. */
  paneOwner: ConversationColumn | null;
}

export interface PaneContentState {
  secondaryDescriptor: SecondaryDescriptor | null;
  channelIndexesAvailable: boolean;
  filesListMounted: boolean;
  threadsListMounted: boolean;
  taskPaneId: string | null;
  secondaryViewInputs: SecondaryViewInputs;
  secondaryConversationDescriptor: ConversationPaneDescriptor | null;
  parentJumpTarget: { channelId: string; messageId: string } | null;
  secondaryJumpTarget: null;
  secondaryJumpPrimePending: boolean;
  threadJumpMessageId: string | null;
  parentJumpPrimePending: boolean;
  threadJumpPhase: JumpPhase | null;
  splitJumpTarget: { channelId: string; messageId: string } | null;
  splitJumpPrimePending: boolean;
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
  closeSecondary: () => void;
  handleMessageAgent: (agentId: string) => void;
  handleDraftOpenedPair: (conversationId: string) => void;
  handleJumped: () => void;
  handleOpenThreadFromChannel: (
    rootMessageId: string,
    jumpToMessageId?: string,
    column?: PaneColumn,
  ) => void;
  openSide: (id: string) => void;
  toggleThreadsList: (column?: PaneColumn) => void;
  toggleFilesList: (column?: PaneColumn) => void;
  /** Close one column of a split view; the other stays as the only column. */
  closeColumn: (column: PaneColumn) => void;
  closeAgentPanel: () => void;
  closeHumanPanel: () => void;
  closeFilesList: () => void;
  openAttachmentPanel: (attachmentId: string, inFiles?: boolean, column?: PaneColumn) => void;
  closeAttachmentPanel: () => void;
  closeThread: () => void;
  closeThreadsList: () => void;
  openThread: (id: string) => void;
}

export type PaneContentRoute = Pick<
  RouteState,
  | "workspaceId"
  | "channelId"
  | "directDraftTarget"
  | "threadRootId"
  | "agentPanelId"
  | "humanPanelId"
  | "attachmentPanelId"
  | "channelSettingsId"
>;
