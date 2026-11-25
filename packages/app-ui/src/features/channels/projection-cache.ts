// Central-ingestor cache seam (#task:1292), extracted verbatim from hooks.ts:
// the message-row content-patch fan-out (#task:1270), live-gap set/read, reader-
// gate probes, and the mainline/cluster/thread-root/follower apply+reconcile
// helpers the conversation ingestor drives. Depends on the message-cache core
// (keys + primitives); the reader hooks that consume the live-gap readers stay
// in hooks.ts.
import { useQuery, type QueryClient } from "@tanstack/react-query";
import type {
  ConversationMessagesDto,
  MessageDto,
  MessageOptionsChangedEvent,
  ReactionAggregateDto,
  TaskChangedEvent,
  ThreadReplyPreviewRow,
} from "@silverhand/shared";
import { normalizeThreadPreviewExcerpt, resolveAttachmentLabels } from "@silverhand/shared";
import { isCommittedMessage } from "@/features/channels/pending-message";
import {
  type MessagesCache,
  type CachedMessagesPage,
  messagesKey,
  clusterMessagesKey,
  threadRootKey,
  threadFollowersKey,
  mainlineLiveGapKey,
  clusterLiveGapKey,
  cacheHasMessage,
  cacheHasRootOrKey,
  lastPageIsTail,
  appendMessageToCache,
} from "@/features/channels/message-cache-core";

/** Apply a `task_changed` frame to a SINGLE message row — the source message
 *  the task was created from, which carries the inline task chip. This is the
 *  one place the source-chip merge lives so the mainline window, reply-cluster
 *  window, and the thread-pane root row all stay consistent (mirrors {@link
 *  applyReactionToMessage}, and the reason the projection-fan-out {@link
 *  patchMessageInActiveProjections} seam can carry it as an opaque `patchRow`).
 *  Idempotent: SETS `linked_task` as an ABSOLUTE snapshot from the event (not a
 *  delta/merge over any prior chip), so a first sighting builds it from null and
 *  replay is a no-op. The caller matches `message.id === task.source_message_id`. */
export function applyTaskChangedToMessage(message: MessageDto, task: TaskChangedEvent): MessageDto {
  return {
    ...message,
    linked_task: {
      task_id: task.task_id,
      task_number: task.task_number,
      title: task.title,
      status: task.status,
      assignee: task.assignee,
    },
  };
}

/** Apply a `message_options_changed` frame to its message row: SETS the
 *  absolute options and current answer from the event, so replay is a no-op. */
export function applyOptionsChangedToMessage(
  message: MessageDto,
  event: MessageOptionsChangedEvent,
): MessageDto {
  return { ...message, options: event.options, answer: event.answer };
}

/** A COMMITTED reaction change carried by the ORDERED plane — the
 *  `message_reaction_changed` WS event (#task:1753).
 *
 *  Only that plane produces this delta. The viewer's own write ACK converges by
 *  authoritative re-read instead ({@link invalidateMessageRowInActiveProjections});
 *  which carrier may assign and why is the contract in
 *  `docs/reactions-design.md` §5.3.
 *
 *  `viewerReacted` is resolved at the transport edge, so the merge never sees an
 *  identity: `op === 'add'` when the event's reactor IS the viewer, and `null`
 *  when the change is someone else's — the count moves, the viewer's own flag
 *  must not.
 *
 *  `count` is the server's absolute post-change aggregate; the merge never
 *  derives it from an op (§5.2). `target` routes the row patch and is read off
 *  the row being changed, never from view state. */
export type CommittedReaction = {
  target: { channelId: string; messageId: string; threadRootId: string | null };
  emoji: string;
  /** Server-authoritative post-change count for this emoji; `0` drops the chip. */
  count: number;
  /** The viewer's terminal state for this emoji, or `null` when this change says
   *  nothing about the viewer (another reactor's). */
  viewerReacted: boolean | null;
};

/** Apply a committed reaction to a SINGLE message row.
 *
 *  Count is **absolute-assigned** from the delta (the server's authoritative
 *  post-change aggregate), never derived from an add/remove op — WS replay /
 *  reorder, the dual ACK+WS apply, and the viewer's own toggle would otherwise
 *  drift (docs/reactions-design.md §5.2). `viewerReacted: null` bumps the count
 *  and leaves the viewer flag untouched. `count <= 0` drops the emoji's chip; the
 *  array collapses to `null` when the last reaction goes.
 *
 *  Returns the SAME object when the merge is a no-op, so a duplicate apply (WS
 *  replay, or a re-read that already landed the same value) costs no re-render.
 *  Callers must preserve that: the projection writers compare identity.
 *
 *  The caller is responsible for matching `message.id === delta.target.messageId`;
 *  this is the one place the reaction-merge logic lives so the main window,
 *  reply-cluster window, and the thread-pane root row all stay consistent
 *  (#task:1122 review, P1.1 — no second copy of the merge). */
export function applyReactionToMessage(message: MessageDto, delta: CommittedReaction): MessageDto {
  const { emoji, count, viewerReacted } = delta;
  const prev = message.reactions ?? [];
  const existing = prev.find((r) => r.emoji === emoji);
  let next: ReactionAggregateDto[];
  if (count <= 0) {
    if (!existing) return message;
    next = prev.filter((r) => r.emoji !== emoji);
  } else if (!existing) {
    next = [...prev, { emoji, count, viewer_reacted: viewerReacted ?? false }];
  } else {
    const viewer = viewerReacted ?? existing.viewer_reacted;
    if (existing.count === count && existing.viewer_reacted === viewer) {
      return message;
    }
    next = prev.map((r) =>
      r.emoji === emoji ? { emoji: r.emoji, count, viewer_reacted: viewer } : r,
    );
  }
  return { ...message, reactions: next.length ? next : null };
}

