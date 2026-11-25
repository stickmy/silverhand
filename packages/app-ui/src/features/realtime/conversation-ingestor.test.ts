import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { QueryClient, QueryObserver, notifyManager } from "@tanstack/react-query";
import {
  ATTACHMENT_CHANGED_EVENT,
  MESSAGE_OPTIONS_CHANGED_EVENT,
  MESSAGE_REACTION_CHANGED_EVENT,
  conversationSideScope,
  conversationScope,
} from "@silverhand/shared";
import type {
  AgentRunState,
  ConversationAgentRuntimeDto,
  MessageDto,
  ServerFrame,
  TaskSummaryDto,
} from "@silverhand/shared";
import { decodeMessageSeq, decodeTransportSeq } from "@silverhand/shared";
import {
  MESSAGE_STREAM_KIND_CONTRACTS,
  REPLAY_SAFETY_CONTRACTS,
  SIDE_KIND_CONTRACTS,
  applyCommittedMessage,
  ingestConversationFrame,
} from "./conversation-ingestor";
import { clusterMessagesKey, messagesKey, threadRootKey } from "@/features/channels/hooks";
import {
  applyCommittedReactionFromEvent,
  messageReactorsKey,
  recoverCommittedReactionsAfterWrite,
} from "@/features/channels/reaction-hooks";
import { clearCurrentUser, setCurrentUserId } from "@/features/session/current-user";
import {
  __resetAttachmentBlobCacheForTest,
  readAttachmentToken,
} from "@/features/attachments/attachment-blob-cache";
import { taskBoardCacheScope, taskByNumberKey } from "@/features/tasks/hooks/use-task-by-number";
import { taskDetailKey } from "@/features/tasks/model/task-detail";
import {
  channelThreadsKey,
  channelThreadsUnreadKey,
} from "@/features/channels/use-channel-threads";
import {
  channelAttachmentsKey,
  channelAttachmentSendersKey,
} from "@/features/channels/use-channel-attachments";
import { agentRunStatesKey } from "@/features/agents/model/agent-run-state-cache";
import { eventWsClient } from "@/lib/ws-client";
import {
  clearPendingAgentStatusPatches,
  mergePendingAgentStatusPatches,
} from "@/features/channels/hooks";

describe("replay-safety contract matrix (#task:1262, reclassified #task:1263 (c))", () => {
  it("classifies the message-stream wire kinds and their cache effects explicitly", () => {
    expect(Object.keys(MESSAGE_STREAM_KIND_CONTRACTS).sort()).toEqual(
      [
        MESSAGE_REACTION_CHANGED_EVENT,
        MESSAGE_OPTIONS_CHANGED_EVENT,
        "message",
        "task_changed",
      ].sort(),
    );
    expect(MESSAGE_STREAM_KIND_CONTRACTS.message).toEqual([
      "mainline_row_window",
      "mainline_reply_count",
      "cluster_message_window",
      "thread_root_reply_count",
      "thread_followers_invalidate",
      "channel_threads_invalidate",
    ]);
    expect(MESSAGE_STREAM_KIND_CONTRACTS.task_changed).toEqual(["task_message_patch"]);
    expect(MESSAGE_STREAM_KIND_CONTRACTS[MESSAGE_REACTION_CHANGED_EVENT]).toEqual([
      "reaction_message_patch",
      "reactors_list_refresh",
    ]);
    expect(MESSAGE_STREAM_KIND_CONTRACTS[MESSAGE_OPTIONS_CHANGED_EVENT]).toEqual([
      "options_message_patch",
    ]);
  });

  it("(A): the reactors popover refresh is a cursor-inert, non-floor-bearing detail invalidate", () => {
    // #1263 hybrid boundary, pinned: a reaction frame keeps the SYNCHRONOUS
    // content-apply floor gate on `reaction_message_patch`. Its `reactors_list_refresh`
    // sub-effect is a lazy hover-detail invalidate — cursor-inert, authoritative-on-
    // open, ZERO delta accumulation — so it is deliberately NOT commit-gated via the
    // message allSettled path. This assertion stops a future "unify satisfaction
    // path" refactor from silently making a tooltip refetch a floor dependency.
    expect(REPLAY_SAFETY_CONTRACTS.reaction_message_patch).toMatchObject({
      cursor: "content-apply",
      recovery: "idempotent",
    });
    expect(REPLAY_SAFETY_CONTRACTS.reactors_list_refresh).toMatchObject({
      cursor: "cursor-inert",
      recovery: "authoritative-invalidate",
    });
    expect(REPLAY_SAFETY_CONTRACTS.reactors_list_refresh.summary).toMatch(
      /not resume-floor-bearing/i,
    );
  });

  it("classifies the side-channel wire kinds as cursor-inert self-healing effects", () => {
    expect(Object.keys(SIDE_KIND_CONTRACTS).sort()).toEqual(
      ["agent_activity", "agent_status", "attachment_changed", "conversation_activity"].sort(),
    );
    for (const effectId of Object.values(SIDE_KIND_CONTRACTS).flat()) {
      const spec = REPLAY_SAFETY_CONTRACTS[effectId];
      expect(spec.cursor).toBe("cursor-inert");
      expect(spec.recovery).toBe("side-scope-self-healing");
    }
  });

  it("(c): reply-count effects are now cursor-inert authoritative-invalidate (no optimistic +1)", () => {
    // The whole #1263 (c) reframe: a reply count is a message-DERIVED projection,
    // recovered authoritatively — not a non-idempotent content delta.
    expect(REPLAY_SAFETY_CONTRACTS.mainline_reply_count).toMatchObject({
      cursor: "cursor-inert",
      recovery: "authoritative-invalidate",
    });
    expect(REPLAY_SAFETY_CONTRACTS.thread_root_reply_count).toMatchObject({
      cursor: "cursor-inert",
      recovery: "authoritative-invalidate",
    });
  });

  it("keeps message CONTENT appends as idempotent content-apply", () => {
    expect(REPLAY_SAFETY_CONTRACTS.mainline_row_window).toMatchObject({
      cursor: "content-apply",
      recovery: "idempotent",
    });
    expect(REPLAY_SAFETY_CONTRACTS.cluster_message_window).toMatchObject({
      cursor: "content-apply",
      recovery: "idempotent",
    });
  });

  it("marks derived invalidations as cursor-inert, not content-apply", () => {
    expect(REPLAY_SAFETY_CONTRACTS.thread_followers_invalidate.cursor).toBe("cursor-inert");
    expect(REPLAY_SAFETY_CONTRACTS.channel_threads_invalidate.cursor).toBe("cursor-inert");
  });
});

// #task:1255 P1a blocker-1 — the App-level ingestor must NOT write the mainline
// window unless a mainline reader (`useChannelMessages`) is mounted. A scope
// kept subscribed by thread/follower INTEREST alone (e.g. an Activity
// thread-source pane that mounts only `ThreadPanel`, no `ChannelView`) would
// otherwise bootstrap an incomplete, live-frame-stitched `messagesKey` window
// with no REST snapshot behind it — the pre-S gap P2 (cold-start two-water-mark)
// closes. P1 = prep, so the old "writes only while useChannelMessages mounted"
// gating is preserved via `hasActiveMainlineReader` (observer count on the exact
// mainline key). See docs/renderer-event-ingestion-v1.md §3.1/§5.

function mountMainlineReader(qc: QueryClient, channelId: string): () => void {
  // A standing observer on the exact mainline key — what `useChannelMessages`
  // registers. `enabled: false` keeps it from fetching; observer count still
  // ticks to 1, which is all the gate reads.
  const observer = new QueryObserver(qc, {
    queryKey: messagesKey(channelId),
    queryFn: () => new Promise<never>(() => {}),
    enabled: false,
  });
  return observer.subscribe(() => {});
}

function mountReader(qc: QueryClient, queryKey: readonly unknown[]): () => void {
  const observer = new QueryObserver(qc, {
    queryKey,
    queryFn: () => new Promise<never>(() => {}),
    enabled: false,
  });
  return observer.subscribe(() => {});
}

function messageFrame(channelId: string, msg: MessageDto): ServerFrame {
  return {
    type: "event",
    scope: `conversation:${channelId}`,
    // #task:1267: EventFrame.seq is a TransportSeq (per-WS-scope transport
    // ordinal), NOT the message's MessageSeq. The fixture reuses the numeric
    // value for convenience but decodes it through the transport boundary so
    // the frame is a well-typed TransportSeq, never a MessageSeq→TransportSeq
    // launder.
    seq: decodeTransportSeq(msg.seq),
    epoch: "epoch-1",
    kind: "message",
    payload: { message: msg },
  } as ServerFrame;
}

function hasMainlineCache(qc: QueryClient, channelId: string): boolean {
  return qc.getQueryData(messagesKey(channelId)) !== undefined;
}

describe("ingestConversationFrame — mainline-reader gate (#task:1255 P1a)", () => {
  it("does NOT bootstrap the mainline window with no reader (interest-only)", () => {
    const qc = new QueryClient();
    // Scope is subscribed (thread/follower interest) but no useChannelMessages
    // observes `messagesKey(c1)`.
    ingestConversationFrame(qc, messageFrame("c1", message({ id: "m1", seq: 1 })));

    expect(hasMainlineCache(qc, "c1")).toBe(false);
  });

  it("applies to the mainline window when a reader is mounted", () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");

    ingestConversationFrame(qc, messageFrame("c1", message({ id: "m1", seq: 1 })));

    expect(hasMainlineCache(qc, "c1")).toBe(true);
    unsub();
  });

  it("gates per channel — a reader on c1 does not open c2", () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");

    // Frame for c2 while only c1 has a reader → c2 stays closed.
    ingestConversationFrame(qc, messageFrame("c2", message({ id: "n1", seq: 1 })));

    expect(hasMainlineCache(qc, "c2")).toBe(false);
    unsub();
  });

  it("ignores non-conversation scopes and non-routable frames", () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");

    ingestConversationFrame(qc, {
      type: "event",
      scope: "viewer:u1",
      seq: 1,
      epoch: "e",
      kind: "message",
      payload: { message: message({ id: "x", seq: 1 }) },
    } as ServerFrame);
    ingestConversationFrame(qc, { type: "ping" } as ServerFrame);

    expect(qc.getQueryData(messagesKey("c1"))).toBeUndefined();
    unsub();
  });
});

// #task:1255 P1b — per-target gating across the thread axis. The blanket
// mainline early-return is gone; cluster / root / follower writes enumerate the
// OPEN reader roots. The load-bearing dual of P1a blocker #1 (Alice): an
// Activity ThreadPanel-only pane has a CLUSTER reader but NO mainline reader —
// the cluster frame must apply while the mainline window stays untouched.
describe("ingestConversationFrame — thread-axis per-target gate (#1255 P1b)", () => {
  it("ThreadPanel-only: cluster applies, mainline messagesKey untouched", () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, clusterMessagesKey("c1", "root1"));
    // No mainline reader on c1.

    ingestConversationFrame(
      qc,
      messageFrame("c1", message({ id: "r1", seq: 2, thread_root_id: "root1" })),
    );

    // Cluster window got the reply...
    expect(qc.getQueryData(clusterMessagesKey("c1", "root1"))).not.toBeUndefined();
    // ...and the mainline window was never bootstrapped.
    expect(qc.getQueryData(messagesKey("c1"))).toBeUndefined();
    unsub();
  });

  it("per-root: a reply to root2 leaves an open root1 cluster untouched", () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, clusterMessagesKey("c1", "root1"));

    // Reply belongs to root2; root1's cluster reader is the only one open.
    ingestConversationFrame(
      qc,
      messageFrame("c1", message({ id: "r2", seq: 3, thread_root_id: "root2" })),
    );

    // root1 cluster: the frame was filtered out (wrong root) → still empty.
    expect(qc.getQueryData(clusterMessagesKey("c1", "root1"))).toBeUndefined();
    // root2 had no reader → not bootstrapped either.
    expect(qc.getQueryData(clusterMessagesKey("c1", "root2"))).toBeUndefined();
    unsub();
  });

  it("no cluster reader → a reply bootstraps no cluster window", () => {
    const qc = new QueryClient();
    // Mainline reader present, but no cluster reader for root1.
    const unsub = mountMainlineReader(qc, "c1");

    ingestConversationFrame(
      qc,
      messageFrame("c1", message({ id: "r1", seq: 2, thread_root_id: "root1" })),
    );

    expect(qc.getQueryData(clusterMessagesKey("c1", "root1"))).toBeUndefined();
    unsub();
  });
});

// #task:1255 P1c — the last pure-invalidate / pure-cache-write consumers move in:
// useChannelThreads (threads-list invalidate on any message + snapshot_required)
// and useAgentRunStates (agent_activity cache write + snapshot_required
// invalidate). Same per-reader gate as the message/cluster axes.
describe("ingestConversationFrame — channel-threads gate (#1255 P1c)", () => {
  it("invalidates the threads list on a message when a reader is mounted", () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, channelThreadsKey("c1"));
    const spy = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, messageFrame("c1", message({ id: "m1", seq: 1 })));

    expect(spy).toHaveBeenCalledWith({ queryKey: channelThreadsKey("c1") });
    unsub();
  });

  it("does NOT invalidate the threads list with no reader", () => {
    const qc = new QueryClient();
    const spy = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, messageFrame("c1", message({ id: "m1", seq: 1 })));

    expect(spy).not.toHaveBeenCalledWith({ queryKey: channelThreadsKey("c1") });
  });
});

