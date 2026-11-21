import type { MessageSeq } from "@silverhand/shared";
import type { JumpPhase } from "@/features/channels/message-jump-phase";
import { lastCommittedSeq, type CachedMessage } from "@/features/channels/pending-message";

/**
 * Pure freshness gate for thread mark-read (#task:1226 surface 4, §9).
 *
 * Opening a thread marks its reply-cluster read (read-to-clear), advancing the
 * viewer's per-thread `conversation_reads` cursor to the OPEN-TIME stream head.
 * The hard part is proving the snapshot came from THIS open's window response —
 * not the previous open's cache that React Query serves synchronously on
 * reopen, and not a `setQueryData` mutation (an optimistic live-append) that
 * bumps `dataUpdatedAt` without being a real window read.
 *
 * So the gate keys on `windowFetchSucceededAt` — an epoch stamped ONLY inside
 * the cluster query's success path (unreachable by `setQueryData`) — compared
 * against `openedAt`, captured when the (channel, root) open began. The
 * once-guard (`alreadyMarkedForOpen`) is re-armed per open by the caller, so a
 * stale cache can never lock the guard before a fresh fetch lands.
 *
 * Returns the snapshot seq to mark, or null to wait / skip. Kept pure so the
 * reopen-stale-cache regression is testable without React render timing.
 */
export interface ThreadMarkReadGateInput {
  /** True once this open has already fired mark-read (re-armed per open). */
  alreadyMarkedForOpen: boolean;
  /** A cluster fetch is in flight (initial window or pagination). */
  isFetching: boolean;
  /** `pages[0].stream_head_seq` of the loaded window — may be stale cache.
   *  A MessageSeq (open-time stream head), or null when no window is loaded. */
  headSeq: MessageSeq | null;
  /** Epoch (ms) of the last SUCCESSFUL window fetch; stamped only on the
   *  queryFn success path, never by `setQueryData`. */
  windowFetchSucceededAt: number;
  /** Epoch (ms) captured at the moment this (channel, root) open began. */
  openedAt: number;
}

export function threadMarkReadSnapshot(input: ThreadMarkReadGateInput): MessageSeq | null {
  // Already marked for this open — do not re-fire (idempotent per open).
  if (input.alreadyMarkedForOpen) return null;
  // Wait for any in-flight fetch to settle before reading the head.
  if (input.isFetching) return null;
  // No window loaded yet.
  if (input.headSeq === null) return null;
  // The decisive gate: the loaded window must come from a fetch that SUCCEEDED
  // at/after this open. A previous-open cache (older success epoch) or a
  // `setQueryData` update (does not advance the epoch) fails here, so the
  // once-guard is never armed on a stale snapshot.
  if (input.windowFetchSucceededAt < input.openedAt) return null;
  return input.headSeq;
}

/**
 * Whether a thread open must trigger a fresh latest-window refetch (#task:1226
 * §9.10). A NORMAL open does: it forces a fresh latest window so its mark
 * snapshot comes from this open. A JUMP open does NOT: `primeAround` already
 * fetched the authoritative around-first-unread window (and stamped the
 * freshness epoch), so a target-less latest refetch here would CLOBBER it —
 * breaking the "click chip → land on first unread" surface-3 contract.
 *
 * Pure so the clobber-prevention decision is locked by a unit test instead of
 * only living in component wiring.
 */
export function threadOpenNeedsLatestRefetch(input: { isJumpOpen: boolean }): boolean {
  return !input.isJumpOpen;
}

/**
 * Whether `useThreadCluster`'s underlying infinite query may auto-fire its
 * initial fetch (#task:1226 §9.10 — the empty-cache race Cindy caught). React
 * Query fires the initial fetch on an EMPTY cache whenever the query is
 * `enabled`, REGARDLESS of `refetchOnMount` (the flag only governs refetch when
 * a cache already exists). On a jump-open that initial fetch is a target-less
 * LATEST window, and it races `primeAround`'s around-first-unread fetch —
 * whichever lands first writes `clusterMessagesKey` and wins, so a latest could
 * clobber the locate window.
 *
 * So while a jump-open's window is LOCKED (`jumpWindowLocked` — the prime is
 * `priming` OR `committed`, see `JumpPhase`) we keep the query DISABLED:
 * `primeAround` is the sole writer of this open's window, and the committed
 * around-window is NOT re-enabled (hence NOT refetchable) until the visual
 * locate finishes and `clearJump` drops the lock. Once the lock releases
 * (`settled` / cleared) the query re-enables: cache present → the around-window
 * is read with no fetch; cache empty (prime failed) → a latest initial fetch
 * fires as a graceful fallback so the pane is never left blank. A normal open
 * never locks, so it keeps its mount-time latest initial fetch exactly as before.
 *
 * Pure so the "locked jump-open does not mount as a latest-initial-fetch-enabled
 * query" invariant is locked by a unit test instead of only living in hook
 * wiring (no RTL/jsdom mount harness in this env).
 */
