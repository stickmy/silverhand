import { selectSecondarySurface } from "./model/secondary-surface";
import { hostsViewTabs } from "./model/view-tabs";
import { openConversationTab } from "@/features/conversations/use-conversation-tabs";
import type { AttachmentSnapshot } from "@silverhand/shared";
import { directPairSenderSeed } from "@/features/search/advanced-search-seed";
import type {
  AttachmentPaneProps,
  ConversationPanesModel,
  SecondaryPaneModel,
} from "./pane-render-models";
import type {
  PaneContentRoute,
  PaneContentRead,
  PaneContentState,
  PaneContentCommands,
} from "./pane-content-inputs";
import { t } from "@/lib/i18n";

export function buildConversationPanes(
  route: PaneContentRoute,
  read: PaneContentRead,
  state: PaneContentState,
  commands: PaneContentCommands,
): ConversationPanesModel {
  return {
    primary: route.directDraftTarget
      ? {
          kind: "draft",
          props: {
            target: route.directDraftTarget,
            onOpenPair: commands.handleDraftOpenedPair,
          },
        }
      : {
          kind: "conversation",
          props: {
            hasChannelNamespace: read.primaryPaneHasChannelNamespace,
            workspaceName: read.workspaceNameById(read.primaryPaneDescriptor?.workspace_id ?? null),
            conversation: read.primaryPaneDescriptor,
            resolving: read.parentChannelResolving,
            loadFailed: read.directPairLoadFailed,
            notFound: read.directPairNotFound,
            parentTitle: null,
            jumpTarget: state.parentJumpTarget,
            jumpPrimePending: state.parentJumpPrimePending,
            onJumped: commands.handleJumped,
            onOpenThread: commands.handleOpenThreadFromChannel,
            onToggleThreadsList:
              state.channelIndexesAvailable && read.primaryPaneDescriptor != null
                ? commands.toggleThreadsList
                : undefined,
            threadsListActive: state.threadsListMounted,
            onToggleFilesList:
              state.channelIndexesAvailable && read.primaryPaneDescriptor != null
                ? commands.toggleFilesList
                : undefined,
            filesListActive: state.filesListMounted,
            conversationRefsById: read.conversationRefsById,
            onOpenConversationRef: commands.openSide,
            onOpenChannelRef: commands.handleOpenChannelByName,
            onOpenTask: commands.openTaskInPane,
            onJumpToMessage: commands.handleJumpToMessage,
            onOpenAdvancedSearch: commands.handleOpenAdvancedSearch,
            viewTabsEnabled: true,
            onOpenAttachmentPanel: commands.openAttachmentPanel,
          },
        },
    secondary: resolveSecondaryPane(route, read, state, commands),
  };
}