// ─── Message-scoped content-patch seam (#task:1270) ──────────────────────────
// SINGLE entry for "set fields on an existing message row, by id, across every
// currently-DISPLAYED projection that holds it". Before this, each row-patch
// event handler (reaction, task_changed) hand-enumerated which projections to
// write — and a new handler could (and #task:1269 item 1 did) forget one,
// silently dropping live patches for that projection. The fix is by-construction:
// handlers no longer enumerate projections; they hand this seam an idempotent
// per-row `patchRow` and it fans out to all three message projections, each
// gated on {active observer AND cache actually holds the row} — never
// bootstrapping an absent window.
//
// SSOT for "which displayed projections hold M" = React Query observer count +
// cache content (NOT the transport interest registry #1260, a bandwidth-scope
// concern that can't answer "is M in this window"). The per-projection walkers
// below are deliberately NOT exported: the only way to patch a message row is
// through this seam (a by-construction fence — you can't call a function that
// isn't exported), backed by a source-scan fence forbidding row-patch handlers
// from touching the reader gates / `setQueryData` directly. `applied` = ≥1
// displayed projection was actually written; it is the SYNCHRONOUS commit signal
// (reaction/task stay on the #1263 hybrid boolean gate, off the async `message`
// allSettled path). Fire-and-forget refreshes (reactors popover, task-by-number
// detail) are NOT this seam's job and never count toward `applied` — they
// self-heal authoritative-on-open.
//
// S1 note: only the resolver (which projections + membership) is React-Query
// specific; the seam SIGNATURE is the S1 projection-recovery shape, so S1 can
// swap the resolver internals without touching callers.
//
// The module owns TWO entries over that same projection set, chosen by what the
// carrier can prove: {@link patchMessageInActiveProjections} ASSIGNS a value (an
// ordered carrier — WS — knows it holds the newest one), {@link
// invalidateMessageRowInActiveProjections} RE-READS (an unordered carrier — an
// HTTP write ACK — does not). Both resolve their projections through the ONE
// enumeration {@link heldMessageProjections}; that shared list, not a comment, is
// why the two planes cannot disagree about which surfaces a committed fact
// reaches.

/** Whether a paginated message-window cache currently holds the row — the one
 *  definition of window MEMBERSHIP, shared by the patch and the re-read seams so
 *  the two planes can never disagree about which projections a fact reaches. */
function windowHoldsMessage(qc: QueryClient, key: readonly unknown[], messageId: string): boolean {
  return cacheHasMessage(qc.getQueryData<MessagesCache>(key), messageId);
}

/** Whether an open thread-pane root cache holds the row: only its OWN root row
 *  can be the target (replies live in the cluster window, not this single-row
 *  cache), and only once loaded. Thread-root sibling of {@link
 *  windowHoldsMessage}. */
function threadRootHoldsMessage(
  qc: QueryClient,
  channelId: string,
  rootId: string,
  messageId: string,
): boolean {
  if (rootId !== messageId) return false;
  const prev = qc.getQueryData<ConversationMessagesDto>(threadRootKey(channelId, rootId));
  return prev?.messages.some((m) => m.id === messageId) ?? false;
}

/** Map the one row `messageId` inside a paginated message-window cache via
 *  `patchRow`. Membership is the CALLER's business (this only runs for a key
 *  {@link heldMessageProjections} already gated). Only committed rows are touched
 *  — a pending optimistic row carries a temp id an event's server id can't match
 *  anyway (belt-and-suspenders).
 *
 *  A `patchRow` that returns its argument unchanged writes nothing: an
 *  equal-value merge must not churn the cache reference and re-render every
 *  observer. */
function writeMessageRowInWindow(
  qc: QueryClient,
  key: readonly unknown[],
  messageId: string,
  patchRow: (message: MessageDto) => MessageDto,
): void {
  qc.setQueryData<MessagesCache>(key, (prev) => {
    if (!prev) return prev;
    let changed = false;
    const pages = prev.pages.map((page) => {
      let pageChanged = false;
      const messages = page.messages.map((message) => {
        if (message.id !== messageId || !isCommittedMessage(message)) {
          return message;
        }
        const patched = patchRow(message);
        if (patched === message) return message;
        changed = true;
        pageChanged = true;
        return patched;
      });
      return pageChanged ? { ...page, messages } : page;
    });
    return changed ? { ...prev, pages } : prev;
  });
}

/** Map the single root row of an open thread-pane root cache via `patchRow`.
 *  (Replies in the pane live in the cluster window, not this single-row root
 *  cache.) Identity-preserving like {@link writeMessageRowInWindow}: an
 *  equal-value merge leaves the cache reference alone. */
function writeMessageRowInThreadRoot(
  qc: QueryClient,
  channelId: string,
  rootId: string,
  messageId: string,
  patchRow: (message: MessageDto) => MessageDto,
): void {
  qc.setQueryData<ConversationMessagesDto>(threadRootKey(channelId, rootId), (p) => {
    if (!p) return p;
    let changed = false;
    const messages = p.messages.map((m) => {
      if (m.id !== messageId) return m;
      const patched = patchRow(m);
      if (patched === m) return m;
      changed = true;
      return patched;
    });
    return changed ? { ...p, messages } : p;
  });
}

/** One DISPLAYED projection that currently holds the row: its query key plus how
 *  to write that row. */
type HeldMessageProjection = {
  key: readonly unknown[];
  write: (patchRow: (message: MessageDto) => MessageDto) => void;
};

/** THE enumeration of "which displayed caches carry message M right now" —
 *  mainline (gated on a mounted reader), every open reply cluster, every open
 *  thread-root row, each membership-gated, never bootstrapping an absent window.
 *
 *  Every operation this module performs over that set consumes THIS list: assign
 *  (patch), re-read (invalidate), and restart-a-racing-read. Keeping it one list
 *  is what makes "the ACK plane and the WS plane reach the same surfaces" a
 *  structural fact rather than two loops that a comment claims are equal — a
 *  third copy of the walk is how the two planes would silently drift apart. */
