import { useMemo } from "react";
import { useInfiniteQuery } from "@tanstack/react-query";
import type { InfiniteData, QueryClient } from "@tanstack/react-query";
import {
  apiPaths,
  type ListChannelThreadsResponse,
  type ThreadSummary,
  type ThreadSummaryInvariant,
} from "@silverhand/shared";
import { api } from "@/lib/api";
import { wsConversationScope } from "@/features/realtime/conversation-scope";
import { useInterest } from "@/features/realtime/interest-registry";

// Re-export so existing consumers (ChannelThreadsPanel) keep their import path
// while the type now lives in the shared barrel (#task:1074 backend landing).
export type { ThreadSummary } from "@silverhand/shared";

/**
 * Channel Threads-list data (#task:1073, P2). Lists every reply-cluster in
 * one channel, newest activity first, so threaded messages are discoverable
 * without scrolling the whole stream.
 *
 * Backed by `GET /api/conversations/:id/threads` (#task:1074). Shape:
 * `ListChannelThreadsResponse` from `@silverhand/shared`.
 */

export const channelThreadsKey = (channelId: string) =>
  ["channels", channelId, "threads-list"] as const;

/** The two threads-list datasets (#task:1773). `all` is the full channel index
 *  (recency-sorted, follow-agnostic); `followed-unread` is the attention subset
 *  the header dot mirrors. They are TWO datasets, never one list re-sorted — the
 *  server owns each keyset (never couple per-viewer unread into the All order). */
export type ChannelThreadsFilter = "all" | "followed-unread";

/** Query key for the `followed ∩ unread` filtered dataset (#task:1773). A CHILD
 *  of {@link channelThreadsKey}, so every family prefix-invalidate (observe,
 *  message, snapshot, off-page fallback) refreshes it for FREE. The S1
 *  `threads_list_entry` merge writes ONLY the exact All key (below), so this
 *  filtered view is never merged locally — its membership is viewer-specific and
 *  server-authoritative, and always settles via invalidate→refetch (Cindy
 *  freshness gate: filtered never guesses rows, only refetches). */
export const channelThreadsUnreadKey = (channelId: string) =>
  ["channels", channelId, "threads-list", "followed-unread"] as const;

/** True when a `useChannelThreads(channelId)` reader is mounted — the exact
 *  threads-list key has at least one active observer. Stage-2 P1c gate
 *  (#task:1255): the central ingestor took over `useChannelThreads.onEvent`,
 *  which only ran while that hook was mounted, so the list is invalidated only
 *  while a reader is alive (interest-only scope subscription must not). */
export function hasActiveChannelThreadsReader(qc: QueryClient, channelId: string): boolean {
  const entry = qc.getQueryCache().find({ queryKey: channelThreadsKey(channelId), exact: true });
  return (entry?.getObserversCount() ?? 0) > 0;
}

/** Invalidate the channel Threads list (#task:1255 P1c) — verbatim move of
 *  `useChannelThreads.onEvent`: any reply (or a reconnect needing a fresh
 *  snapshot) can change the list's contents or order, so refetch. Both the
 *  `message` and `snapshot_required` branches did exactly this. */
export function invalidateChannelThreadsList(qc: QueryClient, channelId: string): Promise<void> {
  // Returns the invalidate Promise (reconcile-settle signal for the P2 snapshot
  // join, #1255 P2); the message-path caller ignores it, strategy unchanged.
  return qc.invalidateQueries({ queryKey: channelThreadsKey(channelId) });
}

/** Invalidate ONLY the `followed ∩ unread` filtered dataset (#task:1773) — never
 *  the exact All key. Used on the S1-patched message path (`conversation-ingestor`):
 *  there the All list took the in-place `threads_list_entry` merge INSTEAD of a
 *  blanket invalidate, but the filtered sibling is never merged (viewer-specific
 *  membership), so it must still refetch. The non-patched / snapshot / observe
 *  paths already reach this child through the {@link channelThreadsKey} family
 *  prefix-invalidate, so this narrow form is only for the merge case. */
