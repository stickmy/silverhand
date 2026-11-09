import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

/**
 * Symbols/endpoints that mutate viewer read state. None may appear on the
 * realtime ingestion bridge. `/api/notifications/read` =
 * `useMarkConversationNotificationsRead`; `conversation_reads` cursor =
 * `useMarkMessageChannelRead` / `useMarkThreadRead` (via `observe` /
 * `streamSourceAtSeq`).
 */
const READ_STATE_WRITERS = [
  "/api/notifications/read",
  "markConversationNotificationsRead",
  "markMessageChannelRead",
  "markThreadRead",
  "conversation_reads",
  "streamSourceAtSeq",
];

function sliceFunctionBody(source: string, declaration: string): string {
  const start = source.indexOf(declaration);
  if (start === -1) {
    throw new Error(`could not find "${declaration}" — was it renamed?`);
  }
  // The function ends where the next top-level function declaration begins.
  const rest = source.slice(start + declaration.length);
  const nextDecl = rest.search(/\n(?:export )?function /);
  return nextDecl === -1 ? rest : rest.slice(0, nextDecl);
}

// The channel data hooks are split by concern; these checks read them as one.
function channelHooksSource(): string {
  return ["message-window", "thread-follow", "agent-runtime", "mark-read"]
    .map((name) =>
      readFileSync(fileURLToPath(new URL(`../channels/${name}.ts`, import.meta.url)), "utf8"),
    )
    .join("\n");
}

describe("replayed-frame cache merge — idempotent (#task:1251)", () => {
  const source = readFileSync(
    fileURLToPath(new URL("../channels/message-cache-core.ts", import.meta.url)),
    "utf8",
  );

  it("appendMessageToCache de-dupes by message id (replay-overlap safe)", () => {
    const core = sliceFunctionBody(source, "function appendToCache(");
    expect(core).toContain("cacheHasMessage(data, msg.id)");
    for (const helper of ["appendMessageToCache", "appendPendingMessage"]) {
      const body = sliceFunctionBody(source, `function ${helper}(`);
      expect(body).toContain("appendToCache(");
    }
  });
});

describe("central conversation ingestor — read-inert (#task:1255)", () => {
  const source = readFileSync(
    fileURLToPath(new URL("./conversation-ingestor.ts", import.meta.url)),
    "utf8",
  );

  it("is the App-level frame consumer (sanity)", () => {
    // If the ingestor stops consuming frames, the read-inert assertion below is
    // fencing a dead module rather than the live choke point.
    expect(source).toContain("onWsFrame");
  });

  it("never writes read state on frame arrival", () => {
    for (const writer of READ_STATE_WRITERS) {
      expect(source).not.toContain(writer);
    }
  });
});

describe("useChannelMessages — pure reader / gate-(b) fence (#task:1255)", () => {
  const source = channelHooksSource();
  const body = sliceFunctionBody(source, "export function useChannelMessages(");

  it("registers no frame consumer (no onWsFrame / useConversationStream)", () => {
    // Fence the CALL form, not the word — the hook's own comments explain that
    // the ingestor is "the sole onWsFrame consumer", and that prose must not
    // trip the guard.
    expect(body).not.toContain("onWsFrame(");
    expect(body).not.toContain("useConversationStream(");
  });

  it("calls no main-stream cache mutator (writes moved to the ingestor)", () => {
    for (const mutator of [
      "appendMessageToCache(",
      "bumpReplyCountInCache(",
      "applyTaskChangedToCache(",
      "applyReactionChangedToChannelCaches(",
    ]) {
      expect(body).not.toContain(mutator);
    }
  });

  it("declares scope interest via the registry + reads the gap fact (positive)", () => {
    expect(body).toContain("useInterest(");
    expect(body).not.toContain("subscribeWsScope(");
    expect(body).toContain("useMainlineLiveGap");
  });

  it("clears the LEAVING channel gap on switch/unmount (#1255 blocker-2)", () => {
    // Per-channel cache key (vs the old hook-local useState) means a stale
    // `true` survives a channel switch unless the effect cleanup clears the
    // channel being left — otherwise switching A→B→A flashes a bogus
    // "jump to latest" on the first render back.
    expect(body).toContain("return () => setMainlineLiveGap(qc, channelId, false)");
  });
});