export function clusterInitialFetchEnabled(input: {
  hasChannelAndRoot: boolean;
  jumpWindowLocked: boolean;
}): boolean {
  return input.hasChannelAndRoot && !input.jumpWindowLocked;
}

/**
 * The window lock as the cluster query applies it. A committed lock protects the
 * primed window; when that window is gone (a live reply removes an unobserved
 * cluster cache before the pane mounts) it protects nothing, and holding it would
 * leave the pane empty with no fetch and no locate to release it.
 */
export function clusterWindowLocked(phase: JumpPhase | null, windowCached: boolean): boolean {
  return phase === "priming" || (phase === "committed" && windowCached);
}

/**
 * Whether ThreadPanel's reply area shows the loader vs. its content (#task:1317).
 * The loader shows ONLY while there is genuinely nothing THIS open can render
 * yet — a real in-flight fetch, OR a jump still `priming` (its around-window has
 * not committed). It is deliberately a COMMITTED-FACT gate with ZERO wall-clock:
 *
 *  - `isLoading` — a real window fetch is in flight (normal open's latest, or a
 *    settled jump's fallback latest). A jump in `priming` holds the query
 *    disabled so `isLoading` is false there — hence the second term.
 *  - `jumpPhase === 'priming'` — a jump is fetching its around-window;
 *    UNCONDITIONALLY cover with the loader, even if React Query is serving the
 *    PREVIOUS open's stale rows on reopen (that stale window must not flash).
 *
 * Once the prime advances to `committed` the around-window is in the cache (rows
 * present) and the window is still LOCKED (no clobber), so we render — keeping
 * the loader up would starve the locate effect of its `#msg-<id>` DOM node. The
 * previous #1317 attempt gated this on a wall-clock epoch and deadlocked; this
 * gate is a pure projection of the controller's lifecycle phase, and `priming`
 * is never terminal (the prime promise always settles) so the loader always
 * clears. No `hasWindow` term (would leak stale rows on reopen), and no
 * wall-clock freshness term (the reverted #1317 epoch). Pure → unit-tested.
 */
export function threadClusterShowsLoader(input: {
  isLoading: boolean;
  jumpPhase: JumpPhase | null;
}): boolean {
  return input.isLoading || input.jumpPhase === "priming";
}

/**
 * The REPLY rows of a thread cluster window — the window minus its root.
 *
 * The cluster read predicate is `id = ? OR thread_root_id = ?`
 * (`message-stream-scope.ts`), so a window that reaches the thread's start
 * CONTAINS the root message. The root is a MAIN-LINE message: it is pinned above
 * the divider (rendered separately), and its `seq` was allocated in the mainline
 * stream. Everything downstream of this function — rendering, grouping, and
 * above all the read cursor — is about the reply stream only.
 *
 * Single authority on purpose: the render list and the read cursor must not each
 * carry their own "which rows are replies" predicate, or one can drift into
 * treating the root as a reply again (#task:1992).
 */
export function threadReplyRows<T extends { id: string }>(
  messages: readonly T[],
  rootMessageId: string,
): T[] {
  return messages.filter((m) => m.id !== rootMessageId);
}

/**
 * The newest committed REPLY seq in a thread cluster window, or null when the
 * window holds no committed reply — the only legal producer of
 * {@link ThreadLiveMarkReadInput.maxVisibleReplySeq}.
 *
 * Why this exists as its own function (#task:1992): a thread read cursor lives
 * in the THREAD stream, whose head (`conversation_stream_heads` keyed on a
 * message's OWN `thread_root_id`) is advanced only by replies — a root bumps the
 * MAINLINE row, so a reply-less thread has no row at all and its head is 0.
 * Taking the max seq of the raw cluster window therefore yields the root's
 * mainline seq on a reply-less thread, and posting that as a thread `at_seq` is
 * a position beyond the thread head — which the backend correctly rejects with
 * `position_future` 400 (observed once in the 2026-08-13 packaged smoke). The
 * fix belongs here, in the cursor's domain, NOT in loosening that validation.
 *
 * A reply-less thread returns null, so the live gate holds and NO observe
 * request is sent. Pending optimistic rows are skipped by `lastCommittedSeq`
 * (they carry no server ordinal), so a just-sent reply advances the cursor only
 * after its ack.
 */
