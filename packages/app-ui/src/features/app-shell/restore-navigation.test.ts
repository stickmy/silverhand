import { describe, expect, it } from "vitest";
import { decodeMessageSeq } from "@silverhand/shared";
import type { MessageUnreadPointerDto } from "@silverhand/shared";
import type { Panes } from "@/navigation/panes";
import { resolveRestoreNavigation, type RestoreNavigationInputs } from "./restore-navigation";

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

function inputs(overrides: Partial<RestoreNavigationInputs> = {}): RestoreNavigationInputs {
  return {
    workspaceId: "w-1",
    selectedChannelId: null,
    allChannels: [{ id: "c-1" }, { id: "c-2" }, { id: "c-3" }],
    activeTopLevel: [{ id: "c-1" }, { id: "c-2" }, { id: "c-3" }],
    jumpTargetChannelId: null,
    excludeId: null,
    rememberedChannelId: null,
    rememberedUnreadPointer: undefined,
    rememberedCloseWatermark: null,
    rememberedPanes: null,
    ...overrides,
  };
}

describe("resolveRestoreNavigation — none guards", () => {
  it("none when no workspace", () => {
    expect(resolveRestoreNavigation(inputs({ workspaceId: null }))).toEqual({
      kind: "none",
    });
  });

  it("none when a channel is already selected", () => {
    expect(resolveRestoreNavigation(inputs({ selectedChannelId: "c-2" }))).toEqual({
      kind: "none",
    });
  });

  it("none when the channel list is still empty (racing the placeholder)", () => {
    expect(resolveRestoreNavigation(inputs({ allChannels: [], activeTopLevel: [] }))).toEqual({
      kind: "none",
    });
  });

  it("none when nothing to jump/remember and no active fallback exists", () => {
    expect(
      resolveRestoreNavigation(inputs({ allChannels: [{ id: "c-1" }], activeTopLevel: [] })),
    ).toEqual({ kind: "none" });
  });
});

describe("resolveRestoreNavigation — honored jump wins", () => {
  it("opens the jump target when it exists in the workspace", () => {
    expect(resolveRestoreNavigation(inputs({ jumpTargetChannelId: "c-2" }))).toEqual({
      kind: "open-channel",
      channelId: "c-2",
      reason: "jump",
    });
  });

  it("jump beats a valid remembered channel", () => {
    expect(
      resolveRestoreNavigation(inputs({ jumpTargetChannelId: "c-3", rememberedChannelId: "c-2" })),
    ).toEqual({ kind: "open-channel", channelId: "c-3", reason: "jump" });
  });

  it("a jump target that no longer exists is ignored (falls through)", () => {
    expect(
      resolveRestoreNavigation(inputs({ jumpTargetChannelId: "gone", rememberedChannelId: "c-2" })),
    ).toEqual({ kind: "open-remembered", channelId: "c-2", panes: null });
  });

  it("an EXPLICIT jump may still land on an archived channel (#task:1413)", () => {
    // Unlike the auto entries, an explicit jump (Saved / search / deep-link) may
    // reach an archived channel — it uses the full `allChannels` union, not the
    // active gate. `c-arch` is archived (union only), yet the jump is honored.
    expect(
      resolveRestoreNavigation(
        inputs({
          jumpTargetChannelId: "c-arch",
          allChannels: [{ id: "c-1" }, { id: "c-arch" }],
          activeTopLevel: [{ id: "c-1" }],
        }),
      ),
    ).toEqual({ kind: "open-channel", channelId: "c-arch", reason: "jump" });
  });
});

describe("resolveRestoreNavigation — remembered channel", () => {
  const panes: Panes = [{ kind: "threads", of: "c-2", thread: null }];

  it("restores the remembered panes when no fresh unread", () => {
    expect(
      resolveRestoreNavigation(inputs({ rememberedChannelId: "c-2", rememberedPanes: panes })),
    ).toEqual({ kind: "open-remembered", channelId: "c-2", panes });
  });

  it("surfaces a fresh unread thread over the remembered panes", () => {
    expect(
      resolveRestoreNavigation(
        inputs({
          rememberedChannelId: "c-2",
          rememberedPanes: panes,
          rememberedUnreadPointer: pointer({ seq: 101 }),
          rememberedCloseWatermark: decodeMessageSeq(100),
        }),
      ),
    ).toEqual({
      kind: "jump-unread",
      channelId: "c-2",
      messageId: "m-1",
      threadRootId: "root-1",
    });
  });

  it("a just-closed unread (seq at watermark) degrades to pane memory", () => {
    expect(
      resolveRestoreNavigation(
        inputs({
          rememberedChannelId: "c-2",
          rememberedPanes: panes,
          rememberedUnreadPointer: pointer({ seq: 100 }),
          rememberedCloseWatermark: decodeMessageSeq(100),
        }),
      ),
    ).toEqual({ kind: "open-remembered", channelId: "c-2", panes });
  });

  it("skips the remembered channel when it is the just-archived primary", () => {
    expect(
      resolveRestoreNavigation(inputs({ rememberedChannelId: "c-2", excludeId: "c-2" })),
    ).toEqual({ kind: "open-channel", channelId: "c-1", reason: "fallback" });
  });

  it("skips a remembered channel that no longer exists", () => {
    expect(resolveRestoreNavigation(inputs({ rememberedChannelId: "gone" }))).toEqual({
      kind: "open-channel",
      channelId: "c-1",
      reason: "fallback",
    });
  });

  it("skips a remembered channel that is now ARCHIVED — auto-restore is active-only (#task:1413)", () => {
    // The channel is in the union (`allChannels`) but not in `activeTopLevel`,
    // i.e. it was archived. It must NOT be auto-restored; falls through to fallback.
    expect(
      resolveRestoreNavigation(
        inputs({
          rememberedChannelId: "c-arch",
          allChannels: [{ id: "c-1" }, { id: "c-2" }, { id: "c-arch" }],
          activeTopLevel: [{ id: "c-1" }, { id: "c-2" }],
        }),
      ),
    ).toEqual({ kind: "open-channel", channelId: "c-1", reason: "fallback" });
  });

  it("does not auto-open an archived remembered channel even with fresh unread (#task:1413)", () => {
    // Exactly shiki's report: a channel archived WHILE it had unread must not be
    // auto-opened on the next workspace switch — the archived+unread pair no
    // longer clears the active gate, so no jump-unread and no open-remembered.
    expect(
      resolveRestoreNavigation(
        inputs({
          rememberedChannelId: "c-arch",
          allChannels: [{ id: "c-1" }, { id: "c-arch" }],
          activeTopLevel: [{ id: "c-1" }],
          rememberedPanes: panes,
          rememberedUnreadPointer: pointer({ conversation_id: "c-arch", seq: 101 }),
          rememberedCloseWatermark: decodeMessageSeq(100),
        }),
      ),
    ).toEqual({ kind: "open-channel", channelId: "c-1", reason: "fallback" });
  });
});

describe("resolveRestoreNavigation — default fallback", () => {
  it("picks the first active top-level channel", () => {
    expect(resolveRestoreNavigation(inputs())).toEqual({
      kind: "open-channel",
      channelId: "c-1",
      reason: "fallback",
    });
  });

  it("excludes a just-archived primary from the fallback", () => {
    expect(resolveRestoreNavigation(inputs({ excludeId: "c-1" }))).toEqual({
      kind: "open-channel",
      channelId: "c-2",
      reason: "fallback",
    });
  });
});
