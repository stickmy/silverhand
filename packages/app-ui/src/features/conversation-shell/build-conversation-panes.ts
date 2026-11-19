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
  const split = buildSplitPane(read, state, commands);
  const context = read.paneContext?.column ?? "primary";
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
                ? () => commands.toggleThreadsList("primary")
                : undefined,
            threadsListActive: context === "primary" && state.threadsListMounted,
            onToggleFilesList:
              state.channelIndexesAvailable && read.primaryPaneDescriptor != null
                ? () => commands.toggleFilesList("primary")
                : undefined,
            filesListActive: context === "primary" && state.filesListMounted,
            conversationRefsById: read.conversationRefsById,
            onOpenConversationRef: commands.openSide,
            onOpenChannelRef: commands.handleOpenChannelByName,
            onOpenTask: commands.openTaskInPane,
            onJumpToMessage: commands.handleJumpToMessage,
            onOpenAdvancedSearch: commands.handleOpenAdvancedSearch,
            viewTabsEnabled: true,
            onOpenAttachmentPanel: (id) => commands.openAttachmentPanel(id, false, "primary"),
            ...(split
              ? {
                  onCloseChild: () => commands.closeColumn("primary"),
                  closeLabel: t("conversationShell.closeColumn"),
                  column: "primary" as const,
                }
              : {}),
          },
        },
    split,
    secondary: resolveSecondaryPane(route, read, state, commands),
  };
}

/** The second column. Its panes open on the right like the first column's,
 *  owned by this column (`?ctx=split`). */
function buildSplitPane(
  read: PaneContentRead,
  state: PaneContentState,
  commands: PaneContentCommands,
): SplitPaneModel | null {
  const split = read.split;
  if (!split?.id) return null;
  const close = () => commands.closeColumn("split");
  if (!split.descriptor) {
    return {
      kind: "unresolved",
      props: {
        conversationId: split.id,
        status: split.status === "resolved" || split.status === null ? "loading" : split.status,
        onClose: close,
      },
    };
  }
  const descriptor = split.descriptor;
  const owns = read.paneContext?.column === "split";
  const member = split.membership.membership === "member";
  return {
    kind: "conversation",
    props: {
      hasChannelNamespace: split.hasChannelNamespace,
      workspaceName: read.workspaceNameById(descriptor.workspace_id),
      conversation: descriptor,
      parentTitle: null,
      jumpTarget: state.splitJumpTarget ?? null,
      jumpPrimePending: state.splitJumpPrimePending ?? false,
      onJumped: commands.handleJumped,
      onOpenThread: (rootId, jumpTo) =>
        commands.handleOpenThreadFromChannel(rootId, jumpTo, "split"),
      onToggleThreadsList: () => commands.toggleThreadsList("split"),
      threadsListActive: owns && member && state.threadsListMounted,
      onToggleFilesList: () => commands.toggleFilesList("split"),
      filesListActive: owns && member && state.filesListMounted,
      conversationRefsById: read.conversationRefsById,
      onOpenConversationRef: commands.openSide,
      onOpenChannelRef: commands.handleOpenChannelByName,
      onOpenTask: commands.openTaskInPane,
      onJumpToMessage: commands.handleJumpToMessage,
      onOpenAdvancedSearch: commands.handleOpenAdvancedSearch,
      viewTabsEnabled: true,
      onOpenAttachmentPanel: (id) => commands.openAttachmentPanel(id, false, "split"),
      onCloseChild: close,
      closeLabel: t("conversationShell.closeColumn"),
      column: "split",
    },
  };
}

/** The coordinates of the column that owns the channel-scoped right panes. */
function contextColumn(route: PaneContentRoute, read: PaneContentRead) {
  const split = read.split;
  if (read.paneContext?.column === "split" && split?.id && split.descriptor) {
    return {
      column: "split" as PaneColumn,
      channelId: split.id,
      workspaceId: split.descriptor.workspace_id,
      title: split.descriptor.title,
      descriptor: split.descriptor,
      parentCounterpart: split.descriptor.counterpart ?? null,
      capabilities: split.capabilities,
      composerDirectory: split.threadComposerDirectory,
      gate: split.membership.gate,
      counterpart: split.pair?.counterpart,
    };
  }
  return {
    column: "primary" as PaneColumn,
    channelId: route.channelId,
    workspaceId: route.workspaceId ?? null,
    title: read.selectedChannel?.title ?? null,
    descriptor: read.selectedChannelDescriptor,
    parentCounterpart: read.primaryPaneDescriptor?.counterpart ?? null,
    capabilities: read.primaryPaneCapabilities,
    composerDirectory: read.primaryThreadComposerDirectory,
    gate: read.parentMembership.gate,
    counterpart: read.directPair?.counterpart,
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

  const ctx = contextColumn(route, read);

  if (surface === "files" && state.filesListMounted && ctx.channelId) {
    const channelId = ctx.channelId;
    return {
      kind: "files",
      props: {
        channelId,
        workspaceId: ctx.workspaceId,
        parentTitle: ctx.title,
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

  if (surface === "attachment" && route.attachmentPanelId && ctx.channelId) {
    return {
      kind: "attachment",
      props: attachmentPaneProps(route.attachmentPanelId, ctx.channelId, read, {
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

  if (surface === "thread" && route.threadRootId && ctx.channelId) {
    const threadChannelId = ctx.channelId;
    const threadWorkspaceId = ctx.workspaceId;
    const threadRoot = route.threadRootId;
    return {
      kind: "thread",
      capabilities: ctx.capabilities,
      props: {
        channelId: threadChannelId,
        workspaceId: threadWorkspaceId,
        directory: ctx.composerDirectory,
        rootMessageId: threadRoot,
        onOpenAttachment: (attachment) => commands.openAttachmentPanel(attachment.id),
        onExpandAttachment: expandIntoPrimary(read, threadChannelId),
        parentTitle: ctx.title,
        parentCounterpart: ctx.parentCounterpart,
        closed: ctx.descriptor?.closed_at != null,
        archived: ctx.descriptor?.archived_at != null,
        gate: ctx.gate,
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
        advancedSearchSender: directPairSenderSeed(ctx.counterpart),
      },
    };
  }

  if (surface === "threads" && state.threadsListMounted && ctx.channelId) {
    return {
      kind: "threads",
      props: {
        channelId: ctx.channelId,
        parentTitle: ctx.title,
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

/** Opens the attachment as a tab of the column showing `conversationId`,
 * offered only while that column has its tab strip. */
function expandIntoPrimary(
  read: PaneContentRead,
  conversationId: string,
): ((attachment: AttachmentSnapshot) => void) | undefined {
  const split = read.split;
  const host =
    read.primaryPaneDescriptor?.id === conversationId
      ? { descriptor: read.primaryPaneDescriptor, gate: read.parentMembership.gate }
      : split?.descriptor?.id === conversationId
        ? { descriptor: split.descriptor, gate: split.membership.gate }
        : null;
  if (!host) return undefined;
  if (!hostsViewTabs(host.descriptor, host.gate.allowMemberEffects)) return undefined;
  return (attachment) =>
    openConversationTab(conversationId, {
      kind: "attachment",
      attachmentId: attachment.id,
      filename: attachment.filename,
      contentType: attachment.content_type,
    });
}
