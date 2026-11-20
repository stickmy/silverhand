import { useEffect } from "react";
import type { ServerFrame } from "@silverhand/shared";
import { useChannelDetail } from "@/features/workspaces/hooks";
import { onWsFrame, subscribeWsScope } from "@/lib/ws-client";
import { wsConversationSideScope } from "@/features/realtime/conversation-scope";

/** A settings inspector remains live even when opened for a conversation pane. */
export function refreshesChannelSettings(
  frame: ServerFrame,
  channelId: string,
  workspaceId: string | null,
): boolean {
  const side = wsConversationSideScope(channelId);
  const workspace = workspaceId ? `workspace:${workspaceId}` : null;
  if (frame.type === "snapshot_required") return frame.scope === side || frame.scope === workspace;
  if (frame.type !== "event") return false;
  return (
    (frame.scope === side && frame.kind === "conversation_members_changed") ||
    (frame.scope === workspace &&
      (frame.kind === "workspace_channels_changed" || frame.kind === "workspace_members_changed"))
  );
}

export function useChannelSettingsDetail(channelId: string) {
  const detail = useChannelDetail(channelId);
  const workspaceId = detail.data?.channel.workspace_id ?? null;
  const { refetch } = detail;
  useEffect(() => {
    const scopes = [
      wsConversationSideScope(channelId),
      ...(workspaceId ? [`workspace:${workspaceId}`] : []),
    ];
    const subscriptions = scopes.map(subscribeWsScope);
    const unsubscribe = onWsFrame((frame) => {
      if (refreshesChannelSettings(frame, channelId, workspaceId)) void refetch();
    });
    return () => {
      unsubscribe();
      for (const subscription of subscriptions) subscription.unsubscribe();
    };
  }, [channelId, workspaceId, refetch]);
  return detail;
}
