import {
  useInfiniteQuery,
  useQuery,
  useQueryClient,
  type InfiniteData,
} from "@tanstack/react-query";
import { useCompactLayout } from "@/navigation/layout";
import { useCallback, useEffect, useMemo } from "react";
import type {
  ConversationMessagesDto,
  ReplyBadgeSummary,
  ThreadReplyPreviewRow,
} from "@silverhand/shared";
import { apiPaths } from "@silverhand/shared";
import { isCommittedMessage } from "@/features/channels/pending-message";
import { api, ApiError } from "@/lib/api";
import { useInterest } from "@/features/realtime/interest-registry";
import { wsConversationScope } from "@/features/realtime/conversation-scope";
import { clusterInitialFetchEnabled } from "@/features/conversation-shell/model/thread-mark-read";
import {
  type CachedMessagesPage,
  type MessagesCache,
  type MessagesPageParam,
  messagesKey,
  clusterMessagesKey,
  clusterFetchEpochKey,
  threadRootKey,
  MESSAGE_PAGE_LIMIT,
  MESSAGE_WINDOW_PAGES,
  trimMessagesWindow,
  AROUND_MESSAGE_LIMIT,
  makeMessagesCache,
  lastPageIsTail,
} from "@/features/channels/message-cache-core";
import {
  useMainlineLiveGap,
  setMainlineLiveGap,
  useClusterLiveGap,
  setClusterLiveGap,
} from "@/features/channels/projection-cache";

/** Build a single messages-window page request URL.
 *
 *  Cursor params are mutually exclusive on the wire (the route 400s
 *  on 2+) — this helper enforces it locally too by treating
 *  `MessagesPageParam` as the union it is. `aroundMessageId` is its
 *  own caller-driven mode used by `usePrimeConversationMessages`,
 *  not by the page-walk path. `signal` is plumbed straight through
 *  to `api()` so callers can abort the in-flight fetch. */
async function fetchMessagesWindow(opts: {
  conversationId: string;
  pageParam?: MessagesPageParam;
  aroundMessageId?: string;
  /** When set, fetch the reply cluster rooted at this message id
   *  (`?thread_root_id=`) instead of the conversation's main stream.
   *  Channel route only — threads have no nested clusters. */
  threadRootId?: string;
  limit: number;
  signal?: AbortSignal;
}): Promise<ConversationMessagesDto> {
  const params = new URLSearchParams({ limit: String(opts.limit) });
  if (opts.aroundMessageId !== undefined) {
    params.set("around_message_id", opts.aroundMessageId);
  } else if (opts.pageParam) {
    if (opts.pageParam.kind === "before") {
      params.set("before_seq", String(opts.pageParam.seq));
    } else {
      params.set("after_seq", String(opts.pageParam.seq));
    }
  }
  if (opts.threadRootId !== undefined) {
    params.set("thread_root_id", opts.threadRootId);
  }
  const path = apiPaths.channelMessages(opts.conversationId);
  return api<ConversationMessagesDto>(`${path}?${params.toString()}`, {
    signal: opts.signal,
  });
}

/** Initial latest-window GET + bidirectional pagination + live
 *  append on realtime events.
 *
 *  Bidirectional is what lets `usePrimeConversationMessages` replace
 *  the cache with a single around-message page and have the user
 *  scroll either direction from there: `getPreviousPageParam` reads
 *  the first page's `has_more_before`, `getNextPageParam` reads the
 *  last page's `has_more_after`.
 *
 *  Live `message` events are appended to the cache only when the last
 *  page is the tail of the conversation (`has_more_after === false`).
 *  `isAtTail` is the durable window-position signal for "latest"
 *  actions. `hasNewerLiveGap` is narrower: it only means a live message
 *  arrived while the window was not at tail. Since Stage-2 P1a the gap is
 *  a per-channel React Query cache fact (`mainlineLiveGapKey`) the central
 *  ingestor SETs and this hook reads/CLEARs — not local hook state — so a
 *  flip drives a rerender via the query subscription.
 */