// The channel Attachments list is a message-derived read model
// (one row per unique attachment referenced by a persisted message). Same
// reader-gate as threads-list, plus a message-branch payload gate:
// `msg.attachments?.length > 0` — an unrelated reply must NOT trigger a full
// list refetch (no full-message storm into a projection refetch).
// Snapshot_required carries no payload → invalidates unconditionally whenever
// the reader is up.
describe("ingestConversationFrame — channel-attachments gate", () => {
  const withAttachment: Partial<MessageDto> = {
    attachments: [
      {
        id: "att_1",
        filename: "foo.png",
        size: 42,
        content_type: "image/png",
      },
    ],
  };

  it("invalidates the attachments list on a mainline attachment message when a reader is mounted", () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, channelAttachmentsKey("c1"));
    const spy = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(
      qc,
      messageFrame("c1", message({ id: "m1", seq: 1, ...withAttachment })),
    );

    expect(spy).toHaveBeenCalledWith({
      queryKey: channelAttachmentsKey("c1"),
    });
    // A new attachment message can introduce a first-time sender, so the
    // (lazy) sender facets are invalidated in lockstep with the list.
    expect(spy).toHaveBeenCalledWith({
      queryKey: channelAttachmentSendersKey("c1"),
    });
    unsub();
  });

  it("invalidates on a THREAD reply that carries an attachment (parent-channel scope)", () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, channelAttachmentsKey("c1"));
    const spy = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(
      qc,
      messageFrame(
        "c1",
        message({
          id: "m2",
          seq: 2,
          thread_root_id: "root1",
          ...withAttachment,
        }),
      ),
    );

    expect(spy).toHaveBeenCalledWith({
      queryKey: channelAttachmentsKey("c1"),
    });
    unsub();
  });

  it("does NOT invalidate on a message WITHOUT attachments (falsifier — no full-list storm)", () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, channelAttachmentsKey("c1"));
    const spy = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(
      qc,
      messageFrame("c1", message({ id: "m3", seq: 3, attachments: null })),
    );

    expect(spy).not.toHaveBeenCalledWith({
      queryKey: channelAttachmentsKey("c1"),
    });
    expect(spy).not.toHaveBeenCalledWith({
      queryKey: channelAttachmentSendersKey("c1"),
    });
    unsub();
  });

  it("does NOT invalidate on an attachment message when no reader is mounted (falsifier)", () => {
    const qc = new QueryClient();
    const spy = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(
      qc,
      messageFrame("c1", message({ id: "m4", seq: 4, ...withAttachment })),
    );

    expect(spy).not.toHaveBeenCalledWith({
      queryKey: channelAttachmentsKey("c1"),
    });
  });

  it("snapshot_required on the MESSAGE scope invalidates unconditionally when a reader is mounted", () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, channelAttachmentsKey("c1"));
    const spy = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, {
      type: "snapshot_required",
      scope: "conversation:c1",
      epoch: "e",
      currentSeq: 5,
    } as ServerFrame);

    expect(spy).toHaveBeenCalledWith({
      queryKey: channelAttachmentsKey("c1"),
    });
    unsub();
  });

  it("snapshot_required does NOT invalidate when no reader is mounted (falsifier)", () => {
    const qc = new QueryClient();
    const spy = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, {
      type: "snapshot_required",
      scope: "conversation:c1",
      epoch: "e",
      currentSeq: 5,
    } as ServerFrame);

    expect(spy).not.toHaveBeenCalledWith({
      queryKey: channelAttachmentsKey("c1"),
    });
  });
});

describe("ingestConversationFrame — agent-run-states gate (#1255 P1c)", () => {
  it("applies agent_activity to the cache when a reader is mounted", () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, agentRunStatesKey("c1"));

    ingestConversationFrame(qc, agentActivityFrame("c1", runState("c1", "a1")));

    const states = qc.getQueryData<AgentRunState[]>(agentRunStatesKey("c1"));
    expect(states?.map((s) => s.agentId)).toEqual(["a1"]);
    unsub();
  });

  it("does NOT bootstrap the run-states cache with no reader", () => {
    const qc = new QueryClient();

    ingestConversationFrame(qc, agentActivityFrame("c1", runState("c1", "a1")));

    expect(qc.getQueryData(agentRunStatesKey("c1"))).toBeUndefined();
  });
});

const agentRuntimeKey = (cid: string) => ["channels", cid, "agent-runtime"] as const;

// The attachment blob cache is module-global; the ingestor only drives its
// generation counters (precise per-id revision on `attachment_changed`, coarse
// epoch on a side-scope `snapshot_required`). These assert the SEAM — that the
// dispatch routes to the right mutator — not any rendering behaviour.
describe("ingestConversationFrame — attachment blob generation (#task:1769)", () => {
  beforeEach(() => __resetAttachmentBlobCacheForTest());
  afterEach(() => __resetAttachmentBlobCacheForTest());

  it("bumps the named attachment revision on an attachment_changed event", () => {
    const qc = new QueryClient();

    ingestConversationFrame(qc, {
      type: "event",
      scope: conversationSideScope("c1"),
      seq: 1,
      epoch: "epoch-1",
      kind: ATTACHMENT_CHANGED_EVENT,
      payload: { attachment_id: "att1", content_revision: 3 },
    } as ServerFrame);

    expect(readAttachmentToken("att1")).toBe("0:3");
    // A sibling id is untouched — the bump is precise, not global.
    expect(readAttachmentToken("att2")).toBe("0:0");
  });

  it("coarse-bumps the global epoch on a side-scope snapshot_required (gap recovery)", () => {
    const qc = new QueryClient();

    ingestConversationFrame(qc, {
      type: "snapshot_required",
      scope: conversationSideScope("c1"),
      epoch: "e",
      currentSeq: 5,
    } as ServerFrame);

    // Every id's token moves — the dropped event's id is unknowable.
    expect(readAttachmentToken("att1")).toBe("1:0");
    expect(readAttachmentToken("att2")).toBe("1:0");
  });

  function attachmentChangedFrame(channelId: string): ServerFrame {
    return {
      type: "event",
      scope: conversationSideScope(channelId),
      seq: 1,
      epoch: "epoch-1",
      kind: ATTACHMENT_CHANGED_EVENT,
      payload: { attachment_id: "att1", content_revision: 2 },
    } as ServerFrame;
  }

  it("invalidates ONLY the same-channel attachments list when its reader is mounted (size refresh)", () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, channelAttachmentsKey("c1"));
    const spy = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, attachmentChangedFrame("c1"));

    expect(spy).toHaveBeenCalledWith({ queryKey: channelAttachmentsKey("c1") });
    // A content update changes bytes/size, not membership → senders left alone.
    expect(spy).not.toHaveBeenCalledWith({
      queryKey: channelAttachmentSendersKey("c1"),
    });
    unsub();
  });

  it("does NOT bootstrap/refetch the attachments list with no reader mounted (falsifier)", () => {
    const qc = new QueryClient();
    const spy = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, attachmentChangedFrame("c1"));

    expect(spy).not.toHaveBeenCalledWith({
      queryKey: channelAttachmentsKey("c1"),
    });
  });
});

describe("ingestConversationFrame — agents (agent_status) gate (#1255 P1c)", () => {
  afterEach(() => {
    clearPendingAgentStatusPatches("c1");
    clearPendingAgentStatusPatches("c2");
  });

  it("patches + buffers agent_status when a reader is mounted", () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, agentRuntimeKey("c1"));

    ingestConversationFrame(qc, agentStatusFrame("c1", "a1", "active"));

    // Buffered for the in-flight snapshot replay.
    const merged = mergePendingAgentStatusPatches("c1", [
      {
        agent_id: "a1",
        scope_kind: "conversation",
        generation: 1,
        cli_session_id: null,
        pid: null,
        status: "idle",
      },
    ]);
    expect(merged[0]?.status).toBe("active");
    unsub();
  });

  it("does NOT buffer agent_status with no reader", () => {
    const qc = new QueryClient();

    ingestConversationFrame(qc, agentStatusFrame("c1", "a1", "active"));

    const merged = mergePendingAgentStatusPatches("c1", [
      {
        agent_id: "a1",
        scope_kind: "conversation",
        generation: 1,
        cli_session_id: null,
        pid: null,
        status: "idle",
      },
    ]);
    expect(merged[0]?.status).toBe("idle"); // untouched
  });

  it("converges mounted A/B AgentSlot projections while keeping ConversationSlot local", () => {
    const qc = new QueryClient();
    const runtime = (
      agentId: string,
      scopeKind: "agent" | "conversation",
    ): ConversationAgentRuntimeDto => ({
      agent_id: agentId,
      scope_kind: scopeKind,
      generation: 1,
      cli_session_id: null,
      pid: 42,
      status: "active",
    });
    qc.setQueryData(agentRuntimeKey("c1"), [
      runtime("aggregate", "agent"),
      runtime("local", "conversation"),
    ]);
    qc.setQueryData(agentRuntimeKey("c2"), [
      runtime("aggregate", "agent"),
      runtime("local", "conversation"),
    ]);
    const unsubA = mountReader(qc, agentRuntimeKey("c1"));
    const unsubB = mountReader(qc, agentRuntimeKey("c2"));

    // One aggregate runtime mutation is emitted by the backend once per
    // authorized projection audience. Both mounted snapshots must converge.
    ingestConversationFrame(qc, agentStatusFrame("c1", "aggregate", "crashed"));
    ingestConversationFrame(qc, agentStatusFrame("c2", "aggregate", "crashed"));
    // A ConversationSlot mutation remains scoped to its owning conversation.
    ingestConversationFrame(qc, agentStatusFrame("c1", "local", "idle"));

    const a = qc.getQueryData<ConversationAgentRuntimeDto[]>(agentRuntimeKey("c1"))!;
    const b = qc.getQueryData<ConversationAgentRuntimeDto[]>(agentRuntimeKey("c2"))!;
    expect(a.find((row) => row.agent_id === "aggregate")?.status).toBe("crashed");
    expect(b.find((row) => row.agent_id === "aggregate")?.status).toBe("crashed");
    expect(a.find((row) => row.agent_id === "local")?.status).toBe("idle");
    expect(b.find((row) => row.agent_id === "local")?.status).toBe("active");

    unsubA();
    unsubB();
  });
});

function agentStatusFrame(
  channelId: string,
  agentId: string,
  status: "idle" | "starting" | "active" | "crashed",
): ServerFrame {
  return {
    type: "event",
    scope: conversationSideScope(channelId),
    seq: 1,
    epoch: "epoch-1",
    kind: "agent_status",
    payload: { agent_id: agentId, status },
  } as ServerFrame;
}

const agentActivityKey = (cid: string, aid: string) =>
  ["conversations", cid, "agent-activity", aid] as const;

describe("ingestConversationFrame — agent activity feed gate (#1255 P1c #2)", () => {
  it("appends conversation_activity to the agent feed when its reader is mounted", () => {
    const qc = new QueryClient();
    // Reader loaded the feed (has data) AND observes the key.
    qc.setQueryData(agentActivityKey("c1", "a1"), { events: [], nextBeforeId: null });
    const unsub = mountReader(qc, agentActivityKey("c1", "a1"));

    ingestConversationFrame(qc, conversationActivityFrame("c1", "a1", 7));

    const data = qc.getQueryData<{ events: { id: number }[] }>(agentActivityKey("c1", "a1"));
    expect(data?.events.map((e) => e.id)).toEqual([7]);
    unsub();
  });

  it("does NOT bootstrap the feed with no reader", () => {
    const qc = new QueryClient();

    ingestConversationFrame(qc, conversationActivityFrame("c1", "a1", 7));

    expect(qc.getQueryData(agentActivityKey("c1", "a1"))).toBeUndefined();
  });

  it("routes by agent_id — a row for a2 leaves a1 feed untouched", () => {
    const qc = new QueryClient();
    qc.setQueryData(agentActivityKey("c1", "a1"), { events: [], nextBeforeId: null });
    const unsub = mountReader(qc, agentActivityKey("c1", "a1"));

    ingestConversationFrame(qc, conversationActivityFrame("c1", "a2", 7));

    const data = qc.getQueryData<{ events: { id: number }[] }>(agentActivityKey("c1", "a1"));
    expect(data?.events).toEqual([]); // a1 untouched
    expect(qc.getQueryData(agentActivityKey("c1", "a2"))).toBeUndefined();
    unsub();
  });
});

function conversationActivityFrame(channelId: string, agentId: string, id: number): ServerFrame {
  return {
    type: "event",
    scope: conversationSideScope(channelId),
    seq: id,
    epoch: "epoch-1",
    kind: "conversation_activity",
    payload: {
      id,
      conversation_id: channelId,
      agent_id: agentId,
      kind: "text",
      payload: { text: `m${id}` },
      created_at: id,
    },
  } as ServerFrame;
}

function runState(conversationId: string, agentId: string): AgentRunState {
  return {
    conversationId,
    agentId,
    runId: `${agentId}-run1`,
    status: "thinking",
    startedAt: 1,
    updatedAt: 1,
    lastActivityAt: 1,
  };
}

function agentActivityFrame(channelId: string, state: AgentRunState): ServerFrame {
  return {
    type: "event",
    scope: conversationSideScope(channelId),
    seq: state.updatedAt,
    epoch: "epoch-1",
    kind: "agent_activity",
    payload: state,
  } as ServerFrame;
}

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

// #task:1255 P2 step-3 — cross-scope routing is DISJOINT: a kind must arrive on
// its own scope. A side kind on the message scope (or a message kind on the side
// scope) is ignored, never mis-applied — the shared parsers keep the two
// recovery classes from leaking. And `snapshot_required` reconciles only the
// caches of the scope it arrived on.
describe("ingestConversationFrame — cross-scope routing is disjoint (#1255 P2 step-3)", () => {
  afterEach(() => clearPendingAgentStatusPatches("c1"));

  it("an agent_status on the MESSAGE scope is ignored (not buffered)", () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, agentRuntimeKey("c1"));
    // agent_status is a SIDE kind — deliver it on the wrong (message) scope.
    ingestConversationFrame(qc, {
      type: "event",
      scope: "conversation:c1",
      seq: 1,
      epoch: "epoch-1",
      kind: "agent_status",
      payload: { agent_id: "a1", status: "active" },
    } as ServerFrame);

    const merged = mergePendingAgentStatusPatches("c1", [
      {
        agent_id: "a1",
        scope_kind: "conversation",
        generation: 1,
        cli_session_id: null,
        pid: null,
        status: "idle",
      },
    ]);
    expect(merged[0]?.status).toBe("idle"); // untouched — wrong scope
    unsub();
  });

  it("a message on the SIDE scope is ignored (mainline untouched)", () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");

    ingestConversationFrame(qc, {
      type: "event",
      scope: conversationSideScope("c1"),
      seq: 1,
      epoch: "epoch-1",
      kind: "message",
      payload: { message: message({ id: "m1", seq: 1 }) },
    } as ServerFrame);

    expect(qc.getQueryData(messagesKey("c1"))).toBeUndefined();
    unsub();
  });

  it("snapshot_required on the MESSAGE scope reconciles mainline, not run-states", () => {
    const qc = new QueryClient();
    const unsubMain = mountMainlineReader(qc, "c1");
    const unsubRun = mountReader(qc, agentRunStatesKey("c1"));
    const spy = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, {
      type: "snapshot_required",
      scope: "conversation:c1",
      epoch: "e",
      currentSeq: 5,
    } as ServerFrame);

    expect(spy).toHaveBeenCalledWith({ queryKey: messagesKey("c1") });
    expect(spy).not.toHaveBeenCalledWith({ queryKey: agentRunStatesKey("c1") });
    unsubMain();
    unsubRun();
  });

  it("snapshot_required on the SIDE scope reconciles run-states, not mainline", () => {
    const qc = new QueryClient();
    const unsubMain = mountMainlineReader(qc, "c1");
    const unsubRun = mountReader(qc, agentRunStatesKey("c1"));
    const spy = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, {
      type: "snapshot_required",
      scope: conversationSideScope("c1"),
      epoch: "e",
      currentSeq: 5,
    } as ServerFrame);

    expect(spy).toHaveBeenCalledWith({ queryKey: agentRunStatesKey("c1") });
    expect(spy).not.toHaveBeenCalledWith({ queryKey: messagesKey("c1") });
    unsubMain();
    unsubRun();
  });
});

