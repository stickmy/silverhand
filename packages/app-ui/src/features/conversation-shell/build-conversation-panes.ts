import { selectSecondarySurface } from "./model/secondary-surface";
import { hostsViewTabs } from "./model/view-tabs";
import { openConversationTab } from "@/features/conversations/use-conversation-tabs";
import type { AttachmentSnapshot } from "@silverhand/shared";
import { directPairSenderSeed } from "@/features/search/advanced-search-seed";
import type {
  AttachmentPaneProps,
  ConversationPanesModel,
  SecondaryPaneModel,
  SplitPaneModel,
} from "./pane-render-models";
import type { PaneColumn } from "@/router";
import type { ConversationColumn } from "./conversation-column";
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
  const split = read.split ? buildSplitPane(read.split, read, state, commands) : null;
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
            ...columnViewProps("primary", read, state, commands, split !== null),
            hasChannelNamespace: read.primary?.hasChannelNamespace ?? false,
            workspaceName: read.workspaceNameById(read.primaryPaneDescriptor?.workspace_id ?? null),
            conversation: read.primaryPaneDescriptor,
            resolving: read.parentChannelResolving,
            loadFailed: read.directPairLoadFailed,
            notFound: read.directPairNotFound,
            jumpTarget: state.parentJumpTarget,
            jumpPrimePending: state.parentJumpPrimePending,
            onToggleThreadsList:
              state.channelIndexesAvailable && read.primaryPaneDescriptor != null
                ? () => commands.toggleThreadsList("primary")
                : undefined,
            onToggleFilesList:
              state.channelIndexesAvailable && read.primaryPaneDescriptor != null
                ? () => commands.toggleFilesList("primary")
                : undefined,
          },
        },
    split,
    secondary: resolveSecondaryPane(route, read, state, commands),
  };
}

/** What a column's view takes the same way in either column: where its
 *  threads and attachments open, whether it owns the open index pane, and —
 *  while split — a close button. */
function columnViewProps(
  column: PaneColumn,
  read: PaneContentRead,
  state: PaneContentState,
  commands: PaneContentCommands,
  split: boolean,
) {
  const owns = read.paneOwner?.column === column;
  return {
    parentTitle: null,
    onJumped: commands.handleJumped,
    onOpenThread: (rootId: string, jumpTo?: string) =>
      commands.handleOpenThreadFromChannel(rootId, jumpTo, column),
    threadsListActive: owns && state.threadsListMounted,
    filesListActive: owns && state.filesListMounted,
    conversationRefsById: read.conversationRefsById,
    onOpenConversationRef: commands.openSide,
    onOpenChannelRef: commands.handleOpenChannelByName,
    onOpenTask: commands.openTaskInPane,
    onJumpToMessage: commands.handleJumpToMessage,
    onOpenAdvancedSearch: commands.handleOpenAdvancedSearch,
    viewTabsEnabled: true,
    onOpenAttachmentPanel: (id: string) => commands.openAttachmentPanel(id, false, column),
    ...(split
      ? {
          onCloseChild: () => commands.closeColumn(column),
          closeLabel: t("conversationShell.closeColumn"),
          column,
        }
      : {}),
  };
}

/** The second column; it renders a placeholder until its conversation resolves. */
function buildSplitPane(
  split: ConversationColumn,
  read: PaneContentRead,
  state: PaneContentState,
  commands: PaneContentCommands,
): SplitPaneModel {
  if (!split.descriptor) {
    return {
      kind: "unresolved",
      props: {
        conversationId: split.id,
        status: split.status === "resolved" ? "loading" : split.status,
        onClose: () => commands.closeColumn("split"),
      },
    };
  }
  return {
    kind: "conversation",
    props: {
      ...columnViewProps("split", read, state, commands, true),
      hasChannelNamespace: split.hasChannelNamespace,
      workspaceName: read.workspaceNameById(split.workspaceId),
      conversation: split.descriptor,
      jumpTarget: state.splitJumpTarget,
      jumpPrimePending: state.splitJumpPrimePending,
      onToggleThreadsList: () => commands.toggleThreadsList("split"),
      onToggleFilesList: () => commands.toggleFilesList("split"),
    },
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

  const owner = read.paneOwner;

  if (surface === "files" && state.filesListMounted && owner) {
    const channelId = owner.id;
    return {
      kind: "files",
      props: {
        channelId,
        workspaceId: owner.workspaceId,
        parentTitle: owner.paneTitle,
        onClose: commands.closeFilesList,
        onJumpToMessage: commands.handleJumpToMessage,
        onOpenAttachment: (attachment) => commands.openAttachmentPanel(attachment.id, true),
      },
      preview: route.attachmentPanelId
        ? attachmentPaneProps(route.attachmentPanelId, owner, {
            onClose: commands.closeFilesList,
            onBack: commands.closeAttachmentPanel,
          })
        : null,
    };
  }

  if (surface === "attachment" && route.attachmentPanelId && owner) {
    return {
      kind: "attachment",
      props: attachmentPaneProps(route.attachmentPanelId, owner, {
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

  if (surface === "thread" && route.threadRootId && owner) {
    const threadChannelId = owner.id;
    const threadWorkspaceId = owner.workspaceId;
    const threadRoot = route.threadRootId;
    return {
      kind: "thread",
      capabilities: owner.capabilities,
      props: {
        channelId: threadChannelId,
        workspaceId: threadWorkspaceId,
        directory: owner.threadComposerDirectory,
        rootMessageId: threadRoot,
        onOpenAttachment: (attachment) => commands.openAttachmentPanel(attachment.id),
        onExpandAttachment: expandIntoColumn(owner),
        parentTitle: owner.paneTitle,
        parentCounterpart: owner.descriptor?.counterpart ?? null,
        closed: owner.descriptor?.closed_at != null,
        archived: owner.descriptor?.archived_at != null,
        gate: owner.membership.gate,
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
        advancedSearchSender: directPairSenderSeed(owner.pair?.counterpart),
      },
    };
  }

  if (surface === "threads" && state.threadsListMounted && owner) {
    return {
      kind: "threads",
      props: {
        channelId: owner.id,
        parentTitle: owner.paneTitle,
        onClose: commands.closeThreadsList,
        onOpenThread: commands.openThread,
      },
    };
  }

  return { kind: "none" };
}

/** Expanding hands the preview to its column's tab and closes it here, so one
 * attachment is never open in both places. */
function attachmentPaneProps(
  attachmentId: string,
  owner: ConversationColumn,
  exits: Pick<AttachmentPaneProps, "onClose" | "onBack">,
): AttachmentPaneProps {
  const expand = expandIntoColumn(owner);
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

/** Opens the attachment as a tab of `column`, offered only while that column
 * has its tab strip. */
function expandIntoColumn(
  column: ConversationColumn,
): ((attachment: AttachmentSnapshot) => void) | undefined {
  if (!column.descriptor) return undefined;
  if (!hostsViewTabs(column.descriptor, column.membership.gate.allowMemberEffects)) {
    return undefined;
  }
  return (attachment) =>
    openConversationTab(column.id, {
      kind: "attachment",
      attachmentId: attachment.id,
      filename: attachment.filename,
      contentType: attachment.content_type,
    });
}