export function useChannelMessages(channelId: string | null) {
  const layout = useCompactLayout() ? "compact" : "regular";
  const qc = useQueryClient();

  // Stage-2 (#task:1255 P1a): the central conversation ingestor is the sole
  // `onWsFrame` consumer and owns the SET of the live-gap affordance. This hook
  // is a pure reader of that cache fact; it keeps only the position-based CLEARS
  // (here, channel switch). See docs/renderer-event-ingestion-v1.md §5.
  const hasNewerLiveGap = useMainlineLiveGap(channelId);

  // Reset the gap whenever the consumer switches conversations —
  // a stale "new messages below" hint from the previous channel
  // would be both wrong and confusing. The cleanup clears the channel
  // we're LEAVING: with a per-channel cache key (vs the old hook-local
  // useState) a stale `true` would otherwise survive the switch and flash
  // a bogus "jump to latest" on the first render back (#task:1255 P1a
  // blocker-2). Faithful to the old useState reset-on-mount semantics.
  useEffect(() => {
    if (!channelId) return;
    setMainlineLiveGap(qc, channelId, false);
    return () => setMainlineLiveGap(qc, channelId, false);
  }, [qc, channelId]);

  const query = useInfiniteQuery<
    CachedMessagesPage,
    Error,
    InfiniteData<CachedMessagesPage, MessagesPageParam>,
    ReturnType<typeof messagesKey>,
    MessagesPageParam
  >({
    queryKey: messagesKey(channelId ?? "none"),
    enabled: !!channelId,
    initialPageParam: undefined as MessagesPageParam,
    // The list renders every loaded row, so the window is bounded. Loading
    // past it drops the far page; once the tail page is dropped the window is
    // off-tail, which live appends, mark-read and the bottom button already
    // handle.
    maxPages: MESSAGE_WINDOW_PAGES,
    queryFn: async ({ pageParam, signal }) =>
      fetchMessagesWindow({
        conversationId: channelId!,
        pageParam,
        limit: MESSAGE_PAGE_LIMIT[layout],
        signal,
      }),
    getPreviousPageParam: (firstPage) => {
      if (!firstPage.has_more_before) return undefined;
      // Pending optimistic rows have no server seq — feeding one as a
      // `before_seq` cursor would be meaningless. `isCommittedMessage` narrows
      // to a real `MessageSeq`; the common case (no pending at the head) still
      // resolves to `messages[0]` in O(1).
      const firstReal = firstPage.messages.find(isCommittedMessage);
      return firstReal ? { kind: "before", seq: firstReal.seq } : undefined;
    },
    getNextPageParam: (lastPage) => {
      if (!lastPage.has_more_after || lastPage.messages.length === 0) {
        return undefined;
      }
      // Symmetric to getPreviousPageParam: skip pending rows so `after_seq`
      // only ever sees a committed `MessageSeq`.
      for (let i = lastPage.messages.length - 1; i >= 0; i--) {
        const m = lastPage.messages[i]!;
        if (isCommittedMessage(m)) {
          return { kind: "after", seq: m.seq };
        }
      }
      return undefined;
    },
  });

  // Whether the cached window reaches the conversation tail. This is
  // deliberately separate from `hasNewerLiveGap`: a saved-message jump
  // can land in mid-history even when no new live message has arrived.
  const isAtTail = query.data ? lastPageIsTail(query.data as MessagesCache) : true;

  // Whenever the cache shape implies the window is back at tail, clear
  // the live-gap flag. Reaching tail can happen via three paths:
  //   1. user scrolled forward enough that `fetchNextPage` returned
  //      a page with `has_more_after: false`,
  //   2. `primeAround` happened to land on a window that already
  //      includes the tail (e.g. the saved message is recent),
  //   3. `resetToLatest` re-fetched the latest window.
  // Without this effect the gap would stay true forever once it
  // flipped, making the "scroll to latest" affordance lie.
  useEffect(() => {
    if (isAtTail && channelId) setMainlineLiveGap(qc, channelId, false);
  }, [qc, channelId, isAtTail]);

  const messages = useMemo(
    () => query.data?.pages.flatMap((page) => page.messages) ?? [],
    [query.data],
  );

  // Per-root reply counts (cut3-6 sidecar). Each page carries the full,
  // pagination-independent count for every root in its window, so merging
  // pages (later pages win) yields the authoritative count for each root
  // currently visible in the main stream. Drives the inline "{N} Reply"
  // badge on main-line message rows.
  const replyCountsByRoot = useMemo(() => {
    const merged: Record<string, number> = {};
    for (const page of query.data?.pages ?? []) {
      for (const [rootId, count] of Object.entries(page.reply_counts_by_root)) {
        merged[rootId] = count;
      }
    }
    return merged;
  }, [query.data]);

  // Per-root enriched reply-badge summaries (P1, #task:1073). Optional
  // sidecar — merged page-by-page like the counts (later pages win). Absent
  // until the backend ships `reply_summaries_by_root`; an empty map then
  // means every badge renders count-only.
  const replySummariesByRoot = useMemo(() => {
    const merged: Record<string, ReplyBadgeSummary> = {};
    for (const page of query.data?.pages ?? []) {
      if (!page.reply_summaries_by_root) continue;
      for (const [rootId, summary] of Object.entries(page.reply_summaries_by_root)) {
        merged[rootId] = summary;
      }
    }
    return merged;
  }, [query.data]);

  // Per-root inline reply PREVIEWS (#task:1746) — the tail-3 most recent
  // replies of each root in this window, merged page-by-page like the counts
  // (later pages win). Optional sidecar, and the two absences differ: a page
  // with no `reply_previews_by_root` object at all comes from a read model that
  // does not project previews (→ degrade to today's count-only badge), while a
  // page that HAS the object but no key for a root states that root had no
  // eligible reply when the window was fetched. Either way this reader shows
  // nothing for that root; the distinction is what lets the live merge open a
  // tail on a first reply without inventing one (`mergeMainlineReplyPreview`).
  // READ-INERT — nothing here observes or scores read state
  // (docs/thread-unread-v1.md §6).
  const replyPreviewsByRoot = useMemo(() => {
    const merged: Record<string, ThreadReplyPreviewRow[]> = {};
    for (const page of query.data?.pages ?? []) {
      if (!page.reply_previews_by_root) continue;
      for (const [rootId, rows] of Object.entries(page.reply_previews_by_root)) {
        merged[rootId] = rows;
      }
    }
    return merged;
  }, [query.data]);

  /** Reset the active query to the latest window. We do NOT keep the
   *  previous pages here: an around-message window may not be
   *  contiguous with latest, and stitching disjoint ranges would
   *  invalidate before/after cursors on either side. */
  const windowRows = MESSAGE_PAGE_LIMIT[layout] * MESSAGE_WINDOW_PAGES;
  const trimWindow = useCallback(
    (keep: "latest" | "earliest") => {
      if (!channelId) return;
      qc.setQueryData<MessagesCache>(messagesKey(channelId), (data) =>
        trimMessagesWindow(data, windowRows, keep),
      );
    },
    [qc, channelId, windowRows],
  );

  const resetToLatest = useCallback(() => {
    if (!channelId) return;
    setMainlineLiveGap(qc, channelId, false);
    void qc.resetQueries({ queryKey: messagesKey(channelId), exact: true });
  }, [qc, channelId]);

  // Pure reader (#task:1255 P1a): no `onWsFrame` here — the central conversation
  // ingestor is the sole consumer and owns all main-stream cache writes
  // (message append / reply-count authoritative invalidate (#1263) / task_changed
  // / reaction) plus the live-gap SET. This hook only declares INTEREST in the
  // stays subscribed while a reader is mounted; subscription is bandwidth,
  // decoupled from frame consumption / cache application. Holding zero
  // `onWsFrame` is what lets Stage-2 P2 drop the orphan gate (gate-(b)). See
  // docs/renderer-event-ingestion-v1.md §4.4.
  // #task:1260 P2: interest routes through the registry (`useInterest`) instead
  // of a direct subscribeWsScope useEffect — identical lifecycle, single
  // authority. The `channelId` null gate maps to the null-scope form; not a
  // correctness boundary (transport retains floor on unsubscribe, resume-replays
  // on re-enable, #1251).
  useInterest(channelId ? wsConversationScope(channelId) : null);

  return {
    ...query,
    data: messages,
    isAtTail,
    hasNewerLiveGap,
    resetToLatest,
    windowRows,
    trimWindow,
    replyCountsByRoot,
    replySummariesByRoot,
    replyPreviewsByRoot,
  };
}