// #task:1255 P2 step-4 — DUAL-WATER FLOOR. The ingestor is the SOLE
// content-driven writer of the transport applied-water for the content-gated
// message-stream scope: a frame that produced ≥1 real content write → exactly one
// `markSatisfied(scope, seq, epoch)`; a no-op / readerless / derived-invalidate-only
// frame → none (false-positive forbidden, §4.1). We spy the transport seam so the
// unit test asserts the CALL without running transport state.
function messageFrameSeq(channelId: string, msg: MessageDto, seq: number): ServerFrame {
  return {
    type: "event",
    scope: conversationScope(channelId),
    seq,
    epoch: "epoch-1",
    kind: "message",
    payload: { message: msg },
  } as ServerFrame;
}

/** A mainline cache at tail carrying an optional root key so a reply bump lands
 *  in-window. `has_more_after:false` ⇒ `lastPageIsTail` true (append path runs). */
function seedMainline(
  qc: QueryClient,
  channelId: string,
  opts: { rootId?: string; replyCount?: number } = {},
): void {
  const reply_counts_by_root = opts.rootId ? { [opts.rootId]: opts.replyCount ?? 0 } : {};
  qc.setQueryData(messagesKey(channelId), {
    pages: [
      {
        messages: [],
        reply_counts_by_root,
        has_more_before: false,
        has_more_after: false,
        stream_head_seq: 5,
      },
    ],
    pageParams: [undefined],
  });
}

function mainlineReplyCount(
  qc: QueryClient,
  channelId: string,
  rootId: string,
): number | undefined {
  const data = qc.getQueryData<{
    pages: { reply_counts_by_root: Record<string, number> }[];
  }>(messagesKey(channelId));
  return data?.pages[0]?.reply_counts_by_root[rootId];
}

describe("ingestConversationFrame — per-event satisfaction disposition (#task:1263 (c) / #1261)", () => {
  let markSatisfied: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    markSatisfied = vi.spyOn(eventWsClient, "markSatisfied").mockImplementation(() => {});
    // Isolate: an event-path frame never commits, but guard against a real
    // commit if a future change routes one through here.
    vi.spyOn(eventWsClient, "commitSnapshotBaseline").mockImplementation(() => {});
  });
  afterEach(() => vi.restoreAllMocks());

  // markSatisfied is now ASYNC for `message` frames (gated on Promise.allSettled),
  // so positives await and negatives flush a few microtasks first.
  const flush = async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  };

  it("① a content-write frame (mainline append) markSatisfies once (scope, seq, epoch) after settle", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    ingestConversationFrame(qc, messageFrameSeq("c1", message({ id: "m1", seq: 6 }), 6));
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    expect(markSatisfied).toHaveBeenCalledWith(conversationScope("c1"), 6, "epoch-1");
    unsub();
  });

  it("② a derived-invalidate reply markSatisfies only AFTER the refetch COMMITS, not on dispatch", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    seedMainline(qc, "c1", { rootId: "root1", replyCount: 0 });
    // A controllable invalidate so we can prove commit-gating (initiated ≠ committed).
    let commitRefetch!: () => void;
    vi.spyOn(qc, "invalidateQueries").mockReturnValue(
      new Promise<void>((resolve) => {
        commitRefetch = () => resolve();
      }),
    );
    ingestConversationFrame(
      qc,
      messageFrameSeq("c1", message({ id: "r1", seq: 6, thread_root_id: "root1" }), 6),
    );
    await flush();
    // Dispatch-gated would fire now; commit-gated must wait for the refetch.
    expect(markSatisfied).not.toHaveBeenCalled();
    expect(mainlineReplyCount(qc, "c1", "root1")).toBe(0); // no optimistic +1
    commitRefetch();
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    expect(markSatisfied).toHaveBeenCalledWith(conversationScope("c1"), 6, "epoch-1");
    unsub();
  });

  it("③ a rejected refetch pins the floor — markSatisfied NOT called", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    seedMainline(qc, "c1", { rootId: "root1", replyCount: 0 });
    vi.spyOn(qc, "invalidateQueries").mockRejectedValue(new Error("refetch failed"));
    ingestConversationFrame(
      qc,
      messageFrameSeq("c1", message({ id: "r1", seq: 6, thread_root_id: "root1" }), 6),
    );
    await flush();
    expect(markSatisfied).not.toHaveBeenCalled();
    unsub();
  });

  it("④ a warm-inactive mainline (no reader) is REMOVED, then the frame satisfies", async () => {
    const qc = new QueryClient();
    seedMainline(qc, "c1"); // warm cache, NO observer
    expect(qc.getQueryData(messagesKey("c1"))).not.toBeUndefined();
    ingestConversationFrame(qc, messageFrameSeq("c1", message({ id: "m1", seq: 6 }), 6));
    // Warm-removed → next open cold-starts (never left to serve stale, B1)...
    expect(qc.getQueryData(messagesKey("c1"))).toBeUndefined();
    // ...and the removeInactive settle (resolved) lets the floor advance.
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    expect(markSatisfied).toHaveBeenCalledWith(conversationScope("c1"), 6, "epoch-1");
  });

  it("⑤ a cold-absent frame (no reader, no cache) satisfies VACUOUSLY", async () => {
    const qc = new QueryClient();
    ingestConversationFrame(qc, messageFrameSeq("c1", message({ id: "m1", seq: 6 }), 6));
    // No cache bootstrapped (the reader gate governs cache-WRITES)...
    expect(qc.getQueryData(messagesKey("c1"))).toBeUndefined();
    // ...but the floor still advances: cold-start is authoritative on next open,
    // and advancing past a cold-absent frame can't strand a warm cache (the #1251
    // hole needs a warm DISPLAYABLE cache, which cold-absent lacks).
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    expect(markSatisfied).toHaveBeenCalledWith(conversationScope("c1"), 6, "epoch-1");
  });

  it("⑥ a multi-target reply markSatisfies EXACTLY ONCE after ALL effects settle", async () => {
    const qc = new QueryClient();
    const unsubMain = mountMainlineReader(qc, "c1");
    seedMainline(qc, "c1", { rootId: "root1", replyCount: 0 });
    const unsubCluster = mountReader(qc, clusterMessagesKey("c1", "root1"));
    const unsubThreads = mountReader(qc, channelThreadsKey("c1"));
    // 3 effects: mainline reply-count invalidate + cluster append + threads-list
    // invalidate → one markSatisfied once they all settle fulfilled.
    ingestConversationFrame(
      qc,
      messageFrameSeq("c1", message({ id: "r1", seq: 6, thread_root_id: "root1" }), 6),
    );
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    expect(markSatisfied).toHaveBeenCalledWith(conversationScope("c1"), 6, "epoch-1");
    unsubMain();
    unsubCluster();
    unsubThreads();
  });

  it("⑦ replaying the same reply frame never double-counts (Cindy #1263 falsifier)", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    seedMainline(qc, "c1", { rootId: "root1", replyCount: 3 });
    const frame = messageFrameSeq("c1", message({ id: "r1", seq: 6, thread_root_id: "root1" }), 6);
    // Deliver the SAME reply twice (live then replay). (c) recovers the count
    // authoritatively (invalidate), never a blind +1 → the count is never bumped
    // locally, so replay cannot double-count (the original bug's regression guard).
    ingestConversationFrame(qc, frame);
    ingestConversationFrame(qc, frame);
    await flush();
    expect(mainlineReplyCount(qc, "c1", "root1")).toBe(3); // never bumped locally
    unsub();
  });

  it("an off-window reply on an ACTIVE mainline satisfies vacuously (authoritative on paginate)", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    seedMainline(qc, "c1"); // no root key → the badge isn't on any loaded page
    ingestConversationFrame(
      qc,
      messageFrameSeq("c1", message({ id: "r1", seq: 6, thread_root_id: "rootX" }), 6),
    );
    // No effect (active mainline is authoritative; the count refetches when the
    // window paginates to rootX) → vacuous satisfy, floor advances.
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    unsub();
  });

  it("task_changed satisfies the floor via the SYNCHRONOUS satisfaction path (hybrid: not the message allSettled path)", () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    // Seed the row the chip patches so the patch lands (applied=true).
    qc.setQueryData(messagesKey("c1"), {
      pages: [
        {
          messages: [message({ id: "t1", seq: 5 })],
          reply_counts_by_root: {},
          has_more_before: false,
          has_more_after: false,
          stream_head_seq: 5,
        },
      ],
      pageParams: [undefined],
    });
    ingestConversationFrame(qc, taskChangedFrame("c1", "t1", 6));
    // Synchronous (no await) — task/reaction are pure idempotent patches, so they
    // satisfy the floor synchronously rather than via the message allSettled path.
    expect(markSatisfied).toHaveBeenCalledTimes(1);
    expect(markSatisfied).toHaveBeenCalledWith(conversationScope("c1"), 6, "epoch-1");
    unsub();
  });

  it("side-scope frames never markSatisfied (side floor auto-advances)", async () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, agentRunStatesKey("c1"));
    ingestConversationFrame(qc, agentActivityFrame("c1", runState("c1", "a1")));
    await flush();
    expect(markSatisfied).not.toHaveBeenCalled();
    unsub();
  });

  it("reaction keeps the SYNCHRONOUS floor gate; its reactors refresh is authoritative-on-open, never accumulated (#1263 (A) falsifier)", () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    // Seed the row the reaction targets so the absolute-count patch lands (applied).
    qc.setQueryData(messagesKey("c1"), {
      pages: [
        {
          messages: [message({ id: "m1", seq: 5 })],
          reply_counts_by_root: {},
          has_more_before: false,
          has_more_after: false,
          stream_head_seq: 5,
        },
      ],
      pageParams: [undefined],
    });
    ingestConversationFrame(qc, reactionFrame("c1", "m1", 6));
    // Synchronous (no await): reaction stays on the boolean `reaction_message_patch`
    // gate despite its cursor-inert `reactors_list_refresh` sub-effect — that refresh
    // is NOT commit-gated into the message allSettled path.
    expect(markSatisfied).toHaveBeenCalledTimes(1);
    expect(markSatisfied).toHaveBeenCalledWith(conversationScope("c1"), 6, "epoch-1");
    // (A) falsifier: the ingestor accumulates NO reactor-list state — the who-reacted
    // popover cache is left untouched (undefined). So advancing the floor past this
    // frame (resume won't replay it) strands nothing: the popover is authoritative-on-
    // open (a fresh REST fetch on hover), correct without the replay.
    expect(qc.getQueryData(messageReactorsKey("m1"))).toBeUndefined();
    unsub();
  });

  it("patches cluster reactions for a cluster-only reader (Activity thread side panel, NO mainline reader) (#task:1269 item 1)", () => {
    const qc = new QueryClient();
    // The Activity thread side panel opens a reply cluster (ThreadPanel) with NO
    // mainline reader mounted. Before the gate widened, ingestReaction only checked
    // hasActiveMainlineReader → live reactions on a reply were silently dropped.
    const unsub = mountReader(qc, clusterMessagesKey("c1", "root1"));
    qc.setQueryData(clusterMessagesKey("c1", "root1"), {
      pages: [
        {
          messages: [message({ id: "r1", seq: 5, thread_root_id: "root1" })],
          reply_counts_by_root: {},
          has_more_before: false,
          has_more_after: false,
          stream_head_seq: 5,
        },
      ],
      pageParams: [undefined],
    });
    ingestConversationFrame(qc, reactionFrame("c1", "r1", 6, "root1"));
    const cache = qc.getQueryData(clusterMessagesKey("c1", "root1")) as {
      pages: { messages: MessageDto[] }[];
    };
    const patched = cache.pages[0]!.messages[0]!;
    expect(patched.reactions?.[0]).toMatchObject({ emoji: "👍", count: 1 });
    // Floor still advances synchronously (reaction stays on the boolean gate).
    expect(markSatisfied).toHaveBeenCalledWith(conversationScope("c1"), 6, "epoch-1");
    unsub();
  });
});