function heldMessageProjections(
  qc: QueryClient,
  target: { channelId: string; messageId: string },
): HeldMessageProjection[] {
  const { channelId, messageId } = target;
  const held: HeldMessageProjection[] = [];
  const addWindow = (key: readonly unknown[]) => {
    if (!windowHoldsMessage(qc, key, messageId)) return;
    held.push({
      key,
      write: (patchRow) => writeMessageRowInWindow(qc, key, messageId, patchRow),
    });
  };
  // MAINLINE window — gated on a mounted mainline reader; then membership-gated.
  if (hasActiveMainlineReader(qc, channelId)) addWindow(messagesKey(channelId));
  // REPLY-CLUSTER windows — every OPEN cluster; each dropped unless its window
  // holds the row. Enumerate-all + membership-gate (rather than only the row's
  // own root cluster) mirrors the mainline-append path and is robust to a row
  // surfacing in more than one loaded window.
  for (const rootId of openClusterReaderRoots(qc, channelId)) {
    addWindow(clusterMessagesKey(channelId, rootId));
  }
  // THREAD-PANE root rows — a separate single-row cache; present only when this
  // open root IS the target message. This is the projection reaction covered but
  // task_changed did not (#task:1269-class gap) — the enumeration closes it
  // uniformly.
  for (const rootId of openThreadRootReaderRoots(qc, channelId)) {
    if (!threadRootHoldsMessage(qc, channelId, rootId, messageId)) continue;
    held.push({
      key: threadRootKey(channelId, rootId),
      write: (patchRow) => writeMessageRowInThreadRoot(qc, channelId, rootId, messageId, patchRow),
    });
  }
  return held;
}

/** THE message-row content-patch seam (#task:1270) — see the block comment
 *  above. Fans an idempotent `patchRow` out to every DISPLAYED projection that
 *  holds `messageId` ({@link heldMessageProjections}).
 *  Returns `applied` = at least one displayed projection held the row (the
 *  synchronous content-commit signal for the resume floor). */
export function patchMessageInActiveProjections(
  qc: QueryClient,
  target: { channelId: string; messageId: string; threadRootId: string | null },
  patchRow: (message: MessageDto) => MessageDto,
): boolean {
  // `threadRootId` is a FUTURE RESOLVER HINT, not a correctness gate — the seam
  // deliberately does not read it. Projection membership is decided by observer
  // count + cache content (walk every open cluster/thread-root, membership-gate),
  // never by trusting the event's declared root. It stays on the target so an S1
  // per-projection recovery can key off it without a signature change; if that
  // never lands, drop the field rather than start trusting it as input.
  const held = heldMessageProjections(qc, target);
  // A read already in flight on one of these projections was ISSUED BEFORE this
  // commit, so its response can be older than the value being assigned — and
  // React Query commits a settled read over any interim `setQueryData`, silently
  // undoing this patch until the next event (#task:1753 review).
  //
  // Restart those reads instead of cancelling them: `invalidateQueries`
  // (cancelRefetch, default) silently cancels the in-flight retryer — whose
  // promise is already rejected, so the stale response can never commit data,
  // independent of whether its queryFn honours `signal` — and issues a fresh read
  // that can only snapshot at-or-after this commit.
  //
  // NOT `cancelQueries`: it reverts the query to its pre-fetch state, discarding
  // every OTHER interim write too (an appended message, another row's patch).
  const racing = held.filter(
    (projection) => qc.getQueryState(projection.key)?.fetchStatus === "fetching",
  );
  for (const projection of held) projection.write(patchRow);
  for (const projection of racing) {
    void qc.invalidateQueries({ queryKey: projection.key, exact: true });
  }
  return held.length > 0;
}

/** Recover ONE message row's server state AUTHORITATIVELY across the SAME
 *  displayed projections {@link patchMessageInActiveProjections} writes — the
 *  seam for a committed fact whose carrier cannot prove it holds the newest value
 *  (#task:1753 review).
 *
 *  An HTTP write ACK shares no ordering axis with the event stream (nor with a
 *  second local write's ACK), so it RE-READS rather than assigns; the full
 *  argument, the rejected alternatives, and why the ordered plane MAY assign are
 *  the contract in `docs/reactions-design.md` §5.3.
 *
 *  Scope is deliberately identical to the patch seam — both consume {@link
 *  heldMessageProjections}, so the two planes converge on exactly the same
 *  projection set by construction, and a warm-inactive cache's semantics stay
 *  untouched (it was never patched either). Membership gating also bounds the
 *  cost: a window that doesn't hold the row is left alone rather than refetched,
 *  and no absent window is ever bootstrapped. A read already in flight needs no
 *  special handling on this plane — `invalidateQueries` cancels and restarts it,
 *  so the value is decided by a read issued after this write committed.
 *
 *  Returns the settle promise of the recovery round, so a caller can join it to a
 *  commit gate; `applied`-style membership is not reported, because "no displayed
 *  projection holds it" and "the re-read landed" are the same outcome here — the
 *  row re-reads authoritatively whenever it next becomes displayed. */
export function invalidateMessageRowInActiveProjections(
  qc: QueryClient,
  target: { channelId: string; messageId: string },
): Promise<void> {
  return Promise.all(
    heldMessageProjections(qc, target).map((projection) =>
      qc.invalidateQueries({ queryKey: projection.key, exact: true }),
    ),
  ).then(() => undefined);
}