/** Read window for one reply cluster: the root message plus its replies,
 *  served by the cut3-4 `GET /api/channels/:id/messages?thread_root_id=`
 *  route. Distinct cache key from the channel's main stream so the two
 *  windows never cross-contaminate.
 *
 *  Replies arrive on the *channel* realtime stream (the cluster is not a
 *  separate conversation), so this hook subscribes to the channel and
 *  filters live `message` events down to this root's cluster — appending
 *  at tail just like the main window. The parent channel's main-stream
 *  hook independently bumps the reply-count badge for the same event. */
export function useThreadCluster(
  channelId: string | null,
  rootMessageId: string | null,
  opts?: {
    /** WINDOW LOCK for a jump-open (#task:1317): true while this open's
     *  `primeAround` around-window must stay protected — the prime is `priming`
     *  (fetch in flight) OR `committed` (written, awaiting the visual locate).
     *  While set, the infinite query is held DISABLED so its empty-cache initial
     *  LATEST fetch cannot race / clobber the around-window, and the committed
     *  window is not re-enabled (not refetchable) until `clearJump` drops the
     *  lock. See `jumpWindowLockedForPhase` / `clusterInitialFetchEnabled`. Omit /
     *  false for a normal open — it then keeps its mount-time latest initial
     *  fetch unchanged. */
    jumpWindowLocked?: boolean;
  },
) {
  const layout = useCompactLayout() ? "compact" : "regular";
  const qc = useQueryClient();
  const enabledBase = !!channelId && !!rootMessageId;
  // The infinite query is enabled (and so allowed to auto-fire its initial
  // latest fetch on an empty cache) only when NOT holding a jump's window lock.
  // The stream subscription + epoch carrier below stay on `enabledBase` so live
  // replies and the freshness epoch are still observed while the initial fetch
  // is suppressed.
  const enabled = clusterInitialFetchEnabled({
    hasChannelAndRoot: enabledBase,
    jumpWindowLocked: opts?.jumpWindowLocked ?? false,
  });
  const key = clusterMessagesKey(channelId ?? "none", rootMessageId ?? "none");

  // Stage-2 P1b: the central ingestor owns the SET of the cluster live-gap; this
  // hook reads it as a cache fact and keeps only the position-based CLEARs.
  const hasNewerLiveGap = useClusterLiveGap(channelId, rootMessageId);

  // "This open's window fetch succeeded" signal (#task:1226 §9/§9.10), carried
  // in a dedicated cache key so BOTH writers reach it: this hook's queryFn AND
  // `primeAround` (the jump-open's around-target fetch). It is NOT
  // `dataUpdatedAt` — React Query bumps that on `setQueryData` too (an
  // optimistic live-append), which would open the mark-read gate
  // before a real window response landed. Only a successful window FETCH writes
  // this epoch, so it proves the loaded window is from this open. Read
  // reactively below via `useQuery`.
  const epochKey = clusterFetchEpochKey(channelId ?? "none", rootMessageId ?? "none");

  // Reset the gap when the consumer switches channel or thread — a stale
  // "newer replies below" hint from the previous cluster would be wrong. The
  // cleanup clears the cluster being LEFT (per-(channel,root) cache key, unlike
  // the old hook-local useState) so a stale `true` can't flash on return
  // (#task:1255 P1b, mirrors the mainline blocker-2 fix).
  useEffect(() => {
    if (!channelId || !rootMessageId) return;
    setClusterLiveGap(qc, channelId, rootMessageId, false);
    return () => setClusterLiveGap(qc, channelId, rootMessageId, false);
  }, [qc, channelId, rootMessageId]);

  const query = useInfiniteQuery<
    CachedMessagesPage,
    Error,
    InfiniteData<CachedMessagesPage, MessagesPageParam>,
    ReturnType<typeof clusterMessagesKey>,
    MessagesPageParam
  >({
    queryKey: key,
    enabled,
    // #task:1226 §9.10: do NOT auto-refetch on mount. A target-less latest
    // refetch would CLOBBER a jump-open's around-first-unread window that
    // `primeAround` wrote (the authoritative this-open window for a jump). The
    // open's fetch is driven explicitly by the consumer instead: a normal open
    // calls `refetch()` (fresh latest → marks read); a jump-open relies on
    // primeAround's around-fetch (which also stamps the epoch below).
    //
    // `refetchOnMount` only governs refetch when a cache ALREADY exists, NOT
    // the initial fetch on an EMPTY cache (that fires whenever `enabled`). So on
    // a jump-open the empty-cache initial latest fetch is suppressed a layer up,
    // via `enabled` (gated on `jumpWindowLocked`, held through `committed`), so
    // primeAround is the sole writer of this open's window and no latest can
    // pre-empt or clobber it until `clearJump` drops the lock (#task:1317).
    refetchOnMount: false,
    initialPageParam: undefined as MessagesPageParam,
    maxPages: MESSAGE_WINDOW_PAGES,
    queryFn: async ({ pageParam, signal }) => {
      const dto = await fetchMessagesWindow({
        conversationId: channelId!,
        threadRootId: rootMessageId!,
        pageParam,
        limit: MESSAGE_PAGE_LIMIT[layout],
        signal,
      });
      // Success path only — reached on a real window response, never via
      // `setQueryData`. Stamp the shared freshness epoch (§9/§9.10).
      qc.setQueryData(epochKey, Date.now());
      return dto;
    },
    getPreviousPageParam: (firstPage) => {
      if (!firstPage.has_more_before) return undefined;
      const firstReal = firstPage.messages.find(isCommittedMessage);
      return firstReal ? { kind: "before", seq: firstReal.seq } : undefined;
    },
    getNextPageParam: (lastPage) => {
      if (!lastPage.has_more_after || lastPage.messages.length === 0) {
        return undefined;
      }
      for (let i = lastPage.messages.length - 1; i >= 0; i--) {
        const m = lastPage.messages[i]!;
        if (isCommittedMessage(m)) {
          return { kind: "after", seq: m.seq };
        }
      }
      return undefined;
    },
  });

  const messages = useMemo(
    () => query.data?.pages.flatMap((page) => page.messages) ?? [],
    [query.data],
  );

  // Whether the cached cluster window reaches the thread tail. A cluster
  // can be mid-history after an around-reply jump: `primeAround` with a
  // `threadRootId` commits an around-window into this SAME cache, so this
  // is NOT always true — it mirrors `useChannelMessages.isAtTail`.
  const isAtTail = query.data ? lastPageIsTail(query.data as MessagesCache) : true;

  // Clear the gap whenever the window is back at tail (forward scroll
  // reached it, a jump landed on a tail-inclusive window, or
  // `resetToLatest` re-fetched it). Without this the hint would latch true.
  useEffect(() => {
    if (isAtTail && channelId && rootMessageId) {
      setClusterLiveGap(qc, channelId, rootMessageId, false);
    }
  }, [qc, channelId, rootMessageId, isAtTail]);

  // Reset the cluster query to the latest (tail) window. We drop the
  // around-window pages: an around-reply window may not be contiguous with
  // the tail, and stitching disjoint ranges would corrupt the cursors.
  const resetToLatest = useCallback(() => {
    if (!channelId || !rootMessageId) return;
    setClusterLiveGap(qc, channelId, rootMessageId, false);
    void qc.resetQueries({ queryKey: key, exact: true });
  }, [qc, channelId, rootMessageId, key]);

  const windowRows = MESSAGE_PAGE_LIMIT[layout] * MESSAGE_WINDOW_PAGES;
  const trimWindow = useCallback(
    (keep: "latest" | "earliest") => {
      qc.setQueryData<MessagesCache>(key, (data) => trimMessagesWindow(data, windowRows, keep));
    },
    [qc, key, windowRows],
  );

  // Pure reader (#task:1255 P1b): no `onWsFrame` here — the central conversation
  // ingestor is the sole consumer and owns the cluster cache writes (reply
  // append / live-gap SET / task chip patch / snapshot reconcile), gated on this
  // cluster reader being mounted. This hook only declares INTEREST in the
  // channel scope so it stays subscribed while open. See
  // docs/renderer-event-ingestion-v1.md §4.4.
  // #task:1260 P2: interest routes through the registry (`useInterest`) instead
  // of a direct subscribeWsScope useEffect — identical lifecycle, single
  // authority. The `enabledBase` gate maps to the null-scope form; not a
  // correctness boundary (transport retains floor on unsubscribe, resume-replays
  // on re-enable, #1251).
  useInterest(enabledBase && channelId ? wsConversationScope(channelId) : null);

  // Head of the loaded cluster window: the FIRST page's `stream_head_seq`.
  // Per §9.10 `stream_head_seq` is window-INDEPENDENT (the route captures the
  // stream head before slicing the window), so this is a correct mark VALUE for
  // ANY window — latest OR a jump's around-first-unread window — never max(seq)
  // of loaded rows. v1 is read-to-clear: opening/jumping marks to this
  // open-time head; the jump window is a locate view, not a coverage promise.
  // null until first data.
  const streamHeadSeq = query.data?.pages[0]?.stream_head_seq ?? null;

  // Reactive read of the shared freshness epoch (written by queryFn above and
  // by `primeAround` on a jump open). `initialData: 0` + `staleTime: Infinity`
  // means the placeholder queryFn never runs — the value only moves via the
  // `setQueryData` writes, observed reactively here.
  const epochQuery = useQuery({
    queryKey: epochKey,
    queryFn: () => 0,
    enabled: enabledBase,
    initialData: 0,
    staleTime: Infinity,
    gcTime: Infinity,
  });

  return {
    ...query,
    data: messages,
    isAtTail,
    hasNewerLiveGap,
    resetToLatest,
    windowRows,
    trimWindow,
    streamHeadSeq,
    windowFetchSucceededAt: epochQuery.data ?? 0,
  };
}