export function invalidateChannelThreadsUnread(qc: QueryClient, channelId: string): Promise<void> {
  return qc.invalidateQueries({ queryKey: channelThreadsUnreadKey(channelId) });
}

/** The SINGLE documented threads-list keyset comparator —
 *  `(last_reply_at DESC, root_seq DESC)`.
 *
 *  ┌─ SOURCE FENCE (#task:1279) ────────────────────────────────────────────┐
 *  │ This comparator is the CLIENT MIRROR of the server's authoritative order.│
 *  │ Authoritative source: `packages/backend/src/repos/message-repo.ts`,     │
 *  │ `listChannelThreads` — `ORDER BY lr.created_at DESC, root.seq DESC`      │
 *  │ (keyset cursor: `lr.created_at < ? OR (lr.created_at = ? AND            │
 *  │ root.seq < ?)`). If that ORDER BY / cursor tuple ever changes, THIS     │
 *  │ comparator AND its golden fence test (`use-channel-threads.test.ts` →     │
 *  │ "SOURCE FENCE") MUST change in the same commit — otherwise a client     │
 *  │ repositioning a row from an S1 `threads_list_entry` patch (#task:1278)  │
 *  │ silently drifts from the authoritative order (Cindy gate ②). The        │
 *  │ reciprocal anchor is a comment on the server ORDER BY pointing back     │
 *  │ here, so a grep from either side lands on the other.                    │
 *  └────────────────────────────────────────────────────────────────────────┘
 *
 *  This is the ONE place the order is expressed — never scatter ad-hoc `.sort`
 *  keys; `root_seq` is the immutable tie-break when `last_reply_at` ties to the
 *  millisecond across roots. */
export function compareThreadSummary(a: ThreadSummary, b: ThreadSummary): number {
  if (b.last_reply_at !== a.last_reply_at) return b.last_reply_at - a.last_reply_at;
  // root_seq is a branded MessageSeq (numeric ordinal); DESC tie-break.
  return b.root_seq - a.root_seq;
}

/** Outcome of merging a `threads_list_entry` patch into the held pages (#task:1278):
 *   - `merged`     — the root was on PAGE 0 (the newest page) AND reordering it in
 *                    place can't desync a keyset cursor (page 0 is terminal, or
 *                    later pages are loaded so the live boundary sits on the last
 *                    page); its viewer-invariant fields were overwritten with the
 *                    absolute values and page 0 re-sorted by the keyset
 *                    (viewer_following kept from the local row). A reply bumps
 *                    `last_reply_at` to now, so the row rises to the head.
 *   - `off-page`   — the local merge is unsafe or a cross-page guess, so we defer to
 *                    invalidate→refetch (which refetches every loaded page and a
 *                    fresh cursor, re-deriving the order). Three cases: the root is
 *                    absent from every loaded page; it sits on a LATER page (a reply
 *                    would move it to page 0 — a cross-page membership guess); or it
 *                    is on a single, non-terminal page 0 whose tail-bound
 *                    `next_cursor` a reorder would desync (#task:1773 Cindy R2 gate).
 *   - `cold-absent`— no page data loaded (reader mounted but still fetching) → its
 *                    own mount-fetch is the authoritative recovery → vacuous. */
export type ThreadsListMergeOutcome = "merged" | "off-page" | "cold-absent";

/** Merge an absolute `ThreadSummaryInvariant` into the held threads-list PAGES,
 *  keeping the local `viewer_following` (the broadcast frame carries no per-viewer
 *  fact — Cindy gate ③), then re-sort by {@link compareThreadSummary}. The cache
 *  is an {@link InfiniteData} of keyset pages; this only repositions a row already
 *  on PAGE 0 and only when that reorder can't desync a live `next_cursor` — never
 *  inserts, never moves a row across a page boundary, and never reorders the sole
 *  loaded page while it still has a next page (see {@link ThreadsListMergeOutcome};
 *  those all fall back to refetch so the order + cursor are re-derived, not
 *  guessed). Replay-idempotent: an identical entry (same `last_reply_message_id` +
 *  sits, and the absolute overwrite + deterministic sort make a re-applied changed
 *  entry converge to the same page. */
