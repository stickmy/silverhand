// Channel-scope viewer-local removal CONTRACT test (#task:1689).
//
// Guards:
//   1. WHOLE-CACHE INVARIANT — after a removal the channel appears NOWHERE in the
//      cache (form-agnostic, delimited-segment match), for BOTH loss modes. This
//      is the falsifier: disable the sweep or a registry cleanup and it goes red.
//      Its seeds are UNOBSERVED, which is what a `membership-loss` drops; the
//      activity-axis fork it grades by is pinned separately (guard 6).
//   2. REFETCH EQUALITY per registry entry — a patched cache must be what a
//      refetch would return (membership-filtered lists), a non-reproducible one
//      must be GONE (ranked/capped result sets, single-object reads), and a
//      server-unfiltered read model must be RETAINED.
//   3. UNREAD ARITHMETIC — `total === sum(by_conversation)` after the patch, and
//      the workspace rollup keeps its relationship to that total.
//   4. REGISTRY ↔ SEED lockstep; REGISTRY ↔ CLASSIFICATION lockstep; and the
//      layer-3 source scan over all of `features/**`.
//   5. DETECTION — the pure resolver's transition table, the CONVERGENCE gate
//      (including the split-brain tick that a `status`-only gate would act on),
//      and the non-selected-workspace pipeline that needs no navigation.
//   6. ACTIVITY-AXIS FORK (#task:1697) — a `membership-loss` keeps and background-
//      revalidates an OBSERVED public-readable window, drops every unobserved
//      (gate-closed) member-only key and never re-requests it; a `visibility-loss`
//      discards even an observed one. Asserted as a cache/request contract.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryObserver } from "@tanstack/react-query";
import type {
  MessageDto,
  NotificationDto,
  SavedMessageDto,
  SearchDto,
  TaskDetailDto,
  TaskSummaryDto,
} from "@silverhand/shared";
import { activityKey } from "../attention/hooks";
import { messageKey } from "../messages/hooks";
import { removeChannelFromSaved, savedHeldChannelIds, savedKey } from "../messages/saved";
import { searchKey } from "../search/hooks";
import {
  boardColumnKey,
  boardCountsKey,
  boardFacetsKey,
  boardFilterKey,
} from "../tasks/model/board-query";
import { DEFAULT_TASK_FILTERS } from "../tasks/model/task-filters";
import { taskDetailKey } from "../tasks/model/task-detail";
import {
  CHANNEL_WORKSPACE_ATTRIBUTION_SOURCES,
  channelWorkspaceConflicts,
  channelWorkspaceOf,
  clearChannelWorkspaceIndex,
  recordChannelWorkspace,
} from "./channel-workspace-index";
import {
  conversationUnreadKey,
  messageUnreadByConversationKey,
  messageUnreadSummaryKey,
} from "./message-unread";
import {
  handleViewerChannelRemoved,
  heldChannelIdsInWorkspace,
  isConvergedListTick,
  observedMemberChannelIds,
  reduceChannelRemoval,
  resolveChannelMembershipLosses,
  VIEWER_LOCAL_CHANNEL_KEY_CLASSIFICATION,
  VIEWER_LOCAL_CHANNEL_READ_MODELS,
  type ChannelMembershipRow,
  type ChannelRemovalBaseline,
} from "./viewer-channel-removal";

const WS = "ws-1";
const OTHER_WS = "ws-2";
const LOST = "ch-lost";
const KEPT = "ch-kept";

/** The board's default (All workspaces / all channels) filter key — the shape
 *  both task surfaces share. */
const BOARD_FILTER_KEY = boardFilterKey(DEFAULT_TASK_FILTERS);

// ── Production payloads ────────────────────────────────────────────────
// Typed as the real DTOs, so a seed cannot drift into a convenient shape the
// server never sends: a field the payload gains makes these fixtures RED.

function message(id: string, conversationId: string): MessageDto {
  return {
    id,
    conversation_id: conversationId,
    from_kind: "user",
    from_agent_id: null,
    from_user_id: "u-1",
    author: {
      status: "active",
      kind: "user",
      id: "u-1",
      name: "User",
      display_name: "User",
      avatar_url: null,
      runtime: null,
    },
    content: "body",
    metadata: null,
    attachments: null,
    reactions: null,
    seq: 3 as MessageDto["seq"],
    created_at: 1,
    thread_root_id: null,
    linked_task: null,
  };
}

function savedRow(id: string, channelId: string, workspaceId = WS): SavedMessageDto {
  return {
    id,
    message: message(`m-${id}`, channelId),
    place: {
      kind: "workspace_channel",
      conversation_id: channelId,
      workspace_id: workspaceId,
      channel_name: "channel",
    },
    saved_at: 2,
  };
}

/** A DM save. Both `SavedPlace` arms carry `conversation_id`, so this row is
 *  indistinguishable from a channel save by that field alone — only `kind`
 *  separates them, which is what the two cells below pin. */
function savedPairRow(id: string, conversationId: string): SavedMessageDto {
  return {
    id,
    message: message(`m-${id}`, conversationId),
    place: { kind: "direct_pair", conversation_id: conversationId },
    saved_at: 2,
  };
}

function taskRow(id: string, channelId: string, workspaceId = WS): TaskSummaryDto {
  return {
    id,
    task_number: 7,
    place: {
      kind: "workspace",
      conversation_id: channelId,
      workspace_id: workspaceId,
      channel_name: "channel",
    },
    thread_root_id: null,
    status: "todo",
    title: "a task",
    assignee: null,
    creator: { type: "user", id: "u-1", name: "shiki" },
    created_at: 1,
    closed_at: null,
  };
}

function taskDetail(id: string, channelId: string, workspaceId = WS): TaskDetailDto {
  return {
    ...taskRow(id, channelId, workspaceId),
    content: "body",
    last_editor: null,
    last_edited_at: null,
    source_message_id: null,
    nav: { kind: "unavailable" },
  };
}

function searchDto(channelId: string, workspaceId = WS): SearchDto {
  return {
    query: {
      q: "hello",
      stream: "all",
      order: "recency_desc",
      limit: 80,
    },
    results: [
      {
        type: "message",
        conversation: {
          id: channelId,
          title: "channel",
          placement_kind: "workspace",
          workspace: {
            id: workspaceId,
            name: "Workspace",
            owner_agent_id: null,
          },
          roster_policy: "open",
          channel_id: channelId,
          closed_at: null,
          created_at: 1,
        },
        message: message(`hit-${channelId}`, channelId),
      },
    ],
    total: 42,
    truncated: true,
  };
}

function notificationRow(id: string, channelId: string): NotificationDto {
  return {
    id,
    kind: "mention_self",
    user_id: "u-1",
    agent_id: null,
    workspace_id: WS,
    conversation_id: channelId,
    message_id: `m-${id}`,
    source_workspace_id: null,
    source_conversation_id: null,
    source_message_id: null,
    payload: null,
    read_at: null,
    created_at: 1,
  };
}

