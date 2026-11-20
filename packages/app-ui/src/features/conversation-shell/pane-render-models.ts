import type { HumanView } from "@/features/profile/human-view";
import type { ComponentProps } from "react";
import type { DirectPairDraftPane } from "@/features/direct-pairs/direct-pair-draft-pane";
import type { ChannelView } from "@/features/conversation-shell/components/channel-view";
import type { AgentView } from "@/features/agents/components/agent-view";
import type { AttachmentSidePanel } from "@/features/conversation-shell/components/attachment-side-panel";
import type { ChannelAttachmentsPanel } from "@/features/conversation-shell/components/channel-attachments-panel";
import type { TaskDetailPanel } from "@/features/tasks/components/task-detail-panel";
import type { SideUnresolvedPlaceholder } from "@/features/conversation-shell/components/side-unresolved-placeholder";
import type { ThreadPanel } from "@/features/conversation-shell/components/thread-panel";
import type { ChannelThreadsPanel } from "@/features/conversation-shell/components/channel-threads-panel";
import type { ConversationPaneCapabilities } from "@/features/conversations/pane-mode";
import type { ChannelSettingsPanel } from "@/features/conversation-shell/components/channel-settings-panel";
export type DraftPaneProps = Pick<
  ComponentProps<typeof DirectPairDraftPane>,
  "target" | "onOpenPair"
>;

export type PrimaryPaneProps = Pick<
  ComponentProps<typeof ChannelView>,
  | "hasChannelNamespace"
  | "workspaceName"
  | "conversation"
  | "resolving"
  | "loadFailed"
  | "notFound"
  | "parentTitle"
  | "jumpTarget"
  | "jumpPrimePending"
  | "onJumped"
  | "onOpenThread"
  | "onToggleThreadsList"
  | "threadsListActive"
  | "onToggleFilesList"
  | "filesListActive"
  | "conversationRefsById"
  | "onOpenConversationRef"
  | "onOpenChannelRef"
  | "onOpenTask"
  | "onJumpToMessage"
  | "onOpenAdvancedSearch"
  | "viewTabsEnabled"
  | "onOpenAttachmentPanel"
  | "onCloseChild"
  | "closeLabel"
  | "attached"
>;

export type AgentPaneProps = Pick<
  ComponentProps<typeof AgentView>,
  | "agentId"
  | "contextConversationId"
  | "contextWorkspaceId"
  | "onClose"
  | "onDeleted"
  | "onMessageAgent"
>;

export type FilesPaneProps = Pick<
  ComponentProps<typeof ChannelAttachmentsPanel>,
  "channelId" | "workspaceId" | "parentTitle" | "onClose" | "onJumpToMessage" | "onOpenAttachment"
>;

export type AttachmentPaneProps = Pick<
  ComponentProps<typeof AttachmentSidePanel>,
  "attachmentId" | "onClose" | "onBack" | "onExpand"
>;

export type TaskPaneProps = Pick<
  ComponentProps<typeof TaskDetailPanel>,
  "taskId" | "onClose" | "onEditingChange"
>;

export type UnresolvedPaneProps = Pick<
  ComponentProps<typeof SideUnresolvedPlaceholder>,
  "conversationId" | "status" | "onClose"
>;

export type ThreadPaneProps = Pick<
  ComponentProps<typeof ThreadPanel>,
  | "channelId"
  | "workspaceId"
  | "directory"
  | "rootMessageId"
  | "onOpenAttachment"
  | "onExpandAttachment"
  | "parentTitle"
  | "parentCounterpart"
  | "closed"
  | "archived"
  | "gate"
  | "onClose"
  | "onBack"
  | "onViewInChannel"
  | "conversationRefsById"
  | "onOpenConversationRef"
  | "onOpenChannelRef"
  | "onOpenTask"
  | "jumpMessageId"
  | "jumpPhase"
  | "onJumped"
  | "onJumpToMessage"
  | "onOpenAdvancedSearch"
  | "advancedSearchSender"
>;

export type ThreadsPaneProps = Pick<
  ComponentProps<typeof ChannelThreadsPanel>,
  "channelId" | "parentTitle" | "onClose" | "onOpenThread"
>;
export type PrimaryPaneModel =
  | { kind: "draft"; props: DraftPaneProps }
  | { kind: "conversation"; props: PrimaryPaneProps };

export type SettingsPaneProps = Pick<
  ComponentProps<typeof ChannelSettingsPanel>,
  "channelId" | "page" | "onDepartChannel" | "onClose" | "onBack" | "onOpenMembers"
>;

/** One attached pane, resolved against the conversation it belongs to. */
export type AttachedPaneModel =
  /** Its conversation is still resolving, or the viewer may not open it there. */
  | { kind: "none" }
  | { kind: "settings"; props: SettingsPaneProps }
  | { kind: "agent"; props: AgentPaneProps }
  | { kind: "human"; props: Pick<ComponentProps<typeof HumanView>, "userId" | "onClose"> }
  | { kind: "files"; props: FilesPaneProps; preview: AttachmentPaneProps | null }
  | { kind: "task"; props: TaskPaneProps }
  | { kind: "conversation"; status: "resolved"; props: PrimaryPaneProps }
  | { kind: "conversation"; status: "unresolved"; props: UnresolvedPaneProps }
  | { kind: "thread"; props: ThreadPaneProps; capabilities: ConversationPaneCapabilities }
  | { kind: "threads"; props: ThreadsPaneProps };
