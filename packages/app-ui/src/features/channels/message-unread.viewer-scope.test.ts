import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryObserver } from "@tanstack/react-query";
import {
  decodeTransportEpoch,
  decodeTransportSeq,
  decodeMessageSeq,
  type ServerFrame,
  type ConversationStreamUnreadDto,
} from "@silverhand/shared";
import { api } from "@/lib/api";
import { conversationUnreadKey, useConversationUnread } from "./message-unread";
import { channelThreadsKey, channelThreadsUnreadKey } from "./use-channel-threads";

// #task:1773: the header dot and the `followed ∩ unread` Threads list are two
// independent reads over one viewer-scoped signal. A viewer `unread_updated`
// or `snapshot_required` must refetch the filtered list alongside the unread
// read-model, and never the All list, whose membership and order are unread-
// and follow-agnostic.

// Run the hook's production effect and query options with real query observers;
// only the React lifecycle, event source and HTTP boundary are supplied here.
const harness = vi.hoisted(() => ({
  client: null as unknown as QueryClient,
  listeners: new Set<(frame: ServerFrame) => void>(),
  cleanup: [] as (() => void)[],
  releases: vi.fn(),
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useEffect: (effect: () => (() => void) | void) => {
    const cleanup = effect();
    if (cleanup) harness.cleanup.push(cleanup);
  },
}));
vi.mock("@/lib/api", async (original) => ({
  ...(await original<typeof import("@/lib/api")>()),
  api: vi.fn(),
}));
vi.mock("@/lib/ws-client", async (original) => ({
  ...(await original<typeof import("@/lib/ws-client")>()),
  subscribeViewerScope: () => ({ unsubscribe: harness.releases }),
  isCurrentViewerScope: (scope: string) => scope === "viewer:human",
  onWsFrame: (listener: (frame: ServerFrame) => void) => {
    harness.listeners.add(listener);
    return () => harness.listeners.delete(listener);
  },
}));
vi.mock("@tanstack/react-query", async (original) => ({
  ...(await original<typeof import("@tanstack/react-query")>()),
  useQueryClient: () => harness.client,
  useQuery: (options: ConstructorParameters<typeof QueryObserver>[1]) =>
    new QueryObserver(harness.client, options),
}));
const snapshot = (count: number): ConversationStreamUnreadDto => ({
  mainline: { unread_count: 0, first_unread_message_id: null, first_unread_seq: null },
  threads: count
    ? {
        root: {
          unread_count: count,
          first_unread_message_id: "reply",
          first_unread_seq: decodeMessageSeq(7),
        },
      }
    : {},
  followed_thread_unread: false,
});
function emit(conversationId: string, kind = "direct_pairs_changed", scope = "viewer:human"): void {
  const frame: ServerFrame = {
    type: "event",
    kind,
    scope,
    epoch: decodeTransportEpoch("e"),
    seq: decodeTransportSeq(1),
    payload: { conversation_id: conversationId },
  };
  harness.listeners.forEach((listener) => listener(frame));
}
beforeEach(() => {
  vi.clearAllMocks();
  harness.client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
});
afterEach(() => {
  harness.cleanup.splice(0).forEach((cleanup) => cleanup());
  harness.listeners.clear();
  harness.client.clear();
});
function observe(id: string): QueryObserver<ConversationStreamUnreadDto> {
  const observer = useConversationUnread(
    id,
  ) as unknown as QueryObserver<ConversationStreamUnreadDto>;
  harness.cleanup.push(observer.subscribe(() => {}));
  return observer;
}
describe("DM stream unread consumes viewer invalidation through the production hook", () => {
  it("rereads only the matching DM, including repeat events and unsubscribe", async () => {
    let current = snapshot(0);
    vi.mocked(api).mockImplementation(async (path) =>
      path === "/api/conversations/dm-a/unread" ? current : snapshot(0),
    );
    const a = observe("dm-a"),
      b = observe("dm-b"),
      channel = observe("channel");
    await Promise.all([a.refetch(), b.refetch(), channel.refetch()]);
    vi.mocked(api).mockClear();
    const childKey = [...conversationUnreadKey("dm-a"), "unrelated"];
    harness.client.setQueryData(childKey, "kept");
    current = snapshot(1);
    emit("dm-a", "direct_pairs_changed", "viewer:other");
    emit("dm-a", "message", "conversation:dm-a");
    expect(api).not.toHaveBeenCalled();
    expect(a.getCurrentResult().data?.threads).toEqual({});
    emit("dm-a");
    await vi.waitFor(() => expect(a.getCurrentResult().data?.threads.root?.unread_count).toBe(1));
    expect(vi.mocked(api).mock.calls.map(([path]) => path)).toEqual([
      "/api/conversations/dm-a/unread",
    ]);
    expect(harness.client.getQueryState(childKey)?.isInvalidated).toBe(false);
    expect(b.getCurrentResult().data?.threads).toEqual({});
    expect(channel.getCurrentResult().data?.threads).toEqual({});
    emit("dm-a");
    await vi.waitFor(() => expect(a.getCurrentResult().fetchStatus).toBe("idle"));
    expect(a.getCurrentResult().data).toEqual(snapshot(1));
    harness.cleanup.splice(0).forEach((cleanup) => cleanup());
    vi.mocked(api).mockClear();
    emit("dm-a");
    expect(api).not.toHaveBeenCalled();
    expect(harness.releases).toHaveBeenCalledTimes(3);
  });
  it("retains workspace unread and viewer snapshot convergence without refreshing All threads", async () => {
    let current = snapshot(0);
    vi.mocked(api).mockImplementation(async () => current);
    const observer = observe("channel");
    await observer.refetch();
    harness.client.setQueryData(channelThreadsKey("channel"), []);
    harness.client.setQueryData(channelThreadsUnreadKey("channel"), []);
    current = snapshot(2);
    emit("channel", "unread_updated");
    await vi.waitFor(() =>
      expect(observer.getCurrentResult().data?.threads.root?.unread_count).toBe(2),
    );
    expect(harness.client.getQueryState(channelThreadsUnreadKey("channel"))?.isInvalidated).toBe(
      true,
    );
    expect(harness.client.getQueryState(channelThreadsKey("channel"))?.isInvalidated).toBe(false);
    harness.client.setQueryData(channelThreadsKey("channel"), []);
    harness.client.setQueryData(channelThreadsUnreadKey("channel"), []);
    current = snapshot(0);
    harness.listeners.forEach((listener) =>
      listener({
        type: "snapshot_required",
        scope: "viewer:human",
        reason: "gap_too_large",
        epoch: decodeTransportEpoch("e"),
        currentSeq: decodeTransportSeq(2),
      }),
    );
    await vi.waitFor(() => expect(observer.getCurrentResult().data?.threads).toEqual({}));
    expect(harness.client.getQueryData(conversationUnreadKey("channel"))).toEqual(snapshot(0));
    expect(harness.client.getQueryState(channelThreadsUnreadKey("channel"))?.isInvalidated).toBe(
      true,
    );
    expect(harness.client.getQueryState(channelThreadsKey("channel"))?.isInvalidated).toBe(false);
  });
});
