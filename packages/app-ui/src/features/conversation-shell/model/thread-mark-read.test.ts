import { describe, expect, it } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import {
  clusterInitialFetchEnabled,
  clusterWindowLocked,
  threadClusterShowsLoader,
  threadLiveMarkReadSeq,
  threadMarkReadSnapshot,
  threadOpenNeedsLatestRefetch,
  threadReplyRows,
  threadTailIsVisible,
  threadVisibleReplySeq,
  type ThreadMarkReadGateInput,
  type ThreadLiveMarkReadInput,
} from "./thread-mark-read";
import { clusterFetchEpochKey } from "@/features/channels/hooks";
import type { CachedMessage } from "@/features/channels/pending-message";
import { decodeMessageSeq, type MessageDto } from "@silverhand/shared";

/** A committed reply/root row for the cursor-domain fixtures below. */
function message(overrides: Partial<Omit<MessageDto, "seq">> & { seq?: number } = {}): MessageDto {
  const { seq, ...rest } = overrides;
  return {
    id: "m1",
    conversation_id: "c1",
    from_kind: "user",
    from_agent_id: null,
    from_user_id: "u1",
    author: {
      status: "active",
      kind: "user",
      id: "u1",
      name: "User",
      display_name: "User",
      avatar_url: null,
      runtime: null,
    },
    content: "body",
    metadata: null,
    attachments: null,
    seq: decodeMessageSeq(seq ?? 1),
    created_at: 0,
    thread_root_id: null,
    linked_task: null,
    reactions: null,
    ...rest,
  };
}

// Baseline: a clean open whose fetch has succeeded after open, nothing marked
// yet, window head loaded. Each test overrides only what it exercises.
// `headSeq` is taken as a raw number (or null) and decoded to a MessageSeq here
// (slice-A pattern), so overriding call-sites keep passing plain literals.
function gate(
  over: Partial<Omit<ThreadMarkReadGateInput, "headSeq">> & {
    headSeq?: number | null;
  } = {},
): ThreadMarkReadGateInput {
  const { headSeq, ...rest } = over;
  return {
    alreadyMarkedForOpen: false,
    isFetching: false,
    headSeq:
      headSeq === undefined
        ? decodeMessageSeq(42)
        : headSeq === null
          ? null
          : decodeMessageSeq(headSeq),
    windowFetchSucceededAt: 2_000,
    openedAt: 1_000,
    ...rest,
  };
}

describe("threadMarkReadSnapshot — fresh-for-this-open gate (#task:1226 §9)", () => {
  it("marks with the window head once this open’s fetch has succeeded", () => {
    expect(threadMarkReadSnapshot(gate({}))).toBe(42);
  });

  it("does not re-mark within the same open (once-guard)", () => {
    expect(threadMarkReadSnapshot(gate({ alreadyMarkedForOpen: true }))).toBeNull();
  });

  it("waits while a fetch is in flight", () => {
    expect(threadMarkReadSnapshot(gate({ isFetching: true }))).toBeNull();
  });

  it("waits until a window has loaded (null head)", () => {
    expect(threadMarkReadSnapshot(gate({ headSeq: null }))).toBeNull();
  });

  // --- The blocker Cindy caught: reopen serves the previous open's cache. ---

  it("NEGATIVE (a): does not mark on the previous open’s stale cache", () => {
    // Reopen at openedAt=5000; React Query synchronously returns the prior
    // open's cached window (head S=42) whose success epoch (2000) predates this
    // open. Must NOT mark, and must NOT arm the once-guard, so the fresh fetch
    // can still mark.
    const stale = gate({
      openedAt: 5_000,
      windowFetchSucceededAt: 2_000, // last success was the PREVIOUS open
      headSeq: 42,
    });
    expect(threadMarkReadSnapshot(stale)).toBeNull();
  });

  it("NEGATIVE (b): does not mark when setQueryData updated data before this open’s refetch succeeded", () => {
    // During this open, a setQueryData mutation (e.g. a live row append) updates
    // the cache. That does NOT advance windowFetchSucceededAt, so even with
    // fresh-looking data present and no fetch in flight, the gate stays closed
    // until the real window response lands.
    const setDataOnly = gate({
      openedAt: 5_000,
      windowFetchSucceededAt: 2_000, // unchanged by setQueryData
      headSeq: 99, // head value present, but not from a window fetch this open
      isFetching: false,
    });
    expect(threadMarkReadSnapshot(setDataOnly)).toBeNull();
  });

  it("marks once this open’s window refetch finally succeeds with the new head", () => {
    // The fix's success path: after the reopen, the real window response lands
    // (success epoch 6000 >= openedAt 5000) carrying the new head S+1.
    const fresh = gate({
      openedAt: 5_000,
      windowFetchSucceededAt: 6_000,
      headSeq: 43, // S+1 — the reply that arrived while the thread was closed
    });
    expect(threadMarkReadSnapshot(fresh)).toBe(43);
  });

  it("treats windowFetchSucceededAt == openedAt as fresh (boundary)", () => {
    expect(threadMarkReadSnapshot(gate({ openedAt: 5_000, windowFetchSucceededAt: 5_000 }))).toBe(
      42,
    );
  });
});

