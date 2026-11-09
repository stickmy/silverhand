import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "@/lib/toast";
import { t } from "@/lib/i18n";
import type { ThreadFollowerStateDto, ThreadFollowMutationDto } from "@silverhand/shared";
import { apiPaths } from "@silverhand/shared";
import { api } from "@/lib/api";
import { channelThreadsKey } from "@/features/channels/use-channel-threads";
import { conversationUnreadKey } from "@/features/channels/message-unread";
import { useInterest } from "@/features/realtime/interest-registry";
import { wsConversationScope } from "@/features/realtime/conversation-scope";
import { threadFollowersKey } from "@/features/channels/message-cache-core";

/** Human follower state for one reply cluster: whether the current viewer
 *  is subscribed, plus the agent followers still visible in the parent
 *  channel. Drives the ThreadPanel follow toggle / context-menu unfollow
 *  (item 5) and the activity-bubble follower filter (item 4).
 *
 *  Backed by `GET /api/channels/:id/threads/:rootMessageId/followers`. The
 *  root id may be the root message or any reply in the cluster — the
 *  backend normalizes it. */
export function useThreadFollowers(channelId: string | null, rootMessageId: string | null) {
  const enabled = !!channelId && !!rootMessageId;
  const key = threadFollowersKey(channelId ?? "none", rootMessageId ?? "none");
  const query = useQuery<ThreadFollowerStateDto>({
    queryKey: key,
    enabled,
    queryFn: ({ signal }) =>
      api<ThreadFollowerStateDto>(apiPaths.channelThreadFollowers(channelId!, rootMessageId!), {
        signal,
      }),
  });

  // The follower set is mutated as a SIDE EFFECT of thread messages:
  // backend auto-follows the author / target / @-mentioned agents on each
  // reply (reply / mention / target_author paths). So this snapshot must
  // track the thread message stream — without it, an open panel keeps a
  // stale `follower_agent_ids` (wrong activity-bubble allow-list) and stale
  // `viewer_following` after someone replies / @-mentions in the cluster.
  //
  // Pure reader (#task:1255 P1b): the central ingestor owns the invalidation
  // (a reply to this root, or a reconnect snapshot_required, refetches the
  // follower set), gated on this followers reader being mounted. This hook only
  // declares INTEREST in the channel scope.
  // #task:1260 P2: interest routes through the registry (`useInterest`) instead
  // of a direct subscribeWsScope useEffect — identical lifecycle, single
  // authority. The compound `enabled` gate (channelId && rootMessageId) maps to
  // the null-scope form; not a correctness boundary (transport retains floor on
  // unsubscribe, resume-replays on re-enable, #1251).
  useInterest(enabled && channelId ? wsConversationScope(channelId) : null);

  return query;
}

/** Subscribe the current human viewer to a reply cluster
 *  (`POST .../follow`). Idempotent server-side; we refetch follower state
 *  on success so the toggle + bubble reflect the new membership. */
export function useFollowThread(channelId: string | null, rootMessageId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async () => {
      if (!channelId || !rootMessageId) throw new Error("no thread selected");
      return api<ThreadFollowMutationDto>(apiPaths.channelThreadFollow(channelId, rootMessageId), {
        method: "POST",
      });
    },
    onSuccess: () => {
      if (!channelId || !rootMessageId) return;
      void qc.invalidateQueries({
        queryKey: threadFollowersKey(channelId, rootMessageId),
      });
      // #task:1773: (un)following changes `followed ∩ unread` membership. The
      // backend publishes a viewer `unread_updated` on a real follow change
      // (`conversation-message-service` follow/unfollow wrappers), so the filtered
      // list + header dot also converge via that WS event (the eventual path,
      // shared with remote devices). This local invalidate is the ACK-side
      // IMMEDIATE path — it converges this device before the event round-trips and
      // even if the WS is disconnected. `channelThreadsKey` is a prefix-invalidate
      // (reaches the filtered child); `conversationUnreadKey` refreshes the dot.
      void qc.invalidateQueries({ queryKey: conversationUnreadKey(channelId) });
      void qc.invalidateQueries({ queryKey: channelThreadsKey(channelId) });
    },
    onError: () => {
      toast.error(t("conversationShell.followFailed"));
    },
  });
}

/** Unsubscribe the current human viewer from a reply cluster
 *  (`DELETE .../follow`). A later reply / mention re-follows automatically
 *  (backend behavior) — unfollow only stops delivery, not visibility. */
export function useUnfollowThread(channelId: string | null, rootMessageId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async () => {
      if (!channelId || !rootMessageId) throw new Error("no thread selected");
      return api<ThreadFollowMutationDto>(apiPaths.channelThreadFollow(channelId, rootMessageId), {
        method: "DELETE",
      });
    },
    onSuccess: () => {
      if (!channelId || !rootMessageId) return;
      void qc.invalidateQueries({
        queryKey: threadFollowersKey(channelId, rootMessageId),
      });
      // #task:1773: unfollow drops this root out of `followed ∩ unread`. The
      // backend publishes a viewer `unread_updated` on a real follow change
      // (`conversation-message-service` follow/unfollow wrappers), so the filtered
      // list + header dot also converge via that WS event (the eventual path,
      // shared with remote devices). This local invalidate is the ACK-side
      // IMMEDIATE path — it converges this device before the event round-trips and
      // even if the WS is disconnected. `channelThreadsKey` is a prefix-invalidate
      // (reaches the filtered child); `conversationUnreadKey` refreshes the dot.
      void qc.invalidateQueries({ queryKey: conversationUnreadKey(channelId) });
      void qc.invalidateQueries({ queryKey: channelThreadsKey(channelId) });
    },
    onError: () => {
      toast.error(t("conversationShell.unfollowFailed"));
    },
  });
}
