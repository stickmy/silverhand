import { hasCollapsibleSidebar } from "@/components/layout/sidebar-slot";
import { useCompactLayout } from "@/navigation/layout";
import { conversationContent } from "./conversation-content";
import {
  closePane,
  conversationPanes,
  openPane,
  paneKey,
  singlePane,
  type Panes,
} from "@/navigation/panes";
import { senderFilterToParam } from "@/features/search/sender-filter-select";
import { useAgents } from "@/features/agents/hooks/hooks";
import { useLayoutKeyboardShortcuts } from "@/features/app-shell/effects";
import { useMessageUnreadCounts } from "@/features/channels/hooks";
import { useChatRouteRecovery } from "@/features/app-shell/use-chat-route-recovery";
import { findAgentDirectPair } from "@/features/direct-pairs/hooks";
import { useNotificationToasts } from "@/features/attention/notifications";
import { useSavedMessages } from "@/features/messages/saved";
import type { AdvancedSearchSeed } from "@/features/search/advanced-search-seed";
import { useLayoutSettings } from "@/features/conversation-shell/use-layout-settings";
import { resolveChannelOpen } from "@/features/conversation-shell/model/channel-open";
import { getChannelPanes, getCloseWatermark } from "@/features/app-shell/place-memory";
import { channelUrlKeeping, paneUrl, routes, useNav, useRouteState } from "@/router";
import { useCallback, useDeferredValue, useEffect, useMemo, useRef } from "react";
import { dissolveConversationPair, recordConversationPair } from "./conversation-pairs";
import type { ConversationShellModel, ConversationShellSignals } from "./conversation-shell-models";
import { useConversationInvalidation } from "./use-conversation-invalidation";
import { useConversationJump } from "./use-conversation-jump";
import { useConversationPanes } from "./use-conversation-panes";
import { useConversationReadModel } from "./use-conversation-read-model";

