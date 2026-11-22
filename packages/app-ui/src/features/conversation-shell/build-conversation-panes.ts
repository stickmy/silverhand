import { hostsViewTabs } from "./model/view-tabs";
import { openConversationTab } from "@/features/conversations/use-conversation-tabs";
import { mountsMemberOnlyPanel } from "@/features/channels/membership";
import type { AttachmentSnapshot } from "@silverhand/shared";
import { directPairSenderSeed } from "@/features/search/advanced-search-seed";
import { paneKey, paneOwner, type Pane } from "@/navigation/panes";
import type {
  AttachedPaneModel,
  AttachmentPaneProps,
  PrimaryPaneModel,
  ThreadDetailModel,
} from "./pane-render-models";
import type { ConversationColumn } from "./conversation-column";
import type {
  PaneContentRoute,
  PaneContentRead,
  PaneContentState,
  PaneContentCommands,
} from "./pane-content-inputs";

export interface PaneContext {
  route: PaneContentRoute;
  read: PaneContentRead;
  state: PaneContentState;
  commands: PaneContentCommands;
}

/** The route's own conversation, first on the stage. */
export function buildPrimaryPane(ctx: PaneContext): PrimaryPaneModel {
  const { route, read, state, commands } = ctx;
  if (route.directDraftTarget) {
    return {
      kind: "draft",
      props: { target: route.directDraftTarget, onOpenPair: commands.handleDraftOpenedPair },
    };
  }
  const id = route.channelId;
  const indexes = state.channelIndexesAvailable && read.primaryPaneDescriptor != null && id;
  return {
    kind: "conversation",
    props: {
      ...conversationViewProps(id ?? "", ctx),
      hasChannelNamespace: read.primary?.hasChannelNamespace ?? false,
      workspaceName: read.workspaceNameById(read.primaryPaneDescriptor?.workspace_id ?? null),
      conversation: read.primaryPaneDescriptor,
      resolving: read.parentChannelResolving,
      loadFailed: read.directPairLoadFailed,
      notFound: read.directPairNotFound,
      jumpTarget: state.parentJumpTarget,
      jumpPrimePending: state.parentJumpPrimePending,
      onToggleThreadsList: indexes ? () => commands.toggleThreadsList(id) : undefined,
      onToggleFilesList: indexes ? () => commands.toggleFilesList(id) : undefined,
    },
  };
}

/** What a conversation's view takes the same way wherever it stands: where its
 *  threads and attachments open, and whether its Threads or Files pane is open. */
function conversationViewProps(id: string, ctx: PaneContext) {
  const { route, read, commands } = ctx;
  const open = (kind: "threads" | "files") =>
    route.panes.some((p) => p.kind === kind && p.of === id);
  return {
    parentTitle: null,
    onJumped: commands.handleJumped,
    onOpenThread: (rootId: string, jumpTo?: string) =>
      commands.handleOpenThreadFromChannel(rootId, jumpTo, id),
    threadsListActive: open("threads"),
    filesListActive: open("files"),
    conversationRefsById: read.conversationRefsById,
    onOpenConversationRef: commands.openConversationRef,
    onOpenChannelRef: commands.handleOpenChannelByName,
    onOpenTask: commands.openTaskInPane,
    onJumpToMessage: commands.handleJumpToMessage,
    onOpenAdvancedSearch: commands.handleOpenAdvancedSearch,
    viewTabsEnabled: true,
    onOpenAttachmentPanel: (attachmentId: string) => commands.openAttachmentPanel(attachmentId, id),
  };
}

/** The main-stream jump aimed at conversation `id`, if any. */
function streamJump(id: string, state: PaneContentState) {
  const target = state.jump.jumpTarget;
  if (!target || target.threadRootId || target.channelId !== id) {
    return { jumpTarget: null, jumpPrimePending: false };
  }
  return {
    jumpTarget: { channelId: target.channelId, messageId: target.messageId },
    jumpPrimePending: state.jump.isPrimePendingFor(target.channelId, target.messageId),
  };
}

/**
 * One attached pane. `owner` is the conversation the pane belongs to,
 * resolved by the caller; null for a pane that belongs to none.
 */