// Seed each registered read model with the lost + a surviving channel, keyed by
// registry id. A lockstep test asserts this set equals the registry, so a new
// read model with no seed here is caught. The RETAINED roots (notifications,
// message-reactions) are deliberately NOT seeded here — the whole-cache invariant
// does not apply to them; each has its own test below.
const SEEDS: Record<string, (qc: QueryClient) => void> = {
  activity: (qc) => {
    const item = (id: string, conversationId: string) => ({
      id,
      kind: "unread" as const,
      salience: "ambient" as const,
      workspace_id: WS,
      conversation_id: conversationId,
      thread_root_id: null,
      message_id: `m-${id}`,
      notification_id: null,
      title: `#${id}`,
      thread_root_preview: null,
      preview: null,
      preview_sender: null,
      timestamp: 1,
      unread_count: 1,
    });
    qc.setQueryData(activityKey(), {
      items: [item("gone", LOST), item("keep", KEPT)],
    });
  },
  "message-unread": (qc) => {
    qc.setQueryData(messageUnreadByConversationKey(WS), {
      by_conversation: { [LOST]: 3, [KEPT]: 5 },
      latest_by_conversation: {
        [LOST]: { message_id: "m-lost", seq: 7, thread_root_id: null },
        [KEPT]: { message_id: "m-kept", seq: 9, thread_root_id: null },
      },
      total: 8,
    });
    qc.setQueryData(messageUnreadSummaryKey, { by_workspace: { [WS]: 8 } });
  },
  saved: (qc) => {
    qc.setQueryData(savedKey, [savedRow("s-gone", LOST), savedRow("s-keep", KEPT)]);
    // A second loaded variant (the Saved pane's filtered read) must be patched too.
    qc.setQueryData([...savedKey, { workspace_id: WS }], [savedRow("s-gone-2", LOST)]);
  },
  tasks: (qc) => {
    // The board's three faces are the whole of this root now (#task:1892 retired
    // the fetch-all list): infinite pages of rows, and two server aggregates.
    qc.setQueryData(boardColumnKey("todo", BOARD_FILTER_KEY), {
      pages: [
        { status: "todo", items: [taskRow("t-gone", LOST)], next_cursor: "cur-1" },
        { status: "todo", items: [taskRow("t-keep", KEPT)], next_cursor: null },
      ],
      pageParams: [null, "cur-1"],
    });
    qc.setQueryData(boardCountsKey(BOARD_FILTER_KEY), {
      counts: { todo: 2, in_progress: 0, in_review: 0, done: 0, closed: 0 },
    });
    qc.setQueryData(boardFacetsKey(), {
      assignees: [],
      creators: [],
      has_unassigned: true,
    });
    qc.setQueryData(taskDetailKey("t-gone"), taskDetail("t-gone", LOST));
    qc.setQueryData(taskDetailKey("t-keep"), taskDetail("t-keep", KEPT));
    qc.setQueryData(["task-by-number", WS, 7], taskRow("t-gone", LOST));
    qc.setQueryData(["task-by-number", WS, 8], taskRow("t-keep", KEPT));
  },
  search: (qc) => {
    qc.setQueryData([...searchKey, { q: "hello" }], searchDto(LOST));
    qc.setQueryData([...searchKey, { q: "other" }], searchDto(KEPT));
  },
  messages: (qc) => {
    // `['messages', <messageId>]` — keyed by the MESSAGE, so layer 1's
    // position-1 sweep cannot see it; the row's `conversation_id` is what
    // attributes it, and that is always the parent channel-like conversation.
    qc.setQueryData(messageKey("msg-gone"), {
      message: message("msg-gone", LOST),
      agent_name: null,
    });
    qc.setQueryData(messageKey("msg-keep"), {
      message: message("msg-keep", KEPT),
      agent_name: null,
    });
  },
};

/** Channel-KEYED caches that are NOT in the registry — layer 1 covers them by
 *  FORM (the id sits in key position 1), with no registration. One per root
 *  prefix in use today, plus a survivor to prove the sweep is targeted. */
function seedChannelKeyedSiblings(qc: QueryClient): void {
  const both = (build: (cid: string) => unknown[]) => {
    qc.setQueryData(build(LOST), { seeded: LOST });
    qc.setQueryData(build(KEPT), { seeded: KEPT });
  };
  both((cid) => ["channels", cid, "messages"]);
  both((cid) => ["channels", cid, "cluster", "root-1", "messages"]);
  both((cid) => ["channels", cid, "thread-followers", "root-1"]);
  both((cid) => ["channels", cid, "threads-list"]);
  both((cid) => ["channels", cid, "attachments-list"]);
  both((cid) => ["channels", cid, "agents"]);
  both((cid) => ["conversations", cid, "agent-run-states"]);
  both((cid) => ["conversations", cid, "agent-activity", "agent-1"]);
  both((cid) => ["conversation-members", cid]);
  both((cid) => ["channel-detail", cid]);
  both((cid) => [...conversationUnreadKey(cid)]);
}

/** Structured reference check (no `JSON.stringify().includes()` — that
 *  false-positives on a preview/title CONTAINING the id). A channel is
 *  "referenced" iff its id is a whole cached value / array element / object key,
 *  or a whole DELIMITED SEGMENT of a composite id. */
function stringReferences(value: string, channelId: string): boolean {
  return value === channelId || value.split(/[:/]/).includes(channelId);
}

function valueReferences(value: unknown, channelId: string): boolean {
  if (typeof value === "string") return stringReferences(value, channelId);
  if (Array.isArray(value)) return value.some((v) => valueReferences(v, channelId));
  if (value && typeof value === "object") {
    return Object.entries(value).some(
      ([k, v]) => stringReferences(k, channelId) || valueReferences(v, channelId),
    );
  }
  return false;
}

function cacheMentions(qc: QueryClient, channelId: string): boolean {
  return qc
    .getQueryCache()
    .getAll()
    .some((q) => {
      const inKey =
        Array.isArray(q.queryKey) &&
        q.queryKey.some((seg) => typeof seg === "string" && stringReferences(seg, channelId));
      return inKey || valueReferences(q.state.data ?? null, channelId);
    });
}

function seedAll(qc: QueryClient): void {
  for (const seed of Object.values(SEEDS)) seed(qc);
  seedChannelKeyedSiblings(qc);
}

const REMOVE_LOST = {
  workspaceId: WS,
  channelId: LOST,
  mode: "visibility-loss",
} as const;