/** Overlay precedence is decided here; renderers consume only the selected branch. */
export function resolveSecondaryPane(
  route: PaneContentRoute,
  read: PaneContentRead,
  state: PaneContentState,
  commands: PaneContentCommands,
): SecondaryPaneModel {
  const surface = selectSecondarySurface({ ...route, ...state });
  if (surface === "agent" && route.agentPanelId) {
    return {
      kind: "agent",
      props: {
        agentId: route.agentPanelId,
        contextConversationId: route.channelId,
        contextWorkspaceId: route.workspaceId ?? null,
        onClose: commands.closeAgentPanel,
        onDeleted: commands.closeAgentPanel,
        onMessageAgent: commands.handleMessageAgent,
      },
    };
  }

  if (surface === "human" && route.humanPanelId) {
    return {
      kind: "human",
      props: { userId: route.humanPanelId, onClose: commands.closeHumanPanel },
    };
  }

  if (surface === "settings" && route.channelSettingsId && route.channelId) {
    return {
      kind: "settings",
      channelId: route.channelSettingsId,
      onDepartChannel: commands.departPrimaryPane,
    };
  }

  if (surface === "files" && state.filesListMounted && route.channelId) {
    const channelId = route.channelId;
    return {
      kind: "files",
      props: {
        channelId: route.channelId,
        workspaceId: route.workspaceId ?? null,
        parentTitle: read.selectedChannel?.title ?? null,
        onClose: commands.closeFilesList,
        onJumpToMessage: commands.handleJumpToMessage,
        onOpenAttachment: (attachment) => commands.openAttachmentPanel(attachment.id, true),
      },
      preview: route.attachmentPanelId
        ? attachmentPaneProps(route.attachmentPanelId, channelId, read, {
            onClose: commands.closeFilesList,
            onBack: commands.closeAttachmentPanel,
          })
        : null,
    };
  }

  if (surface === "attachment" && route.attachmentPanelId && route.channelId) {
    return {
      kind: "attachment",
      props: attachmentPaneProps(route.attachmentPanelId, route.channelId, read, {
        onClose: commands.closeAttachmentPanel,
      }),
    };
  }

  if (surface === "task" && state.taskPaneId && route.channelId) {
    return {
      kind: "task",
      props: {
        taskId: state.taskPaneId,
        onClose: commands.requestCloseTaskPane,
        onEditingChange: commands.setTaskPaneEditing,
      },
    };
  }

  if (surface === "side" && state.secondaryDescriptor) {
    if (state.secondaryDescriptor.status !== "resolved") {
      return {
        kind: "conversation",
        status: "unresolved",
        props: {
          conversationId: state.secondaryDescriptor.id,
          status: state.secondaryDescriptor.status,
          onClose: commands.closeSecondary,
        },
      };
    }
    if (state.secondaryConversationDescriptor) {
      return {
        kind: "conversation",
        status: "resolved",
        props: {
          hasChannelNamespace: state.secondaryViewInputs.hasChannelNamespace,
          workspaceName: read.workspaceNameById(state.secondaryConversationDescriptor.workspace_id),
          conversation: state.secondaryConversationDescriptor,
          parentTitle: null,
          jumpTarget: state.secondaryJumpTarget,
          jumpPrimePending: state.secondaryJumpPrimePending,
          onJumped: commands.handleJumped,
          onCloseChild: commands.closeSecondary,
          closeLabel: t("conversationShell.closeSidePane"),
          isFullscreen: false,
          conversationRefsById: read.conversationRefsById,
          onOpenConversationRef: commands.openSide,
          onOpenChannelRef: commands.handleOpenChannelByName,
          onOpenTask: commands.openTaskInPane,
        },
      };
    }
    return { kind: "none" };
  }

  if (surface === "thread" && route.threadRootId && route.channelId) {
    const threadChannelId = route.channelId;
    const threadWorkspaceId = route.workspaceId ?? null;
    const threadRoot = route.threadRootId;
    return {
      kind: "thread",
      capabilities: read.primaryPaneCapabilities,
      props: {
        channelId: threadChannelId,
        workspaceId: threadWorkspaceId,
        directory: read.primaryThreadComposerDirectory,
        rootMessageId: threadRoot,
        onOpenAttachment: (attachment) => commands.openAttachmentPanel(attachment.id),
        onExpandAttachment: expandIntoPrimary(read, threadChannelId),
        parentTitle: read.selectedChannel?.title ?? null,
        parentCounterpart: read.primaryPaneDescriptor?.counterpart ?? null,
        closed: read.selectedChannelDescriptor?.closed_at != null,
        archived: read.selectedChannelDescriptor?.archived_at != null,
        gate: read.parentMembership.gate,
        onClose: commands.closeThread,
        onViewInChannel: () => {
          // Scroll-locate + flash the root in the main channel stream,
          // then drop the thread pane so the channel is unobstructed.
          commands.requestJump({
            workspaceId: threadWorkspaceId ?? undefined,
            channelId: threadChannelId,
            messageId: threadRoot,
          });
          commands.closeThread();
        },
        conversationRefsById: read.conversationRefsById,
        onOpenConversationRef: commands.openSide,
        onOpenChannelRef: commands.handleOpenChannelByName,
        onOpenTask: commands.openTaskInPane,
        jumpMessageId: state.threadJumpMessageId,
        jumpPhase: state.threadJumpPhase,
        onJumped: commands.handleJumped,
        onJumpToMessage: commands.handleJumpToMessage,
        onOpenAdvancedSearch: commands.handleOpenAdvancedSearch,
        advancedSearchSender: directPairSenderSeed(read.directPair?.counterpart),
      },
    };
  }

  if (surface === "threads" && state.threadsListMounted && route.channelId) {
    return {
      kind: "threads",
      props: {
        channelId: route.channelId,
        parentTitle: read.selectedChannel?.title ?? null,
        onClose: commands.closeThreadsList,
        onOpenThread: commands.openThread,
      },
    };
  }

  return { kind: "none" };
}

/** Expanding hands the preview to the primary pane's tab and closes it here,
 * so one attachment is never open in both places. */
function attachmentPaneProps(
  attachmentId: string,
  conversationId: string,
  read: PaneContentRead,
  exits: Pick<AttachmentPaneProps, "onClose" | "onBack">,
): AttachmentPaneProps {
  const expand = expandIntoPrimary(read, conversationId);
  return {
    attachmentId,
    ...exits,
    onExpand: expand
      ? (attachment) => {
          expand(attachment);
          exits.onClose();
        }
      : undefined,
  };
}

/** Opens the attachment as a tab of the primary pane, offered only while that
 * pane shows `conversationId` with its tab strip. */
function expandIntoPrimary(
  read: PaneContentRead,
  conversationId: string,
): ((attachment: AttachmentSnapshot) => void) | undefined {
  const primary = read.primaryPaneDescriptor;
  if (primary?.id !== conversationId) return undefined;
  if (!hostsViewTabs(primary, read.parentMembership.gate.allowMemberEffects)) return undefined;
  return (attachment) =>
    openConversationTab(conversationId, {
      kind: "attachment",
      attachmentId: attachment.id,
      filename: attachment.filename,
      contentType: attachment.content_type,
    });
}
