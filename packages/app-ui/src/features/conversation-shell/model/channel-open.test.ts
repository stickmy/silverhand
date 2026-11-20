import { describe, expect, it } from "vitest";
import { resolveChannelOpen } from "./channel-open";
import type { MessageUnreadPointerDto } from "@silverhand/shared";
import { decodeMessageSeq } from "@silverhand/shared";
import type { Panes } from "@/navigation/panes";

function pointer(
  overrides: Partial<Omit<MessageUnreadPointerDto, "seq">> & { seq?: number } = {},
): MessageUnreadPointerDto {
  const { seq, ...rest } = overrides;
  return {
    conversation_id: "c-a",
    message_id: "m-1",
    seq: decodeMessageSeq(seq ?? 100),
    thread_root_id: "root-1",
    ...rest,
  };
}

describe("resolveChannelOpen — unread-target > pane memory > main stream", () => {
  const panes: Panes = [{ kind: "threads", of: "c-1", thread: null }];

  it("surfaces the unread thread when the pointer targets a thread and no watermark", () => {
    expect(resolveChannelOpen(pointer(), null, panes)).toEqual({
      kind: "unread-thread",
      threadRootId: "root-1",
      messageId: "m-1",
    });
  });

  it("surfaces the unread thread when the pointer seq is newer than the watermark", () => {
    expect(resolveChannelOpen(pointer({ seq: 101 }), decodeMessageSeq(100), panes)).toEqual({
      kind: "unread-thread",
      threadRootId: "root-1",
      messageId: "m-1",
    });
  });

  it("falls back to pane memory when the pointer seq is at the watermark (just-closed batch)", () => {
    expect(resolveChannelOpen(pointer({ seq: 100 }), decodeMessageSeq(100), panes)).toEqual({
      kind: "panes",
      panes,
    });
  });

  it("falls back to pane memory when the pointer seq is below the watermark", () => {
    expect(resolveChannelOpen(pointer({ seq: 50 }), decodeMessageSeq(100), panes)).toEqual({
      kind: "panes",
      panes,
    });
  });

  it("falls back to pane memory for a main-line unread (no thread_root_id)", () => {
    expect(resolveChannelOpen(pointer({ thread_root_id: null }), null, panes)).toEqual({
      kind: "panes",
      panes,
    });
  });

  it("falls back to pane memory when there is no unread pointer", () => {
    expect(resolveChannelOpen(undefined, null, panes)).toEqual({
      kind: "panes",
      panes,
    });
  });

  it("passes through null panes (main stream) when nothing else applies", () => {
    expect(resolveChannelOpen(undefined, null, null)).toEqual({
      kind: "panes",
      panes: null,
    });
  });
});