describe("viewer-local channel removal contract (#task:1689)", () => {
  // The channel→workspace attribution index is module state (an immutable fact
  // learned once per channel), so each test starts from an empty one.
  beforeEach(clearChannelWorkspaceIndex);

  it("registry & seed set stay in lockstep (every read model is exercised)", () => {
    const registered = VIEWER_LOCAL_CHANNEL_READ_MODELS.map((m) => m.id).sort();
    expect(Object.keys(SEEDS).sort()).toEqual(registered);
  });

  for (const mode of ["visibility-loss", "membership-loss"] as const) {
    it(`after a ${mode} the channel appears NOWHERE in the cache; survivor intact`, () => {
      const qc = new QueryClient();
      seedAll(qc);
      // Precondition, else the invariant could pass vacuously.
      expect(cacheMentions(qc, LOST)).toBe(true);

      handleViewerChannelRemoved(qc, { workspaceId: WS, channelId: LOST, mode });

      expect(cacheMentions(qc, LOST)).toBe(false);
      expect(cacheMentions(qc, KEPT)).toBe(true);
    });
  }

  // Layer 1 grades `membership-loss` by the ACTIVITY axis with the SAME predicate
  // (#task:1697). Everything above seeds UNOBSERVED caches, so it pins the
  // discard half; these pin the fork itself. The contract is a cache/request
  // contract on purpose — `Query.isActive()` and fetch counts, never a rendered
  // shape (@Cindy / @Alice, 2026-07-28).
  describe("membership-loss keeps what is observed, drops what is not (#task:1697)", () => {
    const MAINLINE = ["channels", LOST, "messages"] as const;
    // The member-only families, in the shape their hooks actually take when a
    // gate closes: some unmount, some stay mounted with a null id, i.e. an
    // `enabled: false` observer. Both are `inactive` by `Query.isActive()`.
    const MEMBER_ONLY = [
      ["conversation-members", LOST],
      ["channels", LOST, "threads-list"],
      ["channels", LOST, "attachments-list"],
      ["conversations", LOST, "agent-run-states"],
    ] as const;

    /** Attach a real observer so the query's ACTIVITY is the thing under test.
     *  `staleTime: Infinity` + pre-seeded data means subscribing alone fetches
     *  nothing, so any recorded fetch came from this contract. */
    function observe(
      qc: QueryClient,
      queryKey: readonly unknown[],
      data: unknown,
      enabled = true,
    ): { fetches: () => number; stop: () => void } {
      let fetches = 0;
      qc.setQueryData(queryKey, data);
      const observer = new QueryObserver(qc, {
        queryKey: [...queryKey],
        queryFn: async () => {
          fetches += 1;
          return data;
        },
        staleTime: Infinity,
        enabled,
      });
      const stop = observer.subscribe(() => {});
      return { fetches: () => fetches, stop };
    }

    it("an OBSERVED public-readable window keeps its data and revalidates in the background", async () => {
      const qc = new QueryClient();
      const mainline = observe(qc, MAINLINE, { seeded: LOST });

      handleViewerChannelRemoved(qc, {
        workspaceId: WS,
        channelId: LOST,
        mode: "membership-loss",
      });

      // The pane the viewer is still looking at never goes empty — that emptying
      // is what shiki saw as "整个会话 UI 会刷新一次".
      expect(qc.getQueryData(MAINLINE)).toEqual({ seeded: LOST });
      await vi.waitFor(() => expect(mainline.fetches()).toBe(1));
      expect(qc.getQueryData(MAINLINE)).toEqual({ seeded: LOST });
      mainline.stop();
    });

    it("a member-only query whose gate has closed is DROPPED and never re-requested", async () => {
      const qc = new QueryClient();
      const gated = MEMBER_ONLY.map((key) => ({
        key,
        probe: observe(qc, key, { any: key[1] }, false),
      }));

      handleViewerChannelRemoved(qc, {
        workspaceId: WS,
        channelId: LOST,
        mode: "membership-loss",
      });

      for (const { key, probe } of gated) {
        expect(qc.getQueryState([...key])).toBeUndefined();
        // No request is made for a key nobody observes — an invalidate-everything
        // membership arm would instead leave these queries in place and, if the
        // sweep ever ran BEFORE the membership carrier committed (i.e. from a
        // mutation callback instead of an effect), fire one bound-to-403 read
        // each. The ordering itself is a call-site property; this pins the shape
        // the call site is required to produce.
        await vi.waitFor(() => expect(probe.fetches()).toBe(0));
        probe.stop();
      }
    });

    it("a visibility-loss discards even an OBSERVED window", () => {
      const qc = new QueryClient();
      const mainline = observe(qc, MAINLINE, { seeded: LOST });

      handleViewerChannelRemoved(qc, {
        workspaceId: WS,
        channelId: LOST,
        mode: "visibility-loss",
      });

      // A private channel must not stay resolvable on any read path, and the
      // pane has been navigated away by `onVisibilityLoss`.
      expect(qc.getQueryState(MAINLINE)).toBeUndefined();
      mainline.stop();
    });
  });

  describe("refetch equality per form (patch vs discard vs retain)", () => {
    it("membership-filtered LISTS are patched, in the shape a refetch returns", () => {
      const qc = new QueryClient();
      seedAll(qc);
      handleViewerChannelRemoved(qc, REMOVE_LOST);

      expect(qc.getQueryData(savedKey)).toEqual([savedRow("s-keep", KEPT)]);
      expect(qc.getQueryData([...savedKey, { workspace_id: WS }])).toEqual([]);
      // The `tasks` entry is the same form, but its rows now live inside the
      // board's pages — patched in the test below.
    });

    // Three shapes share the `['tasks', …]` root, so the walk dispatches on
    // the KEY. Duck-typing them would either crash on the page object or,
    // worse, silently skip it and leave the lost channel's rows rendered.
    it("board PAGES are patched; server aggregates are invalidated, not patched", () => {
      const qc = new QueryClient();
      seedAll(qc);
      const countsBefore = qc.getQueryData(boardCountsKey(BOARD_FILTER_KEY));
      handleViewerChannelRemoved(qc, REMOVE_LOST);

      const column = qc.getQueryData(boardColumnKey("todo", BOARD_FILTER_KEY)) as {
        pages: Array<{ items: TaskSummaryDto[]; next_cursor: string | null }>;
        pageParams: unknown[];
      };
      expect(column.pages.map((p) => p.items.map((t) => t.id))).toEqual([[], ["t-keep"]]);
      // Cursors are positions in the surviving order, so paging still continues
      // where it left off — the page list is patched, not re-windowed.
      expect(column.pageParams).toEqual([null, "cur-1"]);
      expect(column.pages.map((p) => p.next_cursor)).toEqual(["cur-1", null]);

      // A `GROUP BY` result cannot be recomputed from what the client holds (a
      // local `−1` would invent a number), and nothing in the value says whether
      // it counted the lost channel — so it is marked for a re-read, keeping the
      // last authoritative value until the authority answers.
      expect(qc.getQueryData(boardCountsKey(BOARD_FILTER_KEY))).toEqual(countsBefore);
      expect(qc.getQueryState(boardCountsKey(BOARD_FILTER_KEY))?.isInvalidated).toBe(true);
      expect(qc.getQueryState(boardFacetsKey())?.isInvalidated).toBe(true);
    });

    it("single-object reads of a now-unreadable task are DISCARDED, not trimmed", () => {
      const qc = new QueryClient();
      seedAll(qc);
      handleViewerChannelRemoved(qc, REMOVE_LOST);

      // A refetch of these would FAIL (403), so an emptied-out body is a shape no
      // server response can produce; the query has to be gone.
      expect(qc.getQueryState(taskDetailKey("t-gone"))).toBeUndefined();
      expect(qc.getQueryState(["task-by-number", WS, 7])).toBeUndefined();
      // The survivor's detail is untouched.
      expect(qc.getQueryData(taskDetailKey("t-keep"))).toEqual(taskDetail("t-keep", KEPT));
      expect(qc.getQueryData(["task-by-number", WS, 8])).toEqual(taskRow("t-keep", KEPT));
    });

    it("a RANKED/CAPPED search set that mentions the channel is discarded; others stay", () => {
      const qc = new QueryClient();
      seedAll(qc);
      handleViewerChannelRemoved(qc, REMOVE_LOST);

      // `total`/`truncated` describe a window computed WITH the removed rows, so
      // filtering rows out would leave a set no refetch can return.
      expect(qc.getQueryState([...searchKey, { q: "hello" }])).toBeUndefined();
      expect(qc.getQueryData([...searchKey, { q: "other" }])).toEqual(searchDto(KEPT));
    });

    it("notifications are RETAINED — the server does not membership-filter them", () => {
      const qc = new QueryClient();
      const listKey = ["notifications", "list", { unread: true, toasts: true }];
      qc.setQueryData(listKey, {
        notifications: [notificationRow("n-1", LOST), notificationRow("n-2", KEPT)],
      });
      handleViewerChannelRemoved(qc, REMOVE_LOST);
      // A local delete would diverge from a refetch (the notification repo filters
      // by RECIPIENT only) and be resurrected on the next fetch. Whether losing a
      // channel should clear its notifications is a backend domain decision.
      expect(qc.getQueryData(listKey)).toEqual({
        notifications: [notificationRow("n-1", LOST), notificationRow("n-2", KEPT)],
      });
      expect(VIEWER_LOCAL_CHANNEL_KEY_CLASSIFICATION.notifications?.verdicts).toEqual(["retained"]);
    });

    it("single-message caches of the lost channel are DISCARDED, by payload attribution", () => {
      const qc = new QueryClient();
      seedAll(qc);
      handleViewerChannelRemoved(qc, REMOVE_LOST);

      // Keyed by MESSAGE id, so only the payload's `conversation_id` can attribute
      // it — and that is always the parent channel-like conversation, never a
      // thread. A refetch of this read would be refused after the loss, and a
      // single-object read has no filtered shape to patch to.
      expect(qc.getQueryState(messageKey("msg-gone"))).toBeUndefined();
      expect(qc.getQueryData(messageKey("msg-keep"))).toEqual({
        message: message("msg-keep", KEPT),
        agent_name: null,
      });
      expect(VIEWER_LOCAL_CHANNEL_KEY_CLASSIFICATION.messages?.verdicts).toEqual(["cleaned"]);
      // The reaction aggregate shares the message-keyed FORM but not the reason:
      // its payload names no conversation, so this client cannot attribute it.
      expect(VIEWER_LOCAL_CHANNEL_KEY_CLASSIFICATION["message-reactions"]?.verdicts).toEqual([
        "retained",
      ]);
    });
  });

  describe("unread arithmetic (total is recomputed, never guessed)", () => {
    it("the patched map is refetch-shaped and its total is the sum of survivors", () => {
      const qc = new QueryClient();
      seedAll(qc);
      handleViewerChannelRemoved(qc, REMOVE_LOST);
      // The workspace map is keyed by the WORKSPACE, so it survives the sweep and
      // must be PATCHED: the key is absent (never a phantom 0 a refetch would omit)
      // and `total` drops by the lost channel's 3.
      expect(qc.getQueryData(messageUnreadByConversationKey(WS))).toEqual({
        by_conversation: { [KEPT]: 5 },
        latest_by_conversation: {
          [KEPT]: { message_id: "m-kept", seq: 9, thread_root_id: null },
        },
        total: 5,
      });
      expect(qc.getQueryData(messageUnreadSummaryKey)).toEqual({
        by_workspace: { [WS]: 5 },
      });
    });

    it("a stale total is REPLACED by the survivor sum, not decremented by it", () => {
      // #task:1689 blocker 4: the two caches are patched independently, so `total`
      // can be stale relative to `by_conversation`. Subtracting the lost count
      // would keep the lie (1 - 4 → 0) and hide four unread messages the viewer
      // still has. The invariant is `total === sum(by_conversation)`.
      const qc = new QueryClient();
      qc.setQueryData(messageUnreadByConversationKey(WS), {
        by_conversation: { [LOST]: 4, [KEPT]: 5 },
        latest_by_conversation: {},
        total: 1,
      });
      qc.setQueryData(messageUnreadSummaryKey, { by_workspace: { [WS]: 1 } });

      handleViewerChannelRemoved(qc, REMOVE_LOST);

      const patched = qc.getQueryData<{
        by_conversation: Record<string, number>;
        total: number;
      }>(messageUnreadByConversationKey(WS));
      expect(patched).toEqual({
        by_conversation: { [KEPT]: 5 },
        latest_by_conversation: {},
        total: 5,
      });
      expect(patched?.total).toBe(
        Object.values(patched?.by_conversation ?? {}).reduce((s, n) => s + n, 0),
      );
      // The map is a single query keyed by the workspace (#task:2078), so once it
      // is loaded the rollup entry is fully determined: it is SET to the survivor
      // sum, not moved by a delta off an independently stale summary.
      expect(qc.getQueryData(messageUnreadSummaryKey)).toEqual({
        by_workspace: { [WS]: 5 },
      });
    });

    it("an emptied map settles the rollup at 0 rather than a stale count", () => {
      const qc = new QueryClient();
      qc.setQueryData(messageUnreadByConversationKey(WS), {
        by_conversation: { [LOST]: 4 },
        latest_by_conversation: {},
        total: 1, // stale: the two caches are patched independently
      });
      qc.setQueryData(messageUnreadSummaryKey, { by_workspace: { [WS]: 1 } });
      handleViewerChannelRemoved(qc, REMOVE_LOST);
      expect(qc.getQueryData(messageUnreadByConversationKey(WS))).toEqual({
        by_conversation: {},
        latest_by_conversation: {},
        total: 0,
      });
      expect(qc.getQueryData(messageUnreadSummaryKey)).toEqual({
        by_workspace: { [WS]: 0 },
      });
    });
  });

  it("is idempotent and cold-safe: a second run / empty cache never throws", () => {
    const qc = new QueryClient();
    expect(() => handleViewerChannelRemoved(qc, REMOVE_LOST)).not.toThrow();
    seedAll(qc);
    handleViewerChannelRemoved(qc, REMOVE_LOST);
    expect(() => handleViewerChannelRemoved(qc, REMOVE_LOST)).not.toThrow();
    expect(cacheMentions(qc, LOST)).toBe(false);
    // A repeat pass must not double-subtract the rollups.
    expect(qc.getQueryData(messageUnreadSummaryKey)).toEqual({
      by_workspace: { [WS]: 5 },
    });
  });

  describe("classification is registry-anchored (layer 3, not a drifting hand-list)", () => {
    const FEATURES_DIR = fileURLToPath(new URL("..", import.meta.url));
    const PAGES_DIR = fileURLToPath(new URL("../../pages", import.meta.url));
    /** The three key shapes the codebase actually writes: a single-line `*Key`
     *  constant, a `*Key` FACTORY (`= (args) => [ 'root', … ]`, possibly spanning
     *  lines), and an inline `queryKey: ['root', …]`. */
    const KEY_ROOT = /(?:queryKey:|Key\s*=(?:[^=;]*?=>)?)\s*\[\s*["']([\w-]+)["']/g;

    function rootsIn(src: string): string[] {
      return [...src.matchAll(KEY_ROOT)].map((m) => m[1]).filter((r): r is string => Boolean(r));
    }

    function scanRoots(dir: string, into = new Set<string>()): Set<string> {
      for (const entry of readdirSync(dir)) {
        const path = join(dir, entry);
        if (statSync(path).isDirectory()) {
          scanRoots(path, into);
          continue;
        }
        if (!/\.tsx?$/.test(entry)) continue;
        if (/\.(test|type-test)\.tsx?$/.test(entry)) continue;
        for (const root of rootsIn(readFileSync(path, "utf8"))) into.add(root);
      }
      return into;
    }

    it("the scanner matches all three key shapes the codebase writes", () => {
      // Fixture, not production source: this is the proof that the net's PATTERN
      // is right. The §12.1#9 residual stands — a key written in a shape not
      // listed here (computed root, key assembled from a variable) escapes it.
      expect(rootsIn(`export const savedKey = ['saved'] as const;`)).toEqual(["saved"]);
      expect(
        rootsIn(
          `export const boardColumnKey = (status: TaskStatus, filterKey: string) =>\n  [\n    'tasks',\n    'board',\n  ];`,
        ),
      ).toEqual(["tasks"]);
      expect(rootsIn(`  return useQuery({\n    queryKey: ['presence', ids],\n  });`)).toEqual([
        "presence",
      ]);
      expect(rootsIn(`const key = buildKey(root);`)).toEqual([]);
    });

    it.each(["'", '"'])("scans multiline roots with %s quotes", (quote) => {
      const literal = `${quote}new-root${quote}`;
      expect(rootsIn(`const sampleKey = [\n ${literal}\n];`)).toEqual(["new-root"]);
      expect(rootsIn(`queryKey: [\n ${literal}, value\n]`)).toEqual(["new-root"]);
      expect(rootsIn(`const sampleKey = (id: string) => [\n ${literal}, id\n];`)).toEqual([
        "new-root",
      ]);
    });

    it("every key root under features/** and pages/** is classified, and every entry is real", () => {
      const scanned = scanRoots(PAGES_DIR, scanRoots(FEATURES_DIR));
      const classified = Object.keys(VIEWER_LOCAL_CHANNEL_KEY_CLASSIFICATION);
      expect(scanned.size).toBeGreaterThanOrEqual(30); // not vacuous
      // BOTH directions: a new root goes red (no "unknown ⇒ exempt" default that
      // would silently empty the net), and a root that no longer exists goes red
      // too, so the table cannot rot into fiction.
      expect([...scanned].sort()).toEqual(classified.sort());
    });

    it("every classification carries at least one verdict and a reason", () => {
      for (const [root, c] of Object.entries(VIEWER_LOCAL_CHANNEL_KEY_CLASSIFICATION)) {
        expect(c.verdicts.length, root).toBeGreaterThan(0);
        expect(c.reason.length, root).toBeGreaterThan(20);
      }
    });

    it("`cleaned` roots and registry roots match in BOTH directions", () => {
      const registryRoots = new Set(VIEWER_LOCAL_CHANNEL_READ_MODELS.flatMap((m) => [...m.roots]));
      const cleanedRoots = new Set(
        Object.entries(VIEWER_LOCAL_CHANNEL_KEY_CLASSIFICATION)
          .filter(([, c]) => c.verdicts.includes("cleaned"))
          .map(([root]) => root),
      );
      expect([...registryRoots].sort()).toEqual([...cleanedRoots].sort());
    });

    it("every registry entry declares a key whose root it also claims", () => {
      for (const rm of VIEWER_LOCAL_CHANNEL_READ_MODELS) {
        const root = Array.isArray(rm.key) ? rm.key[0] : rm.key;
        expect(rm.roots, rm.id).toContain(root);
      }
    });

    it("the declared attribution carriers do not silently shrink", () => {
      // A DON'T-SHRINK guard only. Enumerated from the recording side, it can
      // confirm that a declared carrier still records — it can NEVER see a
      // carrier that forgot to. That direction is covered by the two
      // enumerations below, which start from the request/key side instead.
      function filesRecording(dir: string, into: string[] = []): string[] {
        for (const entry of readdirSync(dir)) {
          const path = join(dir, entry);
          if (statSync(path).isDirectory()) {
            filesRecording(path, into);
            continue;
          }
          if (!/\.tsx?$/.test(entry)) continue;
          if (/\.(test|type-test)\.tsx?$/.test(entry)) continue;
          const src = readFileSync(path, "utf8");
          if (!/recordChannelWorkspace(Rows)?\(/.test(src)) continue;
          // The index itself declares the functions; it is not a carrier.
          if (path.endsWith("channels/channel-workspace-index.ts")) continue;
          into.push(path.slice(FEATURES_DIR.length));
        }
        return into;
      }
      expect(filesRecording(FEATURES_DIR).sort()).toEqual(
        [...CHANNEL_WORKSPACE_ATTRIBUTION_SOURCES].sort(),
      );
    });
  });

  // The conversation-ref carrier is enumerated from the side that PRODUCES the
  // cache entry (the request path and the query key), not from the side that
  // records a binding. A list drawn from the modules that already record can only
  // confirm itself — that is how a second ref producer without a write-through
  // survived the first round of this contract.
  describe("the conversation-ref read point is the only producer (independent enumeration)", () => {
    // Scan the WHOLE renderer tree, not just `features/**`: nothing stops a
    // producer from living in `components/`, `lib/`, or `app.tsx`, and a fence
    // narrower than the surface it protects is not a fence.
    const RENDERER_SRC = fileURLToPath(new URL("../..", import.meta.url));
    const READ_POINT = "features/conversations/conversation-ref-query.ts";
    /** Verbs that may touch this key OUTSIDE the read point: eviction only. Stated
     *  as an allow-list, because "no `fetchQuery`" would miss `setQueryData` —
     *  which sends no request yet fills the same cache with no write-through. */
    function conversationRefCalls(source: string): string[] {
      return [
        ...source.matchAll(
          /\b(\w+)\s*(?:<[^;()]*>)?\s*\(\s*(?:\{[^{};]*?\bqueryKey\s*:\s*)?\[\s*(["'])conversation-ref\2/g,
        ),
      ].map((match) => match[1]!);
    }
    const EVICTION_VERBS = new Set(["invalidateQueries", "removeQueries"]);

    it.each(["'", '"'])(
      "detects writes and eviction across line breaks with %s quotes",
      (quote) => {
        const key = `[\n ${quote}conversation-ref${quote}, id\n]`;
        for (const verb of [
          "setQueryData",
          "fetchQuery",
          "ensureQueryData",
          "prefetchQuery",
          "invalidateQueries",
          "removeQueries",
        ]) {
          const argument = verb === "setQueryData" ? `${key}, value` : `{\n queryKey: ${key}\n}`;
          expect(conversationRefCalls(`qc.${verb}(\n${argument}\n);`)).toEqual([verb]);
          expect(EVICTION_VERBS.has(verb)).toBe(
            ["invalidateQueries", "removeQueries"].includes(verb),
          );
        }
      },
    );

    function sourceFiles(dir: string, into: string[] = []): string[] {
      for (const entry of readdirSync(dir)) {
        const path = join(dir, entry);
        if (statSync(path).isDirectory()) {
          sourceFiles(path, into);
          continue;
        }
        if (!/\.tsx?$/.test(entry)) continue;
        if (/\.(test|type-test)\.tsx?$/.test(entry)) continue;
        into.push(path);
      }
      return into;
    }

    /** Code lines only — a comment that quotes the key or the path is prose. */
    function codeLines(src: string): string[] {
      return src.split("\n").filter((line) => !/^\s*(\/\/|\/\*|\*)/.test(line));
    }

    const files = sourceFiles(RENDERER_SRC).map((path) => ({
      rel: path.slice(RENDERER_SRC.length),
      lines: codeLines(readFileSync(path, "utf8")),
    }));

    it("scans a non-vacuous slice of the renderer", () => {
      expect(files.length).toBeGreaterThan(100);
      expect(files.map((f) => f.rel)).toContain(READ_POINT);
    });

    it("exactly one module builds the conversation-ref request", () => {
      const producers = files
        .filter((f) => f.lines.some((l) => l.includes("apiPaths.conversationRef")))
        .map((f) => f.rel);
      expect(producers).toEqual([READ_POINT]);
    });

    it("that module performs the attribution write-through", () => {
      const src = readFileSync(join(RENDERER_SRC, READ_POINT), "utf8");
      expect(src).toMatch(/recordChannelWorkspace\(/);
    });

    it("no other module fills the conversation-ref cache — eviction verbs only", () => {
      for (const file of files) {
        if (file.rel === READ_POINT) continue;
        for (const verb of conversationRefCalls(file.lines.join("\n"))) {
          expect(EVICTION_VERBS.has(verb), `${file.rel}: ${verb}`).toBe(true);
        }
      }
    });

    it("the key factory does not leave the read point", () => {
      // Closes the bypass the literal scans cannot see: an exported factory lets
      // `setQueryData(conversationRefQueryKey(id), …)` fill this cache while
      // mentioning neither the request path nor the raw key.
      const elsewhere = files
        .filter(
          (f) => f.rel !== READ_POINT && f.lines.some((l) => l.includes("conversationRefQueryKey")),
        )
        .map((f) => f.rel);
      expect(elsewhere).toEqual([]);
      // …and it really is module-private, so the check above cannot be satisfied
      // by an import that simply does not exist yet.
      const src = readFileSync(join(RENDERER_SRC, READ_POINT), "utf8");
      expect(src).not.toMatch(/export\s+(const|function)\s+conversationRefQueryKey/);
    });

    it("the eviction sites are real (the previous check is not vacuous)", () => {
      const evicting = files
        .filter(
          (f) =>
            f.rel !== READ_POINT &&
            conversationRefCalls(f.lines.join("\n")).some((verb) => EVICTION_VERBS.has(verb)),
        )
        .map((f) => f.rel);
      expect(evicting).toContain("features/workspaces/channel-mutations.ts");
    });
  });

  describe("a contradicted binding is unusable, not first-write-wins", () => {
    beforeEach(() => {
      clearChannelWorkspaceIndex();
    });

    it("answers neither workspace once two carriers disagree", () => {
      const spy = vi.spyOn(console, "error").mockImplementation(() => {});
      try {
        recordChannelWorkspace("ch-x", "ws-a");
        expect(channelWorkspaceOf("ch-x")).toBe("ws-a");
        recordChannelWorkspace("ch-x", "ws-b");
        // FALSIFIER for "keep the first value": this index decides which
        // channel-keyed caches a DESTRUCTIVE sweep may claim, so a confident
        // wrong answer is the one outcome that is not allowed.
        expect(channelWorkspaceOf("ch-x")).toBeNull();
        expect(channelWorkspaceOf("ch-x")).not.toBe("ws-a");
        expect(channelWorkspaceOf("ch-x")).not.toBe("ws-b");
        // …and it stays unusable, including if the first carrier repeats itself.
        recordChannelWorkspace("ch-x", "ws-a");
        expect(channelWorkspaceOf("ch-x")).toBeNull();
        expect(spy).toHaveBeenCalledTimes(1);
      } finally {
        spy.mockRestore();
      }
    });

    it("leaves the disagreement observable", () => {
      const spy = vi.spyOn(console, "error").mockImplementation(() => {});
      try {
        recordChannelWorkspace("ch-y", "ws-a");
        recordChannelWorkspace("ch-y", "ws-b");
      } finally {
        spy.mockRestore();
      }
      expect(channelWorkspaceConflicts()).toEqual([
        { channelId: "ch-y", first: "ws-a", second: "ws-b" },
      ]);
    });

    it("agreement is a no-op, and an empty coordinate is skipped not synthesized", () => {
      recordChannelWorkspace("ch-z", "ws-a");
      recordChannelWorkspace("ch-z", "ws-a");
      expect(channelWorkspaceOf("ch-z")).toBe("ws-a");
      expect(channelWorkspaceConflicts()).toEqual([]);
      // A notification may carry a null workspace (`workspace_id` is nullable);
      // the carrier skips it rather than inventing one.
      recordChannelWorkspace("ch-null", "");
      expect(channelWorkspaceOf("ch-null")).toBeNull();
      expect(channelWorkspaceConflicts()).toEqual([]);
    });

    it("a contradicted channel is dropped from the held set, never mis-attributed", () => {
      const spy = vi.spyOn(console, "error").mockImplementation(() => {});
      try {
        recordChannelWorkspace("ch-mixed", WS);
        recordChannelWorkspace("ch-mixed", OTHER_WS);
      } finally {
        spy.mockRestore();
      }
      const qc = new QueryClient();
      qc.setQueryData(["conversation-members", "ch-mixed"], [{ id: "m" }]);
      expect([...heldChannelIdsInWorkspace(qc, WS)]).toEqual([]);
      expect([...heldChannelIdsInWorkspace(qc, OTHER_WS)]).toEqual([]);
    });
  });

  describe("detection from the settled lists (read-path authoritative)", () => {
    const row = (
      id: string,
      is_member: boolean | undefined,
      visibility: "public" | "private" = "private",
    ): ChannelMembershipRow => ({ id, is_member, visibility });

    it("a member row that survives is not a loss", () => {
      expect(
        resolveChannelMembershipLosses({
          observed: [LOST],
          active: [row(LOST, true)],
          archived: [],
        }),
      ).toEqual([]);
    });

    it("archiving is not removal (§12.2#2)", () => {
      expect(
        resolveChannelMembershipLosses({
          observed: [LOST],
          active: [],
          archived: [row(LOST, true)],
        }),
      ).toEqual([]);
    });

    it("an UNRESOLVED carrier never fires — this handler destroys caches", () => {
      expect(
        resolveChannelMembershipLosses({
          observed: [LOST],
          active: [row(LOST, undefined)],
          archived: [],
        }),
      ).toEqual([]);
    });

    it("gone from both lists → visibility-loss", () => {
      expect(
        resolveChannelMembershipLosses({
          observed: [LOST, KEPT],
          active: [row(KEPT, true)],
          archived: [],
        }),
      ).toEqual([{ channelId: LOST, mode: "visibility-loss" }]);
    });

    it("a surviving PUBLIC row flipped to non-member → membership-loss (stays readable)", () => {
      expect(
        resolveChannelMembershipLosses({
          observed: [LOST],
          active: [row(LOST, false, "public")],
          archived: [],
        }),
      ).toEqual([{ channelId: LOST, mode: "membership-loss" }]);
    });

    it("a surviving PRIVATE non-member row → visibility-loss (unreadable, leave it)", () => {
      expect(
        resolveChannelMembershipLosses({
          observed: [LOST],
          active: [row(LOST, false, "private")],
          archived: [],
        }),
      ).toEqual([{ channelId: LOST, mode: "visibility-loss" }]);
    });

    it("a channel never observed as a member is never a loss", () => {
      expect(
        resolveChannelMembershipLosses({
          observed: [],
          active: [row(LOST, false, "public")],
          archived: [],
        }),
      ).toEqual([]);
    });

    it("the observed baseline includes archived memberships", () => {
      expect(observedMemberChannelIds([row(KEPT, true)], [row(LOST, true)])).toEqual(
        new Set([KEPT, LOST]),
      );
    });

    it("the observed baseline excludes non-member and unresolved rows", () => {
      expect(
        observedMemberChannelIds(
          [row("a", false, "public"), row("b", undefined), row("c", true)],
          [],
        ),
      ).toEqual(new Set(["c"]));
    });
  });

  describe("convergence, not `status` (#task:1689 blocker 1)", () => {
    const member = (id: string): ChannelMembershipRow => ({
      id,
      is_member: true,
      visibility: "private",
    });
    /** The same row as the ARCHIVED listing returns it. Both shapes are pinned:
     *  since #task:1691 it carries `is_member`, and a row with the field absent
     *  (an older/unresolved carrier) must be inert rather than a loss. */
    const archivedRow = (id: string, withMemberFlag: boolean): ChannelMembershipRow =>
      withMemberFlag
        ? { id, is_member: true, visibility: "private" }
        : { id, visibility: "private" };

    it("only both-lists-settled-and-not-awaiting-refetch counts as converged", () => {
      const base = {
        activeSettled: true,
        archivedSettled: true,
        activeAwaitingRefetch: false,
        archivedAwaitingRefetch: false,
      };
      expect(isConvergedListTick(base)).toBe(true);
      expect(isConvergedListTick({ ...base, archivedSettled: false })).toBe(false);
      expect(isConvergedListTick({ ...base, activeSettled: false })).toBe(false);
      // The split-brain half `status` cannot express: `success` with a refetch
      // still owed.
      expect(isConvergedListTick({ ...base, archivedAwaitingRefetch: true })).toBe(false);
      expect(isConvergedListTick({ ...base, activeAwaitingRefetch: true })).toBe(false);
    });

    for (const withMemberFlag of [true, false]) {
      it(`archiving a channel mid-refetch fires nothing (archived row ${
        withMemberFlag ? "carries" : "omits"
      } is_member)`, () => {
        // Step 1 — baseline from a converged tick. A holder object, so the
        // sequence's carried state is read the way the hook's ref is.
        const state: { baseline: ChannelRemovalBaseline | null } = { baseline: null };
        const step = (tick: {
          converged: boolean;
          active: ChannelMembershipRow[];
          archived: ChannelMembershipRow[];
        }) => {
          const decision = reduceChannelRemoval(state.baseline, {
            workspaceId: WS,
            ...tick,
          });
          state.baseline = decision.baseline;
          return decision.losses;
        };

        expect(
          step({ converged: true, active: [member(LOST), member(KEPT)], archived: [] }),
        ).toEqual([]);
        expect(state.baseline?.ids).toEqual(new Set([LOST, KEPT]));

        // Step 2 — both lists invalidated: previous rows still in hand, and the
        // tick must be inert (no loss AND no re-baseline).
        expect(
          step({ converged: false, active: [member(LOST), member(KEPT)], archived: [] }),
        ).toEqual([]);
        expect(state.baseline?.ids).toEqual(new Set([LOST, KEPT]));

        // Step 3 — the ACTIVE list settles first, without the archived channel;
        // the archived list has not refetched yet, so LOST is in NEITHER list.
        // This is the split-brain tick.
        expect(step({ converged: false, active: [member(KEPT)], archived: [] })).toEqual([]);

        // Step 4 — the archived list lands carrying the row: still a member.
        expect(
          step({
            converged: true,
            active: [member(KEPT)],
            archived: [archivedRow(LOST, withMemberFlag)],
          }),
        ).toEqual([]);
        // With the flag the row re-enters the baseline; without it, membership is
        // unresolved, so it drops out of the baseline rather than being asserted.
        expect(state.baseline?.ids).toEqual(
          withMemberFlag ? new Set([KEPT, LOST]) : new Set([KEPT]),
        );
      });
    }

    it("FALSIFIER: a `status`-only gate would report a loss on the split-brain tick", () => {
      // The same step 3 with `converged: true` — which is what `status === "success"`
      // on both lists yields during the archived list's background refetch.
      const baseline: ChannelRemovalBaseline = {
        workspaceId: WS,
        ids: new Set([LOST, KEPT]),
      };
      expect(
        reduceChannelRemoval(baseline, {
          workspaceId: WS,
          converged: true,
          active: [member(KEPT)],
          archived: [],
        }).losses,
      ).toEqual([{ channelId: LOST, mode: "visibility-loss" }]);
    });

    it("a workspace switch only re-baselines; it can never report a loss", () => {
      const decision = reduceChannelRemoval(
        { workspaceId: OTHER_WS, ids: new Set([LOST]) },
        { workspaceId: WS, converged: true, active: [member(KEPT)], archived: [] },
      );
      expect(decision.losses).toEqual([]);
      expect(decision.baseline).toEqual({ workspaceId: WS, ids: new Set([KEPT]) });
    });
  });

  // A DM save shares the Saved list with channel saves, and both `SavedPlace`
  // arms carry `conversation_id` — so a mis-written `kind` check compiles
  // silently and the type system cannot catch it. What is downstream is
  // DESTRUCTIVE, not cosmetic: a pair id reported as a held channel reaches the
  // foreign-workspace reconciler, which re-reads that workspace's authoritative
  // channel tables, cannot find a pair among them, and therefore grades it a
  // `ChannelMembershipLoss` — destroying that pair's caches (and possibly moving
  // the pane) over a channel event that never touched it. Hence a fence rather
  // than trust in the narrowing.
  describe("a DM save is never a channel holding (#task:2055)", () => {
    const PAIR = "conv-dm";

    it("a direct_pair row is not reported as a held channel of any workspace", () => {
      const qc = new QueryClient();
      qc.setQueryData(savedKey, [
        savedRow("s-channel", KEPT, OTHER_WS),
        savedPairRow("s-dm", PAIR),
      ]);

      // The saved enumerator skips the arm outright…
      expect(savedHeldChannelIds(qc, OTHER_WS)).toEqual([KEPT]);
      // …and the registry aggregate the reconciler actually consumes never sees
      // the pair id — under no workspace, since a pair belongs to none.
      expect(heldChannelIdsInWorkspace(qc, OTHER_WS)).toEqual(new Set([KEPT]));
      expect(heldChannelIdsInWorkspace(qc, WS)).toEqual(new Set());
    });

    it("a channel removal leaves direct_pair rows untouched", () => {
      const qc = new QueryClient();
      qc.setQueryData(savedKey, [savedRow("s-gone", LOST), savedPairRow("s-dm", PAIR)]);

      // Direct call first, then the whole entry path, so a regression is
      // attributed to the filter rather than to the sweep around it.
      removeChannelFromSaved(qc, LOST);
      expect(qc.getQueryData(savedKey)).toEqual([savedPairRow("s-dm", PAIR)]);

      qc.setQueryData(savedKey, [savedRow("s-gone", LOST), savedPairRow("s-dm", PAIR)]);
      handleViewerChannelRemoved(qc, REMOVE_LOST);
      expect(qc.getQueryData(savedKey)).toEqual([savedPairRow("s-dm", PAIR)]);
    });
  });

  describe("a workspace the viewer is NOT looking at (#task:1689 blocker 2)", () => {
    // The foreign path inverts the direction: with no mounted lists there is no
    // observed member set, so the CACHES say which channels they still describe
    // and those ids are the `observed` input of the same resolver, evaluated
    // against an authoritative re-read. These tests cover that composition; the
    // WS-frame wiring around it lives in the hook.
    function seedForeignWorkspace(qc: QueryClient): void {
      qc.setQueryData(savedKey, [
        savedRow("s-b-gone", LOST, OTHER_WS),
        savedRow("s-b-keep", KEPT, OTHER_WS),
        savedRow("s-a", "ch-in-a", WS),
      ]);
      qc.setQueryData(boardColumnKey("todo", BOARD_FILTER_KEY), {
        pages: [
          {
            status: "todo",
            items: [taskRow("t-b-gone", LOST, OTHER_WS), taskRow("t-a", "ch-in-a", WS)],
            next_cursor: null,
          },
        ],
        pageParams: [null],
      });
      qc.setQueryData(messageUnreadByConversationKey(OTHER_WS), {
        by_conversation: { [LOST]: 2 },
        latest_by_conversation: {
          [LOST]: { message_id: "m", seq: 1, thread_root_id: null },
        },
        total: 2,
      });
      qc.setQueryData(messageUnreadSummaryKey, { by_workspace: { [OTHER_WS]: 2 } });
      qc.setQueryData([...searchKey, { q: "x" }], searchDto(LOST, OTHER_WS));
    }

    it("held ids are workspace-scoped: a channel of another workspace is not reported", () => {
      const qc = new QueryClient();
      seedForeignWorkspace(qc);
      expect(heldChannelIdsInWorkspace(qc, OTHER_WS)).toEqual(new Set([LOST, KEPT]));
      expect(heldChannelIdsInWorkspace(qc, WS)).toEqual(new Set(["ch-in-a"]));
    });

    // The board keeps its rows in PAGES and is the ONLY task surface that fetches
    // them (#task:1892 retired the flat list) — an enumeration that reads arrays
    // alone would report an empty held set and the non-selected workspace would
    // never converge.
    it("held ids are enumerated from the board pages", () => {
      const qc = new QueryClient();
      qc.setQueryData(boardColumnKey("todo", BOARD_FILTER_KEY), {
        pages: [
          {
            status: "todo",
            items: [taskRow("t-b", LOST, OTHER_WS), taskRow("t-a", "ch-in-a", WS)],
            next_cursor: null,
          },
        ],
        pageParams: [null],
      });
      expect(heldChannelIdsInWorkspace(qc, OTHER_WS)).toEqual(new Set([LOST]));
      expect(heldChannelIdsInWorkspace(qc, WS)).toEqual(new Set(["ch-in-a"]));
    });

    it("a removal in the non-selected workspace converges WITHOUT navigating there", () => {
      const qc = new QueryClient();
      seedForeignWorkspace(qc);
      // Authoritative re-read of B's two tables: LOST is gone, KEPT remains.
      const active: ChannelMembershipRow[] = [{ id: KEPT, is_member: true, visibility: "private" }];
      const losses = resolveChannelMembershipLosses({
        observed: heldChannelIdsInWorkspace(qc, OTHER_WS),
        active,
        archived: [],
      });
      expect(losses).toEqual([{ channelId: LOST, mode: "visibility-loss" }]);
      for (const loss of losses) {
        handleViewerChannelRemoved(qc, {
          workspaceId: OTHER_WS,
          channelId: loss.channelId,
          mode: loss.mode,
        });
      }
      // B's projections are settled…
      expect(qc.getQueryData(savedKey)).toEqual([
        savedRow("s-b-keep", KEPT, OTHER_WS),
        savedRow("s-a", "ch-in-a", WS),
      ]);
      expect(
        (
          qc.getQueryData(boardColumnKey("todo", BOARD_FILTER_KEY)) as {
            pages: Array<{ items: TaskSummaryDto[] }>;
          }
        ).pages.map((p) => p.items.map((t) => t.id)),
      ).toEqual([["t-a"]]);
      expect(qc.getQueryData(messageUnreadByConversationKey(OTHER_WS))).toEqual({
        by_conversation: {},
        latest_by_conversation: {},
        total: 0,
      });
      expect(qc.getQueryData(messageUnreadSummaryKey)).toEqual({
        by_workspace: { [OTHER_WS]: 0 },
      });
      expect(qc.getQueryState([...searchKey, { q: "x" }])).toBeUndefined();
      // …and the workspace the viewer IS looking at is untouched.
      expect(cacheMentions(qc, "ch-in-a")).toBe(true);
    });

    it("holding nothing for a channel means nothing to converge", () => {
      const qc = new QueryClient();
      expect(heldChannelIdsInWorkspace(qc, OTHER_WS).size).toBe(0);
    });

    describe("channel-KEYED state alone is enough to hold a foreign workspace", () => {
      // Blocker 2, round 2: after B → A the viewer keeps B's `['channels', cid, …]`
      // family in cache. If B has no Saved / task / Activity / unread / search row,
      // a registry-only held set is EMPTY, the probe is skipped, and those caches
      // survive a removal the viewer will never be told about again — walking back
      // into B only re-baselines. So the held set must include the channel-keyed
      // form, attributed by the immutable channel→workspace identity, not by a
      // stale list.
      function seedChannelKeyedOnly(qc: QueryClient): void {
        // Exactly what a visit to B's channel leaves behind, and what the fetches
        // record on the way in.
        recordChannelWorkspace(LOST, OTHER_WS);
        recordChannelWorkspace(KEPT, OTHER_WS);
        recordChannelWorkspace("ch-in-a", WS);
        for (const cid of [LOST, KEPT, "ch-in-a"]) {
          qc.setQueryData(["channels", cid, "messages"], { seeded: cid });
          qc.setQueryData(["conversation-members", cid], { seeded: cid });
          qc.setQueryData(["channel-detail", cid], { seeded: cid });
          qc.setQueryData(["conversations", cid, "agent-run-states"], { seeded: cid });
        }
      }

      it("FALSIFIER: with NO registry row for B, the held set is still B‑scoped", () => {
        const qc = new QueryClient();
        seedChannelKeyedOnly(qc);
        // No Saved/tasks/unread/search/activity row exists at all — a
        // registry-only held set would be empty here and skip the probe.
        expect(heldChannelIdsInWorkspace(qc, OTHER_WS)).toEqual(new Set([LOST, KEPT]));
        expect(heldChannelIdsInWorkspace(qc, WS)).toEqual(new Set(["ch-in-a"]));
      });

      it("an unattributed channel id is NOT claimed by a workspace", () => {
        // Attribution is identity, not a guess: a channel-keyed cache written
        // before any workspace-carrying DTO was seen belongs to no workspace here
        // (the §12.1#9 residual), and a key whose position 1 is not a channel
        // (`['workspaces', wsId, …]`, `['messages', msgId]`) is never claimed.
        const qc = new QueryClient();
        qc.setQueryData(["channels", "ch-unknown", "messages"], { seeded: 1 });
        qc.setQueryData(["workspaces", OTHER_WS, "members"], { seeded: 1 });
        qc.setQueryData(messageKey("msg-1"), {
          message: message("msg-1", "ch-unknown"),
          agent_name: null,
        });
        expect(heldChannelIdsInWorkspace(qc, OTHER_WS)).toEqual(new Set());
      });

      it("the whole family of B's lost channel is swept, with the viewer still on A", () => {
        const qc = new QueryClient();
        seedChannelKeyedOnly(qc);
        const losses = resolveChannelMembershipLosses({
          observed: heldChannelIdsInWorkspace(qc, OTHER_WS),
          active: [{ id: KEPT, is_member: true, visibility: "private" }],
          archived: [],
        });
        expect(losses).toEqual([{ channelId: LOST, mode: "visibility-loss" }]);
        handleViewerChannelRemoved(qc, {
          workspaceId: OTHER_WS,
          channelId: LOST,
          mode: "visibility-loss",
        });
        expect(cacheMentions(qc, LOST)).toBe(false);
        expect(cacheMentions(qc, KEPT)).toBe(true);
        expect(cacheMentions(qc, "ch-in-a")).toBe(true);
      });
    });
  });
});
