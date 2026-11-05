// Channel message-cache core (#task:1292): the MessagesCache type family, query
// key builders, and the low-level cache primitives (append / dedup / replace),
// extracted verbatim from hooks.ts. The append trio owns the load-bearing
// replay-dedup fenced by hooks/realtime-ingestion-read-inert.test.ts (which now
// reads THIS file for that slice). Leaf module — no dependency on sibling
// channel modules.
import type { InfiniteData } from "@tanstack/react-query";
import type { ConversationMessagesDto, MessageDto, MessageSeq } from "@silverhand/shared";
import { decodeMessageSeq } from "@silverhand/shared";
import {
  isPendingMessage,
  type CachedMessage,
  type PendingMessage,
} from "@/features/channels/pending-message";

/** Page-param shape for the bidirectional infinite query.
 *  - `undefined` (the initial param) → fetch the latest window.
 *  - `{ kind: 'before', seq }` → fetch a window with seq < `seq`.
 *  - `{ kind: 'after', seq }` → fetch a window with seq > `seq`.
 *  We model it as a tagged union instead of a bare number so the
 *  hook knows which direction it's paging without re-deriving it
 *  from "is this a fetchPrevious or fetchNext call". React Query's
 *  inferred page-param type stays `MessagesPageParam` end-to-end. */
export type MessagesPageParam =
  | undefined
  | { kind: "before"; seq: number }
  | { kind: "after"; seq: number };

/** A cached messages page. Identical to the wire {@link ConversationMessagesDto}
 *  except the message list may also hold optimistic {@link PendingMessage} rows
 *  (which the wire never carries). A server response widens into this freely
 *  (`MessageDto[]` ⊆ `CachedMessage[]`); only optimistic inserts add the pending
 *  variant. */
export type CachedMessagesPage = Omit<ConversationMessagesDto, "messages"> & {
  messages: CachedMessage[];
};

export type MessagesCache = InfiniteData<CachedMessagesPage, MessagesPageParam>;

export const messagesKey = (cid: string) => ["channels", cid, "messages"] as const;
/** Cache key for one reply cluster's window. A cluster lives in the same
 *  channel conversation as its root (rows with `thread_root_id = rootId`),
 *  so it is keyed off the channel id plus the root message id — distinct
 *  from the main-stream window (`messagesKey`) which excludes replies. */
export const clusterMessagesKey = (cid: string, rootId: string) =>
  ["channels", cid, "cluster", rootId, "messages"] as const;
/** Cache key carrying the "this-open window fetch succeeded" epoch (ms) for a
 *  reply cluster (#task:1226 §9/§9.10). Written on the success path by both
 *  `useThreadCluster`'s queryFn AND `primeAround`'s around-target fetch, so a
 *  jump open arms the mark-read freshness gate the same way a normal open does.
 *  Deliberately separate from `clusterMessagesKey` so `setQueryData` appends to
 *  the window (live reply / optimistic patch) never advance it. */
export const clusterFetchEpochKey = (cid: string, rootId: string) =>
  ["channels", cid, "cluster", rootId, "fetch-epoch"] as const;
/** Cache key for the thread pane's root row. The root message lives in the
 *  main stream, but ThreadPanel loads it through its own single-row window
 *  query so the pane can mount independently of the channel list. */
export const threadRootKey = (cid: string, rootId: string) =>
  ["channels", cid, "thread-root", rootId] as const;
/** Cache key for one reply cluster's human-follower state (cut3 slice B). */
export const threadFollowersKey = (cid: string, rootId: string) =>
  ["channels", cid, "thread-followers", rootId] as const;
export const agentsKey = (cid: string) => ["channels", cid, "agents"] as const;
export const agentRuntimeKey = (cid: string) => ["channels", cid, "agent-runtime"] as const;
/** Per-window UI-affordance fact (#task:1255 Stage-2): a live mainline message
 *  arrived while this window was NOT at tail, so the UI offers "scroll to
 *  latest" instead of lying with an append. It is NOT the message cache and NOT
 *  a read/cursor input — purely a hint. Stage-2 moves the SET onto the central
 *  conversation ingestor (so the reader view holds zero `onWsFrame`); the view
 *  keeps the position-based CLEARS (channel switch / reached tail / reset),
 *  which are explicit-helper writes of this affordance key, never a frame apply
 *  and never the message cache. See docs/renderer-event-ingestion-v1.md §5. */
export const mainlineLiveGapKey = (cid: string) =>
  ["channels", cid, "live-gap", "mainline"] as const;
/** Per-cluster sibling of {@link mainlineLiveGapKey} (#task:1255 Stage-2 P1b):
 *  a live reply arrived while THIS cluster window was not at tail. Same
 *  contract — ingestor SETs, the reader view CLEARs on position changes. */
export const clusterLiveGapKey = (cid: string, rootId: string) =>
  ["channels", cid, "cluster", rootId, "live-gap"] as const;