// #task:1277 — S1 P1 reply_count projection MERGE. A reply frame carrying an
// absolute `thread_root_reply_count` patch sets the authoritative count on the
// displayed surfaces (thread-root row + mainline badge) IN PLACE — no
// invalidate→whole-window refetch (the amplification kill). Cindy's four gates:
// patch present → NO refetch; active-but-no-patch → fallback/pin; projection
// absent → vacuous; floor only after settle. docs/projection-recovery-s1-v1.md.
describe("ingestConversationFrame — S1 reply_count projection merge (#task:1277)", () => {
  let markSatisfied: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    markSatisfied = vi.spyOn(eventWsClient, "markSatisfied").mockImplementation(() => {});
    vi.spyOn(eventWsClient, "commitSnapshotBaseline").mockImplementation(() => {});
  });
  afterEach(() => vi.restoreAllMocks());

  const flush = async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  };

  /** A reply frame WITH the absolute reply_count projection sidecar. */
  function patchedReplyFrame(
    channelId: string,
    rootId: string,
    replyCount: number,
    seq: number,
  ): ServerFrame {
    return {
      type: "event",
      scope: conversationScope(channelId),
      seq,
      epoch: "epoch-1",
      kind: "message",
      payload: {
        message: message({ id: `r${seq}`, seq, thread_root_id: rootId }),
        projections: {
          thread_root_reply_count: { root_id: rootId, reply_count: replyCount },
        },
      },
    } as ServerFrame;
  }

  function seedThreadRoot(
    qc: QueryClient,
    channelId: string,
    rootId: string,
    replyCount: number,
  ): void {
    qc.setQueryData(threadRootKey(channelId, rootId), {
      messages: [message({ id: rootId, seq: 1 })],
      reply_counts_by_root: { [rootId]: replyCount },
      has_more_before: false,
      has_more_after: false,
      stream_head_seq: 1,
    });
  }
  function threadRootReplyCount(
    qc: QueryClient,
    channelId: string,
    rootId: string,
  ): number | undefined {
    return qc.getQueryData<{ reply_counts_by_root: Record<string, number> }>(
      threadRootKey(channelId, rootId),
    )?.reply_counts_by_root[rootId];
  }

  it("merges the absolute count into an open thread-root row — NO refetch, markSatisfied once", async () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, threadRootKey("c1", "root1"));
    seedThreadRoot(qc, "c1", "root1", 0);
    const invalidate = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, patchedReplyFrame("c1", "root1", 1, 6));

    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    // Merged in place to the authoritative absolute value...
    expect(threadRootReplyCount(qc, "c1", "root1")).toBe(1);
    // ...WITHOUT any invalidate→refetch (the #1277 amplification kill / dogfood判据).
    expect(invalidate).not.toHaveBeenCalled();
    expect(markSatisfied).toHaveBeenCalledWith(conversationScope("c1"), 6, "epoch-1");
    unsub();
  });

  it("merges the absolute count into the mainline reply badge — NO whole-window refetch", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    seedMainline(qc, "c1", { rootId: "root1", replyCount: 0 });
    const invalidate = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, patchedReplyFrame("c1", "root1", 2, 6));

    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    expect(mainlineReplyCount(qc, "c1", "root1")).toBe(2);
    expect(invalidate).not.toHaveBeenCalled(); // old path invalidated the whole window
    unsub();
  });

  it("replaying the same patched frame is idempotent — the absolute value never double-counts", async () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, threadRootKey("c1", "root1"));
    seedThreadRoot(qc, "c1", "root1", 0);
    const frame = patchedReplyFrame("c1", "root1", 1, 6);

    ingestConversationFrame(qc, frame);
    ingestConversationFrame(qc, frame);
    await flush();

    // Absolute set (not +1) → replay lands the same value, never 2.
    expect(threadRootReplyCount(qc, "c1", "root1")).toBe(1);
    unsub();
  });

  it("projection absent (reader mounted, cache empty) → cold-absent-vacuous, no write, no refetch", async () => {
    const qc = new QueryClient();
    // Reader mounted but its snapshot never loaded (still fetching).
    const unsub = mountReader(qc, threadRootKey("c1", "root1"));
    const invalidate = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, patchedReplyFrame("c1", "root1", 1, 6));

    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    // Nothing merged (the root's own mount-fetch is the authoritative recovery)...
    expect(threadRootReplyCount(qc, "c1", "root1")).toBeUndefined();
    // ...and NOT a fallback refetch — cold-absent is vacuous, not pending-fallback.
    expect(invalidate).not.toHaveBeenCalled();
    unsub();
  });

  it("no reader at all → fully vacuous satisfy (floor advances, no cache bootstrapped)", async () => {
    const qc = new QueryClient();
    ingestConversationFrame(qc, patchedReplyFrame("c1", "root1", 1, 6));
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    expect(qc.getQueryData(threadRootKey("c1", "root1"))).toBeUndefined();
    expect(qc.getQueryData(messagesKey("c1"))).toBeUndefined();
  });

  it("active reader but NO patch (additive-compat) → fallback invalidate→refetch, commit-gated", async () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, threadRootKey("c1", "root1"));
    seedThreadRoot(qc, "c1", "root1", 0);
    let commitRefetch!: () => void;
    const invalidate = vi.spyOn(qc, "invalidateQueries").mockReturnValue(
      new Promise<void>((resolve) => {
        commitRefetch = () => resolve();
      }),
    );

    // A reply frame with NO projections sidecar (old backend / non-emit path).
    ingestConversationFrame(
      qc,
      messageFrameSeq("c1", message({ id: "r1", seq: 6, thread_root_id: "root1" }), 6),
    );
    await flush();
    // Fallback took the invalidate path; floor waits for the refetch to COMMIT.
    expect(invalidate).toHaveBeenCalledWith({
      queryKey: threadRootKey("c1", "root1"),
    });
    expect(markSatisfied).not.toHaveBeenCalled();
    commitRefetch();
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    unsub();
  });

  // Cindy #1277 blocker / doc §10 gate #6 — the dirty-sidecar anti-corruption case.
  // A frame is a reply to rootA, but the sidecar's `root_id` disagrees (rootB). The
  // authoritative association key is `msg.thread_root_id` (rootA); a mismatch is
  // "active but no valid patch". Single-point validation drops the patch, so: (a)
  // rootA keeps its invalidate→refetch fallback, commit-gated — NOT silently merged
  // from a stale value; (b) rootB is NEVER merged from the dirty patch, even though
  // its row is displayed. Merging rootB (or suppressing rootA's fallback) would
  // strand rootA's real update once the floor advances.
  it("mismatched sidecar root (msg=rootA, patch.root_id=rootB) → rootA fallback commit-gated, rootB never merged", async () => {
    const qc = new QueryClient();
    const unsubA = mountReader(qc, threadRootKey("c1", "rootA"));
    const unsubB = mountReader(qc, threadRootKey("c1", "rootB"));
    seedThreadRoot(qc, "c1", "rootA", 0);
    seedThreadRoot(qc, "c1", "rootB", 7);
    let commitRefetch!: () => void;
    const invalidate = vi.spyOn(qc, "invalidateQueries").mockReturnValue(
      new Promise<void>((resolve) => {
        commitRefetch = () => resolve();
      }),
    );

    // Frame: reply to rootA, but the sidecar carries rootB → dirty.
    ingestConversationFrame(qc, {
      type: "event",
      scope: conversationScope("c1"),
      seq: 6,
      epoch: "epoch-1",
      kind: "message",
      payload: {
        message: message({ id: "r6", seq: 6, thread_root_id: "rootA" }),
        projections: {
          thread_root_reply_count: { root_id: "rootB", reply_count: 99 },
        },
      },
    } as ServerFrame);
    await flush();

    // (a) rootA fell back to invalidate→refetch, floor waits for COMMIT (not merged).
    expect(invalidate).toHaveBeenCalledWith({
      queryKey: threadRootKey("c1", "rootA"),
    });
    expect(markSatisfied).not.toHaveBeenCalled();
    // (b) rootB was never merged from the dirty patch — stays at its seeded value.
    expect(threadRootReplyCount(qc, "c1", "rootB")).toBe(7);

    commitRefetch();
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    // Still never touched rootB even after the floor advanced.
    expect(threadRootReplyCount(qc, "c1", "rootB")).toBe(7);
    unsubA();
    unsubB();
  });
});

// #task:1745 — the inline reply-PREVIEW tail rides the SAME suppressed-invalidate
// path as the count patch above, so it is patched from the reply frame itself. The
// two gates that matter at this level: the tail actually appears for a root's
// FIRST reply (the common 0→1 case, which has no tail to append to), and it never
// buys that by reintroducing the whole-window refetch #1277 removed.
describe("ingestConversationFrame — inline reply-preview tail (#task:1745)", () => {
  let markSatisfied: ReturnType<typeof vi.spyOn>;
  beforeEach(() => {
    markSatisfied = vi.spyOn(eventWsClient, "markSatisfied").mockImplementation(() => {});
    vi.spyOn(eventWsClient, "commitSnapshotBaseline").mockImplementation(() => {});
  });
  afterEach(() => vi.restoreAllMocks());

  /** A displayed window whose read model projects previews (object present). */
  function seedPreviewWindow(
    qc: QueryClient,
    channelId: string,
    rootId: string,
    previews: Record<string, unknown> | undefined,
  ): void {
    qc.setQueryData(messagesKey(channelId), {
      pages: [
        {
          messages: [message({ id: rootId, seq: 1 })],
          reply_counts_by_root: { [rootId]: 0 },
          ...(previews ? { reply_previews_by_root: previews } : {}),
          has_more_before: false,
          has_more_after: false,
          stream_head_seq: 5,
        },
      ],
      pageParams: [undefined],
    });
  }
  function previewTail(
    qc: QueryClient,
    channelId: string,
    rootId: string,
  ): { message_id: string }[] | undefined {
    return qc.getQueryData<{
      pages: { reply_previews_by_root?: Record<string, { message_id: string }[]> }[];
    }>(messagesKey(channelId))?.pages[0]?.reply_previews_by_root?.[rootId];
  }
  function patchedReply(channelId: string, rootId: string, seq: number): ServerFrame {
    return {
      type: "event",
      scope: conversationScope(channelId),
      seq,
      epoch: "epoch-1",
      kind: "message",
      payload: {
        message: message({ id: `r${seq}`, seq, thread_root_id: rootId }),
        projections: {
          thread_root_reply_count: { root_id: rootId, reply_count: 1 },
        },
      },
    } as ServerFrame;
  }

  it("opens the tail on a root’s FIRST reply — and still never refetches the window", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    seedPreviewWindow(qc, "c1", "root1", {}); // projects previews, root has none yet
    const invalidate = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, patchedReply("c1", "root1", 6));

    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    expect(previewTail(qc, "c1", "root1")?.map((r) => r.message_id)).toEqual(["r6"]);
    expect(mainlineReplyCount(qc, "c1", "root1")).toBe(1);
    expect(invalidate).not.toHaveBeenCalled();
    unsub();
  });

  it("a read model that projects no previews stays badge-only — no tail, no refetch", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    seedPreviewWindow(qc, "c1", "root1", undefined);
    const invalidate = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, patchedReply("c1", "root1", 6));

    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    // Nothing to display and nothing a refetch would add — the badge alone moves.
    expect(previewTail(qc, "c1", "root1")).toBeUndefined();
    expect(mainlineReplyCount(qc, "c1", "root1")).toBe(1);
    expect(invalidate).not.toHaveBeenCalled();
    unsub();
  });

  it("stays bounded at three rows across a burst of live replies", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    seedPreviewWindow(qc, "c1", "root1", {});

    for (const seq of [6, 7, 8, 9]) {
      ingestConversationFrame(qc, patchedReply("c1", "root1", seq));
    }
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(4));

    expect(previewTail(qc, "c1", "root1")?.map((r) => r.message_id)).toEqual(["r7", "r8", "r9"]);
    unsub();
  });
});