export function buildAttachedPane(
  pane: Pane,
  owner: ConversationColumn | null,
  ctx: PaneContext,
): AttachedPaneModel {
  const { route, read, state, commands } = ctx;
  const key = paneKey(pane);
  const close = () => commands.closePane(key);
  const back = () => commands.backPane(key);
  if (paneOwner(pane) !== undefined && !owner) return { kind: "none" };

  switch (pane.kind) {
    case "agent":
      return {
        kind: "agent",
        props: {
          agentId: pane.id,
          contextConversationId: route.channelId,
          contextWorkspaceId: route.workspaceId ?? null,
          onClose: close,
          onDeleted: close,
          onMessageAgent: commands.handleMessageAgent,
        },
      };
    case "human":
      return { kind: "human", props: { userId: pane.id, onClose: close } };
    case "task":
      if (state.taskPaneId !== pane.id) return { kind: "none" };
      return {
        kind: "task",
        props: {
          taskId: pane.id,
          onClose: commands.requestCloseTaskPane,
          onEditingChange: commands.setTaskPaneEditing,
        },
      };
    case "settings":
      return {
        kind: "settings",
        props: {
          channelId: pane.of,
          page: pane.page,
          onDepartChannel: commands.departPrimaryPane,
          onClose: close,
          onBack: pane.page ? back : undefined,
          onOpenMembers: (add) => commands.openChannelMembers(pane.of, add),
        },
      };
  }

  // Threads and Files are member panels of their conversation.
  if (!owner || !mountsMemberOnlyPanel(true, owner.membership.membership)) {
    return { kind: "none" };
  }

  if (pane.kind === "files") {
    return {
      kind: "files",
      props: {
        channelId: owner.id,
        workspaceId: owner.workspaceId,
        parentTitle: owner.paneTitle,
        onClose: close,
        onJumpToMessage: commands.handleJumpToMessage,
        onOpenAttachment: (attachment) => commands.openAttachmentPanel(attachment.id, owner.id),
      },
      preview: pane.attachment
        ? attachmentPaneProps(pane.attachment, owner, { onClose: close, onBack: back })
        : null,
    };
  }

  return {
    kind: "threads",
    props: {
      channelId: owner.id,
      parentTitle: owner.paneTitle,
      onClose: close,
      onOpenThread: (rootId) => commands.openThread(rootId, owner.id),
    },
    thread: pane.thread ? threadDetail(pane.thread, owner, ctx, { close, back }) : null,
  };
}

function threadDetail(
  threadRoot: string,
  owner: ConversationColumn,
  { read, state, commands }: PaneContext,
  { close, back }: { close: () => void; back: () => void },
): ThreadDetailModel {
  const target = state.jump.jumpTarget;
  const jumpMessageId =
    target && target.threadRootId === threadRoot && target.channelId === owner.id
      ? target.messageId
      : null;
  return {
    capabilities: owner.capabilities,
    props: {
      channelId: owner.id,
      workspaceId: owner.workspaceId,
      directory: owner.threadComposerDirectory,
      rootMessageId: threadRoot,
      onOpenAttachment: (attachment) => commands.openAttachmentPanel(attachment.id, owner.id),
      onExpandAttachment: expandIntoConversation(owner),
      parentTitle: owner.paneTitle,
      parentCounterpart: owner.descriptor?.counterpart ?? null,
      closed: owner.descriptor?.closed_at != null,
      archived: owner.descriptor?.archived_at != null,
      gate: owner.membership.gate,
      onClose: close,
      onBack: back,
      onViewInChannel: () => {
        // Scroll-locate + flash the root in its conversation's stream, then
        // step the pane back to the list so the stream is unobstructed.
        commands.requestJump({
          workspaceId: owner.workspaceId ?? undefined,
          channelId: owner.id,
          messageId: threadRoot,
        });
        back();
      },
      conversationRefsById: read.conversationRefsById,
      onOpenConversationRef: commands.openConversationRef,
      onOpenChannelRef: commands.handleOpenChannelByName,
      onOpenTask: commands.openTaskInPane,
      jumpMessageId,
      jumpPhase: jumpMessageId ? state.jump.primePhaseFor(owner.id, jumpMessageId) : null,
      onJumped: commands.handleJumped,
      onJumpToMessage: commands.handleJumpToMessage,
      onOpenAdvancedSearch: commands.handleOpenAdvancedSearch,
      advancedSearchSender: directPairSenderSeed(owner.pair?.counterpart),
    },
  };
}

/** Expanding hands the preview to its conversation's tab and closes it here,
 * so one attachment is never open in both places. */
function attachmentPaneProps(
  attachmentId: string,
  owner: ConversationColumn,
  exits: Pick<AttachmentPaneProps, "onClose" | "onBack">,
): AttachmentPaneProps {
  const expand = expandIntoConversation(owner);
  return {
    attachmentId,
    ...exits,
    onExpand: expand
      ? (attachment) => {
          expand(attachment);
          exits.onBack?.();
        }
      : undefined,
  };
}

/** Opens the attachment as a tab of `owner`, offered only while that
 * conversation has its tab strip. */
function expandIntoConversation(
  owner: ConversationColumn,
): ((attachment: AttachmentSnapshot) => void) | undefined {
  if (!owner.descriptor) return undefined;
  if (!hostsViewTabs(owner.descriptor, owner.membership.gate.allowMemberEffects)) {
    return undefined;
  }
  return (attachment) =>
    openConversationTab(owner.id, {
      kind: "attachment",
      attachmentId: attachment.id,
      filename: attachment.filename,
      contentType: attachment.content_type,
    });
}