describe("channels/hooks.ts — registry-migrated hooks / body-scoped deny (#task:1260 P2)", () => {
  const source = channelHooksSource();
  const MIGRATED = [
    "export function useThreadRoot(",
    "export function useThreadFollowers(",
    "export function useThreadCluster(",
  ];
  const FORBIDDEN = [
    "onWsFrame(",
    "useConversationStream(",
    "appendMessageToCache(",
    "bumpReplyCountInCache(",
    "applyTaskChangedToCache(",
    "applyReactionChangedToChannelCaches(",
    "applyReactionChangedToThreadRootCache(",
  ];

  for (const decl of MIGRATED) {
    const name = decl.replace("export function ", "").replace("(", "");
    const body = sliceFunctionBody(source, decl);

    it(`${name} registers no frame consumer / cache mutator (gate-(b) preserved)`, () => {
      for (const sym of FORBIDDEN) expect(body).not.toContain(sym);
    });

    it(`${name} declares scope interest via the registry, not a direct subscribe (body-scoped deny)`, () => {
      // Call-form deny (prose naming subscribeWsScope must not trip it) on THIS
      // hook's body only.
      expect(body).toContain("useInterest(");
      expect(body).not.toContain("subscribeWsScope(");
    });
  }

  it("hooks.ts is fully migrated: no conversation reader calls subscribeWsScope directly", () => {
    expect(source).not.toContain("subscribeWsScope(");
  });
});

describe("mark-read mutations — global badges ride the WS clear patch, no whole-list refetch (#task:1284)", () => {
  const source = channelHooksSource();

  it("useMarkMessageChannelRead does not invalidate message-unread, settles per-stream via the seam", () => {
    const body = sliceFunctionBody(source, "export function useMarkMessageChannelRead(");
    expect(body).not.toContain("invalidateMessageUnreadQueries(");
    expect(body).toContain("settleStreamUnreadAfterObserve(");
    expect(body).not.toContain("conversationUnreadKey(");
  });

  it("useMarkThreadRead drops message-unread invalidate, settles per-stream via the seam", () => {
    const body = sliceFunctionBody(source, "export function useMarkThreadRead(");
    expect(body).not.toContain("invalidateMessageUnreadQueries(");
    expect(body).toContain("settleStreamUnreadAfterObserve(");
    expect(body).not.toContain("conversationUnreadKey(");
    // channelThreadsKey is a DIFFERENT read-model (per-thread metadata) that
    // still legitimately invalidates.
    expect(body).toContain("channelThreadsKey(");
  });
});

describe("Activity mark-read callers — settle per-stream via the seam, no single-point patch (#task:1402)", () => {
  const source = readFileSync(
    fileURLToPath(new URL("../attention/hooks.ts", import.meta.url)),
    "utf8",
  );

  for (const decl of [
    "export function useClearActivityItem(",
    "export function useMarkActivityAllRead(",
  ]) {
    const name = decl.replace("export function ", "").replace("(", "");
    const body = sliceFunctionBody(source, decl);

    it(`${name} settles per-stream conversationUnread through the single seam`, () => {
      expect(body).toContain("settleStreamUnreadAfterObserve(");
    });

    it(`${name} keeps no single-point patch (no bare conversationUnreadKey / global refetch)`, () => {
      // The whole point of the seam: no caller hand-writes the per-stream
      // invalidate, and none re-introduces the #task:1284 global whole-list
      // refetch (that rides the enriched unread_updated ingestor patch).
      expect(body).not.toContain("conversationUnreadKey(");
      expect(body).not.toContain("invalidateMessageUnreadQueries(");
    });
  }
});