// #task:1278 — S1 P2 threads_list_entry projection MERGE. A reply frame carrying an
// absolute viewer-invariant `threads_list_entry` repositions the HELD threads-list
// row in place (keyset resort) instead of the blanket invalidate→refetch. Same
// anti-corruption + disposition model as P1: valid-on-page → merged (no refetch);
// off-page → fallback (no insert); mismatch → the real root falls back.
describe("ingestConversationFrame — S1 threads_list_entry projection merge (#task:1278)", () => {
  let markSatisfied: ReturnType<typeof vi.spyOn>;
  beforeEach(() => {
    markSatisfied = vi.spyOn(eventWsClient, "markSatisfied").mockImplementation(() => {});
    vi.spyOn(eventWsClient, "commitSnapshotBaseline").mockImplementation(() => {});
  });
  afterEach(() => vi.restoreAllMocks());
  const flush = async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  };

  function invariantEntry(rootId: string, lastReplyAt: number, replyCount: number) {
    return {
      root_message_id: rootId,
      root_author: { kind: "human" as const, id: "u1" },
      root_preview: `root ${rootId}`,
      root_created_at: 0,
      root_seq: decodeMessageSeq(2),
      reply_count: replyCount,
      last_reply_at: lastReplyAt,
      last_reply_message_id: `lr-${rootId}-${lastReplyAt}`,
      repliers: [{ kind: "human" as const, id: "u1" }],
      replier_total: 1,
    };
  }
  /** A reply frame WITH the threads_list_entry projection (no reply_count patch, to
   *  isolate the P2 surface). */
  function threadsListPatchFrame(
    channelId: string,
    rootId: string,
    lastReplyAt: number,
    replyCount: number,
    seq: number,
  ): ServerFrame {
    return {
      type: "event",
      scope: conversationScope(channelId),
      seq,
      epoch: "epoch-1",
      kind: "message",
      payload: {
        message: message({ id: `r${seq}`, seq, thread_root_id: rootId }),
        projections: {
          threads_list_entry: { entry: invariantEntry(rootId, lastReplyAt, replyCount) },
        },
      },
    } as ServerFrame;
  }
  function seedThreadsList(
    qc: QueryClient,
    channelId: string,
    rows: { rootId: string; lastReplyAt: number; replyCount: number }[],
  ): void {
    // Single-page InfiniteData — the on-mount shape of `useChannelThreads`
    // (#task:1773 cursor pagination). The S1 merge only ever touches page 0.
    qc.setQueryData(channelThreadsKey(channelId), {
      pages: [
        {
          threads: rows.map((r) => ({
            ...invariantEntry(r.rootId, r.lastReplyAt, r.replyCount),
            viewer_following: false,
          })),
          next_cursor: null,
        },
      ],
      pageParams: [null],
    });
  }
  const listRow = (qc: QueryClient, channelId: string, rootId: string) =>
    qc
      .getQueryData<{
        pages: { threads: { root_message_id: string; reply_count: number }[] }[];
      }>(channelThreadsKey(channelId))
      ?.pages.flatMap((p) => p.threads)
      .find((t) => t.root_message_id === rootId);

  it("merges + repositions a held row in place — NO threads-list refetch, markSatisfied once", async () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, channelThreadsKey("c1"));
    // root1 is the bottom row; a reply bumps it above root2 → must move to top.
    seedThreadsList(qc, "c1", [
      { rootId: "root2", lastReplyAt: 200, replyCount: 1 },
      { rootId: "root1", lastReplyAt: 100, replyCount: 1 },
    ]);
    const invalidate = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, threadsListPatchFrame("c1", "root1", 300, 5, 6));

    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    // Absolute merge + keyset resort, in place...
    expect(listRow(qc, "c1", "root1")?.reply_count).toBe(5);
    expect(
      qc
        .getQueryData<{ pages: { threads: { root_message_id: string }[] }[] }>(
          channelThreadsKey("c1"),
        )
        ?.pages.flatMap((p) => p.threads.map((t) => t.root_message_id)),
    ).toEqual(["root1", "root2"]);
    // ...WITHOUT invalidating the threads list (the #1278 amplification kill).
    expect(invalidate).not.toHaveBeenCalledWith({ queryKey: channelThreadsKey("c1") });
    unsub();
  });

  it("off-page root (page loaded, root absent) → invalidate fallback, commit-gated (no insert)", async () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, channelThreadsKey("c1"));
    seedThreadsList(qc, "c1", [{ rootId: "other", lastReplyAt: 200, replyCount: 1 }]);
    let commit!: () => void;
    const invalidate = vi
      .spyOn(qc, "invalidateQueries")
      .mockReturnValue(new Promise<void>((resolve) => (commit = () => resolve())));

    ingestConversationFrame(qc, threadsListPatchFrame("c1", "root1", 300, 5, 6));
    await flush();

    // No insert — fell back to a refetch; floor waits for its COMMIT.
    expect(invalidate).toHaveBeenCalledWith({ queryKey: channelThreadsKey("c1") });
    expect(markSatisfied).not.toHaveBeenCalled();
    commit();
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    unsub();
  });

  it("mismatch (msg=rootA, entry.root_message_id=rootB) → rootA blanket-invalidate fallback, rootB not repositioned", async () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, channelThreadsKey("c1"));
    seedThreadsList(qc, "c1", [{ rootId: "rootB", lastReplyAt: 100, replyCount: 1 }]);
    let commit!: () => void;
    const invalidate = vi
      .spyOn(qc, "invalidateQueries")
      .mockReturnValue(new Promise<void>((resolve) => (commit = () => resolve())));

    // Frame is a reply to rootA, but the entry carries rootB → validation drops it.
    ingestConversationFrame(qc, {
      type: "event",
      scope: conversationScope("c1"),
      seq: 6,
      epoch: "epoch-1",
      kind: "message",
      payload: {
        message: message({ id: "r6", seq: 6, thread_root_id: "rootA" }),
        projections: { threads_list_entry: { entry: invariantEntry("rootB", 999, 9) } },
      },
    } as ServerFrame);
    await flush();

    // rootA (the real root) falls back to the blanket threads-list invalidate...
    expect(invalidate).toHaveBeenCalledWith({ queryKey: channelThreadsKey("c1") });
    expect(markSatisfied).not.toHaveBeenCalled();
    // ...and rootB's row was NOT repositioned/merged (stays reply_count 1, not 9).
    expect(listRow(qc, "c1", "rootB")?.reply_count).toBe(1);
    commit();
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    expect(listRow(qc, "c1", "rootB")?.reply_count).toBe(1);
    unsub();
  });

  // #task:1773 — the `followed ∩ unread` filtered dataset is a CHILD of the All
  // key and is NEVER merged locally (its membership is viewer-specific and
  // server-authoritative). On the MERGE path the All list takes the in-place
  // patch INSTEAD of a blanket invalidate, so the filtered sibling would go
  // stale unless it is explicitly invalidated. This pins that seam.
  it("merge path (#task:1773): invalidates the filtered sibling key, NOT the exact All key", async () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, channelThreadsKey("c1"));
    seedThreadsList(qc, "c1", [
      { rootId: "root2", lastReplyAt: 200, replyCount: 1 },
      { rootId: "root1", lastReplyAt: 100, replyCount: 1 },
    ]);
    const invalidate = vi.spyOn(qc, "invalidateQueries");

    ingestConversationFrame(qc, threadsListPatchFrame("c1", "root1", 300, 5, 6));
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));

    // Filtered child refetched (it was not merged)...
    expect(invalidate).toHaveBeenCalledWith({ queryKey: channelThreadsUnreadKey("c1") });
    // ...while the exact All key is NOT invalidated (it took the in-place merge).
    expect(invalidate).not.toHaveBeenCalledWith({ queryKey: channelThreadsKey("c1") });
    unsub();
  });

  it("off-page fallback (#task:1773): the filtered child stays covered by the family prefix-invalidate", async () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, channelThreadsKey("c1"));
    seedThreadsList(qc, "c1", [{ rootId: "other", lastReplyAt: 200, replyCount: 1 }]);
    let commit!: () => void;
    const invalidate = vi
      .spyOn(qc, "invalidateQueries")
      .mockReturnValue(new Promise<void>((resolve) => (commit = () => resolve())));

    ingestConversationFrame(qc, threadsListPatchFrame("c1", "root1", 300, 5, 6));
    await flush();

    // Off-page falls back to the blanket family invalidate, whose prefix
    // (channelThreadsKey) already refreshes the filtered child — so the Unread
    // view never goes stale even when the replied-to root isn't on the held page.
    expect(invalidate).toHaveBeenCalledWith({ queryKey: channelThreadsKey("c1") });
    commit();
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    unsub();
  });
});

// thread-root projection) were the same BUG CLASS: a message-row content-patch
// handler hand-enumerated which projections to write and missed one. The seam
// (`patchMessageInActiveProjections`) fixes it BY CONSTRUCTION — every displayed
// projection holding the row is fanned the same idempotent patch, membership-
// gated, never bootstrapping. This matrix pins that: standard event × projection
// surface (mainline / cluster / thread-root / multi-open / no-reader) × {reaction,
// task}. The `applied` boolean is the synchronous floor signal, so a spied
// `markSatisfied` doubles as the applied assertion (reaction/task keep the
// synchronous satisfaction path — #1263 hybrid boundary).
describe("ingestConversationFrame — message-row patch seam matrix (#task:1270)", () => {
  let markSatisfied: ReturnType<typeof vi.spyOn>;
  beforeEach(() => {
    markSatisfied = vi.spyOn(eventWsClient, "markSatisfied").mockImplementation(() => {});
    vi.spyOn(eventWsClient, "commitSnapshotBaseline").mockImplementation(() => {});
  });
  afterEach(() => vi.restoreAllMocks());

  const windowCache = (rows: MessageDto[]) => ({
    pages: [
      {
        messages: rows,
        reply_counts_by_root: {},
        has_more_before: false,
        has_more_after: false,
        stream_head_seq: 5,
      },
    ],
    pageParams: [undefined],
  });
  const threadRootCache = (row: MessageDto) => ({
    messages: [row],
    reply_counts_by_root: {},
    has_more_before: false,
    has_more_after: false,
    stream_head_seq: 5,
  });
  const rowInWindow = (
    qc: QueryClient,
    key: readonly unknown[],
    id: string,
  ): MessageDto | undefined => {
    const c = qc.getQueryData<{ pages: { messages: MessageDto[] }[] }>(key);
    return c?.pages.flatMap((p) => p.messages).find((m) => m.id === id);
  };
  const rowInThreadRoot = (
    qc: QueryClient,
    channelId: string,
    rootId: string,
    id: string,
  ): MessageDto | undefined => {
    const c = qc.getQueryData<{ messages: MessageDto[] }>(threadRootKey(channelId, rootId));
    return c?.messages.find((m) => m.id === id);
  };

  // Two message-row patch event kinds; the seam is the shared machinery, so the
  // grid runs identically for both — proving the fix is class-wide, not a
  // reaction-local patch (Cindy's requirement).
  const EVENTS = [
    {
      kind: "reaction",
      seedRow: (id: string, rootId: string | null = null) =>
        message({ id, seq: 5, thread_root_id: rootId, reactions: null }),
      fire: (qc: QueryClient, channelId: string, id: string, rootId: string | null = null) =>
        ingestConversationFrame(qc, reactionFrame(channelId, id, 6, rootId)),
      assertPatched: (row: MessageDto | undefined) =>
        expect(row?.reactions?.[0]).toMatchObject({ emoji: "👍", count: 1 }),
      assertUnpatched: (row: MessageDto | undefined) => expect(row?.reactions).toBeNull(),
    },
    {
      kind: "task",
      seedRow: (id: string, rootId: string | null = null) =>
        message({
          id,
          seq: 5,
          thread_root_id: rootId,
          linked_task: {
            task_id: "task-7",
            task_number: 7,
            title: "old",
            status: "todo",
            assignee: null,
          },
        }),
      fire: (qc: QueryClient, channelId: string, id: string) =>
        ingestConversationFrame(qc, taskChangedFrame(channelId, id, 6)),
      assertPatched: (row: MessageDto | undefined) => {
        expect(row?.linked_task?.status).toBe("in_progress");
        expect(row?.linked_task?.title).toBe("do the thing");
      },
      assertUnpatched: (row: MessageDto | undefined) => {
        expect(row?.linked_task?.status).toBe("todo");
        expect(row?.linked_task?.title).toBe("old");
      },
    },
  ] as const;

  for (const ev of EVENTS) {
    describe(ev.kind, () => {
      it("mainline-only reader: patches the mainline row + markSatisfied once", () => {
        const qc = new QueryClient();
        const unsub = mountMainlineReader(qc, "c1");
        qc.setQueryData(messagesKey("c1"), windowCache([ev.seedRow("m1")]));

        ev.fire(qc, "c1", "m1");

        ev.assertPatched(rowInWindow(qc, messagesKey("c1"), "m1"));
        expect(markSatisfied).toHaveBeenCalledTimes(1);
        expect(markSatisfied).toHaveBeenCalledWith(conversationScope("c1"), 6, "epoch-1");
        unsub();
      });

      it("cluster-only reader (NO mainline): patches the cluster row, mainline stays absent", () => {
        const qc = new QueryClient();
        const unsub = mountReader(qc, clusterMessagesKey("c1", "root1"));
        qc.setQueryData(
          clusterMessagesKey("c1", "root1"),
          windowCache([ev.seedRow("r1", "root1")]),
        );

        ev.fire(qc, "c1", "r1", "root1");

        ev.assertPatched(rowInWindow(qc, clusterMessagesKey("c1", "root1"), "r1"));
        // No mainline reader → the mainline window is never bootstrapped.
        expect(qc.getQueryData(messagesKey("c1"))).toBeUndefined();
        expect(markSatisfied).toHaveBeenCalledTimes(1);
        unsub();
      });

      it("thread-root-only reader: patches the root row (the projection the old hand-enumerated handler forgot)", () => {
        const qc = new QueryClient();
        const unsub = mountReader(qc, threadRootKey("c1", "root1"));
        qc.setQueryData(threadRootKey("c1", "root1"), threadRootCache(ev.seedRow("root1")));

        ev.fire(qc, "c1", "root1", "root1");

        ev.assertPatched(rowInThreadRoot(qc, "c1", "root1", "root1"));
        expect(markSatisfied).toHaveBeenCalledTimes(1);
        unsub();
      });

      it("multi-open (mainline + cluster + thread-root all hold the root row): patches ALL three, markSatisfied ONCE", () => {
        const qc = new QueryClient();
        const unsubM = mountMainlineReader(qc, "c1");
        const unsubC = mountReader(qc, clusterMessagesKey("c1", "root1"));
        const unsubR = mountReader(qc, threadRootKey("c1", "root1"));
        qc.setQueryData(messagesKey("c1"), windowCache([ev.seedRow("root1")]));
        qc.setQueryData(clusterMessagesKey("c1", "root1"), windowCache([ev.seedRow("root1")]));
        qc.setQueryData(threadRootKey("c1", "root1"), threadRootCache(ev.seedRow("root1")));

        ev.fire(qc, "c1", "root1", "root1");

        ev.assertPatched(rowInWindow(qc, messagesKey("c1"), "root1"));
        ev.assertPatched(rowInWindow(qc, clusterMessagesKey("c1", "root1"), "root1"));
        ev.assertPatched(rowInThreadRoot(qc, "c1", "root1", "root1"));
        // One frame → exactly one floor advance, no matter how many projections
        // the patch fanned to.
        expect(markSatisfied).toHaveBeenCalledTimes(1);
        unsubM();
        unsubC();
        unsubR();
      });

      it("no reader (warm caches, zero observers): task warm-patches + satisfies; reaction skips", () => {
        const qc = new QueryClient();
        // Warm caches exist but nothing observes them → not DISPLAYED.
        qc.setQueryData(messagesKey("c1"), windowCache([ev.seedRow("m1")]));
        qc.setQueryData(
          clusterMessagesKey("c1", "root1"),
          windowCache([ev.seedRow("r1", "root1")]),
        );

        ev.fire(qc, "c1", "m1");

        if (ev.kind === "task") {
          // Task chip is an ABSOLUTE by-id patch: a warm-inactive cache holding
          // the source row MUST be patched (ws-client forbids satisfying past a
          // displayable-stale cache), then the floor satisfies.
          ev.assertPatched(rowInWindow(qc, messagesKey("c1"), "m1"));
          expect(markSatisfied).toHaveBeenCalledTimes(1);
        } else {
          // Reaction keeps the active-only behavior (scope not expanded this task):
          // no displayed reader → not applied → floor not advanced.
          ev.assertUnpatched(rowInWindow(qc, messagesKey("c1"), "m1"));
          expect(markSatisfied).not.toHaveBeenCalled();
        }
      });

      it("membership-gated: an OPEN reader whose window does NOT hold the row — reaction no-ops; task vacuously satisfies (authoritative-on-paginate)", () => {
        const qc = new QueryClient();
        // Mainline reader open, but its window holds a DIFFERENT row, and no warm
        // cache holds the target → the source row is absent everywhere.
        const unsub = mountMainlineReader(qc, "c1");
        qc.setQueryData(messagesKey("c1"), windowCache([ev.seedRow("other")]));

        ev.fire(qc, "c1", "m1");

        expect(rowInWindow(qc, messagesKey("c1"), "m1")).toBeUndefined();
        ev.assertUnpatched(rowInWindow(qc, messagesKey("c1"), "other"));
        if (ev.kind === "task") {
          // No cache holds the source row → a future paginate fetches it
          // authoritatively → VACUOUS satisfy (never pin the floor on an
          // off-window source row; ws-client pending-satisfaction queue absorbs it).
          expect(markSatisfied).toHaveBeenCalledTimes(1);
        } else {
          expect(markSatisfied).not.toHaveBeenCalled();
        }
        unsub();
      });
    });
  }

  // Alice's decoupling regression: the task-by-number DETAIL projection (inline
  // `#task:<n>` hover/detail) is a SEPARATE projection — authoritative-on-open,
  // fired OUTSIDE the row-patch seam, membership-gated by its OWN cache, and NOT
  // counted as the floor signal. Same class as reaction's reactors-list refresh.
  // This locks: it still refreshes when the mainline is closed and never
  // bootstraps when absent. (The FLOOR itself advances for a source-backed frame
  // via the source-row disposition — displayed/warm patch, else off-window
  // vacuous-satisfy — which is orthogonal to this detail refresh; the detail
  // refresh is not what triggers markSatisfied.)
  describe("task-by-number detail projection rides OUTSIDE the floor seam (Alice)", () => {
    const taskSummary = (): TaskSummaryDto => ({
      // Resolved by-number entries are patched by task IDENTITY, not by
      // (workspace, task_number) — so the seeded id must be the task_id the
      // event names ('task-7'), else the identity match skips it.
      id: "task-7",
      task_number: 7,
      place: {
        kind: "workspace",
        conversation_id: "c1",
        workspace_id: "w1",
        channel_name: "general",
      },
      thread_root_id: null,
      status: "todo",
      title: "old",
      assignee: null,
      creator: { type: "user", id: "u1", name: "shiki" },
      created_at: 0,
      closed_at: null,
    });

    it("refreshes the detail cache when the mainline is CLOSED (decoupled from the floor)", () => {
      const qc = new QueryClient();
      qc.setQueryData(
        taskByNumberKey(taskBoardCacheScope({ workspaceId: "w1", conversationId: null }), 7),
        taskSummary(),
      );
      // No mainline / cluster / thread-root reader → the row seam applies nothing.

      ingestConversationFrame(qc, taskChangedFrame("c1", "t1", 6));

      const detail = qc.getQueryData<TaskSummaryDto>(
        taskByNumberKey(taskBoardCacheScope({ workspaceId: "w1", conversationId: null }), 7),
      );
      expect(detail?.status).toBe("in_progress");
      expect(detail?.title).toBe("do the thing");
      // The floor advance here is the SOURCE-ROW disposition (source 't1' is in no
      // message cache → off-window vacuous-satisfy), NOT the detail refresh — the
      // detail refresh is authoritative-on-open and never the floor signal.
      expect(markSatisfied).toHaveBeenCalledTimes(1);
    });

    it("does not bootstrap an absent detail cache (no throw); source row still vacuously satisfies", () => {
      const qc = new QueryClient();

      ingestConversationFrame(qc, taskChangedFrame("c1", "t1", 6));

      expect(
        qc.getQueryData(
          taskByNumberKey(taskBoardCacheScope({ workspaceId: "w1", conversationId: null }), 7),
        ),
      ).toBeUndefined();
      // No detail bootstrap, but the source-row disposition (source 't1' in no
      // message cache → off-window vacuous-satisfy) still advances the floor once.
      expect(markSatisfied).toHaveBeenCalledTimes(1);
    });
  });
});