/** Pinned-root context for a thread (reply cluster). The root message is
 *  the anchor of the thread and must stay visible regardless of how the
 *  reply pagination window scrolls, so it is fetched independently of the
 *  tail-anchored `useThreadCluster` window.
 *
 *  A single main-stream `around_message_id=<root>&limit=1` read yields
 *  BOTH the root message object AND the authoritative cut3-6
 *  `reply_counts_by_root[root]` (the backend's main-stream window branch
 *  computes the sidecar for every root in the window — see
 *  `routes/conversations.ts`). This decouples "show the root + full reply
 *  count" from "page through replies", satisfying the invariant that the
 *  root is stably pinned and the header count is authoritative (not a
 *  loaded-rows count).
 *
 *  The reply count is recovered AUTHORITATIVELY (#1263 (c)): a non-system
 *  reply to this exact root invalidates the root cache so the header count
 *  refetches from its own sidecar — never an optimistic +1 (replay-safe). */
export function useThreadRoot(channelId: string | null, rootMessageId: string | null) {
  const enabled = !!channelId && !!rootMessageId;
  const key = threadRootKey(channelId ?? "none", rootMessageId ?? "none");

  const query = useQuery<ConversationMessagesDto>({
    queryKey: key,
    enabled,
    queryFn: ({ signal }) =>
      fetchMessagesWindow({
        conversationId: channelId!,
        aroundMessageId: rootMessageId!,
        limit: 1,
        signal,
      }),
  });

  // Pure reader (#task:1255 P1b): the central ingestor owns the root-row cache
  // writes (root reaction merge + reply-count authoritative recovery/invalidate),
  // gated on this thread-root reader being mounted. This hook only declares
  // INTEREST in the channel scope.
  // #task:1260 P2: interest routes through the registry (`useInterest`) instead
  // of a direct subscribeWsScope useEffect — identical lifecycle, single
  // authority. The compound `enabled` gate (channelId && rootMessageId) maps to
  // the null-scope form: not-enabled → no interest → transport unsubscribes
  // (drops pending, RETAINS floor); re-enable fresh-subscribes + resume-replays
  // (#1251), so `enabled` is not a correctness boundary.
  useInterest(enabled && channelId ? wsConversationScope(channelId) : null);

  const root = query.data?.messages.find((m) => m.id === rootMessageId) ?? null;
  const replyCount = rootMessageId ? (query.data?.reply_counts_by_root[rootMessageId] ?? 0) : 0;

  return {
    root,
    replyCount,
    isLoading: query.isLoading,
    isError: query.isError,
  };
}