describe("useAgentRunStates — pure reader / gate-(b) fence (#task:1255 P1c)", () => {
  const source = channelHooksSource();
  const body = sliceFunctionBody(source, "export function useAgentRunStates(");

  it("registers no frame consumer (no onWsFrame / useConversationStream)", () => {
    expect(body).not.toContain("onWsFrame(");
    expect(body).not.toContain("useConversationStream(");
  });

  it("declares scope interest via the registry, not a direct subscribe (#task:1260 P2 slice 8)", () => {
    // Last conversation reader in hooks.ts migrated. Call-form deny (prose naming
    // subscribeWsScope must not trip it), body-scoped. snapshot_required /
    // agent_activity buffer logic lives in the ingestor, not this hook — untouched.
    expect(body).toContain("useInterest(");
    expect(body).not.toContain("subscribeWsScope(");
  });
});

describe("useChannelThreads — pure reader / gate-(b) fence (#task:1255 P1c)", () => {
  const source = readFileSync(
    fileURLToPath(new URL("../channels/use-channel-threads.ts", import.meta.url)),
    "utf8",
  );
  const body = sliceFunctionBody(source, "export function useChannelThreads(");

  it("registers no frame consumer (no onWsFrame / useConversationStream)", () => {
    expect(body).not.toContain("onWsFrame(");
    expect(body).not.toContain("useConversationStream(");
  });

  it("declares scope interest via the registry, not a direct subscribe (#task:1260 P2)", () => {
    expect(body).toContain("useInterest(");
    expect(body).not.toContain("subscribeWsScope(");
  });
});

describe("channel membership/runtime readers — gate-(b) fence (#task:2026)", () => {
  const source = channelHooksSource();
  const membershipBody = sliceFunctionBody(source, "export function useChannelAgents(");
  const runtimeBody = sliceFunctionBody(source, "export function useChannelAgentRuntime(");

  it("keeps the membership reader free of runtime interest and buffers", () => {
    expect(membershipBody).not.toContain("onWsFrame(");
    expect(membershipBody).not.toContain("useConversationStream(");
    expect(membershipBody).not.toContain("useInterest(");
    expect(membershipBody).not.toContain("AgentStatus");
  });

  it("lets only the runtime reader own scope interest and drain the status buffer", () => {
    expect(runtimeBody).toContain("useInterest(");
    expect(runtimeBody).not.toContain("subscribeWsScope(");
    expect(runtimeBody).toContain("mergePendingAgentStatusPatches(");
    // Per-channel buffer is refcounted — released on leave, cleared only when
    // the last reader for the channel goes (multi-reader safe).
    expect(runtimeBody).toContain("retainAgentStatusBuffer(");
    expect(runtimeBody).toContain("releaseAgentStatusBuffer(");
  });
});

describe("ChannelView — read-cursor cache-driven / gate-(b) (#task:1255 P1c)", () => {
  const source = readFileSync(
    fileURLToPath(new URL("../conversation-shell/components/channel-view.tsx", import.meta.url)),
    "utf8",
  );

  it("registers no frame consumer (no onWsFrame / useConversationStream)", () => {
    expect(source).not.toContain("onWsFrame(");
    expect(source).not.toContain("useConversationStream(");
  });

  it("still marks read on the read axis (useMarkMessageChannelRead)", () => {
    expect(source).toContain("useMarkMessageChannelRead");
  });

  it("marks read off the mainline tail, not frame arrival (cache-driven)", () => {
    // The decision is the pure `nextTailReadMark` helper (unit-tested in
    // features/conversation-shell/model/channel-read.test.ts), and the old arrival-driven handler is gone.
    expect(source).toContain("nextTailReadMark(");
    expect(source).not.toContain("onStreamEvent");
  });
});