export function mergeThreadsListEntry(
  qc: QueryClient,
  channelId: string,
  entry: ThreadSummaryInvariant,
): ThreadsListMergeOutcome {
  const prev = qc.getQueryData<InfiniteData<ListChannelThreadsResponse, string | null>>(
    channelThreadsKey(channelId),
  );
  if (!prev || prev.pages.length === 0) return "cold-absent";

  // Locate the root across every loaded page (the merge/refetch decision needs to
  // know WHICH page holds it, not just whether it's present).
  let pageIndex = -1;
  let existing: ThreadSummary | undefined;
  for (const [i, pg] of prev.pages.entries()) {
    const found = pg.threads.find((t) => t.root_message_id === entry.root_message_id);
    if (found) {
      pageIndex = i;
      existing = found;
      break;
    }
  }
  if (!existing) return "off-page";

  // Idempotency short-circuit BEFORE the page gate (mirrors P1's scalar guard): a
  // reply always changes the last-reply identity + count, so equal on both ⇒
  // nothing to merge and the row is already correctly positioned → no write and no
  // refetch, on WHICHEVER page the row sits (an exact replay must stay a no-op even
  // for a deep-page row). root_preview/root_* only change on non-reply edits, which
  // don't emit this projection.
  if (
    existing.last_reply_message_id === entry.last_reply_message_id &&
    existing.reply_count === entry.reply_count
  ) {
    return "merged";
  }

  // A genuine change bumps `last_reply_at` to now (>= every held row), so the row
  // belongs at the head of page 0. If it's on a LATER page, repositioning it there
  // crosses a page boundary — the cross-page membership guess we refuse. Fall back
  // to invalidate→refetch, which refetches every loaded page in keyset order.
  if (pageIndex > 0) return "off-page";

  // pageIndex === 0 (the row is on the newest page). But a keyset `next_cursor`
  // is bound to the page's TAIL TUPLE, not its membership set — so reordering
  // page 0 can desync the boundary `fetchNextPage` resumes from. `fetchNextPage`
  // rides the LAST loaded page's cursor, so this only bites when page 0 IS the
  // last page (single page loaded) AND still has a next page: bumping the tail
  // row to the head leaves a NEW tail, but the opaque cursor still points at the
  // OLD tail → the next fetch resumes from the wrong boundary (skips/duplicates).
  // Refuse the local merge there and fall back to the authoritative refetch,
  // which rebuilds page 0 + a fresh cursor (#task:1773 Cindy R2 gate). Safe to
  // merge in place otherwise: a terminal page 0 (`next_cursor === null`) has no
  // next boundary, and with later pages loaded the unloaded boundary lives on the
  // last page, which a page-0 reorder never touches.
  const page0 = prev.pages[0];
  if (!page0) return "cold-absent";
  if (prev.pages.length === 1 && page0.next_cursor !== null) return "off-page";
  // Absolute overwrite of every viewer-invariant field; KEEP the local
  // viewer_following (the frame carries none — never default it, Cindy gate ③).
  const merged: ThreadSummary = {
    ...entry,
    viewer_following: existing.viewer_following,
  };
  const nextThreads = page0.threads
    .map((t) => (t.root_message_id === entry.root_message_id ? merged : t))
    .sort(compareThreadSummary);
  const nextPages = prev.pages.slice();
  nextPages[0] = { ...page0, threads: nextThreads };
  qc.setQueryData<InfiniteData<ListChannelThreadsResponse, string | null>>(
    channelThreadsKey(channelId),
    { ...prev, pages: nextPages },
  );
  return "merged";
}