export const MESSAGE_PAGE_LIMIT = 100;
/** Pages kept in the channel window. Phones get one fewer: a full window
 *  measured ~30 ms per relayout on an iPhone 12 Pro (rotation, keyboard). */
export const MESSAGE_WINDOW_PAGES = { regular: 3, compact: 2 } as const;
export const AROUND_MESSAGE_LIMIT = 100;

export function makeMessagesCache(
  page: CachedMessagesPage,
  pageParam: MessagesPageParam = undefined,
): MessagesCache {
  return { pages: [page], pageParams: [pageParam] };
}

export function cacheHasMessage(data: MessagesCache | undefined, messageId: string): boolean {
  return data?.pages.some((page) => page.messages.some((m) => m.id === messageId)) ?? false;
}

/** Whether a reply badge for `rootId` is on a loaded main-stream page — the
 *  root's `reply_counts_by_root` key is already present OR the root message row
 *  is in the window. #1263 (c) gates the authoritative reply-count recovery on
 *  this: an ON-window root → invalidate → refetch the count; an OFF-window root
 *  → no-op, since the badge isn't displayed and the next window fetch re-derives
 *  the authoritative count when the user paginates to it (#task:1255 §4.1). */
export function cacheHasRootOrKey(data: MessagesCache | undefined, rootId: string): boolean {
  return (
    data?.pages.some(
      (page) =>
        Object.prototype.hasOwnProperty.call(page.reply_counts_by_root, rootId) ||
        page.messages.some((m) => m.id === rootId),
    ) ?? false
  );
}

/** Live-append is only safe when the last cached page is the tail
 *  of the conversation — otherwise we would lie to the user (the
 *  appended message wouldn't follow the message right above it).
 *  Callers MUST gate on this; the helper itself stays loose so the
 *  optimistic-send path can still bootstrap an empty cache. */
export function lastPageIsTail(data: MessagesCache | undefined): boolean {
  if (!data || data.pages.length === 0) return true;
  const last = data.pages[data.pages.length - 1];
  return last?.has_more_after === false;
}

/** Shared append core. `bootstrapHead` is the `stream_head_seq` stamped only
 *  when this append bootstraps a page from an empty cache; an append into an
 *  existing tail page carries the prior head forward untouched. A committed
 *  message supplies its own seq as the head; a pending row has none, so its
 *  caller passes a provisional head (§3.3) — that page is provisional anyway
 *  (replaced on server ack / refetch). */
export function appendToCache(
  data: MessagesCache | undefined,
  msg: CachedMessage,
  bootstrapHead: MessageSeq,
): MessagesCache {
  if (!data) {
    return makeMessagesCache({
      messages: [msg],
      reply_counts_by_root: {},
      has_more_before: false,
      has_more_after: false,
      stream_head_seq: bootstrapHead,
    });
  }
  if (cacheHasMessage(data, msg.id)) return data;

  if (data.pages.length === 0) {
    return {
      ...data,
      pages: [
        {
          messages: [msg],
          reply_counts_by_root: {},
          has_more_before: false,
          has_more_after: false,
          stream_head_seq: bootstrapHead,
        },
      ],
      pageParams: [undefined as MessagesPageParam],
    };
  }

  const lastIndex = data.pages.length - 1;
  return {
    ...data,
    pages: data.pages.map((page, index) =>
      index === lastIndex ? { ...page, messages: [...page.messages, msg] } : page,
    ),
  };
}

/** Append a committed (server-acked) message. For a bootstrap page `msg.seq` IS
 *  the new stream head; for a live-append into an existing tail the head is
 *  carried forward. This mainline cache's head is never consumed as a thread
 *  mark-read snapshot — that comes only from the thread window response
 *  (`useThreadCluster` → stream_head_seq) — so it cannot swallow a tail. */
export function appendMessageToCache(
  data: MessagesCache | undefined,
  msg: MessageDto,
): MessagesCache {
  return appendToCache(data, msg, msg.seq);
}

/** Append an optimistic pending message (no server seq yet). If it bootstraps an
 *  empty cache the page head is provisional (0) — honest "no server head yet",
 *  corrected on the server ack (`replacePendingMessageInCache`) or a refetch. */
export function appendPendingMessage(
  data: MessagesCache | undefined,
  msg: PendingMessage,
): MessagesCache {
  return appendToCache(data, msg, decodeMessageSeq(0));
}

export function removeMessageFromCache(
  data: MessagesCache | undefined,
  messageId: string,
): MessagesCache | undefined {
  if (!data) return data;
  return {
    ...data,
    pages: data.pages.map((page) => ({
      ...page,
      messages: page.messages.filter((m) => m.id !== messageId),
    })),
  };
}