/** Patch the source-message row in WARM-INACTIVE caches — a warm cache with NO
 *  live reader, which React Query still serves on a remount within `staleTime`,
 *  so it is still DISPLAYABLE. Companion to {@link patchMessageInActiveProjections}
 *  for the task-chip convergence: a task's source message can sit many windows
 *  back, held only by a warm cache; leaving it unpatched would (a) surface a
 *  stale chip on paginate-back and (b) — per the ws-client resume-floor
 *  invariant — forbid satisfying the floor at all. Because the task chip snapshot
 *  is ABSOLUTE (a by-id field SET, not an APPEND), an in-place warm patch is
 *  authoritative and cheap — unlike a message append, which must cold-start via
 *  §6 warm-remove. A no-op when no warm cache holds the row. NOT used by reaction
 *  (whose active-only scope is intentionally unchanged this task). Returns
 *  whether any warm row was written (for tests; the caller satisfies the floor
 *  regardless, since an absent row is authoritative-on-paginate). */
export function patchWarmInactiveMessageRow(
  qc: QueryClient,
  channelId: string,
  messageId: string,
  patchRow: (message: MessageDto) => MessageDto,
): boolean {
  let patched = false;
  // Membership-then-write, the same two steps {@link heldMessageProjections}
  // composes for the displayed set. This path keeps its own walk because its
  // membership question is the OPPOSITE one (warm caches with zero observers),
  // and because a warm cache has no read in flight to race.
  const patchWindow = (key: readonly unknown[]): boolean => {
    if (!windowHoldsMessage(qc, key, messageId)) return false;
    writeMessageRowInWindow(qc, key, messageId, patchRow);
    return true;
  };
  // Warm-inactive mainline window.
  if (hasWarmInactiveMainline(qc, channelId) && patchWindow(messagesKey(channelId))) {
    patched = true;
  }
  // Warm-inactive reply-cluster windows.
  for (const rootId of warmInactiveClusterRoots(qc, channelId)) {
    if (patchWindow(clusterMessagesKey(channelId, rootId))) patched = true;
  }
  // Warm-inactive thread-pane root rows (single-row cache; no-op unless this
  // closed-but-warm root IS the source message).
  for (const rootId of warmInactiveThreadRootRoots(qc, channelId)) {
    if (!threadRootHoldsMessage(qc, channelId, rootId, messageId)) continue;
    writeMessageRowInThreadRoot(qc, channelId, rootId, messageId, patchRow);
    patched = true;
  }
  return patched;
}

/** Roots of WARM-INACTIVE thread-root rows (warm cache, zero observers) —
 *  the thread-root sibling of {@link warmInactiveClusterRoots}. */
function warmInactiveThreadRootRoots(qc: QueryClient, channelId: string): string[] {
  return qc
    .getQueryCache()
    .findAll({ queryKey: ["channels", channelId, "thread-root"] })
    .filter((q) => q.getObserversCount() === 0 && q.state.data !== undefined)
    .map((q) => (q.queryKey as readonly unknown[])[3] as string);
}
export function setMainlineLiveGap(qc: QueryClient, channelId: string, value: boolean): void {
  qc.setQueryData<boolean>(mainlineLiveGapKey(channelId), value);
}

/** True when a `useChannelMessages(channelId)` reader is mounted — i.e. the
 *  exact mainline window key has at least one active observer.
 *
 *  Stage-2 P1a gate (#task:1255): the central ingestor took over what used to
 *  be `useChannelMessages.onEvent`, which only ran while that hook was mounted.
 *  To stay behavior-equivalent (P1 = prep, NOT correctness), mainline cache
 *  writes must happen ONLY while a mainline reader is alive — thread/follower
 *  scope INTEREST keeps the WS scope subscribed but must not bootstrap or patch
 *  the mainline window (that would surface a live-frame-stitched, incomplete
 *  window with no REST snapshot behind it). P2 does NOT remove this gate — it
 *  makes `markSatisfied` honor it (a frame with no active reader is not applied
 *  and must not advance the cursor); full decouple is P3
 *  (docs/renderer-event-ingestion-v1.md §3.1/§5).
 *  `messagesKey` is observed only by `useChannelMessages`; primers/jumps use
 *  `setQueryData`/`fetchQuery` and register no standing observer. */
export function hasActiveMainlineReader(qc: QueryClient, channelId: string): boolean {
  const entry = qc.getQueryCache().find({ queryKey: messagesKey(channelId), exact: true });
  return (entry?.getObserversCount() ?? 0) > 0;
}

/** Apply a live/replayed mainline `message` frame to the channel main-stream
 *  caches (#task:1255 Stage-2 / #task:1263 (c)). Returns this frame's mainline
 *  SATISFACTION promise, or `null` when it touches no mainline display path (the
 *  ingestor collects the non-null promises and advances the resume floor only
 *  once every effect settled fulfilled — §4.1 / #1261):
 *   - a reply (`thread_root_id != null`), non-system, whose root is on a loaded
 *     page → the root's reply badge is a message-DERIVED projection, so recover it
 *     AUTHORITATIVELY (invalidate → refetch the count). #1263 (c): this replaces
 *     the old optimistic `+1`, which was non-idempotent and double-counted on
 *     replay (the Cindy #1263 falsifier). Cursor-inert: the invalidate INITIATES
 *     recovery; the floor advances only once it COMMITS (#1261 satisfaction).
 *     Returns the invalidate Promise.
 *   - a reply to an OFF-window root, or a system reply → `null` (the badge isn't
 *     displayed; a fresh count arrives when the window paginates to that root).
 *   - a mainline row at tail → append (id-deduped) → content committed → resolved.
 *   - a mainline row while the window is NOT at tail → set the live-gap fact; the
 *     row loads authoritatively when the user reaches the tail, so the affordance
 *     IS the commit → resolved (an already-gapped bump is equally satisfied —
 *     #1261 lets the floor advance rather than pin, since paginate is authoritative). */