describe("AgentActivityTimeline — pure reader / gate-(b) (#task:1255 P1c #2)", () => {
  const source = readFileSync(
    fileURLToPath(new URL("../agents/components/agent-activity-timeline.tsx", import.meta.url)),
    "utf8",
  );

  it("registers no frame consumer (no onWsFrame / useConversationStream)", () => {
    expect(source).not.toContain("onWsFrame(");
    expect(source).not.toContain("useConversationStream(");
  });

  it("reads the feed via the cache hook (positive)", () => {
    expect(source).toContain("useAgentActivityFeed(");
  });
});

describe("useAgentActivityFeed — read-inert content cache (#task:1255 P1c #2)", () => {
  const source = readFileSync(
    fileURLToPath(new URL("../agents/hooks/use-agent-activity-feed.ts", import.meta.url)),
    "utf8",
  );

  it("never writes read state (content cache only)", () => {
    for (const writer of READ_STATE_WRITERS) {
      expect(source).not.toContain(writer);
    }
  });

  it("declares scope interest via the registry + does not bootstrap on append (positive)", () => {
    expect(source).toContain("useInterest(");
    expect(source).not.toContain("subscribeWsScope(");
    // The live-append helper leaves an unloaded feed alone (no reader / no
    // snapshot behind it) — `prev ? … : prev`.
    const body = sliceFunctionBody(source, "export function appendAgentActivityToCache(");
    expect(body).toContain("prev ?");
  });
});

describe("conversation-scope subscribe whitelist (#task:1255 P2 step-3)", () => {
  const hooksSrc = channelHooksSource();

  // Side consumers living in hooks.ts (sliced bodies — the file holds many hooks).
  for (const decl of [
    "export function useChannelAgentRuntime(",
    "export function useAgentRunStates(",
  ]) {
    const name = decl.replace("export function ", "").replace("(", "");
    const body = sliceFunctionBody(hooksSrc, decl);
    it(`${name} subscribes the SIDE scope`, () => {
      expect(body).toContain("wsConversationSideScope(");
      expect(body).not.toContain("wsConversationScope(");
    });
  }

  // Message-stream consumers living in hooks.ts stay on the message scope.
  for (const decl of [
    "export function useChannelMessages(",
    "export function useThreadCluster(",
    "export function useThreadRoot(",
    "export function useThreadFollowers(",
  ]) {
    const name = decl.replace("export function ", "").replace("(", "");
    const body = sliceFunctionBody(hooksSrc, decl);
    it(`${name} stays on the MESSAGE-STREAM scope`, () => {
      expect(body).toContain("wsConversationScope(");
      expect(body).not.toContain("wsConversationSideScope(");
    });
  }

  it("useAgentActivityFeed subscribes the SIDE scope", () => {
    const src = readFileSync(
      fileURLToPath(new URL("../agents/hooks/use-agent-activity-feed.ts", import.meta.url)),
      "utf8",
    );
    expect(src).toContain("wsConversationSideScope(");
    expect(src).not.toContain("wsConversationScope(");
  });

  it("useConversationMembers declares SIDE-scope interest via the registry + keeps its own onWsFrame", () => {
    const src = readFileSync(
      fileURLToPath(new URL("../conversations/use-conversation-members.ts", import.meta.url)),
      "utf8",
    );
    expect(src).toContain("wsConversationSideScope(");
    expect(src).not.toContain("wsConversationScope(");
    expect(src).toContain("useInterest(");
    expect(src).not.toContain("subscribeWsScope(");
    expect(src).toContain("onWsFrame(");
  });

  it("useChannelThreads stays on the MESSAGE-STREAM scope", () => {
    const src = readFileSync(
      fileURLToPath(new URL("../channels/use-channel-threads.ts", import.meta.url)),
      "utf8",
    );
    expect(src).toContain("wsConversationScope(");
    expect(src).not.toContain("wsConversationSideScope(");
  });
});