describe("threadOpenNeedsLatestRefetch — jump-window clobber prevention (§9.10)", () => {
  it("forces a latest refetch on a NORMAL open (snapshot must be this open’s)", () => {
    expect(threadOpenNeedsLatestRefetch({ isJumpOpen: false })).toBe(true);
  });

  it("does NOT refetch on a JUMP open (primeAround owns the around-window)", () => {
    // The blocker: a target-less latest refetch would clobber the
    // around-first-unread window primeAround wrote, breaking "click chip → land
    // on first unread". A jump open must rely on primeAround instead.
    expect(threadOpenNeedsLatestRefetch({ isJumpOpen: true })).toBe(false);
  });
});

describe("clusterInitialFetchEnabled — empty-cache jump race (#task:1317)", () => {
  it("enables the initial fetch on a NORMAL open (no window lock)", () => {
    // A normal open keeps its mount-time latest initial fetch — that fetch IS
    // this open's window and what arms the mark-read gate.
    expect(
      clusterInitialFetchEnabled({
        hasChannelAndRoot: true,
        jumpWindowLocked: false,
      }),
    ).toBe(true);
  });

  it("DISABLES the query while a jump-open holds the window lock (priming OR committed)", () => {
    // The lock spans priming AND committed: primeAround is the sole writer of
    // this open's window, and the committed around-window is not re-enabled (not
    // refetchable) until clearJump drops the lock — so no latest can pre-empt or
    // clobber it.
    expect(
      clusterInitialFetchEnabled({
        hasChannelAndRoot: true,
        jumpWindowLocked: true,
      }),
    ).toBe(false);
  });

  it("re-enables once the lock releases (settled/cleared → around read, or latest fallback)", () => {
    // When the jump settles/clears, the lock drops and the query re-enables. On
    // a committed window still in cache → refetchOnMount:false reads it with no
    // fetch; on an empty cache (prime failed) → a latest initial fetch fires as
    // a graceful fallback (pane never blank).
    expect(
      clusterInitialFetchEnabled({
        hasChannelAndRoot: true,
        jumpWindowLocked: false,
      }),
    ).toBe(true);
  });

  it("stays disabled without a channel/root regardless of lock state", () => {
    expect(
      clusterInitialFetchEnabled({
        hasChannelAndRoot: false,
        jumpWindowLocked: false,
      }),
    ).toBe(false);
  });
});

