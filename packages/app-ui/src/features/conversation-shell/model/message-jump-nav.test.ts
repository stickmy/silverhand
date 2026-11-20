import { describe, expect, it } from "vitest";
import type { DirectPairAuthority } from "@/features/app-shell/chat-place-restore";
import type { JumpRoute } from "./message-jump-nav";
import { jumpBasePath, resolveJumpNav, resolveJumpRoute } from "./message-jump-nav";

const ready = (ids: string[]): DirectPairAuthority => ({
  status: "ready",
  conversationIds: new Set(ids),
});
const refetching = (ids: string[]): DirectPairAuthority => ({
  status: "refetching",
  conversationIds: new Set(ids),
});
const pending: DirectPairAuthority = { status: "pending" };
const failed: DirectPairAuthority = { status: "failed" };

describe("resolveJumpNav", () => {
  it("routes a target that carries its own workspace id straight to the workspace arm", () => {
    // A channel jump must never be gated on the pair list being ready — that
    // would make an unrelated query a precondition for every jump.
    // Prove it with the list UNANSWERED and mid-refetch: still workspace-resolve.
    for (const pairs of [pending, failed, ready([]), refetching([])]) {
      expect(
        resolveJumpNav({
          hasWorkspaceCoordinate: true,
          channelId: "chan",
          pairs,
        }),
      ).toBe("workspace-resolve");
    }
  });

  it("opens a workspace-less target that the answered list knows as a pair", () => {
    expect(
      resolveJumpNav({
        hasWorkspaceCoordinate: false,
        channelId: "pair-conv",
        pairs: ready(["pair-conv", "other-pair"]),
      }),
    ).toBe("to-direct");
  });

  it("sends a workspace-less NON-pair on an answered list to the workspace arm", () => {
    // Answered + miss = a normal channel target that arrived without its
    // workspace id; the pre-existing "use the selected workspace" path still
    // handles it. Only the unanswered case changes behavior.
    expect(
      resolveJumpNav({
        hasWorkspaceCoordinate: false,
        channelId: "some-channel",
        pairs: ready(["pair-conv"]),
      }),
    ).toBe("workspace-resolve");
  });

  it("fails closed on a workspace-less target while the pair list has not answered", () => {
    // Cold deep-link / first refetch with nothing cached / failed read: the list
    // is the sole authority that tells a DM from a channel, and it has not
    // spoken. Guessing the selected workspace is the wrong-place jump this seam
    // prevents. (A refetch that DOES hold rows is its own `refetching` state,
    // covered below.)
    for (const pairs of [pending, failed]) {
      expect(
        resolveJumpNav({
          hasWorkspaceCoordinate: false,
          channelId: "unknown-conv",
          pairs,
        }),
      ).toBe("fail-closed");
    }
  });

  it("opens a workspace-less HIT even mid-refetch — pair membership only grows", () => {
    // `['direct-pairs']` is viewer-global and every incoming DM invalidates it,
    // so a one-shot jump routinely lands mid-refetch. Making it wait (like
    // restore does) would silently swallow the click with no replay. A hit is
    // safe: the list never drops a pair the viewer already has, so a cached
    // membership can't be revoked by the read in flight.
    expect(
      resolveJumpNav({
        hasWorkspaceCoordinate: false,
        channelId: "pair-conv",
        pairs: refetching(["pair-conv", "other-pair"]),
      }),
    ).toBe("to-direct");
  });

  it("fails closed on a workspace-less MISS mid-refetch — could be a new pair", () => {
    // The other half of the asymmetry: a miss on a refetching list is exactly
    // the case a freshly minted pair looks like before the in-flight read brings
    // it back. Routing it to the workspace arm would build a wrong-place URL, so
    // it must fail closed even though the list has answered before.
    expect(
      resolveJumpNav({
        hasWorkspaceCoordinate: false,
        channelId: "maybe-new-pair",
        pairs: refetching(["other-pair"]),
      }),
    ).toBe("fail-closed");
  });

  it("never fails closed for a workspaced target, even on a failed pair read", () => {
    // The fail-closed branch is bound to the TARGET's missing coordinate, not to
    // the pair list's health — a failed pair read must not strand channel jumps.
    expect(
      resolveJumpNav({
        hasWorkspaceCoordinate: true,
        channelId: "chan",
        pairs: failed,
      }),
    ).not.toBe("fail-closed");
  });
});