export function applyMainlineMessageToChannelCaches(
  qc: QueryClient,
  channelId: string,
  msg: MessageDto,
): Promise<void> | null {
  if (msg.thread_root_id !== null) {
    if (msg.from_kind === "system") return null;
    const rootId = msg.thread_root_id;
    const cur = qc.getQueryData<MessagesCache>(messagesKey(channelId));
    if (!cacheHasRootOrKey(cur, rootId)) return null;
    return qc.invalidateQueries({ queryKey: messagesKey(channelId) });
  }
  const cur = qc.getQueryData<MessagesCache>(messagesKey(channelId));
  if (!lastPageIsTail(cur)) {
    setMainlineLiveGap(qc, channelId, true);
    return Promise.resolve();
  }
  qc.setQueryData<MessagesCache>(messagesKey(channelId), (prev) => appendMessageToCache(prev, msg));
  return Promise.resolve();
}

/** Reconcile the channel main-stream window on `snapshot_required` (reconnect
 *  epoch mismatch / replay gap) — verbatim move of `useChannelMessages.onEvent`'s
 *  snapshot_required branch: drop back to latest (invalidate) and clear the
 *  live-gap hint. */
export function reconcileChannelMainlineOnSnapshotRequired(
  qc: QueryClient,
  channelId: string,
): Promise<void> {
  setMainlineLiveGap(qc, channelId, false);
  // Returns the invalidate Promise (reconcile-settle signal for the P2 join,
  // #1255 P2); invalidate strategy unchanged.
  return qc.invalidateQueries({ queryKey: messagesKey(channelId) });
}

/** Reactive reader for the mainline live-gap affordance (#task:1255 P1a). The
 *  central ingestor owns the SET; this just subscribes the cache key so the
 *  view re-renders when the hint flips. No queryFn fetch — the value lives only
 *  in cache (default `false`); `staleTime: Infinity` keeps the placeholder
 *  `queryFn` from ever clobbering an ingestor-written `true`. */
export function useMainlineLiveGap(channelId: string | null): boolean {
  const { data } = useQuery<boolean>({
    queryKey: mainlineLiveGapKey(channelId ?? "none"),
    enabled: !!channelId,
    queryFn: () => false,
    initialData: false,
    staleTime: Infinity,
  });
  return data ?? false;
}

// ─── Stage-2 P1b: thread cluster / root / followers ingestor helpers ─────────
// Move the cache writes that lived in useThreadCluster / useThreadRoot /
// useThreadFollowers `onEvent` into the central ingestor. As with the mainline
// path (P1a), behavior is preserved (P1 = prep): every write is gated on the
// matching reader being mounted. A channel can hold several open clusters at
// once, so the ingestor enumerates the OPEN reader roots (those with a live
// observer) and applies per-root — exactly mirroring "each mounted hook ran its
// own onEvent". P2 does NOT lift these gates — `markSatisfied` honors them (a
// frame with no active reader is not applied and must not advance the cursor);
// full decouple from view mount is P3.

export function setClusterLiveGap(
  qc: QueryClient,
  channelId: string,
  rootId: string,
  value: boolean,
): void {
  qc.setQueryData<boolean>(clusterLiveGapKey(channelId, rootId), value);
}

export function useClusterLiveGap(channelId: string | null, rootId: string | null): boolean {
  const { data } = useQuery<boolean>({
    queryKey: clusterLiveGapKey(channelId ?? "none", rootId ?? "none"),
    enabled: !!channelId && !!rootId,
    queryFn: () => false,
    initialData: false,
    staleTime: Infinity,
  });
  return data ?? false;
}

/** Roots of the OPEN clusters in this channel (cluster window has ≥1 observer). */
export function openClusterReaderRoots(qc: QueryClient, channelId: string): string[] {
  return qc
    .getQueryCache()
    .findAll({ queryKey: ["channels", channelId, "cluster"] })
    .filter((q) => {
      const k = q.queryKey as readonly unknown[];
      // The cluster id space holds two keys per root (`…cluster/<root>/messages`
      // and `…/fetch-epoch`); only the window query is a reader.
      return k[4] === "messages" && q.getObserversCount() > 0;
    })
    .map((q) => (q.queryKey as readonly unknown[])[3] as string);
}

/** Roots of the OPEN thread-root rows in this channel (≥1 observer). */
export function openThreadRootReaderRoots(qc: QueryClient, channelId: string): string[] {
  return qc
    .getQueryCache()
    .findAll({ queryKey: ["channels", channelId, "thread-root"] })
    .filter((q) => q.getObserversCount() > 0)
    .map((q) => (q.queryKey as readonly unknown[])[3] as string);
}

/** Roots of the OPEN follower snapshots in this channel (≥1 observer). */
export function openThreadFollowerReaderRoots(qc: QueryClient, channelId: string): string[] {
  return qc
    .getQueryCache()
    .findAll({ queryKey: ["channels", channelId, "thread-followers"] })
    .filter((q) => q.getObserversCount() > 0)
    .map((q) => (q.queryKey as readonly unknown[])[3] as string);
}

/** Roots of reply clusters in this channel that hold a WARM cache but have NO
 *  active reader. `useThreadCluster` is `refetchOnMount: false`, so an inactive,
 *  merely-invalidated cluster would serve stale data on its next open. On
 *  `snapshot_required` these must be REMOVED (cold-start on next mount) rather
 *  than invalidated — the "旧内容已同步移除且不会展示" arm of the `satisfied`
 *  predicate (#task:1255 §6). Active clusters are covered by
 *  {@link openClusterReaderRoots} (invalidate→refetch settles the promise). */
export function warmInactiveClusterRoots(qc: QueryClient, channelId: string): string[] {
  return qc
    .getQueryCache()
    .findAll({ queryKey: ["channels", channelId, "cluster"] })
    .filter((q) => {
      const k = q.queryKey as readonly unknown[];
      return k[4] === "messages" && q.getObserversCount() === 0 && q.state.data !== undefined;
    })
    .map((q) => (q.queryKey as readonly unknown[])[3] as string);
}