// #task:1753 — a committed reaction reaches the renderer on TWO planes: the
// `message_reaction_changed` WS echo and the ACK of the viewer's own write. The
// viewer's own toggle produces BOTH, in either order, and its window may not even
// be subscribed to the echo. What the two planes may DO differs, and that is the
// point of this grid:
//   - WS is ORDERED per scope (replay + a `snapshot_required` gap fence), so its
//     absolute aggregate can be ASSIGNED; a re-delivered frame carrying the same
//     value must write nothing at all (identity-preserving merge → no re-render
//     per reaction per projection).
//   - the ACK shares NO ordering axis with that stream, nor with a second local
//     toggle's ACK, so it may not assign: it is a FRESHNESS TRIGGER that re-reads
//     the row authoritatively. Two falsifiers below pin that (a late OLDER ACK
//     cannot undo a newer fact; reversed ACKs of two local toggles converge on
//     the server) — both of which an absolute ACK write regresses, permanently.
//   - the ACK's diagnostic `changed:false` cannot gate any of this, and is not
//     representable: the recovery seam takes only a target, never the payload.
// Both planes converge over the SAME displayed projections, and only WS touches
// the resume floor.
describe("committed reactions converge across the ACK and WS planes (#task:1753)", () => {
  const VIEWER = "u-self";

  beforeEach(() => {
    vi.spyOn(eventWsClient, "markSatisfied").mockImplementation(() => {});
    vi.spyOn(eventWsClient, "commitSnapshotBaseline").mockImplementation(() => {});
    // The WS plane resolves `viewerReacted` by comparing the event's reactor id
    // against the hydrated viewer; the ACK plane never does. Both are exercised.
    setCurrentUserId(VIEWER);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    clearCurrentUser();
  });

  const windowCache = (rows: MessageDto[]) => ({
    pages: [
      {
        messages: rows,
        reply_counts_by_root: {},
        has_more_before: false,
        has_more_after: false,
        stream_head_seq: 5,
      },
    ],
    pageParams: [undefined],
  });
  const rowIn = (qc: QueryClient, key: readonly unknown[], id: string): MessageDto | undefined =>
    qc
      .getQueryData<{ pages: { messages: MessageDto[] }[] }>(key)
      ?.pages.flatMap((p) => p.messages)
      .find((m) => m.id === id);
  const chip = (row: MessageDto | undefined, emoji = "👍") =>
    row?.reactions?.find((r) => r.emoji === emoji);
  const thumbsUp = (count: number, viewer: boolean) => [
    { emoji: "👍", count, viewer_reacted: viewer },
  ];

  /** The ACK plane, driven exactly as `useToggleReaction.onSuccess` drives it:
   *  the write committed, so recover this row. Note what is NOT passed — the
   *  ACK's `emoji` / `count` / `changed` cannot reach the cache even by mistake. */
  const ackWrite = (
    qc: QueryClient,
    args: { messageId: string; threadRootId?: string | null },
  ): Promise<void> =>
    recoverCommittedReactionsAfterWrite(qc, {
      channelId: "c1",
      messageId: args.messageId,
      threadRootId: args.threadRootId ?? null,
    });

  const invalidated = (qc: QueryClient, key: readonly unknown[]): boolean =>
    qc.getQueryState(key)?.isInvalidated ?? false;

  /** A mounted reader whose refetch returns the CURRENT server state — the
   *  authoritative round a recovery triggers. Seeded data + `staleTime: Infinity`
   *  means mounting itself fetches nothing, so every fetch counted here is one
   *  the code under test asked for. (A plain observer stands in for
   *  `useChannelMessages`: the seam reads only the observer count and the window
   *  shape.) */
  const mountAuthoritativeReader = (
    qc: QueryClient,
    key: readonly unknown[],
    server: { rows: MessageDto[]; fetches: number },
  ): (() => void) => {
    const observer = new QueryObserver(qc, {
      queryKey: key,
      staleTime: Infinity,
      queryFn: () => {
        server.fetches += 1;
        return Promise.resolve(windowCache(server.rows));
      },
    });
    return observer.subscribe(() => {});
  };

  it("the WS plane assigns the absolute count; a re-delivered frame writes nothing", () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    qc.setQueryData(messagesKey("c1"), windowCache([message({ id: "m1", seq: 5 })]));

    ingestConversationFrame(
      qc,
      reactionFrame("c1", "m1", 6, null, { reactorId: VIEWER, count: 1 }),
    );
    expect(chip(rowIn(qc, messagesKey("c1"), "m1"))).toEqual({
      emoji: "👍",
      count: 1,
      viewer_reacted: true,
    });

    // The same committed value delivered again. Capture the cache object first:
    // an identity-preserving no-op leaves the whole page container untouched,
    // which is what keeps subscribers from re-rendering.
    const before = qc.getQueryData(messagesKey("c1"));
    ingestConversationFrame(
      qc,
      reactionFrame("c1", "m1", 7, null, { reactorId: VIEWER, count: 1 }),
    );
    expect(qc.getQueryData(messagesKey("c1"))).toBe(before);
    expect(chip(rowIn(qc, messagesKey("c1"), "m1"))).toEqual({
      emoji: "👍",
      count: 1,
      viewer_reacted: true,
    });
    unsub();
  });

  it("the ACK plane recovers the row instead of writing it — the payload never reaches the cache", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    qc.setQueryData(
      messagesKey("c1"),
      windowCache([message({ id: "m1", seq: 5, reactions: thumbsUp(1, false) })]),
    );
    qc.setQueryData(messageReactorsKey("m1"), []);
    const before = qc.getQueryData(messagesKey("c1"));

    await ackWrite(qc, { messageId: "m1" });

    // Not one byte of the row was written by the ACK — the value it reported is
    // not the renderer's to assign. Both caches are marked for re-read instead.
    expect(qc.getQueryData(messagesKey("c1"))).toBe(before);
    expect(invalidated(qc, messagesKey("c1"))).toBe(true);
    expect(invalidated(qc, messageReactorsKey("m1"))).toBe(true);
    unsub();
  });

  it("FALSIFIER: a delayed OLDER ACK cannot undo a newer committed fact", async () => {
    const qc = new QueryClient();
    // Server truth once both writes have committed: the viewer's own reaction
    // plus a third party's, aggregate 2.
    const server = {
      rows: [message({ id: "m1", seq: 5, reactions: thumbsUp(2, true) })],
      fetches: 0,
    };
    qc.setQueryData(messagesKey("c1"), windowCache([message({ id: "m1", seq: 5 })]));
    const unsub = mountAuthoritativeReader(qc, messagesKey("c1"), server);

    // The viewer POSTs (server aggregate 1). Before its ACK returns, a third
    // party reacts and that frame lands first — the ordered plane assigns 2.
    ingestConversationFrame(qc, reactionFrame("c1", "m1", 6, null, { reactorId: "u2", count: 2 }));
    expect(chip(rowIn(qc, messagesKey("c1"), "m1"))?.count).toBe(2);

    // Now the viewer's own ACK finally arrives, carrying the aggregate AS OF its
    // own write (1). Assigning it would drop the third party's reaction and STICK
    // there until the next event — the regression this whole seam exists to
    // exclude. A recovery cannot express that: the cache is untouched until the
    // authoritative read lands.
    const recovering = ackWrite(qc, { messageId: "m1" });
    expect(chip(rowIn(qc, messagesKey("c1"), "m1"))?.count).toBe(2);
    await recovering;

    expect(server.fetches).toBe(1);
    expect(chip(rowIn(qc, messagesKey("c1"), "m1"))).toEqual({
      emoji: "👍",
      count: 2,
      viewer_reacted: true,
    });
    unsub();

    // WITNESS that the invariant above is not vacuous: the degenerate model —
    // assign the ACK's absolute aggregate, as the ORDERED plane legitimately may
    // — applied to the very same state drops the third party's reaction, and
    // nothing brings it back until the next event.
    const degenerate = new QueryClient();
    const unsubD = mountMainlineReader(degenerate, "c1");
    degenerate.setQueryData(
      messagesKey("c1"),
      windowCache([message({ id: "m1", seq: 5, reactions: thumbsUp(2, false) })]),
    );
    applyCommittedReactionFromEvent(degenerate, {
      target: { channelId: "c1", messageId: "m1", threadRootId: null },
      emoji: "👍",
      count: 1,
      viewerReacted: true,
    });
    expect(chip(rowIn(degenerate, messagesKey("c1"), "m1"))?.count).toBe(1);
    unsubD();
  });

  it("FALSIFIER: reversed ACKs of two local toggles converge on the server, not on the last ACK", async () => {
    const qc = new QueryClient();
    // The viewer added then removed; both writes have committed, so server truth
    // is "no reaction". Only the ACKs are still in flight, and reversed.
    const server = { rows: [message({ id: "m1", seq: 5 })], fetches: 0 };
    qc.setQueryData(
      messagesKey("c1"),
      windowCache([message({ id: "m1", seq: 5, reactions: thumbsUp(1, true) })]),
    );
    const unsub = mountAuthoritativeReader(qc, messagesKey("c1"), server);

    // The remove's ACK lands first and converges the row.
    await ackWrite(qc, { messageId: "m1" });
    expect(rowIn(qc, messagesKey("c1"), "m1")?.reactions).toBeNull();

    // Then the ADD's ACK — older, and it says `count: 1, viewer_reacted: true`.
    // Assigning it would resurrect a reaction the viewer already removed.
    await ackWrite(qc, { messageId: "m1" });
    expect(rowIn(qc, messagesKey("c1"), "m1")?.reactions).toBeNull();
    expect(server.fetches).toBe(2);
    unsub();

    // WITNESS: that same late ADD ACK, assigned, resurrects a reaction the viewer
    // already removed — and the row then disagrees with the server indefinitely.
    const degenerate = new QueryClient();
    const unsubD = mountMainlineReader(degenerate, "c1");
    degenerate.setQueryData(messagesKey("c1"), windowCache([message({ id: "m1", seq: 5 })]));
    applyCommittedReactionFromEvent(degenerate, {
      target: { channelId: "c1", messageId: "m1", threadRootId: null },
      emoji: "👍",
      count: 1,
      viewerReacted: true,
    });
    expect(chip(rowIn(degenerate, messagesKey("c1"), "m1"))).toEqual({
      emoji: "👍",
      count: 1,
      viewer_reacted: true,
    });
    unsubD();
  });

  /** A mounted reader whose FIRST fetch HANGS until released — the in-flight
   *  authoritative read a live patch can race. Its response is the server as of
   *  BEFORE the patched commit; any later fetch resolves with current server
   *  truth, because a read issued after a commit cannot miss it. */
  const mountHeldReader = (
    qc: QueryClient,
    key: readonly unknown[],
    stale: MessageDto[],
    server: { rows: MessageDto[]; fetches: number },
  ) => {
    let release = () => {};
    let aborted = false;
    const observer = new QueryObserver(qc, {
      queryKey: key,
      staleTime: Infinity,
      retry: false,
      queryFn: ({ signal }) => {
        server.fetches += 1;
        if (server.fetches > 1) return Promise.resolve(windowCache(server.rows));
        signal.addEventListener("abort", () => {
          aborted = true;
        });
        return new Promise<ReturnType<typeof windowCache>>((resolve) => {
          release = () => resolve(windowCache(stale));
        });
      },
    });
    const unsub = observer.subscribe(() => {});
    return { unsub, release: () => release(), wasAborted: () => aborted };
  };

  it("FALSIFIER: an authoritative read already in flight cannot commit over a newer WS patch", async () => {
    const qc = new QueryClient();
    qc.setQueryData(messagesKey("c1"), windowCache([message({ id: "m1", seq: 5 })]));
    // Server truth once the third party's reaction has committed: aggregate 2.
    const server = {
      rows: [message({ id: "m1", seq: 5, reactions: thumbsUp(2, true) })],
      fetches: 0,
    };
    // ...and the read the ACK recovery starts a moment earlier, which snapshots
    // the server BEFORE that reaction and therefore answers 1.
    const held = mountHeldReader(
      qc,
      messagesKey("c1"),
      [message({ id: "m1", seq: 5, reactions: thumbsUp(1, true) })],
      server,
    );

    const recovering = ackWrite(qc, { messageId: "m1" });
    await Promise.resolve();
    expect(server.fetches).toBe(1);

    // The third party's frame lands while that read is still out. Assigning is
    // this plane's privilege — but React Query commits a settled read over the
    // WHOLE query result, so an absolute patch must not leave a read that was
    // issued before its commit in flight.
    ingestConversationFrame(qc, reactionFrame("c1", "m1", 6, null, { reactorId: "u2", count: 2 }));
    expect(chip(rowIn(qc, messagesKey("c1"), "m1"))?.count).toBe(2);

    // The pre-commit response comes back now. It must reach nothing: the patch
    // restarted that read, which aborts the old one and rejects its promise.
    held.release();
    await recovering;

    expect(held.wasAborted()).toBe(true);
    expect(server.fetches).toBe(2);
    expect(chip(rowIn(qc, messagesKey("c1"), "m1"))).toEqual({
      emoji: "👍",
      count: 2,
      viewer_reacted: true,
    });
    held.unsub();

    // WITNESS that the restart is load-bearing: the degenerate model — assign and
    // leave the racing read alone — has its patch silently undone by the stale
    // response, and stays wrong until the next event.
    const degenerate = new QueryClient();
    const serverD = {
      rows: [message({ id: "m1", seq: 5, reactions: thumbsUp(2, true) })],
      fetches: 0,
    };
    degenerate.setQueryData(messagesKey("c1"), windowCache([message({ id: "m1", seq: 5 })]));
    const heldD = mountHeldReader(
      degenerate,
      messagesKey("c1"),
      [message({ id: "m1", seq: 5, reactions: thumbsUp(1, true) })],
      serverD,
    );
    const recoveringD = ackWrite(degenerate, { messageId: "m1" });
    await Promise.resolve();
    degenerate.setQueryData(
      messagesKey("c1"),
      windowCache([message({ id: "m1", seq: 5, reactions: thumbsUp(2, true) })]),
    );
    heldD.release();
    await recoveringD;
    expect(chip(rowIn(degenerate, messagesKey("c1"), "m1"))?.count).toBe(1);
    expect(serverD.fetches).toBe(1);
    heldD.unsub();
  });

  it("recovery reaches EVERY displayed projection holding the row, not just the rendered one", async () => {
    const qc = new QueryClient();
    // A thread root is displayed three ways at once: the mainline row, the
    // cluster's first row, and the thread-root projection.
    const unsubM = mountMainlineReader(qc, "c1");
    const unsubC = mountReader(qc, clusterMessagesKey("c1", "root1"));
    const unsubR = mountReader(qc, threadRootKey("c1", "root1"));
    const row = message({ id: "root1", seq: 5 });
    qc.setQueryData(messagesKey("c1"), windowCache([row]));
    qc.setQueryData(clusterMessagesKey("c1", "root1"), windowCache([row]));
    qc.setQueryData(threadRootKey("c1", "root1"), {
      messages: [row],
      reply_counts_by_root: {},
      has_more_before: false,
      has_more_after: false,
      stream_head_seq: 5,
    });

    await ackWrite(qc, { messageId: "root1" });

    expect(invalidated(qc, messagesKey("c1"))).toBe(true);
    expect(invalidated(qc, clusterMessagesKey("c1", "root1"))).toBe(true);
    expect(invalidated(qc, threadRootKey("c1", "root1"))).toBe(true);
    unsubM();
    unsubC();
    unsubR();
  });

  it("recovery scope equals the patch seam scope: a cluster-only reader, an unmounted mainline left alone", async () => {
    const qc = new QueryClient();
    // The reacted row is a reply in an open cluster; the mainline holds it too
    // but has NO reader. Reaction's scope is the displayed projections — the same
    // set the WS patch writes — so the mainline stays out of it (its warm-cache
    // semantics are unchanged by this task).
    const unsub = mountReader(qc, clusterMessagesKey("c1", "root1"));
    const reply = message({ id: "r1", seq: 5, thread_root_id: "root1" });
    qc.setQueryData(clusterMessagesKey("c1", "root1"), windowCache([reply]));
    qc.setQueryData(messagesKey("c1"), windowCache([reply]));
    const mainlineBefore = qc.getQueryData(messagesKey("c1"));

    await ackWrite(qc, { messageId: "r1", threadRootId: "root1" });

    expect(invalidated(qc, clusterMessagesKey("c1", "root1"))).toBe(true);
    expect(invalidated(qc, messagesKey("c1"))).toBe(false);
    expect(qc.getQueryData(messagesKey("c1"))).toBe(mainlineBefore);
    unsub();
  });

  it("a displayed projection that does not hold the row is neither refetched nor bootstrapped", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    qc.setQueryData(messagesKey("c1"), windowCache([message({ id: "other", seq: 5 })]));

    await ackWrite(qc, { messageId: "m1" });

    // Membership gating is what bounds the cost: no window-wide refetch for a row
    // this window never showed, and no absent projection brought to life.
    expect(invalidated(qc, messagesKey("c1"))).toBe(false);
    expect(rowIn(qc, messagesKey("c1"), "m1")).toBeUndefined();
    expect(qc.getQueryState(clusterMessagesKey("c1", "root1"))).toBeUndefined();
    unsub();
  });

  it("the ACK plane never advances the resume floor", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    qc.setQueryData(messagesKey("c1"), windowCache([message({ id: "m1", seq: 5 })]));

    await ackWrite(qc, { messageId: "m1" });

    // The water-mark is a TRANSPORT fact: only frames may move it, or a resume
    // would skip events an HTTP round trip never carried.
    expect(vi.mocked(eventWsClient.markSatisfied)).not.toHaveBeenCalled();
    unsub();
  });

  it("recovery refreshes the who-reacted detail but never writes reactor rows", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    qc.setQueryData(messagesKey("c1"), windowCache([message({ id: "m1", seq: 5 })]));
    const seeded: unknown[] = [];
    qc.setQueryData(messageReactorsKey("m1"), seeded);

    await ackWrite(qc, { messageId: "m1" });

    // Authoritative-on-open, same as the WS plane: mark it stale, never derive
    // reactor rows from a write.
    expect(invalidated(qc, messageReactorsKey("m1"))).toBe(true);
    expect(qc.getQueryData(messageReactorsKey("m1"))).toBe(seeded);
    unsub();
  });
});

