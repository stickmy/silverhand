import { hasCollapsibleSidebar } from "@/components/layout/sidebar-slot";
import { useCompactLayout } from "@/navigation/layout";
import { conversationContent } from "./conversation-content";
import { singlePane } from "@/navigation/panes";
import { senderFilterToParam } from "@/features/search/sender-filter-select";
import { useAgents } from "@/features/agents/hooks/hooks";
import { useLayoutKeyboardShortcuts } from "@/features/app-shell/effects";
import { useChatRouteRecovery } from "@/features/app-shell/use-chat-route-recovery";
import { findAgentDirectPair } from "@/features/direct-pairs/hooks";
import { useNotificationToasts } from "@/features/attention/notifications";
import { useSavedMessages } from "@/features/messages/saved";
import type { AdvancedSearchSeed } from "@/features/search/advanced-search-seed";
import { useLayoutSettings } from "@/features/conversation-shell/use-layout-settings";
import { channelUrlKeeping, routes, useNav, useRouteState } from "@/router";
import { useCallback, useDeferredValue, useMemo } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { channelWorkspaceOf } from "@/features/channels/channel-workspace-index";
import { conversationRefQueryOptions } from "@/features/conversations/conversation-ref-query";
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
    directPairAuthority: read.directPairAuthority,
    jumpTarget,
    recentlyRemovedPrimaryRef,
    compact,
  });

  const panes = useConversationPanes(read, jump, { compact });

  useNotificationToasts(panes.screen, (target) => handleJumpToMessage(target));

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
    const sameWorkspace = workspaceId === selectedWorkspaceId;
    // A=Activity-follows-channel (#task:1189): while an agent pane is open,
    // switching channel keeps it and just re-points the locked Activity
    // context — not restore that channel's remembered panes (which would drop
    // the agent pane). Only
    // same-workspace: a cross-workspace open switches workspace anyway and the
    // docked agent panel can't follow it.
    if (!compact && sameWorkspace && agentPanelId) {
      nav.go(channelUrlKeeping(workspaceId, id, { agent: agentPanelId }));
      return;
    }
    // Pane visibility effects advance read cursors. Keeping
    // this handler side-effect free avoids duplicate mark-read/refetch storms.
    // An unread thread reply is signalled on its row and the Threads tab, and
    // never reopens a pane.
    nav.toChannel(workspaceId, id);
  };

  const qc = useQueryClient();
  /** A `#conv:` chip opens its conversation in place of the current one. */
  const openConversationRef = (id: string): void => {
    if (read.directPairs.some((p) => p.conversation_id === id)) {
      nav.toDirect(id);
      return;
    }
    const known = channelWorkspaceOf(id);
    if (known) {
      handleSelectChannel(id, known);
      return;
    }
    void qc
      .fetchQuery(conversationRefQueryOptions(id))
      .then((res) => handleSelectChannel(id, res.ref.workspace_id))
      .catch(() => undefined);
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
        openConversationRef,
        openThread: nav.openThread,
        toggleThreadsList: nav.toggleThreadsList,
        toggleFilesList: nav.toggleFilesList,
        openAttachmentPanel: nav.openAttachmentPanel,
        openChannelMembers: nav.openChannelMembers,
        closePane: nav.closePane,
        backPane: nav.backPane,
        movePane: nav.movePane,
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
