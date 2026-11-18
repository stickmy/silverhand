import type { WorkspaceChannelReference } from "@/features/messages/components/conversation-ref";
import type { JumpPhase } from "@/features/channels/message-jump-phase";
import type { ResolvedChannelMembership } from "@/features/channels/use-channel-membership";
import type { MessageJumpTarget } from "@/features/channels/use-message-jump-controller";
import type { ConversationPaneDescriptor } from "@/features/conversations/pane-descriptor";
import type {
  ComposerDirectory,
  ConversationPaneCapabilities,
} from "@/features/conversations/pane-mode";
import type { DirectPairPaneProps } from "@/features/direct-pairs/hooks";
import type { AdvancedSearchSeed } from "@/features/search/advanced-search-seed";
import type { ChannelListItem } from "@/features/workspaces/hooks";
import type {
  SecondaryDescriptor,
  SecondaryViewInputs,
} from "@/features/conversation-shell/model/secondary-pane";
import type { RouteState } from "@/router";
import type { PaneAttachmentRequest } from "./conversation-shell-models";

export interface PaneContentRead {
  workspaceNameById: (wid: string | null) => string | null;
  directPair: DirectPairPaneProps["pair"];
  directPairLoadFailed: boolean;
  directPairNotFound: boolean;
  selectedChannel: ChannelListItem | null;
  parentMembership: ResolvedChannelMembership;
  parentChannelResolving: boolean;
  conversationRefsById: Map<string, WorkspaceChannelReference>;
  selectedChannelDescriptor: ConversationPaneDescriptor | null;
  primaryPaneDescriptor: ConversationPaneDescriptor | null;
  primaryPaneCapabilities: ConversationPaneCapabilities;
  primaryPaneHasChannelNamespace: boolean;
  primaryThreadComposerDirectory: ComposerDirectory;
}

export interface PaneContentState {
  secondaryDescriptor: SecondaryDescriptor | null;
  channelIndexesAvailable: boolean;
  filesListMounted: boolean;
  threadsListMounted: boolean;
  taskPaneId: string | null;
  paneAttachmentRequest: PaneAttachmentRequest | null;
  secondaryViewInputs: SecondaryViewInputs;
  secondaryConversationDescriptor: ConversationPaneDescriptor | null;
  parentJumpTarget: { channelId: string; messageId: string } | null;
  secondaryJumpTarget: null;
  secondaryJumpPrimePending: boolean;
  threadJumpMessageId: string | null;
  parentJumpPrimePending: boolean;
  threadJumpPhase: JumpPhase | null;
}

export interface PaneContentCommands {
  departPrimaryPane: (channelId: string) => void;
  requestJump: (target: MessageJumpTarget) => void;

  handleOpenAdvancedSearch: (seed: AdvancedSearchSeed) => void;
  openTaskInPane: (taskId: string) => void;
  setTaskPaneEditing: (editing: boolean) => void;
  requestCloseTaskPane: () => void;
  setPaneAttachmentRequest: (request: PaneAttachmentRequest | null) => void;
  handleOpenChannelByName: (workspaceId: string, channelId: string) => void;
  handleJumpToMessage: (target: MessageJumpTarget) => boolean;
  closeSecondary: () => void;
  handleMessageAgent: (agentId: string) => void;
  handleDraftOpenedPair: (conversationId: string) => void;
  handleJumped: () => void;
  handleOpenThreadFromChannel: (rootMessageId: string, jumpToMessageId?: string) => void;
  openSide: (id: string) => void;
  toggleThreadsList: () => void;
  toggleFilesList: () => void;
  closeAgentPanel: () => void;
  closeHumanPanel: () => void;
  closeFilesList: () => void;
  openAttachmentPanel: (attachmentId: string, inFiles?: boolean) => void;
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
