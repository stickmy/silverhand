import { useCallback, useEffect, useMemo, useRef } from "react";
import { usePageLive } from "@/navigation/page-stack";
import { useChannelMessages, useMarkMessageChannelRead } from "@/features/channels/hooks";
import { lastCommittedSeq } from "@/features/channels/pending-message";
import { findThreadAnchor, readAnchorMessageId } from "@/features/messages/model/thread-anchor";
import {
  nextTailReadMark,
  observeVisibleTail,
  type TailReadMark,
} from "@/features/conversation-shell/model/channel-read";

/**
 * A channel pane's message window: the query, the thread-anchor row it hides,
 * the prop-driven jump, and the mainline read cursor. `listProps` is the
 * window's half of `MessageList`'s props; the pane adds rendering and actions.
 */
export function useChannelMessageWindow({
  conversationId,
  jumpTarget,
  jumpPrimePending,
  onJumped,
  onThreadAnchorResolved,
  markReadEnabled,
}: {
  conversationId: string;
  jumpTarget: { channelId: string; messageId: string } | null | undefined;
  jumpPrimePending: boolean;
  onJumped: (() => void) | undefined;
  onThreadAnchorResolved: ((originalMessageId: string | null) => void) | undefined;
  /** A member effect: off for a non-member browsing a public channel. */
  markReadEnabled: boolean;
}) {
  const {
    data: messages = [],
    isLoading,
    hasPreviousPage,
    isFetchingPreviousPage,
    fetchPreviousPage,
    hasNextPage,
    isFetchingNextPage,
    fetchNextPage,
    isAtTail,
    hasNewerLiveGap,
    resetToLatest,
    windowRows,
    trimWindow,
    replyCountsByRoot,
    replySummariesByRoot,
    replyPreviewsByRoot,
  } = useChannelMessages(conversationId);

  // Advance the message read cursor as the visible mainline tail advances
  // (#task:1255 P1c). ChannelView is rendered only when the channel is visible,
  // but read advancement is VISIBILITY-driven, not frame-ARRIVAL-driven
  // (#task:1244 / #task:1251 read-inert axis) — so we mark read off the mainline
  // CACHE, not an `onWsFrame` handler (the hook is now a
  // pure reader; gate-(b)). We mark only when the window is genuinely at the
  // tail (`isAtTail && !hasNewerLiveGap`) and the newest mainline row actually
  // advanced. By construction `useChannelMessages` keeps reply-only rows OUT of
  // `messages` (they bump a badge, not the list) and does NOT append while the
  // window is off-tail (it sets the gap instead) — so scrolled-in-history and
  // reply-only frames never mark the mainline read. This is a deliberate read
  // tightening over the old "any message frame → markRead" (which marked on
  // arrival even off-tail / reply-only): see the dogfood handoff note.
  const markRead = useMarkMessageChannelRead();
  const live = usePageLive();
  // Gate off the newest COMMITTED row — a pending optimistic tail has no
  // MessageSeq, and mark-read must advance on a real ordinal (§3.3/§3.5).
  const tailSeq = lastCommittedSeq(messages);
  const lastMarkedRef = useRef<TailReadMark | null>(null);

  useEffect(() => {
    // Mark-read is a member effect: a non-member browsing a public channel must
    // never advance a read cursor / write read-state (`markReadEnabled`).
    // This is a BEHAVIOR gate, not a hidden control — the auto-effect itself is
    // suppressed, not merely a button.
    if (!markReadEnabled || !live) return;
    const next = nextTailReadMark(
      lastMarkedRef.current,
      conversationId,
      tailSeq,
      isAtTail,
      hasNewerLiveGap,
    );
    if (!next) return;
    return observeVisibleTail(
      next,
      (mark) => markRead.mutateAsync({ id: mark.channelId, snapshotSeq: mark.seq }),
      (mark) => {
        lastMarkedRef.current = mark;
      },
    );
    // mutateAsync is stable; cleanup cancels recovery if this tail is no longer visible.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [conversationId, tailSeq, isAtTail, hasNewerLiveGap, markReadEnabled, live]);

  // The thread anchor row: surfaced in the header as "Thread — #parent" with a
  // jump back, and filtered out of the list below.
  const anchor = useMemo(() => findThreadAnchor(messages), [messages]);
  const anchorMessageId = anchor ? readAnchorMessageId(anchor.metadata) : null;

  // Tell the parent pane which original message this thread is anchored
  // to so it can highlight that row. Re-fires on anchor/id change;
  // cleanup clears the highlight when this view unmounts.
  useEffect(() => {
    if (!onThreadAnchorResolved) return;
    onThreadAnchorResolved(anchorMessageId);
    return () => onThreadAnchorResolved(null);
  }, [onThreadAnchorResolved, anchorMessageId]);

  const visibleMessages = useMemo(
    () => (anchor ? messages.filter((m) => m.id !== anchor.id) : messages),
    [messages, anchor],
  );
  const loadEarlierMessages = useCallback(() => {
    if (!hasPreviousPage || isFetchingPreviousPage) return;
    // Scroll and startReached can arrive before React commits the fetching
    // flag. Let the query deduplicate them rather than cancel/restart the page.
    void fetchPreviousPage({ cancelRefetch: false });
  }, [fetchPreviousPage, hasPreviousPage, isFetchingPreviousPage]);
  const loadLaterMessages = useCallback(() => {
    if (!hasNextPage || isFetchingNextPage) return;
    void fetchNextPage({ cancelRefetch: false });
  }, [fetchNextPage, hasNextPage, isFetchingNextPage]);

  // Resolve the prop-driven jump target — fires when the global Saved
  // view or the Tasks panel hands us a `{channelId, messageId}` for a
  // message inside this channel. Cleared by `onJumped` once the
  // message-list scroll completes.
  const effectiveJumpMessageId =
    jumpTarget && jumpTarget.channelId === conversationId ? jumpTarget.messageId : null;
  const handleMessageJumped = useCallback(() => {
    if (jumpTarget && jumpTarget.channelId === conversationId) {
      onJumped?.();
    }
  }, [conversationId, jumpTarget, onJumped]);

  return {
    anchor,
    anchorMessageId,
    visibleMessages,
    effectiveJumpMessageId,
    listProps: {
      messages: visibleMessages,
      isLoading,
      hasMoreBefore: !!hasPreviousPage,
      isFetchingEarlier: isFetchingPreviousPage,
      onLoadEarlier: loadEarlierMessages,
      hasMoreAfter: !!hasNextPage,
      isFetchingLater: isFetchingNextPage,
      onLoadLater: loadLaterMessages,
      windowRows,
      onTrimWindow: trimWindow,
      jumpToMessageId: effectiveJumpMessageId,
      // Only this pane's prop-driven jump (the App-orchestrated saved/task
      // jump) goes through the prime path; activity-strip jumps stay local,
      // since those messages are recent and already cached.
      jumpPrimePending: jumpPrimePending && !!jumpTarget && jumpTarget.channelId === conversationId,
      onJumped: handleMessageJumped,
      isAtTail,
      hasNewerLiveGap,
      onResetToLatest: resetToLatest,
      replyCountsByRoot,
      replySummariesByRoot,
      replyPreviewsByRoot,
    },
  };
}
