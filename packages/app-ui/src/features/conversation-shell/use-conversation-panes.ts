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
import { routes, useNav, useRouteState, type PaneColumn } from "@/router";
import { dissolveSplitPair } from "./split-pairs";
import { useCallback, useEffect, useMemo, useRef } from "react";
import type { PaneJumpModel, PaneReadModel } from "./conversation-shell-models";

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
  const { primaryPointer, hasChannelNamespace, deferredChannelId } = read;
  // Channel-scoped right panes (thread, threads, attachments) belong to one
  // column at a time; without a split view that is the route's conversation.
  const paneContext = read.paneContext ?? {
    column: "primary" as PaneColumn,
    channelId: selectedChannelId,
    workspaceId: selectedWorkspaceId,
    membership: read.parentChannelMembership,
  };
  const contextChannelId = paneContext.channelId;
  const splitDescriptor = read.split?.descriptor ?? null;
  const { jumpTarget, isPrimePendingFor, primePhaseFor, handleJumpToMessage } = jump;

  const lastSelectedMessageReadKey = useRef<string | null>(null);

  const lastThreadMessageReadKey = useRef<string | null>(null);

  const lastSplitMessageReadKey = useRef<string | null>(null);

  const { secondaryDescriptor, visibleConversations, focusedConversation } = useSecondaryPaneModel({
    primary: primaryPointer,
    sideConversationId,
    selectedWorkspaceId,
    routeFullscreen: routeFullscreen || compact,
  });

  const markMessageChannelRead = useMarkMessageChannelRead();

  const channelIndexesAvailable = selectedChannelId !== null;

  const filesListMounted =
    contextChannelId !== null && mountsMemberOnlyPanel(filesListOpen, paneContext.membership);

  const threadsListMounted =
    contextChannelId !== null && mountsMemberOnlyPanel(threadsListOpen, paneContext.membership);

  const taskPaneId = resolveTaskDetailSurface(route) === "pane" ? taskDetailId : null;
  const surface = selectSecondarySurface({
    ...route,
    filesListMounted,
    taskPaneId,
    secondaryDescriptor,
    threadsListMounted,
  });
  const compactSecondary = compact && surface !== "none";
  const visiblePrimary = compactSecondary ? null : visibleConversations.primary;
  // The second column is on screen beside the first unless a pane takes the
  // whole stage; compact shows one column only.
  const visibleSplit =
    splitDescriptor && !compact && !routeFullscreen
      ? { id: splitDescriptor.id, rosterPolicy: splitDescriptor.roster_policy }
      : null;
  const visibleSecondary = compact && surface !== "side" ? null : visibleConversations.secondary;
  const notificationFocus =
    compactSecondary && surface !== "side" && surface !== "thread"
      ? null
      : compact && surface === "side" && !visibleSecondary
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

  useEffect(() => {
    if (!visibleSplit) {
      lastSplitMessageReadKey.current = null;
      return;
    }
    if (lastSplitMessageReadKey.current === visibleSplit.id) {
      return;
    }
    lastSplitMessageReadKey.current = visibleSplit.id;
    markMessageChannelRead.mutate(visibleSplit.id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [visibleSplit?.id]);

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

  const splitJumpTarget = useMemo(
    () =>
      jumpTarget && !jumpTarget.threadRootId && jumpTarget.channelId === splitDescriptor?.id
        ? { channelId: jumpTarget.channelId, messageId: jumpTarget.messageId }
        : null,
    [jumpTarget, splitDescriptor?.id],
  );

  const splitJumpPrimePending = useMemo(
    () =>
      splitJumpTarget !== null &&
      isPrimePendingFor(splitJumpTarget.channelId, splitJumpTarget.messageId),
    [isPrimePendingFor, splitJumpTarget],
  );

  const threadJumpMessageId = useMemo(
    () =>
      jumpTarget &&
      jumpTarget.threadRootId &&
      jumpTarget.threadRootId === threadRootId &&
      jumpTarget.channelId === contextChannelId
        ? jumpTarget.messageId
        : null,
    [jumpTarget, threadRootId, contextChannelId],
  );

  const parentJumpPrimePending = useMemo(
    () =>
      parentJumpTarget !== null &&
      isPrimePendingFor(parentJumpTarget.channelId, parentJumpTarget.messageId),
    [isPrimePendingFor, parentJumpTarget],
  );

  const threadJumpPhase = useMemo(
    () =>
      threadJumpMessageId !== null && contextChannelId !== null
        ? primePhaseFor(contextChannelId, threadJumpMessageId)
        : null,
    [primePhaseFor, threadJumpMessageId, contextChannelId],
  );

  const handleOpenThreadFromChannel = (
    rootMessageId: string,
    jumpToMessageId?: string,
    column: PaneColumn = "primary",
  ): void => {
    // A jump navigates to the thread's own conversation, so it would replace
    // the split view; the second column opens its thread in place and lets the
    // thread pane find the reply.
    if (jumpToMessageId && column === "primary" && selectedChannelId) {
      handleJumpToMessage({
        workspaceId: selectedWorkspaceId ?? undefined,
        channelId: selectedChannelId,
        threadRootId: rootMessageId,
        messageId: jumpToMessageId,
      });
      return;
    }
    nav.openThread(rootMessageId, column);
  };

  const closeColumn = (column: PaneColumn): void => {
    if (selectedChannelId) dissolveSplitPair(selectedChannelId);
    if (column === "split" || !splitDescriptor) {
      nav.closeSplit();
      return;
    }
    nav.promoteSplit(
      splitDescriptor.workspace_id
        ? routes.chatChannel(splitDescriptor.workspace_id, splitDescriptor.id)
        : routes.chatDirect(splitDescriptor.id),
    );
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
    splitJumpTarget,
    splitJumpPrimePending,
    handleOpenThreadFromChannel,
    closeColumn,
    visibleSplitId: visibleSplit?.id ?? null,
  };
}