describe("threadClusterShowsLoader — committed-fact loader, zero wall-clock (#task:1317)", () => {
  it("shows the loader on a real in-flight fetch", () => {
    expect(threadClusterShowsLoader({ isLoading: true, jumpPhase: null })).toBe(true);
  });

  it("shows the loader while a jump is PRIMING (its around-window has not committed)", () => {
    // Pre-commit: query disabled (isLoading false), around-window not yet in
    // cache. Cover with the loader — even on a reopen where React Query serves
    // the PREVIOUS open's stale rows, they must not flash (no `hasWindow` term).
    expect(threadClusterShowsLoader({ isLoading: false, jumpPhase: "priming" })).toBe(true);
  });

  it("RENDERS once the prime is COMMITTED, even though the jump is still active", () => {
    // committed: the around-window is in the (still-locked) cache; keeping the
    // loader up would starve the locate effect of its `#msg-<id>` DOM node. The
    // window stays protected by the lock, not by the loader.
    expect(threadClusterShowsLoader({ isLoading: false, jumpPhase: "committed" })).toBe(false);
  });

  it("RENDERS on a settled fallback (no loader)", () => {
    expect(threadClusterShowsLoader({ isLoading: false, jumpPhase: "settled" })).toBe(false);
  });

  it("does not show the loader with no active jump once loaded", () => {
    expect(threadClusterShowsLoader({ isLoading: false, jumpPhase: null })).toBe(false);
  });
});

describe("cluster freshness epoch carrier (primeAround → gate, §9.10)", () => {
  // Integration at the cache layer (no React render harness in this env): the
  // epoch carrier is the channel by which primeAround's around-fetch arms the
  // mark-read gate. A live reply / optimistic patch writes the WINDOW key, not
  // this epoch key, so it must not move the epoch and must not open the gate.
  it("lets a primeAround-style epoch write open the gate, while a window-only write does not", () => {
    const qc = new QueryClient();
    const channelId = "chan-1";
    const rootId = "root-1";
    const epochKey = clusterFetchEpochKey(channelId, rootId);

    // This open began at t=5000 (captured at render before any fetch resolved).
    const openedAt = 5_000;
    const headSeq = decodeMessageSeq(43); // S+1 head from the around-window response

    // (1) Before primeAround stamps: no epoch → gate closed.
    const before = qc.getQueryData<number>(epochKey) ?? 0;
    expect(before).toBe(0);
    expect(
      threadMarkReadSnapshot({
        alreadyMarkedForOpen: false,
        isFetching: false,
        headSeq,
        windowFetchSucceededAt: before,
        openedAt,
      }),
    ).toBeNull();

    // (2) A live reply / optimistic patch touches the WINDOW, never the epoch
    //     key — so the epoch is still 0 and the gate stays closed.
    const afterSetData = qc.getQueryData<number>(epochKey) ?? 0;
    expect(afterSetData).toBe(0);

    // (3) primeAround's around-fetch succeeds and stamps the epoch (>= openedAt)
    //     → gate opens with the around-window head.
    qc.setQueryData(epochKey, 6_000);
    const afterPrime = qc.getQueryData<number>(epochKey) ?? 0;
    expect(afterPrime).toBe(6_000);
    expect(
      threadMarkReadSnapshot({
        alreadyMarkedForOpen: false,
        isFetching: false,
        headSeq,
        windowFetchSucceededAt: afterPrime,
        openedAt,
      }),
    ).toBe(headSeq);
  });
});

