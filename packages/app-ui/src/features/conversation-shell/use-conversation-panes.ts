import { useNavigationDriver } from "@/navigation/provider";
import { selectSecondarySurface } from "./model/secondary-surface";
import { useContextualCloseResponder } from "@/features/app-shell/keyboard-shortcuts";
import { useSecondaryPaneModel } from "@/features/app-shell/secondary-pane-model";
import { useMarkMessageChannelRead } from "@/features/channels/hooks";
import { mountsMemberOnlyPanel } from "@/features/channels/membership";
import { resolveTaskDetailSurface } from "@/features/tasks/model/task-detail-surface";
import {
  selectSecondaryViewInputs,
  selectSidebarSelectedConversationId,
} from "@/features/conversation-shell/model/secondary-pane";
import { useNav, useRouteState } from "@/router";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  PaneAttachmentRequest,
  PaneJumpModel,
  PaneReadModel,
} from "./conversation-shell-models";

/** Owns pane-local state and visible-conversation effects across Chat/Activity presentations. */
export function useConversationPanes(
  read: PaneReadModel,
  jump: PaneJumpModel,
  { compact }: { compact: boolean },
) {
  const route = useRouteState();
  const nav = useNav();
  const {
    workspaceId: selectedWorkspaceId,
    channelId: selectedChannelId,
    sideConversationId,
    fullscreen: routeFullscreen,
    filesListOpen,
    threadsListOpen,
    taskDetailId,
    threadRootId,
  } = route;
  const { primaryPointer, parentChannelMembership, hasChannelNamespace, deferredChannelId } = read;
  const { jumpTarget, isPrimePendingFor, primePhaseFor, handleJumpToMessage } = jump;

  const lastSelectedMessageReadKey = useRef<string | null>(null);

  const lastThreadMessageReadKey = useRef<string | null>(null);

  const { secondaryDescriptor, visibleConversations, focusedConversation } = useSecondaryPaneModel({
    primary: primaryPointer,
    sideConversationId,
    selectedWorkspaceId,
    routeFullscreen: routeFullscreen || compact,
  });

  const markMessageChannelRead = useMarkMessageChannelRead();

  const channelIndexesAvailable = selectedChannelId !== null;

  const filesListMounted =
    channelIndexesAvailable && mountsMemberOnlyPanel(filesListOpen, parentChannelMembership);

  const threadsListMounted =
    channelIndexesAvailable && mountsMemberOnlyPanel(threadsListOpen, parentChannelMembership);

  const taskPaneId = resolveTaskDetailSurface(route) === "pane" ? taskDetailId : null;
  const surface = selectSecondarySurface({
    agent: Boolean(route.agentPanelId),
    human: Boolean(route.humanPanelId),
    settings: Boolean(route.channelSettingsId && selectedChannelId),
    files: Boolean(filesListMounted && selectedChannelId),
    attachment: Boolean(route.attachmentPanelId && selectedChannelId),
    task: Boolean(taskPaneId && selectedChannelId),
    conversation: Boolean(secondaryDescriptor),
    thread: Boolean(threadRootId && selectedChannelId),
    threads: Boolean(threadsListMounted && selectedChannelId),
  });
  const compactSecondary = compact && surface !== "none";
  const visiblePrimary = compactSecondary ? null : visibleConversations.primary;
  const visibleSecondary =
    compact && surface !== "conversation" ? null : visibleConversations.secondary;
  const notificationFocus =
    compactSecondary && surface !== "conversation" && surface !== "thread"
      ? null
      : compact && surface === "conversation" && !visibleSecondary
        ? null
        : focusedConversation;

  useEffect(() => {
    if (!visiblePrimary) {
      lastSelectedMessageReadKey.current = null;
      return;
    }
    const readKey = visiblePrimary.id;
    if (lastSelectedMessageReadKey.current === readKey) {
      return;
    }
    lastSelectedMessageReadKey.current = readKey;
    markMessageChannelRead.mutate(visiblePrimary.id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [visiblePrimary?.id]);

  useEffect(() => {
    if (!visibleSecondary) {
      lastThreadMessageReadKey.current = null;
      return;
    }
    const readKey = visibleSecondary.id;
    if (lastThreadMessageReadKey.current === readKey) {
      return;
    }
    lastThreadMessageReadKey.current = readKey;
    markMessageChannelRead.mutate(visibleSecondary.id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [visibleSecondary?.id]);

  const openTaskRef = useRef(nav.openTask);

  openTaskRef.current = nav.openTask;

  const openTaskInPane = useCallback((taskId: string) => openTaskRef.current(taskId), []);

  const taskPaneEditingRef = useRef(false);
  const { registerLeaveGuard } = useNavigationDriver();
  useEffect(
    () => registerLeaveGuard(() => taskPaneId === null || !taskPaneEditingRef.current),
    [registerLeaveGuard, taskPaneId],
  );

  const setTaskPaneEditing = useCallback((editing: boolean) => {
    taskPaneEditingRef.current = editing;
  }, []);

  const requestCloseTaskPane = useCallback(() => {
    if (taskPaneEditingRef.current) {
      return;
    }
    nav.closeTask();
  }, [nav]);

  const [paneAttachmentRequest, setPaneAttachmentRequest] = useState<PaneAttachmentRequest | null>(
    null,
  );

  useContextualCloseResponder(
    { ...route, filesListOpen: filesListMounted, threadsListOpen: threadsListMounted },
    { ...nav, closeTask: requestCloseTaskPane },
    taskPaneId !== null,
  );

  const closeSecondary = useCallback((): void => {
    nav.closeSide();
  }, [nav.closeSide]);

  const secondaryViewInputs = useMemo(
    () =>
      selectSecondaryViewInputs({
        secondary: secondaryDescriptor,
        primaryHasChannelNamespace: hasChannelNamespace,
        primaryWorkspaceId: selectedWorkspaceId,
      }),
    [secondaryDescriptor, hasChannelNamespace, selectedWorkspaceId],
  );

  const secondaryConversationDescriptor = secondaryViewInputs.conversationDescriptor;

  const sidebarSelectedConversationId = selectSidebarSelectedConversationId({
    primarySelectedChannelId: selectedChannelId,
    secondary: secondaryDescriptor,
    primaryWorkspaceId: selectedWorkspaceId,
  });

  const parentJumpTarget = useMemo(
    () =>
      jumpTarget && !jumpTarget.threadRootId && jumpTarget.channelId === deferredChannelId
        ? { channelId: jumpTarget.channelId, messageId: jumpTarget.messageId }
        : null,
    [jumpTarget, deferredChannelId],
  );

  const secondaryJumpTarget: { channelId: string; messageId: string } | null = null;

  const secondaryJumpPrimePending = false;

  const threadJumpMessageId = useMemo(
    () =>
      jumpTarget &&
      jumpTarget.threadRootId &&
      jumpTarget.threadRootId === threadRootId &&
      jumpTarget.channelId === selectedChannelId
        ? jumpTarget.messageId
        : null,
    [jumpTarget, threadRootId, selectedChannelId],
  );

  const parentJumpPrimePending = useMemo(
    () =>
      parentJumpTarget !== null &&
      isPrimePendingFor(parentJumpTarget.channelId, parentJumpTarget.messageId),
    [isPrimePendingFor, parentJumpTarget],
  );

  const threadJumpPhase = useMemo(
    () =>
      threadJumpMessageId !== null && selectedChannelId !== null
        ? primePhaseFor(selectedChannelId, threadJumpMessageId)
        : null,
    [primePhaseFor, threadJumpMessageId, selectedChannelId],
  );

  const handleOpenThreadFromChannel = (rootMessageId: string, jumpToMessageId?: string): void => {
    if (jumpToMessageId && selectedChannelId) {
      handleJumpToMessage({
        workspaceId: selectedWorkspaceId ?? undefined,
        channelId: selectedChannelId,
        threadRootId: rootMessageId,
        messageId: jumpToMessageId,
      });
      return;
    }
    nav.openThread(rootMessageId);
  };
  return {
    secondaryDescriptor,
    focusedConversation: notificationFocus,
    channelIndexesAvailable,
    filesListMounted,
    threadsListMounted,
    openTaskInPane,
    taskPaneId,
    setTaskPaneEditing,
    requestCloseTaskPane,
    paneAttachmentRequest,
    setPaneAttachmentRequest,
    closeSecondary,
    secondaryViewInputs,
    secondaryConversationDescriptor,
    sidebarSelectedConversationId,
    parentJumpTarget,
    secondaryJumpTarget,
    secondaryJumpPrimePending,
    threadJumpMessageId,
    parentJumpPrimePending,
    threadJumpPhase,
    handleOpenThreadFromChannel,
  };
}