/** Insert a committed row into the tail page at the position dictated by its
 *  immutable {@link MessageSeq}. Committed rows stay seq-ordered; pending rows
 *  carry no seq, so they keep their tail-by-insertion position and a committed
 *  insert always lands before the first pending row. An empty / page-less cache
 *  bootstraps via {@link appendMessageToCache} (which stamps the head). Callers
 *  own id-dedup — this helper does not check for an existing row. */
function insertCommittedBySeq(data: MessagesCache | undefined, msg: MessageDto): MessagesCache {
  if (!data || data.pages.length === 0) {
    return appendMessageToCache(data, msg);
  }
  const lastIndex = data.pages.length - 1;
  return {
    ...data,
    pages: data.pages.map((page, index) => {
      if (index !== lastIndex) return page;
      const msgs = page.messages;
      let at = msgs.length;
      for (let i = 0; i < msgs.length; i++) {
        const existing = msgs[i]!;
        if (isPendingMessage(existing) || existing.seq > msg.seq) {
          at = i;
          break;
        }
      }
      return {
        ...page,
        messages: [...msgs.slice(0, at), msg, ...msgs.slice(at)],
      };
    }),
  };
}

/** Converge the optimistic temp row against the committed message on ACK.
 *
 *  The ACK swap only has authority over a cache that still owns THIS mutation —
 *  and the committed row's position is governed by its immutable
 *  {@link MessageSeq}, never by ACK arrival order (the ACK is itself an
 *  authoritative, potentially out-of-order input: two local sends can race and
 *  ACK newest-first). Three cases, by what the cache still holds:
 *  - **committed id already present** (a WS echo delivered it in seq order — WS
 *    is the in-order transport) → drop only the temp row; never move it.
 *  - **committed absent, temp present** → the cache still owns this send. Swap:
 *    drop the temp row and seq-upsert the committed row into the tail page
 *    (before the first pending row or the first committed row of a higher seq),
 *    so a later WS echo dedupes by id and does not reorder it.
 *  - **neither present** → a jump/prime replaced this window (temp gone,
 *    possibly a non-tail authoritative window). The swap has NO right to splice a
 *    row into a window it no longer owns — that would break seq adjacency.
 *    Return it untouched; the shared committed-message seam then applies the row
 *    under the existing tail / gap-affordance / eviction rules. */
export function replacePendingMessageInCache(
  data: MessagesCache | undefined,
  tempId: string,
  msg: MessageDto,
): MessagesCache | undefined {
  if (cacheHasMessage(data, msg.id)) {
    return removeMessageFromCache(data, tempId);
  }
  if (cacheHasMessage(data, tempId)) {
    return insertCommittedBySeq(removeMessageFromCache(data, tempId), msg);
  }
  return data;
}

/** Converge the optimistic temp row when a send FAILS. An HTTP error is an
 *  AMBIGUOUS result — the send may still have committed and a WS frame (own
 *  echo or a concurrent message) may already have applied to this cache. So the
 *  rollback must undo ONLY this mutation's optimistic temp fact, never overwrite
 *  committed facts that arrived after the optimistic write.
 *
 *  Mutation-local CAS, no persistent coordination state: `optimistic` is the
 *  exact cache reference written when the temp row was appended.
 *  - `current === optimistic` → nothing authoritative/concurrent landed since
 *    (an equal-valued projection merge short-circuits to the SAME reference, so
 *    a structural no-op counts here too — by definition no committed fact sits
 *    on this key). Restore the prior snapshot, recovering a rebuilt non-tail
 *    browsing window. `prev` may be undefined (bootstrapped from empty) → fall
 *    through to surgical removal.
 *  - reference changed → a committed/concurrent fact is present. Remove only the
 *    temp row; keep the committed row, concurrent rows, and any advanced
 *    projections/floor (which live on their own keys). */
export function rollbackOptimisticSend(
  current: MessagesCache | undefined,
  prev: MessagesCache | undefined,
  optimistic: MessagesCache | undefined,
  tempId: string,
): MessagesCache | undefined {
  if (prev !== undefined && current === optimistic) {
    return prev;
  }
  return removeMessageFromCache(current, tempId);
}

/** Only this authenticated sender's exact logical send can replace a pending row. */
export function reconcilePendingMessage(
  data: MessagesCache | undefined,
  msg: MessageDto,
  userId: string | null,
): MessagesCache | undefined {
  if (!userId || msg.from_kind !== "user" || msg.from_user_id !== userId || !msg.client_message_id)
    return data;
  const pending = data?.pages
    .flatMap((page) => page.messages)
    .find(
      (row) =>
        isPendingMessage(row) &&
        row.from_user_id === userId &&
        row.conversation_id === msg.conversation_id &&
        row.thread_root_id === msg.thread_root_id &&
        row.client_message_id === msg.client_message_id,
    );
  return pending ? replacePendingMessageInCache(data, pending.id, msg) : data;
}
