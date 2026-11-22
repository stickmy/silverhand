import { useNavigationDriver } from "@/navigation/provider";
import { onScreen } from "@/navigation/panes";
import { useContextualCloseResponder } from "@/features/app-shell/keyboard-shortcuts";
import { useMarkMessageChannelRead } from "@/features/channels/hooks";
import { resolveTaskDetail } from "@/features/tasks/model/task-detail-surface";
import { useNav, useRouteState } from "@/router";
import { useCallback, useEffect, useMemo, useRef } from "react";
import type { PaneJumpModel, PaneReadModel } from "./conversation-shell-models";
import { threadJumpOutlivedPane } from "./model/thread-jump-lifetime";

/**
 * Owns pane-local state and the on-screen effects across Chat/Activity. A pane
 * in the URL is on screen, so what is visible is the open set itself
 * (`onScreen`); compact shows the last pane only.
 */
export function useConversationPanes(
  read: PaneReadModel,
  jump: PaneJumpModel,
  { compact }: { compact: boolean },
) {
  const route = useRouteState();
  const nav = useNav();
  const { workspaceId: selectedWorkspaceId, channelId: selectedChannelId, panes } = route;
  const { primaryPointer, deferredChannelId } = read;
  const { jumpTarget, isPrimePendingFor, handleJumpToMessage, clearJump } = jump;

  const screen = useMemo(
    () => onScreen(selectedChannelId, panes, compact),
    [selectedChannelId, panes, compact],
  );

  const shownThreadsRef = useRef(screen.threads);
  useEffect(() => {
    const before = shownThreadsRef.current;
    shownThreadsRef.current = screen.threads;
    if (threadJumpOutlivedPane(jumpTarget, before, screen.threads)) clearJump();
  }, [screen.threads, jumpTarget, clearJump]);

  const markMessageChannelRead = useMarkMessageChannelRead();
  const lastPrimaryReadKey = useRef<string | null>(null);
  // The route's conversation is read once it shows.
  const visiblePrimary =
    primaryPointer && screen.conversations.includes(primaryPointer.id) ? primaryPointer : null;
  useEffect(() => {
    if (!visiblePrimary) {
      lastPrimaryReadKey.current = null;
      return;
    }
    if (lastPrimaryReadKey.current === visiblePrimary.id) return;
    lastPrimaryReadKey.current = visiblePrimary.id;
    markMessageChannelRead.mutate(visiblePrimary.id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [visiblePrimary?.id]);

  const openTaskRef = useRef(nav.openTask);
  openTaskRef.current = nav.openTask;
  const openTaskInPane = useCallback((taskId: string) => openTaskRef.current(taskId), []);

  const task = resolveTaskDetail(route);
  const taskPaneId = task?.surface === "pane" ? task.id : null;
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
    route,
    { ...nav, closeTask: requestCloseTaskPane },
    taskPaneId !== null,
  );

  const parentJump = useMemo(() => {
    const target =
      jumpTarget && !jumpTarget.threadRootId && jumpTarget.channelId === deferredChannelId
        ? { channelId: jumpTarget.channelId, messageId: jumpTarget.messageId }
        : null;
    return {
      target,
      primePending: target !== null && isPrimePendingFor(target.channelId, target.messageId),
    };
  }, [jumpTarget, deferredChannelId, isPrimePendingFor]);

  const handleOpenThreadFromChannel = (
    rootMessageId: string,
    jumpToMessageId: string | undefined,
    of: string,
  ): void => {
    if (jumpToMessageId && of === selectedChannelId) {
      handleJumpToMessage({
        workspaceId: selectedWorkspaceId ?? undefined,
        channelId: of,
        threadRootId: rootMessageId,
        messageId: jumpToMessageId,
      });
      return;
    }
    nav.openThread(rootMessageId, of);
  };

  return {
    screen,
    channelIndexesAvailable: selectedChannelId !== null,
    openTaskInPane,
    taskPaneId,
    setTaskPaneEditing,
    requestCloseTaskPane,
    parentJumpTarget: parentJump.target,
    parentJumpPrimePending: parentJump.primePending,
    handleOpenThreadFromChannel,
  };
}
