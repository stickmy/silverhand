import { describe, expect, it } from "vitest";
import type { ActivityItemDto } from "@silverhand/shared";
import { ObserveSourceSchema, decodeMessageSeq } from "@silverhand/shared";
import {
  activityItemSource,
  explicitStreamClearHead,
  mainlineStreamSourceHead,
  notificationSource,
  streamSourceAtSeq,
} from "./observe";

// The renderer constructs source_refs by hand, so these AUTO-path builders are
// the client edge of the §9.10 caller-intent contract. The tests lock two
// things: (1) every built source is wire-valid against the shared schema, and
// (2) the AUTO builders can NEVER emit thread + `stream_head` (the regression
// these typed builders exist to make unrepresentable). This is scoped to the
// AUTO builders ONLY — phase B's explicit `explicitStreamClearHead(conv, root|null)`
// will knowingly emit thread + `stream_head`; that is an explicit path, not a
// regression, so it gets its own builder and its own test.
describe("observe auto-path source builders", () => {
  it("builds a thread source as at_seq (never stream_head)", () => {
    const src = streamSourceAtSeq("conv-1", "root-9", decodeMessageSeq(42));
    expect(src).toEqual({
      kind: "stream",
      conversation_id: "conv-1",
      thread_root_id: "root-9",
      position: { kind: "at_seq", seq: 42 },
    });
    expect(() => ObserveSourceSchema.parse(src)).not.toThrow();
  });

  it("builds a channel-with-snapshot source as at_seq (root null)", () => {
    const src = streamSourceAtSeq("conv-1", null, decodeMessageSeq(7));
    expect(src.kind).toBe("stream");
    expect(src).toMatchObject({ thread_root_id: null, position: { kind: "at_seq", seq: 7 } });
    expect(() => ObserveSourceSchema.parse(src)).not.toThrow();
  });

  it("builds a mainline head source that is ALWAYS root-null + stream_head", () => {
    const src = mainlineStreamSourceHead("conv-1");
    expect(src).toEqual({
      kind: "stream",
      conversation_id: "conv-1",
      thread_root_id: null,
      position: { kind: "stream_head" },
    });
    // §9.10: the AUTO `stream_head` builder pairs `stream_head` with a null
    // thread_root_id — the only AUTO builder that emits stream_head structurally
    // cannot carry a thread root. (Phase B's explicit builder may; not this one.)
    if (src.kind !== "stream") throw new Error("expected a stream source");
    expect(src.thread_root_id).toBeNull();
    expect(() => ObserveSourceSchema.parse(src)).not.toThrow();
  });

  it("builds a directed notification source by id", () => {
    const src = notificationSource("notif-3");
    expect(src).toEqual({ kind: "notification", notification_id: "notif-3" });
    expect(() => ObserveSourceSchema.parse(src)).not.toThrow();
  });
});

// The EXPLICIT phase-B builder is the deliberate counterpart: an explicit user
// clear (hover-✓ / Mark all) MAY tail-swallow, so it CAN emit `stream_head` on a
// thread — exactly what the AUTO builders above forbid. The split is intent, not
// stream shape (§9.10); these tests lock that the explicit builder carries the
// thread root through and stays wire-valid.
describe("observe explicit-path source builder", () => {
  it("builds a thread stream clear to head (the AUTO builders cannot)", () => {
    const src = explicitStreamClearHead("conv-1", "root-9");
    expect(src).toEqual({
      kind: "stream",
      conversation_id: "conv-1",
      thread_root_id: "root-9",
      position: { kind: "stream_head" },
    });
    expect(() => ObserveSourceSchema.parse(src)).not.toThrow();
  });

  it("builds a mainline stream clear to head when root is null", () => {
    const src = explicitStreamClearHead("conv-1", null);
    expect(src).toMatchObject({
      kind: "stream",
      thread_root_id: null,
      position: { kind: "stream_head" },
    });
    expect(() => ObserveSourceSchema.parse(src)).not.toThrow();
  });
});

// `activityItemSource` is the single eliminate-route chokepoint. The load-bearing
// rule it must lock: the route is keyed on `kind` (the source_ref MECHANISM), NOT
// `salience` (the display tier) — these are different axes. The DM-unread case is
// the regression point: it is `kind:'unread'` + `salience:'directed'` and MUST
// still resolve to a stream clear, not a notification (or it silently drops from
// hover-✓ / Mark-all).
function activityItem(overrides: Partial<ActivityItemDto>): ActivityItemDto {
  return {
    id: "render-key",
    kind: "unread",
    salience: "ambient",
    location: { kind: "workspace_channel", workspace_id: "ws-1", title: "general" },
    conversation_id: "conv-1",
    thread_root_id: null,
    message_id: "msg-1",
    notification_id: null,
    thread_root_preview: null,
    preview: null,
    preview_sender: null,
    timestamp: 0,
    unread_count: 1,
    mention_count: 0,
    ...overrides,
  };
}

describe("activityItemSource (eliminate-route chokepoint)", () => {
  it("routes an agent_failed to its notification_id", () => {
    const src = activityItemSource(
      activityItem({
        kind: "agent_failed",
        salience: "directed",
        notification_id: "notif-9",
        message_id: null,
      }),
    );
    expect(src).toEqual({ kind: "notification", notification_id: "notif-9" });
  });

  it("routes a mainline unread to a stream-head clear", () => {
    const src = activityItemSource(activityItem({ kind: "unread", salience: "ambient" }));
    expect(src).toMatchObject({
      kind: "stream",
      conversation_id: "conv-1",
      thread_root_id: null,
      position: { kind: "stream_head" },
    });
  });

  it("routes a thread unread to a stream-head clear that carries the root", () => {
    const src = activityItemSource(
      activityItem({ kind: "unread", salience: "ambient", thread_root_id: "root-2" }),
    );
    expect(src).toMatchObject({
      kind: "stream",
      thread_root_id: "root-2",
      position: { kind: "stream_head" },
    });
  });

  it("REGRESSION: DM unread (kind:unread + salience:directed) still routes to a stream clear", () => {
    // The exact axis bug: salience is directed-grade, but the eliminate route is
    // the stream cursor (notification_id is null). Routing on salience would
    // return null here and silently drop the item from hover-✓ and Mark-all.
    const src = activityItemSource(
      activityItem({
        kind: "unread",
        salience: "directed",
        notification_id: null,
      }),
    );
    expect(src).not.toBeNull();
    expect(src).toMatchObject({
      kind: "stream",
      conversation_id: "conv-1",
      position: { kind: "stream_head" },
    });
  });

  it("returns null for a malformed notification-backed item with no id", () => {
    const src = activityItemSource(
      activityItem({
        kind: "agent_failed",
        salience: "directed",
        notification_id: null,
        message_id: null,
      }),
    );
    expect(src).toBeNull();
  });
});