describe("threadLiveMarkReadSeq (#task:1226 visible-live mark-read)", () => {
  // `maxVisibleReplySeq` / `lastMarkedSeq` are taken as raw numbers (or null) and
  // decoded to MessageSeq here (slice-A pattern), so overrides pass plain
  // literals. Default: actual tail visible, document visible, tail 42, marked 10.
  const live = (
    over: Partial<Omit<ThreadLiveMarkReadInput, "maxVisibleReplySeq" | "lastMarkedSeq">> & {
      maxVisibleReplySeq?: number | null;
      lastMarkedSeq?: number | null;
    } = {},
  ): ThreadLiveMarkReadInput => {
    const { maxVisibleReplySeq, lastMarkedSeq, ...rest } = over;
    return {
      tailVisible: true,
      documentVisible: true,
      maxVisibleReplySeq:
        maxVisibleReplySeq === undefined
          ? decodeMessageSeq(42)
          : maxVisibleReplySeq === null
            ? null
            : decodeMessageSeq(maxVisibleReplySeq),
      lastMarkedSeq:
        lastMarkedSeq === undefined
          ? decodeMessageSeq(10)
          : lastMarkedSeq === null
            ? null
            : decodeMessageSeq(lastMarkedSeq),
      ...rest,
    };
  };

  it("tail visible + newer reply → marks to the visible tail seq", () => {
    expect(threadLiveMarkReadSeq(live())).toBe(42);
  });

  it("tail not visible (scrolled up) → null (no swallow of unrendered tail)", () => {
    expect(threadLiveMarkReadSeq(live({ tailVisible: false }))).toBeNull();
  });

  it("document hidden / backgrounded → null even when pinned at tail", () => {
    expect(threadLiveMarkReadSeq(live({ documentVisible: false }))).toBeNull();
  });

  it("empty window (no visible seq) → null", () => {
    expect(threadLiveMarkReadSeq(live({ maxVisibleReplySeq: null }))).toBeNull();
  });

  it("nothing marked yet (lastMarkedSeq null) → marks the visible tail", () => {
    // Explicit-absence gate (#task:1267 Axis-B): a null "nothing marked" must
    // never block the first advance the way a 0 sentinel silently would.
    expect(threadLiveMarkReadSeq(live({ maxVisibleReplySeq: 42, lastMarkedSeq: null }))).toBe(42);
  });

  it("already marked at/after the visible tail → null (forward-only)", () => {
    expect(threadLiveMarkReadSeq(live({ maxVisibleReplySeq: 42, lastMarkedSeq: 42 }))).toBeNull();
    expect(threadLiveMarkReadSeq(live({ maxVisibleReplySeq: 41, lastMarkedSeq: 42 }))).toBeNull();
  });

  it("retriable: a stalled mark leaves lastMarkedSeq low, so the same seq is still returned next trigger", () => {
    // onSuccess is what advances lastMarkedSeq; a failed mutation leaves it
    // unchanged, so re-evaluating with the same window re-proposes the seq.
    expect(threadLiveMarkReadSeq(live({ maxVisibleReplySeq: 42, lastMarkedSeq: 10 }))).toBe(42);
  });

  it("visibility transition: a reply received while hidden marks only after returning to foreground (Cindy review)", () => {
    // A reply arrives while the app is backgrounded and the panel is pinned at
    // the tail. While hidden the gate holds it unread (no swallow of an unseen
    // reply)…
    expect(
      threadLiveMarkReadSeq(
        live({
          tailVisible: true,
          maxVisibleReplySeq: 42,
          lastMarkedSeq: 10,
          documentVisible: false,
        }),
      ),
    ).toBeNull();
    // …and the moment the user returns to the foreground (visibilitychange →
    // documentVisible true, same visible tail) the SAME seq becomes markable.
    // The component wires `documentVisible` as a state/effect dependency so this
    // transition actually re-runs the gate.
    expect(
      threadLiveMarkReadSeq(
        live({
          tailVisible: true,
          maxVisibleReplySeq: 42,
          lastMarkedSeq: 10,
          documentVisible: true,
        }),
      ),
    ).toBe(42);
  });
});