/** Independent helper for "prime the messages cache around message X
 *  for any conversation". Lives in its own hook so the App-level
 *  jump path doesn't depend on `useChannelMessages` being mounted
 *  for the target conversation (which would be a footgun on jumps
 *  that swap the active channel). The hook holds no state — it
 *  only exposes a stable `primeAround` function bound to the
 *  current `QueryClient`.
 *
 *  Stale-guard contract: the App passes `shouldCommit?: () =>
 *  boolean`. After the network request resolves, we re-check
 *  `shouldCommit()` immediately before `setQueryData`; if it
 *  returns false we discard the response. This keeps the utility
 *  oblivious to "current jump target" — App owns that decision.
 */
export function usePrimeConversationMessages() {
  const qc = useQueryClient();
  const layout = useCompactLayout() ? "compact" : "regular";

  const primeAround = useCallback(
    async (
      conversationId: string,
      messageId: string,
      opts?: {
        shouldCommit?: () => boolean;
        signal?: AbortSignal;
        /** When set, prime the reply cluster rooted at this message id
         *  rather than the conversation's main stream. The window is
         *  fetched via `?thread_root_id=` and committed to
         *  `clusterMessagesKey(conversationId, threadRootId)`, which is
         *  exactly the cache `useThreadCluster` reads — so a cluster jump
         *  lands on the right window instead of 404-ing against the main
         *  stream (whose anchor requires `thread_root_id IS NULL`). */
        threadRootId?: string;
      },
    ): Promise<{ committed: boolean; reason?: "stale" | "aborted" | "not_found" }> => {
      // A cluster target writes to the cluster cache; everything else to
      // the main stream cache. Both the fetch scope and the commit key must
      // agree, or we'd prime one window and read another.
      const cacheKey = opts?.threadRootId
        ? clusterMessagesKey(conversationId, opts.threadRootId)
        : messagesKey(conversationId);
      try {
        if (opts?.signal?.aborted) return { committed: false, reason: "aborted" };
        const page = await fetchMessagesWindow({
          conversationId,
          aroundMessageId: messageId,
          threadRootId: opts?.threadRootId,
          limit: AROUND_MESSAGE_LIMIT[layout],
          // Plumb signal into the underlying fetch so an abort on the
          // App side actually cancels the in-flight request, not just
          // the post-response commit. Without this, callers that only
          // pass `signal` (and not `shouldCommit`) would still race
          // against the network round-trip.
          signal: opts?.signal,
        });
        if (opts?.signal?.aborted) return { committed: false, reason: "aborted" };
        if (opts?.shouldCommit && !opts.shouldCommit()) {
          return { committed: false, reason: "stale" };
        }
        // Cancel any in-flight latest/before/after fetch so React
        // Query doesn't overwrite our prime with a stale response.
        await qc.cancelQueries({
          queryKey: cacheKey,
          exact: true,
        });
        // `cancelQueries` is async — by the time it resolves the
        // user may have clicked another saved message. Re-check both
        // guards before committing so we never write a stale window
        // that the App has already moved past.
        if (opts?.signal?.aborted) return { committed: false, reason: "aborted" };
        if (opts?.shouldCommit && !opts.shouldCommit()) {
          return { committed: false, reason: "stale" };
        }
        // Replace the entire cache with this single window. The
        // around window may not be contiguous with whatever the
        // hook held before, so stitching is unsafe; the hook will
        // page outward from here using before/after cursors.
        qc.setQueryData<MessagesCache>(cacheKey, makeMessagesCache(page, undefined));
        // §9.10: a jump-open's around-target fetch IS this open's authoritative
        // window fetch. Stamp the shared freshness epoch so `useThreadCluster`'s
        // mark-read gate opens with THIS around-window (not a clobbering latest
        // refetch). Only for the cluster path — main-stream primes don't gate a
        // thread mark-read.
        if (opts?.threadRootId) {
          qc.setQueryData(clusterFetchEpochKey(conversationId, opts.threadRootId), Date.now());
        }
        return { committed: true };
      } catch (err) {
        // Aborted fetches surface as DOMException 'AbortError'; treat
        // as a non-error stale path so callers don't have to wrap.
        if ((err as { name?: string } | null)?.name === "AbortError" || opts?.signal?.aborted) {
          return { committed: false, reason: "aborted" };
        }
        // 404 from the backend means the message id is not in this
        // conversation. Surface that to App so it can clear the
        // jump target instead of spinning on a phantom.
        if (err instanceof ApiError && err.status === 404) {
          return { committed: false, reason: "not_found" };
        }
        throw err;
      }
    },
    [qc, layout],
  );

  return { primeAround };
}