// The second half of the seam: once a jump is ACCEPTED, which pane
// it opens and whether it carries the reply cluster. Also a behavior branch, and
// the bug it guards is asymmetry — the direct arm used to drop `threadRootId`
// while the channel arm carried it, so a DM attachment sourced from a thread
// opened the pair's main line and never located the message. Every assertion
// below is stated for BOTH kinds on purpose: a per-arm thread decision is the
// shape that drifts.
describe("resolveJumpRoute", () => {
  it("carries the reply cluster on BOTH pane kinds", () => {
    expect(
      resolveJumpRoute({
        decision: "to-direct",
        channelId: "pair-conv",
        navWorkspaceId: null,
        threadRootId: "root-1",
      }),
    ).toEqual({
      kind: "direct",
      conversationId: "pair-conv",
      threadRootId: "root-1",
    });
    expect(
      resolveJumpRoute({
        decision: "workspace-resolve",
        channelId: "chan",
        navWorkspaceId: "w-1",
        threadRootId: "root-1",
      }),
    ).toEqual({
      kind: "channel",
      workspaceId: "w-1",
      channelId: "chan",
      threadRootId: "root-1",
    });
  });

  it("normalizes a main-line target to a null cluster on BOTH pane kinds", () => {
    // `undefined` (no cluster named) and `null` (the main line — what the
    // caller's own param actually carries) must be one value downstream, since
    // it is fed straight to `paneUrl`, whose null field serializes to the
    // bare path.
    for (const threadRootId of [undefined, null]) {
      expect(
        resolveJumpRoute({
          decision: "to-direct",
          channelId: "pair-conv",
          navWorkspaceId: null,
          threadRootId,
        })?.threadRootId,
      ).toBeNull();
      expect(
        resolveJumpRoute({
          decision: "workspace-resolve",
          channelId: "chan",
          navWorkspaceId: "w-1",
          threadRootId,
        })?.threadRootId,
      ).toBeNull();
    }
  });

  it("never reads the workspace coordinate for a direct target", () => {
    // A pair has no container to name; a resolved workspace that leaked in here
    // must not turn the jump into a channel URL under a workspace the pair is
    // not in. (The caller resolves the workspace unconditionally, so this arm
    // really is handed a non-null value.)
    expect(
      resolveJumpRoute({
        decision: "to-direct",
        channelId: "pair-conv",
        navWorkspaceId: "w-selected",
        threadRootId: "root-1",
      }),
    ).toEqual({
      kind: "direct",
      conversationId: "pair-conv",
      threadRootId: "root-1",
    });
  });

  it("withholds a channel target whose workspace could not be resolved", () => {
    // Pre-existing `if (!wid) return false` — a channel URL cannot be built
    // without a container, and inventing one is the wrong-place jump.
    for (const threadRootId of [undefined, "root-1"]) {
      expect(
        resolveJumpRoute({
          decision: "workspace-resolve",
          channelId: "chan",
          navWorkspaceId: null,
          threadRootId,
        }),
      ).toBeNull();
    }
  });
});

describe("jumpBasePath — section × placement → base path (#task:2115)", () => {
  const direct = (conversationId: string): JumpRoute => ({
    kind: "direct",
    conversationId,
    threadRootId: null,
  });
  const channel = (workspaceId: string, channelId: string): JumpRoute => ({
    kind: "channel",
    workspaceId,
    channelId,
    threadRootId: null,
  });

  it("keeps a DirectPair jump IN the Activity shell — the dogfood regression", () => {
    // The whole point: a DM opened from the Activity list must NOT fall back to
    // `/chat/d/:id` (which left the section). This is the falsifier — flip the
    // direct arm back to `routes.chatDirect` and this reddens.
    expect(jumpBasePath("activity", direct("conv-1"))).toBe("/activity/d/conv-1");
  });

  it("keeps a channel jump in the Activity shell too (unchanged #task:1394 arm)", () => {
    expect(jumpBasePath("activity", channel("w-1", "c-a"))).toBe("/activity/w/w-1/c/c-a");
  });

  it("lands both placements in chat from the chat section", () => {
    expect(jumpBasePath("chat", direct("conv-1"))).toBe("/chat/d/conv-1");
    expect(jumpBasePath("chat", channel("w-1", "c-a"))).toBe("/chat/w/w-1/c/c-a");
  });

  it("treats every non-activity section as chat (a notification click viewed elsewhere)", () => {
    // The caller's original ternary was `section === 'activity' ? … : chat`, so
    // anything that is not Activity routes to chat — pin that so a future added
    // section does not silently acquire its own arm.
    for (const section of ["tasks", "members", "computers", "settings"] as const) {
      expect(jumpBasePath(section, direct("conv-1"))).toBe("/chat/d/conv-1");
      expect(jumpBasePath(section, channel("w-1", "c-a"))).toBe("/chat/w/w-1/c/c-a");
    }
  });
});