describe("thread reply-stream cursor domain (#task:1992)", () => {
  // The cluster read predicate is `id = ? OR thread_root_id = ?`, so a window
  // that reaches the thread's start CONTAINS the main-line root. These fixtures
  // reproduce the shape the 2026-08-13 packaged smoke hit: root at mainline
  // seq 6, first reply not yet written (it landed later at seq 13).
  const root = message({ id: "root", seq: 6, thread_root_id: null });
  const reply = (id: string, seq: number): MessageDto =>
    message({ id, seq, thread_root_id: "root" });
  const pendingReply = (id: string): CachedMessage => {
    const { seq: _seq, ...rest } = reply(id, 0);
    return { ...rest, pending: true };
  };

  it("threadReplyRows drops the pinned root and keeps reply order", () => {
    const rows = threadReplyRows([root, reply("r1", 13), reply("r2", 20)], "root");
    expect(rows.map((m) => m.id)).toEqual(["r1", "r2"]);
  });

  it("root-only window → null cursor (the reply-less thread that produced the 400)", () => {
    // The thread stream head is 0 here (no `conversation_stream_heads` row is
    // written until a REPLY exists), so proposing the root's mainline seq 6 is a
    // position beyond the head — `position_future` 400. Null means the live gate
    // holds and no observe request is sent at all.
    expect(threadVisibleReplySeq([root], "root")).toBeNull();
  });

  it("root + replies → the newest REPLY seq, never the root’s mainline seq", () => {
    expect(threadVisibleReplySeq([root, reply("r1", 13)], "root")).toBe(13);
    // Identity, not ordinal comparison: a root row anywhere in the window, at
    // any seq, is excluded. (A real root's mainline seq is always below its
    // replies'; this pins the rule itself rather than that incidental ordering.)
    const laterRoot = message({ id: "root", seq: 99, thread_root_id: null });
    expect(threadVisibleReplySeq([reply("r1", 13), laterRoot], "root")).toBe(13);
  });

  it("around-window (root off-window) → that window’s newest reply seq", () => {
    // A jump-opened mid-history window holds replies only; the cursor is still
    // max-VISIBLE, so it stays at the loaded window's tail reply.
    expect(threadVisibleReplySeq([reply("r4", 40), reply("r5", 41)], "root")).toBe(41);
  });

  it("pending optimistic reply carries no ordinal → cursor stays at the last committed reply", () => {
    expect(threadVisibleReplySeq([root, reply("r1", 13), pendingReply("p1")], "root")).toBe(13);
    // Root + a pending reply only: still nothing committed in this stream.
    expect(threadVisibleReplySeq([root, pendingReply("p1")], "root")).toBeNull();
  });

  it("composed with the live gate: a watched root-only thread proposes nothing, then follows the first reply", () => {
    // Tail visible + foreground + nothing marked yet — every other gate term is
    // OPEN, so this isolates the domain rule: the debounce fires against a null
    // proposal, i.e. zero observe requests (not a swallowed 400).
    const watched = (window: readonly CachedMessage[]): ThreadLiveMarkReadInput => ({
      tailVisible: true,
      documentVisible: true,
      maxVisibleReplySeq: threadVisibleReplySeq(window, "root"),
      lastMarkedSeq: null,
    });
    expect(threadLiveMarkReadSeq(watched([root]))).toBeNull();
    // The first reply arrives while the panel is watched: advance by REPLY seq.
    expect(threadLiveMarkReadSeq(watched([root, reply("r1", 13)]))).toBe(13);
  });
});

describe("threadTailIsVisible (#task:1610 live unread)", () => {
  it("uses committed viewport position, not a mutable follow-intent ref", () => {
    expect(
      threadTailIsVisible({
        streamAtTail: true,
        scrollHeight: 1_000,
        scrollTop: 600,
        clientHeight: 400,
      }),
    ).toBe(true);
  });

  it("does not treat the local bottom of a mid-history window as the stream tail", () => {
    expect(
      threadTailIsVisible({
        streamAtTail: false,
        scrollHeight: 1_000,
        scrollTop: 600,
        clientHeight: 400,
      }),
    ).toBe(false);
  });

  it("preserves the existing 20px bottom tolerance without swallowing a scrolled-up tail", () => {
    expect(
      threadTailIsVisible({
        streamAtTail: true,
        scrollHeight: 1_000,
        scrollTop: 581,
        clientHeight: 400,
      }),
    ).toBe(true);
    expect(
      threadTailIsVisible({
        streamAtTail: true,
        scrollHeight: 1_000,
        scrollTop: 579,
        clientHeight: 400,
      }),
    ).toBe(false);
  });
});

describe("clusterWindowLocked", () => {
  it("holds the lock while priming and while a committed window is cached", () => {
    expect(clusterWindowLocked("priming", false)).toBe(true);
    expect(clusterWindowLocked("committed", true)).toBe(true);
  });

  it("releases a committed lock whose window is gone, so the pane fetches instead of sitting empty", () => {
    expect(clusterWindowLocked("committed", false)).toBe(false);
  });

  it("is open when settled or without a jump", () => {
    expect(clusterWindowLocked("settled", true)).toBe(false);
    expect(clusterWindowLocked(null, false)).toBe(false);
  });
});