/**
 * Reactive channel Threads list. Refetches when the channel message stream
 * advances (a new reply bumps a cluster's `last_reply_at` / `reply_count`,
 * or a new reply turns a plain root into a thread), so an open list re-sorts
 * without manual refresh.
 *
 * `filter` selects the dataset (#task:1773): `all` (default — the full,
 * follow-agnostic index, keyed by {@link channelThreadsKey}) or
 * `followed-unread` (the `followed ∩ unread` attention subset via
 * `?filter=followed-unread`, keyed by {@link channelThreadsUnreadKey}). Both are
 * plain readers over the SAME endpoint/projection; only the All key participates
 * in the S1 `threads_list_entry` merge — the filtered dataset always refetches.
 *
 * Cursor-paginated (keyset over `(last_reply_at, root seq)`): the first page
 * loads on mount and `fetchNextPage` walks the opaque `next_cursor` for infinite
 * scroll. Each next-page request carries THIS dataset's `filter`, so the filtered
 * set paginates its own keyset (backend #task:1774) and never silently reverts to
 * All on a later page. The S1 merge still writes ONLY page 0 of the All cache and
 * never guesses cross-page membership (see {@link mergeThreadsListEntry}).
 */
const NO_THREADS: ThreadSummary[] = [];

export function useChannelThreads(
  channelId: string | null,
  filter: ChannelThreadsFilter = "all",
): {
  threads: ThreadSummary[];
  isLoading: boolean;
  isError: boolean;
  isFetchingNextPage: boolean;
  hasNextPage: boolean;
  fetchNextPage: () => void;
  refetch: () => void;
} {
  const enabled = !!channelId;
  const key =
    filter === "followed-unread"
      ? channelThreadsUnreadKey(channelId ?? "none")
      : channelThreadsKey(channelId ?? "none");

  const query = useInfiniteQuery<
    ListChannelThreadsResponse,
    Error,
    InfiniteData<ListChannelThreadsResponse, string | null>,
    typeof key,
    string | null
  >({
    queryKey: key,
    enabled,
    initialPageParam: null,
    queryFn: ({ pageParam, signal }) => {
      const base = apiPaths.conversationThreads(channelId!);
      // The filter rides EVERY page (not just page 1) so a paginated filtered set
      // stays filtered; `cursor` is the opaque keyset token from the prior page.
      const params = new URLSearchParams();
      if (filter === "followed-unread") params.set("filter", "followed-unread");
      if (pageParam != null) params.set("cursor", pageParam);
      const qs = params.toString();
      return api<ListChannelThreadsResponse>(qs ? `${base}?${qs}` : base, {
        signal,
      });
    },
    getNextPageParam: (lastPage) => lastPage.next_cursor,
  });

  // Pure reader (#task:1255 P1c): no `onWsFrame` here — the central conversation
  // ingestor invalidates this list (gated on a mounted reader) when a reply
  // lands or a reconnect needs a fresh snapshot. We keep only the WS scope
  // INTEREST alive so the transport stays subscribed while the panel is open.
  // #task:1260 P2: interest is declared through the registry (was a direct
  // subscribeWsScope useEffect) — identical lifecycle, single authority. The
  // `enabled` gate maps to the null-scope form: disabled → no interest → the
  // transport unsubscribes (drops pending, RETAINS floor); re-enable
  // fresh-subscribes + resume-replays frames missed while disabled (#1251), so
  // `enabled` is not a correctness boundary.
  useInterest(channelId ? wsConversationScope(channelId) : null);

  // Flattened once per page set, so consumers memoizing on the list keep their
  // result between renders.
  const pages = query.data?.pages;
  const threads = useMemo(() => pages?.flatMap((p) => p.threads) ?? NO_THREADS, [pages]);

  return {
    threads,
    isLoading: query.isLoading,
    isError: query.isError,
    isFetchingNextPage: query.isFetchingNextPage,
    hasNextPage: !!query.hasNextPage,
    fetchNextPage: () => {
      if (query.hasNextPage && !query.isFetchingNextPage) {
        void query.fetchNextPage();
      }
    },
    refetch: () => {
      void query.refetch();
    },
  };
}
