import { useEffect, useMemo, useRef } from "react";
import { useQueries } from "@tanstack/react-query";
import type { MenuBarConversation } from "@silverhand/client";
import { getPlatform } from "@/platform";
import { useNavActions } from "@/router";
import { useLocale } from "@/lib/i18n";
import { useAttentionModesAcrossWorkspaces } from "@/features/attention/hooks";
import { channelNodeBadge } from "@/features/attention/row-badges";
import { loudDirectPairs } from "@/features/app-shell/app-badge-total";
import { messageUnreadByConversationQuery } from "@/features/channels/message-unread";
import { presentCounterpart } from "@/features/direct-pairs/counterpart-presentation";
import { useDirectPairsList } from "@/features/direct-pairs/hooks";
import { useChannelsAcrossWorkspaces, useWorkspaces } from "@/features/workspaces/hooks";

type MenuBarTarget = MenuBarConversation & { workspaceId: string | null };

/**
 * Feeds the system menu bar the conversations with unread messages and opens
 * the one picked there. A conversation is listed when its sidebar row shows a
 * loud badge, with that badge's count, so a muted channel stays out of the list
 * as it stays quiet in the sidebar. Rendered only where the host has a menu bar.
 */
export function MenuBarSync(): null {
  const nav = useNavActions();
  const locale = useLocale();
  const workspaces = useWorkspaces();
  const pairs = useDirectPairsList();

  const liveWorkspaces = useMemo(
    () => (workspaces.data ?? []).filter((w) => w.archived_at === null),
    [workspaces.data],
  );
  const workspaceIds = useMemo(() => liveWorkspaces.map((w) => w.id), [liveWorkspaces]);
  const channelLists = useChannelsAcrossWorkspaces(workspaceIds);
  const unreadLists = useQueries({
    queries: workspaceIds.map((id) => messageUnreadByConversationQuery(id)),
  });
  const { byChannel: attentionByChannel } = useAttentionModesAcrossWorkspaces(workspaceIds);

  const targets: MenuBarTarget[] = [];
  liveWorkspaces.forEach((workspace, index) => {
    const unread = unreadLists[index]?.data?.by_conversation ?? {};
    for (const channel of channelLists[index]?.data ?? []) {
      if (channel.archived_at !== null) {
        continue;
      }

      const badge = channelNodeBadge(
        channel,
        attentionByChannel.get(channel.id),
        unread[channel.id] ?? 0,
      );
      if (badge?.tier === "loud") {
        targets.push({
          id: channel.id,
          title: channel.title,
          project: workspace.name,
          count: badge.count,
          workspaceId: workspace.id,
        });
      }
    }
  });
  for (const { pair, count } of loudDirectPairs(pairs.data?.direct_pairs ?? [])) {
    targets.push({
      id: pair.conversation_id,
      title: presentCounterpart(pair.counterpart).label,
      count,
      workspaceId: null,
    });
  }
  targets.sort((a, b) => b.count - a.count || a.title.localeCompare(b.title));

  const targetsRef = useRef(targets);
  targetsRef.current = targets;

  // Main keeps the last list it was sent, so only a change needs a send.
  const signature = JSON.stringify(targets);
  useEffect(() => {
    getPlatform().menuBar?.setConversations(
      targetsRef.current.map(({ id, title, project, count }) => ({ id, title, project, count })),
    );
  }, [signature, locale]);

  useEffect(() => {
    return getPlatform().menuBar?.onOpenConversation((conversationId) => {
      const target = targetsRef.current.find((t) => t.id === conversationId);
      if (!target) {
        return;
      }
      nav.openConversation(target.workspaceId, target.id);
    });
  }, [nav]);

  return null;
}