/** Warm-remove one inactive cluster window on `snapshot_required` (#task:1255
 *  §6). `removeQueries` is synchronous — the returned resolved Promise lets it
 *  join the P2 `Promise.allSettled` commit gate as a fulfilled reconcile. Its
 *  live-gap fact is cleared alongside so a stale "new below" hint can't survive
 *  the cold-start. */
export function removeInactiveClusterCache(
  qc: QueryClient,
  channelId: string,
  rootId: string,
): Promise<void> {
  setClusterLiveGap(qc, channelId, rootId, false);
  qc.removeQueries({
    queryKey: clusterMessagesKey(channelId, rootId),
    exact: true,
  });
  return Promise.resolve();
}

/** True when this channel's main-stream window (`messagesKey`) holds a WARM
 *  cache but has NO active reader. `useChannelMessages` runs at the global
 *  default `staleTime: 5_000`, so a remount within that window serves the warm
 *  cache WITHOUT a refetch — meaning an inactive mainline cache can NOT be
 *  treated as absent/vacuous on `snapshot_required`: it would still display
 *  post-commit stale rows. Like {@link warmInactiveClusterRoots} (the cluster
 *  axis), it must be REMOVED (cold-start on next open), not merely invalidated.
 *  `event-ws-transport-v1.md §8` lists both mainline `useChannelMessages` and
 *  `useThreadCluster` in the warm-remove class. Active mainlines are covered by
 *  {@link hasActiveMainlineReader} (invalidate→refetch settles the promise). */
export function hasWarmInactiveMainline(qc: QueryClient, channelId: string): boolean {
  const entry = qc.getQueryCache().find({ queryKey: messagesKey(channelId), exact: true });
  return entry !== undefined && entry.getObserversCount() === 0 && entry.state.data !== undefined;
}

/** Warm-remove the inactive main-stream window on `snapshot_required` (#task:1255
 *  §6). `removeQueries` is synchronous — the resolved Promise joins the P2
 *  `Promise.allSettled` commit gate as a fulfilled reconcile. The mainline
 *  live-gap fact is cleared alongside so a stale "new below" hint can't survive
 *  the cold-start. Sibling of {@link removeInactiveClusterCache}. */
export function removeInactiveMainlineCache(qc: QueryClient, channelId: string): Promise<void> {
  setMainlineLiveGap(qc, channelId, false);
  qc.removeQueries({ queryKey: messagesKey(channelId), exact: true });
  return Promise.resolve();
}

/** Reconcile one OPEN thread-root row on `snapshot_required` — refetch its
 *  authoritative reply count. This is the covering recovery that releases a
 *  reply-count invalidate that pinned the floor ({@link applyThreadRootReplyBump}
 *  §4.1). Returns the invalidate Promise for the P2 commit join. */
export function reconcileThreadRootOnSnapshotRequired(
  qc: QueryClient,
  channelId: string,
  rootId: string,
): Promise<void> {
  return qc.invalidateQueries({ queryKey: threadRootKey(channelId, rootId) });
}

/** Apply a `message` frame to one OPEN cluster window — verbatim move of
 *  useThreadCluster.onEvent's message branch (filter to this cluster's rows;
 *  append at tail, else raise the cluster live-gap). Returns this cluster's
 *  SATISFACTION promise, or `null` when the row is not part of this cluster:
 *  a cluster append is an id-deduped CONTENT write (idempotent on replay), and a
 *  live-gap flip is an affordance whose row loads authoritatively when the user
 *  reaches the tail — both are committed synchronously → resolved. An already-
 *  gapped bump is equally satisfied (#1261 lets the floor advance vs pin, since
 *  paginate is authoritative). #task:1255 Stage-2 / #task:1263. */
export function applyClusterMessageToCaches(
  qc: QueryClient,
  channelId: string,
  rootId: string,
  msg: MessageDto,
): Promise<void> | null {
  if (msg.thread_root_id !== rootId && msg.id !== rootId) return null;
  const cur = qc.getQueryData<MessagesCache>(clusterMessagesKey(channelId, rootId));
  if (!lastPageIsTail(cur)) {
    setClusterLiveGap(qc, channelId, rootId, true);
    return Promise.resolve();
  }
  qc.setQueryData<MessagesCache>(clusterMessagesKey(channelId, rootId), (prev) =>
    appendMessageToCache(prev, msg),
  );
  return Promise.resolve();
}

/** Reconcile one OPEN cluster on `snapshot_required` — clear its live-gap and
 *  invalidate the window so it refetches (mirrors useThreadCluster.onEvent).
 *  Returns the invalidate Promise (reconcile-settle signal for the P2 join,
 *  #1255 P2); strategy unchanged. */
export function reconcileClusterOnSnapshotRequired(
  qc: QueryClient,
  channelId: string,
  rootId: string,
): Promise<void> {
  setClusterLiveGap(qc, channelId, rootId, false);
  return qc.invalidateQueries({ queryKey: clusterMessagesKey(channelId, rootId) });
}

/** Recover the authoritative reply count on one OPEN thread-root row (#task:1255
 *  Stage-2 / #task:1263 (c)). Returns this root's SATISFACTION promise, or `null`
 *  when the frame touches no loaded root display path:
 *   - a non-system reply to this loaded root → the count is a message-DERIVED
 *     projection, so recover it AUTHORITATIVELY (invalidate → refetch). #1263 (c)
 *     replaces the old optimistic `+1`, which was non-idempotent and double-counted
 *     on replay. Cursor-inert: the invalidate INITIATES
 *     recovery; the floor advances only once it COMMITS (#1261). Returns the
 *     invalidate Promise.
 *   - a system reply, a row for another root, or a not-loaded root → `null`
 *     (nothing displayed; the count is authoritative on the root row's own fetch). */
export function applyThreadRootReplyBump(
  qc: QueryClient,
  channelId: string,
  rootId: string,
  msg: MessageDto,
): Promise<void> | null {
  if (msg.thread_root_id !== rootId || msg.from_kind === "system") return null;
  const prev = qc.getQueryData<ConversationMessagesDto>(threadRootKey(channelId, rootId));
  if (!prev) return null;
  return qc.invalidateQueries({ queryKey: threadRootKey(channelId, rootId) });
}