// #task:1270 — SOURCE-SCAN FENCE (orthogonal to the by-construction fence: the
// 4 per-projection fan-out exports were deleted so a handler physically cannot
// hand-pick projections). This fence guards the OTHER direction: the message-row
// patch handlers must route EVERY projection write through the seam — never
// touch a projection cache directly (`setQueryData`) and never re-derive the
// per-projection reader gates (`hasActiveMainlineReader` / `openClusterReaderRoots`
// / `openThreadRootReaderRoots`). Those gates can't be globally unexported
// (ingestMessage's append path legitimately uses them), so the fence is body-
// scoped to the two row-patch handlers. The node test env can't render, so — in
// the spirit of the read-inert fences — we statically slice + scan the source.
describe("message-row patch handlers — route through the seam, never hand-pick projections (#task:1270)", () => {
  const read = (relative: string): string =>
    readFileSync(fileURLToPath(new URL(relative, import.meta.url)), "utf8");
  const ingestorSource = read("./conversation-ingestor.ts");
  const reactionSource = read("../channels/reaction-hooks.ts");

  function sliceFunctionBody(source: string, declaration: string): string {
    const start = source.indexOf(declaration);
    if (start === -1) {
      throw new Error(`could not find "${declaration}" — was it renamed?`);
    }
    const rest = source.slice(start + declaration.length);
    const nextDecl = rest.search(/\n(?:export )?function /);
    return nextDecl === -1 ? rest : rest.slice(0, nextDecl);
  }

  // The reader-gate predicates a hand-enumerated handler would re-derive to pick
  // projections, plus the raw cache writer. All must be absent from the row-patch
  // handler bodies — the seam owns projection selection + writes.
  const FORBIDDEN = [
    "setQueryData",
    "hasActiveMainlineReader(",
    "openClusterReaderRoots(",
    "openThreadRootReaderRoots(",
  ];

  // Each row-patch handler names the seam it delegates to. `ingestTaskChanged`
  // calls the row seam directly; `ingestReaction` reaches it through the
  // ordered-plane committed-reaction seam (#task:1753). That seam is the chain's
  // second link and is scanned here too — otherwise "route through the seam"
  // could be satisfied by a wrapper that hand-picks projections itself.
  const LINKS = [
    {
      source: ingestorSource,
      decl: "function ingestReaction(",
      seam: "applyCommittedReactionFromEvent(",
    },
    {
      source: ingestorSource,
      decl: "function ingestTaskChanged(",
      seam: "patchMessageInActiveProjections(",
    },
    {
      source: reactionSource,
      decl: "export function applyCommittedReactionFromEvent(",
      seam: "patchMessageInActiveProjections(",
    },
  ];

  for (const { source, decl, seam } of LINKS) {
    const name = decl.replace("export ", "").replace("function ", "").replace("(", "");
    const body = sliceFunctionBody(source, decl);

    it(`${name} routes projection writes through ${seam.replace("(", "")}`, () => {
      expect(body).toContain(seam);
    });

    it(`${name} never hand-picks projections (no gate predicate / direct setQueryData)`, () => {
      for (const sym of FORBIDDEN) expect(body).not.toContain(sym);
    });
  }

  // #task:1753 review — the ACK plane's own fence. An HTTP ACK shares no ordering
  // axis with the WS stream, so it may recover the row but never ASSIGN a value
  // (the two falsifiers in the convergence grid above are what an assignment
  // breaks). Those falsifiers can only fail if someone re-adds the assignment, and
  // the tempting shortcut — "we already have the authoritative count, just patch
  // it" — is exactly what this scan forbids, including reading the payload at all.
  it("the write ACK converges by recovery only, never by assigning its payload", () => {
    const body = sliceFunctionBody(reactionSource, "export function useToggleReaction(");
    expect(body).toContain("recoverCommittedReactionsAfterWrite(");
    for (const sym of [
      ...FORBIDDEN,
      "applyReactionToMessage(",
      "applyCommittedReactionFromEvent(",
      "patchMessageInActiveProjections(",
      "ack.count",
      "ack.emoji",
      "ack.changed",
    ]) {
      expect(body).not.toContain(sym);
    }
  });
});

// #task:1715 — `['task-detail', id]` has exactly ONE realtime owner, and it is
// NOT this ingestor. The open modal's own `useTaskDetail` listener is the writer;
// it attributes by frame SCOPE (`viewer:<userId>`), so a conversation-scope frame
// must leave this key completely alone. A second writer is not a harmless extra
// refresh: `invalidateQueries` defaults to `cancelRefetch: true`, so it cancels
// and restarts the owner's in-flight refetch on every frame.
describe("task-detail cache is untouched by conversation-scope frames (#task:1715)", () => {
  it("neither rewrites nor invalidates the open modal detail entry", () => {
    const qc = new QueryClient();
    const seeded = { id: "task-7", title: "as painted" };
    qc.setQueryData(taskDetailKey("task-7"), seeded);

    // Same task, live status change, arriving on the conversation stream.
    ingestConversationFrame(qc, taskChangedFrame("c1", "src-1", 6));

    expect(qc.getQueryData(taskDetailKey("task-7"))).toBe(seeded);
    // The falsifier for the cancel+restart regression: a copied-in
    // `invalidateQueries` leaves no data trace at all, only this flag.
    expect(qc.getQueryState(taskDetailKey("task-7"))?.isInvalidated).toBe(false);
  });

  it("never names the detail key at all (fence against re-adding a second owner)", () => {
    const source = readFileSync(
      fileURLToPath(new URL("./conversation-ingestor.ts", import.meta.url)),
      "utf8",
    );
    // Comment lines are dropped: the ingestor DOCUMENTS that it leaves
    // `['task-detail', …]` alone, and that sentence must not read as a violation.
    const code = source
      .split("\n")
      .filter((line) => {
        const t = line.trimStart();
        return !t.startsWith("//") && !t.startsWith("*") && !t.startsWith("/*");
      })
      .join("\n");
    expect(code).not.toContain("taskDetailKey");
    expect(code).not.toContain("task-detail");
  });
});

function taskChangedFrame(channelId: string, sourceMessageId: string, seq: number): ServerFrame {
  return {
    type: "event",
    scope: conversationScope(channelId),
    seq,
    epoch: "epoch-1",
    kind: "task_changed",
    payload: {
      kind: "status",
      workspace_id: "w1",
      channel_id: channelId,
      thread_root_id: null,
      conversation_id: channelId,
      task_number: 7,
      task_id: "task-7",
      source_message_id: sourceMessageId,
      status: "in_progress",
      assignee: null,
      title: "do the thing",
      closed_at: null,
    },
  } as ServerFrame;
}

function reactionFrame(
  channelId: string,
  messageId: string,
  seq: number,
  threadRootId: string | null = null,
  // The reactor / op / count the default add-by-`u2` fixture pins. Overridable
  // for the ACK-plane convergence grid (#task:1753), which needs the viewer's own
  // echo and removes.
  payload: Partial<{
    emoji: string;
    op: "add" | "remove";
    reactorId: string;
    count: number;
  }> = {},
): ServerFrame {
  return {
    type: "event",
    scope: conversationScope(channelId),
    seq,
    epoch: "epoch-1",
    kind: MESSAGE_REACTION_CHANGED_EVENT,
    payload: {
      message_id: messageId,
      conversation_id: channelId,
      thread_root_id: threadRootId,
      emoji: payload.emoji ?? "👍",
      op: payload.op ?? "add",
      reactor_kind: "user",
      reactor_id: payload.reactorId ?? "u2",
      count: payload.count ?? 1,
    },
  } as ServerFrame;
}

