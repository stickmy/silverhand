import { useMutation, useQueryClient } from "@tanstack/react-query";
import type { MessageSeq } from "@silverhand/shared";
import { api } from "@/lib/api";
import { channelThreadsKey } from "@/features/channels/use-channel-threads";
import {
  mainlineStreamSourceHead,
  postObserve,
  streamSourceAtSeq,
} from "@/features/attention/observe";
import {
  reconcileUnreadAfterObserveIfRealtimeUnavailable,
  settleStreamUnreadAfterObserve,
} from "@/features/attention/observe-cache-fallback";

/**
 * Clear a CONVERSATION's notification badges (read_at-only) — the
 * notification-surface / focused-conversation cleanup path (#task:1231). Renamed
 * from `useMarkChannelRead`: it does NOT advance the read cursor (that is
 * `observe(stream)` via `useMarkMessageChannelRead`), it clears the conversation's
 * notification badges, INCLUDING anchorless ones (`agent_start_failed`) that the
 * stream-cursor overlap can't reach (overlap only covers `mention_self` with a
 * seq). This stays the single owner of conversation-level notification bulk;
 * `read_at` legitimately has two writers (this surface + `observe`'s overlap),
 * but the cursor has exactly one (`observe`). Name == behavior: notification
 * badges, never a cursor.
 */
export function useMarkConversationNotificationsRead() {
  return useMutation({
    mutationFn: async (channelId: string) => {
      await api("/api/notifications/read", {
        method: "POST",
        body: JSON.stringify({ conversation_id: channelId }),
      });
    },
  });
}

/**
 * Advance the viewer's read cursor for a channel (mainline) to its latest
 * message — the §5 `observe(stream)` mainline path (#task:1231). `stream_head`
 * is server-resolved (channel open / jump knowingly accept concurrent
 * tail-swallow, §2); a channel carries no thread root. Backed by the
 * conversation_reads cursor — clears message unread badges.
 */
export function useMarkMessageChannelRead() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (params: string | { id: string; snapshotSeq?: MessageSeq }) => {
      const channelId = typeof params === "string" ? params : params.id;
      return postObserve([
        typeof params !== "string" && params.snapshotSeq !== undefined
          ? streamSourceAtSeq(channelId, null, params.snapshotSeq)
          : mainlineStreamSourceHead(channelId),
      ]);
    },
    onSuccess: (result, params) => {
      reconcileUnreadAfterObserveIfRealtimeUnavailable(qc, result);
      // Settle this client's per-stream `conversationUnread` at once via the
      // single seam (#task:1402) — symmetric with the thread path. Today no
      // surface renders `conversationUnread.mainline`, so this is a narrow no-op
      // settle of the same key, not UI built for mainline; if one ever does, it
      // inherits the immediate settle for free instead of the #task:1400 window.
      const channelId = typeof params === "string" ? params : params.id;
      settleStreamUnreadAfterObserve(
        qc,
        [
          typeof params !== "string" && params.snapshotSeq !== undefined
            ? streamSourceAtSeq(channelId, null, params.snapshotSeq)
            : mainlineStreamSourceHead(channelId),
        ],
        result,
      );
    },
    // No onSuccess message-unread invalidate while event WS is connected
    // (#task:1284). observe's post-commit enriched `unread_updated` message
    // variant is fanned to EVERY viewer — including THIS client that issued the
    // observe (no actor exclusion / no origin-connection suppression, backend
    // contract) — and the viewer-unread ingestor patches the badge caches to 0
    // in place. Invalidating on the healthy path would defeat that with a
    // whole-list refetch (the exact regression the projection replaces). The
    // onSuccess fallback above only runs while event WS is not connected, when
    // that post-commit frame cannot reach this client.
    // Absolute-value payloads are idempotent, so HTTP-return vs WS-frame arrival
    // order can't leave a stale badge.
  });
}

/**
 * Advance the viewer's read cursor for ONE reply-cluster (thread) to the
 * snapshot the viewer actually saw (#task:1226, surface 4) — the §5
 * `observe(stream)` thread path (#task:1231). A thread MUST use `at_seq` (the
 * open-time `stream_head_seq` from `useThreadCluster`): `streamSourceAtSeq` makes
 * a thread `stream_head` unrepresentable here, so the §9.10 invariant — AUTO
 * paths (open / live) pin `at_seq` and never knowingly tail-swallow — is enforced
 * STRUCTURALLY at the caller layer. §9.10 guards the caller-intent axis, not the
 * stream-shape axis: the backend can't tell intent from a single request (it no
 * longer rejects thread + `stream_head`, `9c8c9aa`), so the guard lives here. The
 * cursor advances only to what was rendered, so a reply that landed after open
 * stays unread.
 *
 * Human-attention only: this writes the viewer's `conversation_reads` cursor
 * and nothing else. It does NOT touch thread_followers / delivery / wake.
 */
export function useMarkThreadRead() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (params: {
      channelId: string;
      threadRootId: string;
      snapshotSeq: MessageSeq;
    }) => {
      return postObserve([
        streamSourceAtSeq(params.channelId, params.threadRootId, params.snapshotSeq),
      ]);
    },
    onSuccess: (result, params) => {
      reconcileUnreadAfterObserveIfRealtimeUnavailable(qc, result);
      // message-unread badge (sidebar + global roll-up, incl. followed-thread
      // unread) is cleared by observe's post-commit enriched `unread_updated`
      // patch via the viewer-unread ingestor (#task:1284) — NO invalidate here
      // (it would force a whole-list refetch and defeat the projection).
      //
      // The root reply chip / Threads-list "N new" are a DIFFERENT read-model:
      // per-stream `conversationUnread`, not carried by the enriched
      // `unread_updated` roll-up. Settle it locally via the single seam so this
      // client's chip re-settles from REST at once (#task:1400 window, absorbed
      // into the #task:1402 seam — do not hand-write the invalidate here).
      settleStreamUnreadAfterObserve(
        qc,
        [streamSourceAtSeq(params.channelId, params.threadRootId, params.snapshotSeq)],
        result,
      );
      // The Threads-list thread METADATA (reply count etc.) is a SEPARATE
      // read-model that only invalidates on stream message / connected, never on
      // `unread_updated`, so it still needs this explicit invalidate.
      qc.invalidateQueries({ queryKey: channelThreadsKey(params.channelId) });
    },
  });
}