/** S1 (#task:1277 / docs/projection-recovery-s1-v1.md §5): idempotently SET the
 *  authoritative reply count for `rootId` on the OPEN thread-root row cache from
 *  a backend absolute-value patch — the merge that REPLACES
 *  {@link applyThreadRootReplyBump}'s invalidate→refetch on the patch path (the
 *  #1277 amplification kill). Returns `true` when the displayed cache holds the
 *  root (→ `merged`), `false` when its snapshot isn't loaded (→
 *  `cold-absent-vacuous`; the root's own mount-fetch is the authoritative
 *  recovery). Absolute set is replay-idempotent (an equal value is a no-op that
 *  skips the write to avoid needless re-render churn). */
export function mergeThreadRootReplyCount(
  qc: QueryClient,
  channelId: string,
  rootId: string,
  replyCount: number,
): boolean {
  const prev = qc.getQueryData<ConversationMessagesDto>(threadRootKey(channelId, rootId));
  if (!prev) return false;
  if (prev.reply_counts_by_root[rootId] === replyCount) return true;
  qc.setQueryData<ConversationMessagesDto>(threadRootKey(channelId, rootId), (p) =>
    p
      ? {
          ...p,
          reply_counts_by_root: {
            ...p.reply_counts_by_root,
            [rootId]: replyCount,
          },
        }
      : p,
  );
  return true;
}

/** S1 (#task:1277 / §5): idempotently SET the authoritative reply count for
 *  `rootId` on the mainline window (the inline reply badge) from a backend
 *  absolute-value patch — REPLACES the whole-window invalidate of
 *  {@link applyMainlineMessageToChannelCaches}'s reply branch on the patch path.
 *  Returns `true` when the badge is displayed (→ `merged`), `false` when the root
 *  isn't on a loaded page (→ `cold-absent-vacuous`). Writes the value onto every
 *  page already carrying the root's key/row so the reader's page-merge ("later
 *  wins") yields the authoritative count regardless of page order; an equal value
 *  is a no-op (no ref churn). */
export function mergeMainlineReplyCount(
  qc: QueryClient,
  channelId: string,
  rootId: string,
  replyCount: number,
): boolean {
  const cur = qc.getQueryData<MessagesCache>(messagesKey(channelId));
  if (!cacheHasRootOrKey(cur, rootId)) return false;
  qc.setQueryData<MessagesCache>(messagesKey(channelId), (prev) => {
    if (!prev) return prev;
    let changed = false;
    const pages = prev.pages.map((page) => {
      const holdsRoot =
        Object.prototype.hasOwnProperty.call(page.reply_counts_by_root, rootId) ||
        page.messages.some((m) => m.id === rootId);
      if (!holdsRoot || page.reply_counts_by_root[rootId] === replyCount) {
        return page;
      }
      changed = true;
      return {
        ...page,
        reply_counts_by_root: {
          ...page.reply_counts_by_root,
          [rootId]: replyCount,
        },
      };
    });
    return changed ? { ...prev, pages } : prev;
  });
  return true;
}

/** Wire bound on the displayed tail, mirroring the read model's `ROW_NUMBER() <= 3`
 *  and the schema's `.max(3)`. The cache converges to it on every write: the
 *  reader's own tail-3 is a last line of defense, not the bound — a cache that
 *  keeps growing violates the contract its own type declares and, on a long-lived
 *  channel with a hot thread, keeps every reply ever seen alive per page. */
const THREAD_PREVIEW_TAIL_SIZE = 3;

/** Converge a patched tail to exactly what an authoritative fetch would return:
 *  the `THREAD_PREVIEW_TAIL_SIZE` highest-`seq` rows, `seq`-ascending, one row per
 *  `message_id`. `seq` is immutable insert order (no edit/delete write path), so
 *  seq-ordering IS recency and an out-of-order frame lands in its true position
 *  instead of at the end. */
function convergeReplyPreviewTail(rows: readonly ThreadReplyPreviewRow[]): ThreadReplyPreviewRow[] {
  const byId = new Map(rows.map((row) => [row.message_id, row]));
  return [...byId.values()].sort((a, b) => a.seq - b.seq).slice(-THREAD_PREVIEW_TAIL_SIZE);
}

/** Build this reply's preview row from the frame that delivered it (#task:1745
 *  renderer freshness, the handoff #task:1746 names): a reply's WS frame carries
 *  an absolute `thread_root_reply_count` patch, which SUPPRESSES the mainline
 *  invalidate — so nothing else would refresh the tail, and the badge would tick
 *  to `N+1` above lines that predate the reply it just counted.
 *
 *  Every field is derived from the same inputs the read model uses, and the one
 *  two that could drift are derived through their SHARED SSOTs: the attachment
 *  label from `resolveAttachmentLabels()` over the whole list (the positional
 *  fallback depends on sibling ordinals) so a patched line names the attachment
 *  exactly as the body does, and the excerpt from `normalizeThreadPreviewExcerpt()`
 *  (#task:1746) — the field must read byte-identically whether it arrived cold
 *  from the read model or was merged live from this frame.
 *
 *  `null` for a system reply — eligibility mirrors `reply_counts_by_root`. */