/** Session-lived assembly: directory reads, pane effects and section-specific entry actions. */
export function useConversationShell({
  attentionBadges,
  unreadCounts,
}: ConversationShellSignals): ConversationShellModel {
  const compact = useCompactLayout();
  const { layout, toggleWorkspaceSidebar } = useLayoutSettings();

  const route = useRouteState();
  const deferredRoute = useDeferredValue(route);

  const nav = useNav();

  const handleOpenAdvancedSearch = useCallback(
    (seed: AdvancedSearchSeed) => {
      nav.toChatSearch({
        q: seed.query,
        reset: true,
        workspace: seed.workspaceId,
        sender: seed.sender ? senderFilterToParam(seed.sender) : undefined,
      });
    },
    [nav],
  );

  const {
    section: appSection,
    chatView: view,
    workspaceId: selectedWorkspaceId,
    channelId: selectedChannelId,
    directDraftTarget,
    panes: routePanes,
  } = route;
  const agentPanelId = singlePane(routePanes, "agent")?.id ?? null;
  const [pairedConversationId = null] = conversationPanes(routePanes);

  const read = useConversationReadModel();

  const workspaceSidebarPane = layout.workspaceSidebar;

  const workspaceSidebarVisible = workspaceSidebarPane.visible;

  useLayoutKeyboardShortcuts(toggleWorkspaceSidebar, hasCollapsibleSidebar(appSection));

  const { data: agents = [] } = useAgents();

  const agentsById = useMemo(() => new Map(agents.map((a) => [a.id, a])), [agents]);

  const { recentlyRemovedPrimaryRef, departPrimaryPane } = useConversationInvalidation({
    activeChannels: read.activeChannels,
    archivedChannels: read.archivedChannels,
    memberActiveChannels: read.memberActiveChannels,
    activeSettled: read.activeSettled,
    archivedSettled: read.archivedSettled,
    visibleWorkspaceIds: read.visibleWorkspaceIds,
  });

  const { data: savedItems } = useSavedMessages();

  const { data: messageUnreadData } = useMessageUnreadCounts(
    selectedWorkspaceId,
    read.workspaceResolved,
  );

  const openAgentConversation = (conversationId: string, workspaceId: string | null): void => {
    if (workspaceId === null) {
      nav.toDirect(conversationId);
      return;
    }
    const wid = read.resolveNavWorkspaceId(workspaceId, conversationId);
    if (!wid) {
      return;
    }
    nav.toChannel(wid, conversationId);
  };

  const jump = useConversationJump(read.directPairAuthority, read.resolveNavWorkspaceId);
  const { jumpTarget, requestJump, handleJumpToMessage, handleJumpToMessageStable, handleJumped } =
    jump;

  useChatRouteRecovery({
    workspaces: read.workspaces,
    channels: read.channels,
    memberActiveChannels: read.memberActiveChannels,
    messageUnreadData,
    directPairAuthority: read.directPairAuthority,
    jumpTarget,
    onJump: (target) => handleJumpToMessage(target),
    recentlyRemovedPrimaryRef,
    compact,
  });

  const panes = useConversationPanes(read, jump, { compact });

  useNotificationToasts(panes.screen, (target) => handleJumpToMessage(target));

  // Two conversations side by side are a pair the sidebar keeps until the
  // viewer closes one. Recorded once the conversation pane resolves, so a dead
  // id never lands in the sidebar.
  const pairedResolvedId = read.paired?.descriptor ? read.paired.id : null;
  useEffect(() => {
    if (appSection !== "chat" || !selectedChannelId || !pairedResolvedId) return;
    recordConversationPair({ left: selectedChannelId, right: pairedResolvedId });
  }, [appSection, selectedChannelId, pairedResolvedId]);

  // Closing either half on the stage (its header X, ⌘W) dissolves the pair, the
  // same as the sidebar's X. Switching to another conversation does not: the
  // pair is gone from screen, not closed.
  const shownPair = useRef<{ left: string; right: string } | null>(null);
  useEffect(() => {
    const prev = shownPair.current;
    const closedHalf =
      prev !== null &&
      pairedConversationId === null &&
      (selectedChannelId === prev.left || selectedChannelId === prev.right);
    if (closedHalf) dissolveConversationPair(prev.left);
    shownPair.current =
      appSection === "chat" && selectedChannelId && pairedResolvedId
        ? { left: selectedChannelId, right: pairedResolvedId }
        : null;
  }, [appSection, selectedChannelId, pairedConversationId, pairedResolvedId]);

  const activePair =
    appSection === "chat" && selectedChannelId && pairedConversationId
      ? { left: selectedChannelId, right: pairedConversationId }
      : null;

  /** Reopen a sidebar pair: its first conversation with the second beside it,
   *  in the layout the pair was left in (each half's threads, files, ...). */
  const openPair = (workspaceId: string | null, left: string, right: string): void => {
    const path = workspaceId ? routes.chatChannel(workspaceId, left) : routes.chatDirect(left);
    // A layout left with another conversation beside `left` gives that one up.
    const stored = workspaceId ? (getChannelPanes(left) ?? []) : [];
    const remembered = stored
      .filter((p) => p.kind === "conversation" && p.id !== right)
      .reduce<Panes>((panes, p) => closePane(panes, paneKey(p)), stored);
    nav.go(paneUrl(path, openPane(remembered, { kind: "conversation", id: right })));
  };

  const closePairHalf = (half: "left" | "right"): void => {
    if (selectedChannelId) dissolveConversationPair(selectedChannelId);
    if (half === "left") {
      panes.closeRouteConversation();
      return;
    }
    if (pairedConversationId) nav.closePane(`conversation:${pairedConversationId}`);
  };

  const openBeside = (conversationId: string): void => {
    if (compact || !selectedChannelId || conversationId === selectedChannelId) return;
    nav.openBeside(conversationId);
  };

  // Regular width defers a section's teardown past the click (`SectionLayer`).
  // A compact page mounts fresh and keeps the page it covers, so it renders its
  // own section at once.
  const deferred = useDeferredValue(appSection);
  const deferredSection = compact ? appSection : deferred;

  const handleSelectWorkspace = (id: string): void => {
    // If the workspace has unread mentions, jump straight to the latest
    // source message — that is what the user clicked the badge to see.
    // Falls back to the
    // remembered/default channel pick when there's no pointer.
    const pointer = unreadCounts?.latest_by_workspace?.[id];
    if (pointer) {
      handleJumpToMessage({
        channelId: pointer.conversation_id,
        messageId: pointer.message_id,
        // Carry the reply cluster's root when the newest unread is a thread
        // reply — otherwise the jump anchors the reply id against the mainline
        // stream (404) and never enters the thread observe/mark-read path, so
        // the badge never clears (#task:1519). `handleSelectChannel` already
        // threads its `threadRootId` through; the workspace-badge entry must
        // match — every unread-pointer jump carries `thread_root_id`.
        threadRootId: pointer.thread_root_id ?? undefined,
        workspaceId: id,
      });
      return;
    }
    nav.toWorkspace(id);
  };

  const handleSelectChannel = (
    id: string,
    workspaceId: string | null = selectedWorkspaceId,
  ): void => {
    if (!workspaceId) {
      return;
    }
    if (compact) {
      nav.toChannel(workspaceId, id);
      return;
    }
    const sameWorkspace = workspaceId === selectedWorkspaceId;
    // A=Activity-follows-channel (#task:1189): while an agent pane is open,
    // switching channel keeps it and just re-points the locked Activity
    // context — not restore that channel's remembered panes (which would drop
    // the agent pane). Only
    // same-workspace: a cross-workspace open switches workspace anyway and the
    // docked agent panel can't follow it.
    if (sameWorkspace && agentPanelId) {
      nav.go(channelUrlKeeping(workspaceId, id, { agent: agentPanelId }));
      return;
    }
    // Pane visibility effects advance read cursors. Keeping
    // this handler side-effect free avoids duplicate mark-read/refetch storms.
    // Channel-open precedence (#task:1082): a fresh unread reply in one
    // of this channel's threads (newer than the explicit-close
    // watermark) auto-surfaces that thread; otherwise restore the
    // channel's remembered panes.
    const intent = resolveChannelOpen(
      messageUnreadData?.latest_by_conversation?.[id],
      getCloseWatermark(id),
      getChannelPanes(id),
    );
    if (intent.kind === "unread-thread") {
      handleJumpToMessage({
        channelId: id,
        messageId: intent.messageId,
        threadRootId: intent.threadRootId,
        workspaceId,
      });
    } else {
      nav.go(paneUrl(routes.chatChannel(workspaceId, id), intent.panes ?? []));
    }
  };

  const handleOpenChannelByName = useCallback(
    (workspaceId: string, channelId: string) => {
      // Preserve the docked agent panel across a `#<channel>` jump too, so A
      // stays consistent: clicking a channel ref re-points the panel's Activity
      // context instead of closing it (#task:1189).
      if (agentPanelId) {
        nav.go(channelUrlKeeping(workspaceId, channelId, { agent: agentPanelId }));
        return;
      }
      nav.toChannel(workspaceId, channelId);
    },
    [agentPanelId, nav.go, nav.toChannel],
  );

  const handleMessageAgent = useCallback(
    (agentId: string): void => {
      const existing = findAgentDirectPair(read.directPairs, agentId);
      if (existing) {
        nav.toDirect(existing.conversation_id);
        return;
      }
      nav.toDirectDraft({ kind: "agent", id: agentId });
    },
    [read.directPairs, nav.toDirect, nav.toDirectDraft],
  );

  const handleDraftOpenedPair = useCallback(
    (conversationId: string): void => {
      nav.go(routes.chatDirect(conversationId), { replace: true });
    },
    [nav.go],
  );

  return {
    chat: {
      route,
      nav,
      appSection,
      view,
      selectedWorkspaceId,

      directDraftTarget,
      workspaceSidebarPane,

      workspaces: read.workspaces,
      workspacesLoading: read.workspacesLoading,
      agentsById,

      savedItems,

      attentionBadges,
      resolveNavWorkspaceId: read.resolveNavWorkspaceId,
      deferredSection,
      handleSelectWorkspace,
      handleSelectChannel,
      handleJumpToMessage,

      sidebarSelectedConversationId: selectedChannelId,
      activePair,
      openBeside,
      openPair,
      closePairHalf,
    },
    activity: {
      appSection,
      selectedChannelId,
      agentsById,
      workspaceRefById: read.workspaceRefById,
      deferredSection,
      handleJumpToMessageStable,
    },
    content: conversationContent({
      // The route's view switches channels a render late (`deferredChannelId`).
      // Until it does, the previous channel keeps its own panes: the next
      // channel's beside it would narrow it and mount a beat before their own
      // conversation, so each column would settle twice.
      route: read.deferredChannelId === selectedChannelId ? route : deferredRoute,
      read,
      state: {
        channelIndexesAvailable: panes.channelIndexesAvailable,
        taskPaneId: panes.taskPaneId,
        parentJumpTarget: panes.parentJumpTarget,
        parentJumpPrimePending: panes.parentJumpPrimePending,
        jump,
      },
      commands: {
        requestJump,
        departPrimaryPane,
        handleOpenAdvancedSearch,
        openTaskInPane: panes.openTaskInPane,
        setTaskPaneEditing: panes.setTaskPaneEditing,
        requestCloseTaskPane: panes.requestCloseTaskPane,
        handleOpenChannelByName,
        handleJumpToMessage,
        handleMessageAgent,
        handleDraftOpenedPair,
        handleJumped,
        handleOpenThreadFromChannel: panes.handleOpenThreadFromChannel,
        openConversationPane: nav.openConversationPane,
        openThread: nav.openThread,
        toggleThreadsList: nav.toggleThreadsList,
        toggleFilesList: nav.toggleFilesList,
        openAttachmentPanel: nav.openAttachmentPanel,
        openChannelMembers: nav.openChannelMembers,
        closePane: nav.closePane,
        backPane: nav.backPane,
        movePane: nav.movePane,
        closeRouteConversation: panes.closeRouteConversation,
      },
    }),
    app: {
      toggleWorkspaceSidebar,
      nav,
      appSection,
      workspaceSidebarVisible,
      selectedWorkspace: read.selectedWorkspace,
      openAgentConversation,
      deferredSection,
      handleJumpToMessage,
      handleMessageAgent,
      resolveNavWorkspaceId: read.resolveNavWorkspaceId,
    },
  };
}
