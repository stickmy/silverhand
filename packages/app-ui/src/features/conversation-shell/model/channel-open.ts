/**
 * When the user switches into a channel, three signals compete for its
 * panes. The precedence was locked with @Cindy:
 *
 *   1. **unread-target** — a fresh reply landed in one of the channel's
 *      threads (the unread pointer carries a `thread_root_id`) and it
 *      post-dates the explicit-close watermark (`seq > watermark`).
 *      Surface that thread and locate the message.
 *   2. **pane memory** — otherwise restore the channel's remembered panes.
 *   3. **main stream** — no remembered panes opens the bare channel.
 *
 * The close-watermark makes an explicit close the stronger intent: the
 * unread batch you just dismissed is NOT yanked back on re-entry, but a
 * genuinely newer reply (higher `seq`) still re-surfaces. The resolver is
 * pure so the precedence is unit-tested independent of React/nav wiring.
 */

import type { MessageUnreadPointerDto, MessageSeq } from "@silverhand/shared";
import type { Panes } from "@/navigation/panes";

export type ChannelOpenIntent =
  | { kind: "unread-thread"; threadRootId: string; messageId: string }
  | { kind: "panes"; panes: Panes | null };

export function resolveChannelOpen(
  pointer: MessageUnreadPointerDto | undefined,
  closeWatermark: MessageSeq | null,
  panes: Panes | null,
): ChannelOpenIntent {
  // Same-stream (this channel) MessageSeq compare, guarded on explicit absence:
  // no watermark (null) → any unread thread surfaces; otherwise only a strictly
  // newer pointer re-surfaces past the just-closed batch (#task:1267 §3.4 —
  // explicit absence, not a -1 sentinel).
  if (pointer?.thread_root_id && (closeWatermark === null || pointer.seq > closeWatermark)) {
    return {
      kind: "unread-thread",
      threadRootId: pointer.thread_root_id,
      messageId: pointer.message_id,
    };
  }
  return { kind: "panes", panes };
}
