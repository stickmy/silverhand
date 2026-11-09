// The public import surface for channel data hooks; each concern lives in its own module.
export {
  applySendSuccess,
  prepareSendMessageBody,
  uploadContentAsTextAttachment,
  useSendMessage,
  type PreparedSendBody,
  type SendMessageInput,
} from "./send-message";
export {
  useClearAgentSession,
  useRemoveAgentFromChannel,
  useRemoveHumanFromChannel,
  useStartAgent,
  useStopAgent,
} from "./agent-control";
export {
  messagesKey,
  clusterMessagesKey,
  clusterFetchEpochKey,
  threadRootKey,
  mainlineLiveGapKey,
  clusterLiveGapKey,
} from "@/features/channels/message-cache-core";
export type { MessagesPageParam } from "@/features/channels/message-cache-core";
export {
  invalidateMessageUnreadQueries,
  messageUnreadByConversationKey,
  messageUnreadSummaryKey,
  patchMessageUnreadByConversation,
  patchMessageUnreadSummary,
  removeWorkspaceFromMessageUnread,
  useChannelUnreadCounts,
  useMessageUnreadCounts,
  useMessageUnreadSummary,
  conversationUnreadKey,
  useConversationUnread,
} from "@/features/channels/message-unread";
export {
  clearPendingAgentStatusPatches,
  retainAgentStatusBuffer,
  releaseAgentStatusBuffer,
  mergePendingAgentStatusPatches,
  hasActiveChannelAgentRuntimeReader,
  applyAgentStatusToCache,
  reconcileChannelRuntimeOnSnapshotRequired,
} from "@/features/channels/agent-status-cache";
export type { ConversationAgentStatusPatch } from "@/features/channels/agent-status-cache";
export {
  applyTaskChangedToMessage,
  applyReactionToMessage,
  patchMessageInActiveProjections,
  patchWarmInactiveMessageRow,
  setMainlineLiveGap,
  hasActiveMainlineReader,
  applyMainlineMessageToChannelCaches,
  reconcileChannelMainlineOnSnapshotRequired,
  setClusterLiveGap,
  openClusterReaderRoots,
  openThreadRootReaderRoots,
  openThreadFollowerReaderRoots,
  warmInactiveClusterRoots,
  removeInactiveClusterCache,
  hasWarmInactiveMainline,
  removeInactiveMainlineCache,
  reconcileThreadRootOnSnapshotRequired,
  applyClusterMessageToCaches,
  reconcileClusterOnSnapshotRequired,
  applyThreadRootReplyBump,
  mergeThreadRootReplyCount,
  mergeMainlineReplyCount,
  mergeMainlineReplyPreview,
  applyThreadFollowersMessage,
  invalidateThreadFollowers,
} from "@/features/channels/projection-cache";
export {
  useChannelMessages,
  useThreadCluster,
  useThreadRoot,
  usePrimeConversationMessages,
} from "./message-window";
export { useThreadFollowers, useFollowThread, useUnfollowThread } from "./thread-follow";
export { useChannelAgents, useChannelAgentRuntime, useAgentRunStates } from "./agent-runtime";
export {
  useMarkConversationNotificationsRead,
  useMarkMessageChannelRead,
  useMarkThreadRead,
} from "./mark-read";