export function threadVisibleReplySeq(
  messages: readonly CachedMessage[],
  rootMessageId: string,
): MessageSeq | null {
  return lastCommittedSeq(threadReplyRows(messages, rootMessageId));
}

export interface ThreadLiveMarkReadInput {
  /** The viewer's actual scroll viewport reaches the true stream tail — the
   *  newest reply is on screen. This is deliberately an observed DOM fact, not
   *  the panel's follow/pin intent: that mutable intent can become stale after
   *  a jump or a no-op programmatic scroll. */
  tailVisible: boolean;
  /** The app window / document is visible. A panel that is "open" but
   *  backgrounded hasn't actually been seen, so a live append must not mark it
   *  read (Cindy bar #4). */
  documentVisible: boolean;
  /** Highest REPLY seq the viewer has actually seen — at-tail this is the loaded
   *  window's max (tail) reply seq, produced by {@link threadVisibleReplySeq}.
   *  Deliberately NOT `stream_head_seq`, NOT the max of every cached reply
   *  (the semantic is max-VISIBLE, not latest-known — Cindy bar #3), and NOT the
   *  max of the raw cluster window (which contains the MAIN-LINE root, whose seq
   *  is outside this stream — #task:1992). Null when there is no visible reply,
   *  including the reply-less thread whose window holds only the root. */
  maxVisibleReplySeq: MessageSeq | null;
  /** Highest seq already SUCCESSFULLY marked read this open — a MessageSeq, or
   *  null when nothing has been marked yet (explicit absence, not a 0 sentinel:
   *  #task:1267 Axis-B discipline forbids collapsing "none" into value 0). */
  lastMarkedSeq: MessageSeq | null;
}

/**
 * Visible-live mark-read decision (#task:1226 — shiki dogfood: a reply that
 * arrives while the panel is open + watched should clear unread without a
 * close/reopen). Initial open still marks the open-time snapshot
 * (`threadMarkReadSnapshot`); THIS governs subsequent live appends.
 *
 * Returns the seq to advance the read cursor to, or null to hold unread. We
 * advance ONLY when the newest reply is genuinely on screen (the actual
 * viewport reaches the stream tail AND the window is visible) and it is newer
 * than what we've marked — so a reply that
 * lands while the user is scrolled up, or the app is backgrounded, stays unread
 * until they actually see it. This preserves the no-swallow invariant: the
 * cursor never jumps past an unrendered tail.
 *
 * The proposed seq is always a REPLY seq (see {@link threadVisibleReplySeq}):
 * this cursor is written into the thread stream, so a main-line ordinal — the
 * pinned root's — is not a candidate at any input, however visible it is.
 *
 * The caller owns the side-effecting concerns this purposely excludes: advance
 * `lastMarkedSeq` only on mutation SUCCESS (Cindy bar #2), debounce bursts, and
 * skip while a mark is in flight. Pure so the gate is unit-tested without React
 * render/scroll timing.
 */
export function threadLiveMarkReadSeq(input: ThreadLiveMarkReadInput): MessageSeq | null {
  if (!input.tailVisible) return null;
  if (!input.documentVisible) return null;
  if (input.maxVisibleReplySeq === null) return null;
  // Same-stream (this thread) MessageSeq compare, guarded on explicit absence:
  // "nothing marked yet" (null) never blocks the first advance (#task:1267 B4).
  if (input.lastMarkedSeq !== null && input.maxVisibleReplySeq <= input.lastMarkedSeq) return null;
  return input.maxVisibleReplySeq;
}

export interface ThreadTailVisibilityInput {
  /** Whether the loaded cluster window reaches the authoritative stream tail. */
  streamAtTail: boolean;
  /** Scroll-container metrics sampled after the current render committed. */
  scrollHeight: number;
  scrollTop: number;
  clientHeight: number;
  /** Existing ThreadPanel bottom tolerance, in CSS pixels. */
  tolerancePx?: number;
}

/**
 * Whether the newest reply is actually visible in the thread viewport.
 *
 * Keeping this as an observed scroll fact closes a subtle split-brain: the
 * follow-bottom ref is navigation intent, and can remain false when a jump's
 * `scrollIntoView()` is clamped/no-op at an already-visible bottom. Read-state
 * must follow what the user can see, not that stale intent bit.
 */
export function threadTailIsVisible(input: ThreadTailVisibilityInput): boolean {
  if (!input.streamAtTail) return false;
  const remaining = input.scrollHeight - input.scrollTop - input.clientHeight;
  return remaining <= (input.tolerancePx ?? 20);
}
