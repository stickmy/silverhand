import { type RefObject, useCallback, useEffect, useRef, useState } from "react";
import type { MessageSeq } from "@silverhand/shared";
import { useMarkThreadRead } from "@/features/channels/hooks";
import type { CachedMessage } from "@/features/channels/pending-message";
import { usePageLive } from "@/navigation/page-stack";
import {
  threadLiveMarkReadSeq,
  threadMarkReadSnapshot,
  threadOpenNeedsLatestRefetch,
  threadTailIsVisible,
  threadVisibleReplySeq,
} from "./model/thread-mark-read";

/** The cluster facts this hook reads; `useThreadCluster` returns them. */
interface ThreadMarkReadCluster {
  isFetching: boolean;
  streamHeadSeq: MessageSeq | null;
  windowFetchSucceededAt: number;
  refetch: () => unknown;
}

/** The thread panel's read cursor: the open-time mark, the visible-live mark
 *  while the tail is on screen, and the flush of a seen tail when the open
 *  ends. Writes only the viewer's read cursor, never delivery or wake.
 *  `scrollTrigger` is any value that changes when the reader scrolls toward
 *  or away from the tail, so the visible-live gate re-samples the viewport. */
export function useThreadMarkRead({
  channelId,
  rootMessageId,
  jumpMessageId,
  canParticipate,
  cluster,
  messages,
  isAtTail,
  scrollRef,
  scrollTrigger,
}: {
  channelId: string;
  rootMessageId: string;
  jumpMessageId: string | null;
  canParticipate: boolean;
  cluster: ThreadMarkReadCluster;
  messages: CachedMessage[];
  isAtTail: boolean;
  scrollRef: RefObject<HTMLElement | null>;
  scrollTrigger: unknown;
}): void {
  const markThreadRead = useMarkThreadRead();
  const live = usePageLive();
  const liveRef = useRef(live);
  liveRef.current = live;

  // Mark this reply-cluster read once per open (#task:1226 surface 4, §9/§9.10).
  // Opening a thread IS reading it (read-to-clear whole opened thread), so we
  // advance the viewer's per-thread `conversation_reads` cursor to the
  // OPEN-TIME stream head. The snapshot must come from THIS open's window fetch
  // — not the previous open's cache React Query serves synchronously on reopen,
  // and not a `setQueryData` update. `threadMarkReadSnapshot` enforces that via
  // `windowFetchSucceededAt >= openedAt`.
  //
  // `openedAt` / the once-guard / "was this a jump open" are captured at
  // RENDER time (the earliest point — before effects, before a fast localhost
  // `primeAround` could resolve), keyed by (channelId, rootMessageId). The
  // once-guard is therefore re-armed per open, never a permanent per-root lock,
  // so a stale cache can't lock it before the fresh fetch.
  // Human-attention only — writes the read cursor, never delivery/wake.
  const openKeyRef = useRef("");
  const openedAtRef = useRef(0);
  const markedForOpenRef = useRef(false);
  // Thread mark-read forward-only gates. `null` = "nothing marked / in flight
  // yet" — explicit absence, not a 0 sentinel (#task:1267 Axis-B: never collapse
  // "none" into value 0). Both hold a MessageSeq for THIS thread stream.
  const markInflightSeqRef = useRef<MessageSeq | null>(null);
  const lastMarkedSeqRef = useRef<MessageSeq | null>(null);
  const liveMarkTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // The latest tail seq that PASSED the visible-live gate but has not been marked
  // yet — the debounce may still be pending. Kept in sync with the gate by the
  // visible-live effect (set to the gated seq, or null the moment the gate closes
  // — scrolled up / backgrounded). Read only by the close/switch flush below, so a
  // seen-but-not-yet-debounced reply still advances the cursor when the panel is
  // closed within the 300ms window, WITHOUT ever swallowing a tail the user
  // scrolled away from (that path already nulled this ref).
  const liveGatedSeqRef = useRef<MessageSeq | null>(null);
  const isJumpOpenRef = useRef(false);
  const openKey = `${channelId}::${rootMessageId}`;
  if (openKeyRef.current !== openKey) {
    openKeyRef.current = openKey;
    openedAtRef.current = Date.now();
    markedForOpenRef.current = false;
    markInflightSeqRef.current = null;
    lastMarkedSeqRef.current = null;
    // Whether THIS open began as a first-unread jump. Captured once at open so
    // a later `clearJump` (which nulls `jumpMessageId`) can't flip it and
    // trigger a latest refetch that clobbers the around-window mid-read.
    isJumpOpenRef.current = jumpMessageId != null;
  }

  // Issue a mark-read to `seq`: forward-only, once per seq. `lastMarkedSeqRef`
  // advances on mutation SUCCESS only — a pending/failed mark must not look
  // cleared locally while the server cursor hasn't moved. The in-flight guard
  // collapses re-fires while one is pending; on error the cursor is untouched so
  // the next trigger retries. Human-attention only — writes the viewer's read
  // cursor, never delivery/wake.
  const markThreadReadMutate = markThreadRead.mutate;
  const issueMarkRead = useCallback(
    (seq: MessageSeq, onMarked?: () => void) => {
      // Mark-read is a member effect: a non-member browsing a
      // public channel's thread must never advance thread read-state. Behavior
      // gate, not a hidden control — the auto-effect itself is suppressed.
      if (!canParticipate) return;
      // Same-stream (this thread) MessageSeq compares, guarded on explicit
      // absence: a null gate ("nothing marked / none in flight") never blocks
      // the first mark (#task:1267 Axis-B).
      if (lastMarkedSeqRef.current !== null && seq <= lastMarkedSeqRef.current) return;
      if (markInflightSeqRef.current !== null && seq <= markInflightSeqRef.current) return;
      markInflightSeqRef.current = seq;
      markThreadReadMutate(
        { channelId, threadRootId: rootMessageId, snapshotSeq: seq },
        {
          onSuccess: () => {
            if (lastMarkedSeqRef.current === null || seq > lastMarkedSeqRef.current)
              lastMarkedSeqRef.current = seq;
            onMarked?.();
          },
          onSettled: () => {
            if (markInflightSeqRef.current === seq) markInflightSeqRef.current = null;
          },
        },
      );
    },
    [channelId, rootMessageId, markThreadReadMutate, canParticipate],
  );

  // §9.10: drive this open's window fetch explicitly (the query itself has
  // `refetchOnMount: false`). A NORMAL open forces a fresh latest window so its
  // mark snapshot is from this open and the gate opens. A JUMP open does NOT
  // refetch — `primeAround` already fetched the authoritative around-first-unread
  // window (and stamped the freshness epoch); a target-less latest refetch here
  // would clobber it. Keyed on (channelId, rootMessageId) so it runs once per
  // open, NOT on `jumpMessageId` (which clears after locate).
  useEffect(() => {
    if (threadOpenNeedsLatestRefetch({ isJumpOpen: isJumpOpenRef.current })) {
      void cluster.refetch();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [channelId, rootMessageId]);

  // OPEN-TIME mark (read-to-clear the whole opened thread): advance to the
  // open-time stream head, gated by `threadMarkReadSnapshot` to THIS open's
  // fresh window fetch. The once-guard `markedForOpenRef` is armed on mutation
  // SUCCESS only — NOT optimistically — so a failed open-time mark stays
  // retryable: `issueMarkRead` is forward-only + in-flight-guarded, so re-running
  // this effect while the mark is pending is a no-op, and on failure the guard
  // is never armed, so the next dep change (a new reply bumps `streamHeadSeq`, a
  // refetch bumps `windowFetchSucceededAt`) retries. The visible-live effect
  // below independently covers the pinned-tail case.
  useEffect(() => {
    if (markedForOpenRef.current || !live) return;
    const snapshot = threadMarkReadSnapshot({
      alreadyMarkedForOpen: markedForOpenRef.current,
      isFetching: cluster.isFetching,
      headSeq: cluster.streamHeadSeq,
      windowFetchSucceededAt: cluster.windowFetchSucceededAt,
      openedAt: openedAtRef.current,
    });
    if (snapshot === null) return;
    issueMarkRead(snapshot, () => {
      markedForOpenRef.current = true;
    });
  }, [
    channelId,
    rootMessageId,
    cluster.isFetching,
    cluster.streamHeadSeq,
    cluster.windowFetchSucceededAt,
    issueMarkRead,
    live,
  ]);

  // Document visibility as REACTIVE state. The live-mark gate reads visibility,
  // but `document.hidden` read inline isn't reactive — `visibilitychange` fires
  // no React render. Without this, a reply that arrives while the app is
  // backgrounded correctly stays unread, but when the user returns to the
  // foreground (tail reply now on screen) nothing re-runs the
  // gate, so it lingers unread until the next message/scroll/reopen. Mirroring
  // visibility into state makes the return-to-visible transition a dependency
  // that re-fires the live-mark effect.
  const [documentShown, setDocumentVisible] = useState(() => !document.hidden);
  const documentVisible = documentShown && live;
  useEffect(() => {
    const onVisibility = (): void => setDocumentVisible(!document.hidden);
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, []);

  // VISIBLE-LIVE mark: a reply arriving while the panel is open + watched should
  // clear unread without a close/reopen. Follow the read cursor to the newest
  // reply ONLY while the committed DOM viewport reaches the true stream tail
  // AND the window is visible — never swallowing an unrendered tail (scrolled
  // up) or a backgrounded panel. The actual viewport is sampled here instead
  // of trusting `pinBottomRef`: that ref controls follow intent and can be stale
  // after a clamped/no-op jump scroll. `threadLiveMarkReadSeq` is the pure gate;
  // debounced so a
  // burst of replies collapses to one mark at the latest visible seq
  // (max-VISIBLE, not latest-known — `messages` is the loaded
  // window). `lastMarkedSeqRef` (advanced on success) keeps it forward-only.
  // `documentVisible` is a dep so returning to the foreground re-runs the gate.
  // The cursor comes from the REPLY rows only (`threadVisibleReplySeq`): the
  // cluster window also contains the pinned main-line root, whose seq belongs to
  // the channel stream and is beyond this thread's head — posting it would be a
  // `position_future` 400 on a reply-less thread (#task:1992). Null there, so
  // the gate holds and no request is sent.
  const maxLoadedReplySeq = threadVisibleReplySeq(messages, rootMessageId);
  useEffect(() => {
    const viewport = scrollRef.current;
    const seq = threadLiveMarkReadSeq({
      tailVisible:
        viewport !== null &&
        threadTailIsVisible({
          streamAtTail: isAtTail,
          scrollHeight: viewport.scrollHeight,
          scrollTop: viewport.scrollTop,
          clientHeight: viewport.clientHeight,
        }),
      documentVisible,
      maxVisibleReplySeq: maxLoadedReplySeq,
      lastMarkedSeq: lastMarkedSeqRef.current,
    });
    // Track the gated seq for the close/switch flush. When the gate closes
    // (scrolled up / backgrounded → seq null) this clears, so a later close can
    // never flush a tail the user is no longer watching (no-swallow preserved).
    liveGatedSeqRef.current = seq;
    if (seq === null) return;
    if (liveMarkTimerRef.current) clearTimeout(liveMarkTimerRef.current);
    liveMarkTimerRef.current = setTimeout(() => {
      liveMarkTimerRef.current = null;
      // Re-check at fire time: the user may have scrolled up or backgrounded
      // the app during the debounce window.
      const currentViewport = scrollRef.current;
      if (
        currentViewport === null ||
        !threadTailIsVisible({
          streamAtTail: isAtTail,
          scrollHeight: currentViewport.scrollHeight,
          scrollTop: currentViewport.scrollTop,
          clientHeight: currentViewport.clientHeight,
        }) ||
        document.hidden ||
        !liveRef.current
      ) {
        return;
      }
      issueMarkRead(seq);
    }, 300);
    return () => {
      if (liveMarkTimerRef.current) {
        clearTimeout(liveMarkTimerRef.current);
        liveMarkTimerRef.current = null;
      }
    };
  }, [scrollTrigger, documentVisible, maxLoadedReplySeq, isAtTail, issueMarkRead]);

  // Flush a SEEN-but-unmarked tail reply when THIS open ends — the panel closes
  // (unmount) or switches to another thread — before the 300ms visible-live
  // debounce fired. Without this, closing the panel within the debounce window
  // clears the pending timer and the read cursor stays behind the reply the user
  // already saw (the ghost-unread badge, #task:1519). Keyed on `issueMarkRead`,
  // which is itself keyed on (channelId, rootMessageId) — so the cleanup runs on
  // close (unmount) AND on switch-to-another-thread, but NOT on scroll/append.
  // Those change the gate inputs (a different effect) and already keep
  // `liveGatedSeqRef` truthful, so a tail the user scrolled away from is null here
  // and not flushed. `issueMarkRead` is forward-only + in-flight-guarded, so this
  // can only advance the cursor, never swallow the concurrent tail or double-mark.
  useEffect(() => {
    return () => {
      const seq = liveGatedSeqRef.current;
      liveGatedSeqRef.current = null;
      if (seq !== null) issueMarkRead(seq);
    };
  }, [issueMarkRead]);
}