export function threadReplyPreviewRowFromMessage(msg: MessageDto): ThreadReplyPreviewRow | null {
  if (msg.from_kind === "system") return null;
  const snapshots = msg.attachments ?? [];
  // Carry the server-authoritative author DTO straight through (#task:1792) —
  // the preview row's identity is the SAME resolved author the byline uses, so a
  // deleted replier keeps its snapshot name and an unresolved one degrades to
  // "Unknown …". A non-system message always resolves an author server-side; a
  // null here is a degraded frame, so it falls back to the non-aggregating
  // `unresolved` author rather than fabricating one from the (nulled) FK.
  const author: ThreadReplyPreviewRow["author"] = msg.author ?? {
    kind: msg.from_kind === "agent" ? "agent" : "user",
    status: "unresolved",
    id: null,
    name: null,
    display_name: null,
    avatar_url: null,
  };
  return {
    message_id: msg.id,
    seq: msg.seq,
    author,
    preview: normalizeThreadPreviewExcerpt(msg.content),
    attachment_count: snapshots.length,
    first_attachment_label:
      snapshots.length > 0 ? (resolveAttachmentLabels(snapshots)[0] ?? null) : null,
  };
}

/** Patch the mainline reply-PREVIEW tail for `rootId` from the reply frame
 *  itself (#task:1745), the companion to {@link mergeMainlineReplyCount} on the
 *  patched-reply path where the window is deliberately not refetched.
 *
 *  ── What "no key for this root" means, per page ────────────────────────────
 *  A page that carries `reply_previews_by_root` AT ALL comes from a preview-
 *  projecting read model, and that read model COMPUTES the sidecar over the
 *  page's complete eligible root set (every non-system top-level message, not
 *  just the ones with replies) and simply OMITS the key for a root whose tail is
 *  empty. So on such a page a missing key is not "unknown": it is the
 *  authoritative statement "this root had no eligible reply when the window was
 *  fetched", and this frame's reply is therefore the tail's first row (Cindy,
 *  #task:1745 BLOCKER 1 — the common 0→1 reply would otherwise only move the
 *  badge). The two genuinely unknown cases stay untouched, both cache-only,
 *  neither fixable by a refetch: a page from a read model that does not project
 *  previews at all (no `reply_previews_by_root` object → nothing to display, so
 *  today's
 *  badge-only row is correct), and a root that is on no loaded page. That is
 *  why this stays a pure cache patch and never falls back to the invalidate
 *  #task:1277 removed: reintroducing it here would refetch the whole window on
 *  every first reply — and on EVERY reply against a non-projecting read model —
 *  which is exactly the amplification that change killed.
 *
 *  Eligibility is mirrored, not re-derived: a system reply is skipped (as in
 *  `reply_counts_by_root`), and a root is seeded only when the page displays it
 *  as a non-system top-level message — the read model's own filter, so an
 *  ineligible root can never grow a tail here that a fetch would then remove.
 *
 *  Replay-safe and BOUNDED: the tail is converged on every write
 *  ({@link convergeReplyPreviewTail}) — one row per `message_id`, seq-ordered,
 *  tail-3 — so replay, an out-of-order frame, and a long burst all land the same
 *  three rows an authoritative fetch would.
 *
 *  Returns `true` when the root's displayed tail is current after this call,
 *  `false` when this root displays no tail at all (ineligible reply, absent
 *  projection, or off-window root) — in every `false` case there is nothing the
 *  caller could recover, so it is a no-op, not a fallback trigger. */
export function mergeMainlineReplyPreview(
  qc: QueryClient,
  channelId: string,
  rootId: string,
  msg: MessageDto,
): boolean {
  const row = threadReplyPreviewRowFromMessage(msg);
  if (!row) return false;
  /** The read model's `rootIds` filter, evaluated against a loaded page. */
  const pageDisplaysEligibleRoot = (page: CachedMessagesPage): boolean =>
    page.messages.some(
      (m) => m.id === rootId && m.thread_root_id === null && m.from_kind !== "system",
    );
  const nextTailFor = (page: CachedMessagesPage): ThreadReplyPreviewRow[] | null => {
    const previews = page.reply_previews_by_root;
    if (!previews) return null; // read model does not project previews
    const tail = previews[rootId];
    if (!tail) {
      return pageDisplaysEligibleRoot(page) ? [row] : null;
    }
    if (tail.some((r) => r.message_id === row.message_id)) return null;
    return convergeReplyPreviewTail([...tail, row]);
  };

  const cur = qc.getQueryData<MessagesCache>(messagesKey(channelId));
  const displaysTail =
    cur?.pages.some(
      (page) =>
        page.reply_previews_by_root != null &&
        (page.reply_previews_by_root[rootId] != null || pageDisplaysEligibleRoot(page)),
    ) ?? false;
  if (!displaysTail) return false;
  qc.setQueryData<MessagesCache>(messagesKey(channelId), (prev) => {
    if (!prev) return prev;
    let changed = false;
    const pages = prev.pages.map((page) => {
      const next = nextTailFor(page);
      if (!next) return page;
      changed = true;
      return {
        ...page,
        reply_previews_by_root: {
          ...page.reply_previews_by_root,
          [rootId]: next,
        },
      };
    });
    return changed ? { ...prev, pages } : prev;
  });
  return true;
}

/** Invalidate one OPEN cluster's follower snapshot when a reply to this root
 *  lands — verbatim move of useThreadFollowers.onEvent's message branch. Returns
 *  the invalidate Promise (a derived-invalidate SATISFACTION signal, cursor-inert),
 *  or `null` when the row is not part of this cluster. #task:1263. */
export function applyThreadFollowersMessage(
  qc: QueryClient,
  channelId: string,
  rootId: string,
  msg: MessageDto,
): Promise<void> | null {
  if (msg.thread_root_id !== rootId && msg.id !== rootId) return null;
  return qc.invalidateQueries({ queryKey: threadFollowersKey(channelId, rootId) });
}

/** Invalidate one OPEN cluster's follower snapshot on `snapshot_required`.
 *  Returns the invalidate Promise (reconcile-settle signal for the P2 join,
 *  #1255 P2); strategy unchanged. */
export function invalidateThreadFollowers(
  qc: QueryClient,
  channelId: string,
  rootId: string,
): Promise<void> {
  return qc.invalidateQueries({ queryKey: threadFollowersKey(channelId, rootId) });
}