describe("ingestConversationFrame — snapshot commit gate (#task:1255 P2 step-4)", () => {
  let commit: ReturnType<typeof vi.spyOn>;
  beforeEach(() => {
    commit = vi.spyOn(eventWsClient, "commitSnapshotBaseline").mockImplementation(() => {});
  });
  afterEach(() => vi.restoreAllMocks());

  function snapshotFrame(channelId: string): ServerFrame {
    return {
      type: "snapshot_required",
      scope: conversationScope(channelId),
      epoch: "epoch-9",
      currentSeq: 42,
    } as ServerFrame;
  }

  it("commits (scope, epoch) once every reconcile settles fulfilled", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    ingestConversationFrame(qc, snapshotFrame("c1"));
    await vi.waitFor(() => expect(commit).toHaveBeenCalledWith(conversationScope("c1"), "epoch-9"));
    expect(commit).toHaveBeenCalledTimes(1);
    unsub();
  });

  it("an empty reconcile set (no readers) commits vacuously", async () => {
    const qc = new QueryClient();
    ingestConversationFrame(qc, snapshotFrame("c1"));
    await vi.waitFor(() => expect(commit).toHaveBeenCalledTimes(1));
  });

  it("does NOT commit if a reconcile rejects (floor stays pinned)", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    vi.spyOn(qc, "invalidateQueries").mockRejectedValue(new Error("refetch failed"));
    ingestConversationFrame(qc, snapshotFrame("c1"));
    // Flush the allSettled microtasks; commit must never fire.
    await Promise.resolve();
    await Promise.resolve();
    expect(commit).not.toHaveBeenCalled();
    unsub();
  });

  it("a side-scope snapshot never commits (side floor auto-advances)", async () => {
    const qc = new QueryClient();
    const unsub = mountReader(qc, agentRunStatesKey("c1"));
    ingestConversationFrame(qc, {
      type: "snapshot_required",
      scope: conversationSideScope("c1"),
      epoch: "epoch-9",
      currentSeq: 42,
    } as ServerFrame);
    await Promise.resolve();
    await Promise.resolve();
    expect(commit).not.toHaveBeenCalled();
    unsub();
  });
});

describe("ingestConversationFrame — warm-remove inactive cluster on snapshot (#task:1255 P2 step-4)", () => {
  afterEach(() => vi.restoreAllMocks());

  it("removes a warm-but-inactive cluster cache (refetchOnMount:false cold-start)", () => {
    const qc = new QueryClient();
    // A cluster window with data but NO observer (warm-inactive).
    qc.setQueryData(clusterMessagesKey("c1", "root1"), {
      pages: [
        {
          messages: [message({ id: "x", seq: 1, thread_root_id: "root1" })],
          reply_counts_by_root: {},
          has_more_before: false,
          has_more_after: false,
          stream_head_seq: 1,
        },
      ],
      pageParams: [undefined],
    });
    expect(qc.getQueryData(clusterMessagesKey("c1", "root1"))).not.toBeUndefined();

    ingestConversationFrame(qc, {
      type: "snapshot_required",
      scope: conversationScope("c1"),
      epoch: "e",
      currentSeq: 5,
    } as ServerFrame);

    // Removed → next open cold-starts (invalidate alone wouldn't refetch it).
    expect(qc.getQueryData(clusterMessagesKey("c1", "root1"))).toBeUndefined();
  });

  it("removes a warm-but-inactive mainline cache (default staleTime:5s would serve stale)", async () => {
    const qc = new QueryClient();
    const commit = vi.spyOn(eventWsClient, "commitSnapshotBaseline").mockImplementation(() => {});
    // A mainline window with data but NO observer (warm-inactive): ChannelView
    // unmounted but the cache is still inside the gcTime/staleTime window.
    seedMainline(qc, "c1");
    expect(qc.getQueryData(messagesKey("c1"))).not.toBeUndefined();

    ingestConversationFrame(qc, {
      type: "snapshot_required",
      scope: conversationScope("c1"),
      epoch: "e",
      currentSeq: 5,
    } as ServerFrame);

    // Removed (not left as absent/vacuous), so a remount within staleTime can't
    // display post-commit stale rows; the removeQueries settle is still
    // fulfilled, so the floor commit proceeds.
    expect(qc.getQueryData(messagesKey("c1"))).toBeUndefined();
    await vi.waitFor(() => expect(commit).toHaveBeenCalledWith(conversationScope("c1"), "e"));
  });

  it("removes BOTH warm mainline AND warm cluster in one snapshot (regression guard)", () => {
    // Guard both warm-remove classes together so a future change can't silently
    // drop one (the mainline hole was exactly "cluster covered, mainline not").
    const qc = new QueryClient();
    vi.spyOn(eventWsClient, "commitSnapshotBaseline").mockImplementation(() => {});
    seedMainline(qc, "c1");
    qc.setQueryData(clusterMessagesKey("c1", "root1"), {
      pages: [
        {
          messages: [message({ id: "x", seq: 1, thread_root_id: "root1" })],
          reply_counts_by_root: {},
          has_more_before: false,
          has_more_after: false,
          stream_head_seq: 1,
        },
      ],
      pageParams: [undefined],
    });
    expect(qc.getQueryData(messagesKey("c1"))).not.toBeUndefined();
    expect(qc.getQueryData(clusterMessagesKey("c1", "root1"))).not.toBeUndefined();

    ingestConversationFrame(qc, {
      type: "snapshot_required",
      scope: conversationScope("c1"),
      epoch: "e",
      currentSeq: 5,
    } as ServerFrame);

    expect(qc.getQueryData(messagesKey("c1"))).toBeUndefined();
    expect(qc.getQueryData(clusterMessagesKey("c1", "root1"))).toBeUndefined();
  });
});

// #task:1749 — the SHARED committed-message apply seam. The human send ACK and the
// WS echo are BOTH authoritative inputs for the SAME committed message, so they
// enter through `applyCommittedMessage`, which (1) runs the derived cache
// convergence and (2) wraps the synchronous cache writes in `notifyManager.batch`
// so an observer never sees a half-applied 0→1 (preview tail present while the
// reply count is still absent — Norman's headerless-block coupling).
//
// There is deliberately NO once-only guard (Cindy + shiki, #task:1749): the two
// planes may each run the effects for the same committed message. Correctness rests
// on the data writes being by-id/absolute/idempotent — a second run converges to
// the same cache; only the invalidate-only effects may refetch once more, an
// accepted best-effort cost (a message-level receipt to erase it would trade real
// state complexity for that bounded refetch). So these tests assert the CONTRACT —
// dual apply is data-idempotent (no duplicate row, no inflated count) and the WS
// plane still owns the floor — NOT once-only. `markSatisfied` stays WS-exclusive
// (the ACK writes NO transport water).
describe("applyCommittedMessage — shared committed seam, data-idempotent under dual apply (#task:1749)", () => {
  let markSatisfied: ReturnType<typeof vi.spyOn>;
  beforeEach(() => {
    markSatisfied = vi.spyOn(eventWsClient, "markSatisfied").mockImplementation(() => {});
    vi.spyOn(eventWsClient, "commitSnapshotBaseline").mockImplementation(() => {});
  });
  afterEach(() => vi.restoreAllMocks());

  const flush = async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  };

  // A message with an active threads reader drives one invalidate-only effect per
  // seam run: the blanket channel-threads-list invalidate. Counting its calls
  // DOCUMENTS the accepted duplicate — it runs once per plane — while the data
  // assertions prove that duplicate never corrupts the cache.
  const countThreadsInvalidations = (spy: ReturnType<typeof vi.spyOn>, channelId: string): number =>
    spy.mock.calls.filter(
      (c: unknown[]) =>
        JSON.stringify((c[0] as { queryKey?: unknown } | undefined)?.queryKey) ===
        JSON.stringify(channelThreadsKey(channelId)),
    ).length;

  const mainlineRowCount = (qc: QueryClient, channelId: string, id: string): number =>
    (
      qc
        .getQueryData<{ pages: { messages: { id: string }[] }[] }>(messagesKey(channelId))
        ?.pages.flatMap((p) => p.messages) ?? []
    ).filter((m) => m.id === id).length;

  it("ACK then WS: dual apply yields ONE row (no duplicate); the invalidate may repeat; WS advances the floor", async () => {
    const qc = new QueryClient();
    const unsubMain = mountMainlineReader(qc, "c1");
    const unsubThreads = mountReader(qc, channelThreadsKey("c1"));
    const spy = vi.spyOn(qc, "invalidateQueries");
    const msg = message({ id: "m1", seq: 6 });

    // ACK plane applies first, then the SAME committed message arrives on the wire.
    await applyCommittedMessage(qc, "c1", msg, undefined);
    ingestConversationFrame(qc, messageFrameSeq("c1", msg, 6));
    await flush();

    // DATA IDEMPOTENT: the content append short-circuits on `cacheHasMessage`, so
    // the row exists exactly once despite two authoritative applies.
    expect(mainlineRowCount(qc, "c1", "m1")).toBe(1);
    // The invalidate-only effect is NOT deduped — it runs once per plane. That
    // duplicate is the accepted best-effort cost; the row assertion above proves it
    // never translates into corrupted data (the whole reason a receipt was cut).
    expect(countThreadsInvalidations(spy, "c1")).toBe(2);
    // markSatisfied is WS-exclusive and fires once (the ACK writes no transport
    // water), so the resume floor advances even though the ACK applied first.
    await vi.waitFor(() => expect(markSatisfied).toHaveBeenCalledTimes(1));
    expect(markSatisfied).toHaveBeenCalledWith(conversationScope("c1"), 6, "epoch-1");
    unsubThreads();
    unsubMain();
  });

  it("WS then ACK: order-independent — still ONE row", async () => {
    const qc = new QueryClient();
    const unsubMain = mountMainlineReader(qc, "c1");
    const msg = message({ id: "m1", seq: 6 });

    ingestConversationFrame(qc, messageFrameSeq("c1", msg, 6));
    await flush();
    // ACK replay of the same committed message re-converges the same by-id write.
    await applyCommittedMessage(qc, "c1", msg, undefined);

    expect(mainlineRowCount(qc, "c1", "m1")).toBe(1);
    unsubMain();
  });

  it("patched reply applied twice keeps the reply count ABSOLUTE (1, not incremented to 2)", async () => {
    const qc = new QueryClient();
    const unsub = mountMainlineReader(qc, "c1");
    qc.setQueryData(messagesKey("c1"), {
      pages: [
        {
          messages: [message({ id: "root1", seq: 1 })],
          reply_counts_by_root: { root1: 0 },
          reply_previews_by_root: {},
          has_more_before: false,
          has_more_after: false,
          stream_head_seq: 5,
        },
      ],
      pageParams: [undefined],
    });
    const reply = message({ id: "reply1", seq: 2, thread_root_id: "root1" });
    const patch = { thread_root_reply_count: { root_id: "root1", reply_count: 1 } };

    // Both planes carry the SAME absolute count patch; a second apply must SET, not
    // increment.
    await applyCommittedMessage(qc, "c1", reply, patch);
    await applyCommittedMessage(qc, "c1", reply, patch);
    await flush();

    const count = qc.getQueryData<{
      pages: { reply_counts_by_root: Record<string, number> }[];
    }>(messagesKey("c1"))?.pages[0]?.reply_counts_by_root.root1;
    expect(count).toBe(1);
    unsub();
  });

  it("atomic visibility: count + preview tail converge in the SAME tick, so a React observer never renders a tail without its count (Norman/Cindy #1749 falsifier)", async () => {
    const qc = new QueryClient();
    // Seed a tail window that PROJECTS previews (the object is present) with root1
    // at count 0 and no tail row yet — the exact 0→1 first-reply transition
    // Norman's headerless-block coupling is about.
    qc.setQueryData(messagesKey("c1"), {
      pages: [
        {
          messages: [message({ id: "root1", seq: 1 })],
          reply_counts_by_root: { root1: 0 },
          reply_previews_by_root: {},
          has_more_before: false,
          has_more_after: false,
          stream_head_seq: 5,
        },
      ],
      pageParams: [undefined],
    });

    type Snap = { count: number | undefined; tailLen: number };
    const readSnap = (): Snap => {
      const page = qc.getQueryData<{
        pages: {
          reply_counts_by_root: Record<string, number>;
          reply_previews_by_root?: Record<string, unknown[]>;
        }[];
      }>(messagesKey("c1"))?.pages[0];
      return {
        count: page?.reply_counts_by_root.root1,
        tailLen: page?.reply_previews_by_root?.root1?.length ?? 0,
      };
    };

    // Model a React consumer EXACTLY: `useBaseQuery` subscribes with
    // `notifyManager.batchCalls(onStoreChange)`, so re-renders are coalesced
    // through the SAME notifyManager the seam's `batch` holds open, and React reads
    // the snapshot at flush time — NOT once per synchronous write (a raw observer
    // listener fires per write and would model the wrong layer). Capturing the
    // cache on each coalesced fire is the render sequence a component actually
    // sees. This observer also makes `hasActiveMainlineReader` true so the count
    // and tail merges run.
    const rendered: Snap[] = [];
    const observer = new QueryObserver(qc, {
      queryKey: messagesKey("c1"),
      queryFn: () => new Promise<never>(() => {}),
      enabled: false,
    });
    const unsub = observer.subscribe(notifyManager.batchCalls(() => rendered.push(readSnap())));

    const settled = applyCommittedMessage(
      qc,
      "c1",
      message({ id: "reply1", seq: 2, thread_root_id: "root1" }),
      { thread_root_reply_count: { root_id: "root1", reply_count: 1 } },
    );

    // SAME-TICK co-presence — the strongest falsifier: the count merge (registry)
    // and the tail seed (mergeMainlineReplyPreview) are BOTH synchronous writes
    // inside the seam's batch, so the instant the seam has run (before any await)
    // the cache already holds both. If the count were recovered by an async refetch
    // (the pre-#1749 headerless bug), tail would be present here with count still 0.
    expect(readSnap()).toEqual({ count: 1, tailLen: 1 });

    await settled;
    await new Promise((resolve) => setTimeout(resolve, 0)); // let the coalesced flush run

    // The render sequence a React consumer sees never shows a tail without its
    // matching count — the headerless-block state is structurally absent.
    expect(rendered.every((f) => !(f.tailLen > 0 && (f.count ?? 0) < 1))).toBe(true);
    expect(rendered.at(-1)).toMatchObject({ count: 1, tailLen: 1 });
    unsub();
  });
});
