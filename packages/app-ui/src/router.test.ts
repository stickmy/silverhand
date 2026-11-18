import { routes, withAttachmentQuery, withHumanQuery } from "./router";
import { describe, expect, it } from "vitest";
import {
  buildAgentPanelOpenUrl,
  buildChannelLayoutUrl,
  buildChannelPathPreservingAgent,
  buildChannelPathPreservingSide,
  buildSideOpenUrl,
  buildTaskOpenUrl,
  buildThreadOpenUrl,
  layoutMemoryFromRoute,
  parseRouteState,
  withAgentQuery,
  withSideQuery,
  withTaskQuery,
  withThreadQuery,
  withThreadsQuery,
  withFilesQuery,
  withChannelSettingsQuery,
} from "./router";
import type { SecondaryRouteFields } from "./router";

// Tests focus on the URL → state matrix: the base path is
// `/chat/w/:workspaceId/c/:channelId` (the legacy `/t/<threadId>` fossil
// path segment was removed with the isolated-thread viewer) plus the
// `?side` / `?thread` / `?threads` / `?fullscreen` / `?task` query
// params. The pure parser keeps these shapes deterministic; the React
// selectors layer on top.
describe("parseRouteState — chat URL matrix", () => {
  it("parses workspace + channel without secondary", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "");
    expect(r).toMatchObject({
      section: "chat",
      chatView: "channel",
      workspaceId: "w-1",
      channelId: "c-a",
      sideConversationId: null,
      fullscreen: false,
    });
  });

  it("parses ?side=<id> alone", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?side=c-other");
    expect(r).toMatchObject({
      sideConversationId: "c-other",
      fullscreen: false,
    });
  });

  it("parses ?side with ?fullscreen=1", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?side=c-other&fullscreen=1");
    expect(r.sideConversationId).toBe("c-other");
    expect(r.fullscreen).toBe(true);
  });

  it("treats empty ?side= as unset (defensive — prevents spurious unresolved placeholder)", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?side=");
    expect(r.sideConversationId).toBeNull();
  });

  it("preserves ad-hoc query params untouched", () => {
    // Non-relevant query params (analytics, future flags, etc.) must
    // not be lost by the parser — withSideQuery preserves them too.
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?side=c-other&utm_source=test");
    expect(r.sideConversationId).toBe("c-other");
  });

  it("saved/search/archived chat views still parse", () => {
    expect(parseRouteState("/chat/saved", "").chatView).toBe("saved");
    expect(parseRouteState("/chat/search", "").chatView).toBe("search");
    expect(parseRouteState("/chat/archived", "").chatView).toBe("archived");
  });

  it("non-chat sections set section + leave chat ids null", () => {
    const r = parseRouteState("/members/agent-1", "");
    expect(r.section).toBe("members");
    expect(r.agentId).toBe("agent-1");
    expect(r.userId).toBeNull();
    expect(r.workspaceId).toBeNull();
  });

  it("parses /members/u/:userId into userId and leaves agentId null", () => {
    const r = parseRouteState("/members/u/u-1", "");
    expect(r.section).toBe("members");
    expect(r.userId).toBe("u-1");
    expect(r.agentId).toBeNull();
  });

  it("parses bare /activity into the activity section with no channel", () => {
    const r = parseRouteState("/activity", "");
    expect(r.section).toBe("activity");
    expect(r.workspaceId).toBeNull();
    expect(r.channelId).toBeNull();
  });

  it("parses /activity/w/:wid/c/:cid — same channel shape as chat, activity section", () => {
    const r = parseRouteState("/activity/w/w-1/c/c-a", "");
    expect(r).toMatchObject({
      section: "activity",
      workspaceId: "w-1",
      channelId: "c-a",
    });
  });

  it("parses ?thread on an activity channel route (secondary pane shared with chat)", () => {
    const r = parseRouteState("/activity/w/w-1/c/c-a", "?thread=root-1");
    expect(r.section).toBe("activity");
    expect(r.workspaceId).toBe("w-1");
    expect(r.channelId).toBe("c-a");
    expect(r.threadRootId).toBe("root-1");
  });

  it("parses /activity/d/:conversationId — DM selected IN the Activity shell (#task:2115)", () => {
    // The workspace-less twin of `/activity/w/:wid/c/:cid`: a DirectPair opened
    // from the Activity list stays in the Activity section instead of falling
    // back to `/chat/d/:id`. `directConversationId` is the placement marker and
    // is section-independent; `channelId` mirrors it so every channelId-keyed
    // consumer (secondary panes, ⌘W, read cursors) needs no activity branch.
    const r = parseRouteState("/activity/d/conv-1", "");
    expect(r).toMatchObject({
      section: "activity",
      workspaceId: null,
      channelId: "conv-1",
      directConversationId: "conv-1",
    });
  });

  it("carries ?thread on the activity DM route (same secondary-pane helper as chat)", () => {
    const r = parseRouteState("/activity/d/conv-1", "?thread=root-1");
    expect(r).toMatchObject({
      section: "activity",
      directConversationId: "conv-1",
      channelId: "conv-1",
      threadRootId: "root-1",
    });
  });

  it("workspace-only /activity/w/:wid yields NO workspace selection (no bare-workspace state)", () => {
    // The selected Activity state is per-PLACEMENT — `/activity/w/:wid/c/:cid`
    // (channel) or `/activity/d/:cid` (DM), both naming a whole conversation.
    // There is deliberately no workspace-ONLY `/activity/w/:wid` route, so a
    // hand-typed one must NOT resolve a phantom workspace — that would show a
    // titlebar / fire ws-scoped queries for a workspace not derived from any
    // selected Activity item (#task:1394 review).
    const r = parseRouteState("/activity/w/w-1", "");
    expect(r.section).toBe("activity");
    expect(r.workspaceId).toBeNull();
    expect(r.channelId).toBeNull();
  });
});

describe("withSideQuery — URL writer for openSide / closeSide", () => {
  it("writes ?side=<id> on a bare path", () => {
    const out = withSideQuery("/chat/w/w-1/c/c-a", "", "c-other");
    expect(out).toBe("/chat/w/w-1/c/c-a?side=c-other");
  });

  it("overwrites an existing ?side", () => {
    const out = withSideQuery("/chat/w/w-1/c/c-a", "?side=c-old", "c-new");
    expect(out).toBe("/chat/w/w-1/c/c-a?side=c-new");
  });

  it("strips ?side on close", () => {
    const before = "/chat/w/w-1/c/c-a";
    const opened = withSideQuery(before, "", "c-other");
    expect(opened).toBe(`${before}?side=c-other`);
    const closed = withSideQuery(before, "?side=c-other", null);
    expect(closed).toBe(before);
  });

  it("strips ?side AND ?fullscreen on close", () => {
    // closeSide must drop fullscreen too — leaving fullscreen=1
    // dangling after closing a fullscreened side overlay would yank
    // the primary into fullscreen mode unexpectedly.
    const out = withSideQuery("/chat/w/w-1/c/c-a", "?side=c-other&fullscreen=1", null);
    expect(out).toBe("/chat/w/w-1/c/c-a");
  });

  it("preserves unrelated query params on both open and close", () => {
    const opened = withSideQuery("/chat/w/w-1/c/c-a", "?utm_source=test", "c-other");
    expect(opened).toContain("utm_source=test");
    expect(opened).toContain("side=c-other");
    const closed = withSideQuery("/chat/w/w-1/c/c-a", "?side=c-other&utm_source=test", null);
    expect(closed).toBe("/chat/w/w-1/c/c-a?utm_source=test");
  });
});

describe("buildSideOpenUrl — openSide URL writer (PR-4 follow-up)", () => {
  // The whole point: sigil clicks promise "side pane, doesn't replace
  // your main view". If openSide inherited a stale `?fullscreen=1`,
  // `shouldSecondaryFullscreen` would render only the side and HIDE the
  // primary the user was reading. Locking this behavior here so a future
  // refactor can't silently regress the prompt promise.

  it("writes ?side=<id> on a bare path", () => {
    expect(buildSideOpenUrl("/chat/w/w-1/c/c-a", "", "c-other")).toBe(
      "/chat/w/w-1/c/c-a?side=c-other",
    );
  });

  it("strips ?fullscreen when opening side from a fullscreen view", () => {
    // User was at a fullscreen view. Clicking #conv:<id> must NOT inherit
    // fullscreen — that would hide the channel under a fullscreen side pane.
    const out = buildSideOpenUrl("/chat/w/w-1/c/c-a", "?fullscreen=1", "c-other");
    expect(out).toBe("/chat/w/w-1/c/c-a?side=c-other");
    expect(out).not.toContain("fullscreen");
  });

  it("preserves unrelated query params and overwrites stale ?side", () => {
    const out = buildSideOpenUrl(
      "/chat/w/w-1/c/c-a",
      "?utm_source=test&side=c-old&fullscreen=1",
      "c-new",
    );
    expect(out).toContain("utm_source=test");
    expect(out).toContain("side=c-new");
    expect(out).not.toContain("side=c-old");
    expect(out).not.toContain("fullscreen");
  });
});

describe("buildChannelPathPreservingSide — view-in-channel keeps side overlay", () => {
  // PR-2 blocker per Cindy review of msg `tEZtWtHiHzpq8PxC`: clicking
  // "View in channel" on a cross-workspace `?side=<conv>` previously
  // navigated primary via `nav.toChannel(wid, cid)`, which writes a
  // bare path and silently drops `?side`. The user's open side pane
  // disappears even though they only asked to see its parent. This
  // helper builds the URL with `?side` preserved.
  it("writes /chat/w/<wid>/c/<cid>?side=<sideId> for cross-workspace parent jump", () => {
    expect(buildChannelPathPreservingSide("w-other", "c-parent", "c-side")).toBe(
      "/chat/w/w-other/c/c-parent?side=c-side",
    );
  });

  it("keeps the side overlay even when destination workspace differs from the side", () => {
    // The helper is intentionally indifferent to workspace alignment —
    // `?side=<id>` resolves through `useConversationById` server-side,
    // so any conversation id is legal regardless of the primary's
    // workspace. The helper just writes the URL.
    const out = buildChannelPathPreservingSide("w-A", "c-A", "c-in-w-B");
    expect(out).toBe("/chat/w/w-A/c/c-A?side=c-in-w-B");
  });

  it("does not propagate ?fullscreen — view-in-channel implies the user wants primary visible", () => {
    // Hard contract: the helper takes only (workspaceId, channelId,
    // sideId). It never reads or writes `?fullscreen`. Asserting via
    // shape — output never contains the substring.
    const out = buildChannelPathPreservingSide("w-1", "c-1", "c-side");
    expect(out).not.toContain("fullscreen");
  });
});

describe("parseRouteState — ?task=<task_id> detail pointer (#task:1715)", () => {
  // `?task` carries the task IDENTITY, not its per-workspace number: the
  // detail modal is an app-level surface reachable from any workspace, so an
  // id is the whole coordinate while a number would need a second axis.
  // Consequence for parsing: the value is OPAQUE. There is nothing to
  // validate beyond "present and non-empty" — an unresolvable id is the
  // modal's not-found state, not a pointer the router may silently discard.
  it("carries the id from ?task=<task_id> verbatim", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?task=t-abc123");
    expect(r.taskDetailId).toBe("t-abc123");
  });

  it("keeps an id the viewer cannot resolve — the modal explains it", () => {
    // Dropping it here would strip the pointer the user just clicked and
    // leave them with no account of what happened.
    expect(parseRouteState("/chat/w/w-1/c/c-a", "?task=deleted-task").taskDetailId).toBe(
      "deleted-task",
    );
  });

  it("treats an empty value and an absent param alike → null", () => {
    expect(parseRouteState("/chat/w/w-1/c/c-a", "?task=").taskDetailId).toBeNull();
    expect(parseRouteState("/chat/w/w-1/c/c-a", "").taskDetailId).toBeNull();
  });

  it("coexists with ?side and ?fullscreen — independent dimensions", () => {
    // The detail modal, the side overlay, and fullscreen are orthogonal URL
    // dimensions; all three must survive parsing.
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?side=c-other&task=t-7&fullscreen=1");
    expect(r.taskDetailId).toBe("t-7");
    expect(r.sideConversationId).toBe("c-other");
    expect(r.fullscreen).toBe(true);
  });
});

describe("withTaskQuery — URL writer for openTask / closeTask", () => {
  it("writes ?task=<task_id> on a bare path", () => {
    expect(withTaskQuery("/chat/w/w-1/c/c-a", "", "t-5")).toBe("/chat/w/w-1/c/c-a?task=t-5");
  });

  it("overwrites an existing ?task", () => {
    expect(withTaskQuery("/chat/w/w-1/c/c-a", "?task=t-3", "t-9")).toBe(
      "/chat/w/w-1/c/c-a?task=t-9",
    );
  });

  it("strips ?task AND ?fullscreen on close, revealing the restore stack", () => {
    // closeTask drops `?task` and `?fullscreen` (task is an overlay pane, so
    // it clears fullscreen on close like every other pane). A preserved
    // `?thread` resurfaces as the restore stack; `?side` cannot coexist with a
    // task overlay (it was cleared on open), so it is not part of this case.
    const out = withTaskQuery("/chat/w/w-1/c/c-a", "?thread=m-1&task=t-12&fullscreen=1", null);
    expect(out).toContain("thread=m-1");
    expect(out).not.toContain("fullscreen=1");
    expect(out).not.toContain("task=");
  });

  it("returns the bare path when no params remain after close", () => {
    expect(withTaskQuery("/chat/w/w-1/c/c-a", "?task=t-7", null)).toBe("/chat/w/w-1/c/c-a");
  });

  it("preserves unrelated query params on open", () => {
    const out = withTaskQuery("/chat/w/w-1/c/c-a", "?utm_source=test", "t-4");
    expect(out).toContain("utm_source=test");
    expect(out).toContain("task=t-4");
  });
});

describe("buildTaskOpenUrl — openTask URL writer", () => {
  // `task` is an OVERLAY pane: opening it clears `?side` (correctness) + the
  // other overlays + `?fullscreen`, but preserves `?thread` / `?threads` as
  // the restore stack (matrix asserts this below).

  it("writes ?task=<task_id> on a bare path", () => {
    expect(buildTaskOpenUrl("/chat/w/w-1/c/c-a", "", "t-7")).toBe("/chat/w/w-1/c/c-a?task=t-7");
  });

  it("clears ?side and ?fullscreen, preserving ?thread as the restore stack", () => {
    const out = buildTaskOpenUrl(
      "/chat/w/w-1/c/c-a",
      "?side=c-other&thread=m-1&fullscreen=1",
      "t-11",
    );
    expect(out).not.toContain("side=c-other");
    expect(out).not.toContain("fullscreen=1");
    expect(out).toContain("thread=m-1");
    expect(out).toContain("task=t-11");
  });

  it("overwrites a stale ?task", () => {
    const out = buildTaskOpenUrl("/chat/w/w-1/c/c-a", "?task=t-3&utm_source=test", "t-8");
    expect(out).toContain("task=t-8");
    expect(out).not.toContain("task=t-3");
    expect(out).toContain("utm_source=test");
  });
});

// Thread pane (`?thread=<rootMessageId>`). A thread is NOT a
// conversation — it points at a main-line message whose replies the right
// pane shows. It is mutually exclusive with `?side` at the nav layer:
// opening one clears the other. (`?cluster=` was retired with the legacy
// `/t/` viewer; the URL now speaks the user-facing "thread".)
describe("parseRouteState — ?thread=<rootMessageId> thread pane pointer", () => {
  it("parses ?thread=<id> alone", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?thread=m-1");
    expect(r).toMatchObject({
      channelId: "c-a",
      threadRootId: "m-1",
      sideConversationId: null,
    });
  });

  it("defaults threadRootId to null with no query", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "");
    expect(r.threadRootId).toBeNull();
  });

  it("treats empty ?thread= as unset", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?thread=");
    expect(r.threadRootId).toBeNull();
  });
});

describe("withThreadQuery — URL writer for openThread / closeThread", () => {
  it("writes ?thread=<id>, clearing ?side and ?fullscreen", () => {
    const out = withThreadQuery("/chat/w/w-1/c/c-a", "?side=c-other&fullscreen=1", "m-1");
    expect(out).toContain("thread=m-1");
    expect(out).not.toContain("side=c-other");
    expect(out).not.toContain("fullscreen=1");
  });

  it("clears ?thread (and ?fullscreen) when id is null", () => {
    const closed = withThreadQuery("/chat/w/w-1/c/c-a", "?thread=m-1&fullscreen=1", null);
    expect(closed).not.toContain("thread");
    expect(closed).not.toContain("fullscreen");
  });

  it("preserves unrelated params but clears the task overlay when opening", () => {
    // thread is a `replace` pane → opening it clears every other secondary
    // pane, including the `task` overlay. Truly unrelated params survive.
    const out = withThreadQuery("/chat/w/w-1/c/c-a", "?task=7&utm_source=test", "m-1");
    expect(out).toContain("thread=m-1");
    expect(out).toContain("utm_source=test");
    expect(out).not.toContain("task=7");
  });
});

describe("buildThreadOpenUrl — openThread URL writer", () => {
  it("is additive over withThreadQuery and mutually exclusive with side", () => {
    expect(buildThreadOpenUrl("/chat/w/w-1/c/c-a", "", "m-1")).toBe("/chat/w/w-1/c/c-a?thread=m-1");
    const out = buildThreadOpenUrl("/chat/w/w-1/c/c-a", "?side=c-other", "m-1");
    expect(out).toContain("thread=m-1");
    expect(out).not.toContain("side=c-other");
  });
});

describe("parseRouteState — ?threads=1 channel Threads-list pane (#task:1073)", () => {
  it("parses ?threads=1 as threadsListOpen", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?threads=1");
    expect(r).toMatchObject({
      channelId: "c-a",
      threadsListOpen: true,
      sideConversationId: null,
      threadRootId: null,
    });
  });

  it("defaults threadsListOpen to false with no query", () => {
    expect(parseRouteState("/chat/w/w-1/c/c-a", "").threadsListOpen).toBe(false);
  });

  it('treats any value other than "1" as closed', () => {
    expect(parseRouteState("/chat/w/w-1/c/c-a", "?threads=0").threadsListOpen).toBe(false);
    expect(parseRouteState("/chat/w/w-1/c/c-a", "?threads=").threadsListOpen).toBe(false);
  });
});

describe("withThreadsQuery — URL writer for openThreadsList / closeThreadsList", () => {
  it("writes ?threads=1, clearing ?side, ?thread and ?fullscreen", () => {
    const out = withThreadsQuery(
      "/chat/w/w-1/c/c-a",
      "?side=c-other&thread=m-1&fullscreen=1",
      true,
    );
    expect(out).toContain("threads=1");
    expect(out).not.toContain("side=c-other");
    expect(out).not.toContain("thread=m-1");
    expect(out).not.toContain("fullscreen=1");
  });

  it("clears ?threads (and ?fullscreen) when closing", () => {
    const closed = withThreadsQuery("/chat/w/w-1/c/c-a", "?threads=1&fullscreen=1", false);
    expect(closed).not.toContain("threads");
    expect(closed).not.toContain("fullscreen");
  });

  it("preserves unrelated params but clears the task overlay when opening", () => {
    // threads is a `replace` pane → opening it clears the `task` overlay.
    const out = withThreadsQuery("/chat/w/w-1/c/c-a", "?task=7&utm_source=test", true);
    expect(out).toContain("threads=1");
    expect(out).toContain("utm_source=test");
    expect(out).not.toContain("task=7");
  });
});

describe("mutual exclusion — opening side/thread clears ?threads", () => {
  it("buildSideOpenUrl drops an open ?threads", () => {
    const out = buildSideOpenUrl("/chat/w/w-1/c/c-a", "?threads=1", "c-other");
    expect(out).toContain("side=c-other");
    expect(out).not.toContain("threads");
  });

  it("withThreadQuery (open) drops an open ?threads", () => {
    const out = withThreadQuery("/chat/w/w-1/c/c-a", "?threads=1", "m-1");
    expect(out).toContain("thread=m-1");
    expect(out).not.toContain("threads=1");
  });
});

describe("parseRouteState — ?files=1 channel Attachments panel", () => {
  it("parses ?files=1 as filesListOpen", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?files=1");
    expect(r).toMatchObject({
      channelId: "c-a",
      filesListOpen: true,
      sideConversationId: null,
      threadRootId: null,
      threadsListOpen: false,
    });
  });

  it("defaults filesListOpen to false with no query", () => {
    expect(parseRouteState("/chat/w/w-1/c/c-a", "").filesListOpen).toBe(false);
  });

  it('treats any value other than "1" as closed (falsifier for slack parsing)', () => {
    expect(parseRouteState("/chat/w/w-1/c/c-a", "?files=0").filesListOpen).toBe(false);
    expect(parseRouteState("/chat/w/w-1/c/c-a", "?files=").filesListOpen).toBe(false);
    expect(parseRouteState("/chat/w/w-1/c/c-a", "?files=true").filesListOpen).toBe(false);
  });
});

describe("withFilesQuery — URL writer for openFilesList / closeFilesList", () => {
  it("writes ?files=1, clearing ?side/?agent/?fullscreen but PRESERVING ?thread/?threads", () => {
    // Overlay contract (mirrors ?agent, #task:1433): opening Attachments
    // keeps the underlying conversation pane in the URL so closing restores
    // it. ?side is the exception — a covered CONVERSATION would still count
    // as visible and silence its toasts, so it (and ?agent) is swept.
    const out = withFilesQuery(
      "/chat/w/w-1/c/c-a",
      "?side=c-other&thread=m-1&threads=1&agent=a-9&fullscreen=1",
      true,
    );
    expect(out).toContain("files=1");
    expect(out).not.toContain("side=c-other");
    expect(out).not.toContain("agent=a-9");
    expect(out).not.toContain("fullscreen=1");
    // Preserved — the restore stack.
    expect(out).toContain("thread=m-1");
    expect(out).toContain("threads=1");
  });

  it("closing over a preserved ?thread reveals the thread (thread+files → close files = thread)", () => {
    const opened = withFilesQuery("/chat/w/w-1/c/c-a", "?thread=m-1", true);
    expect(opened).toContain("thread=m-1");
    expect(opened).toContain("files=1");
    const closed = withFilesQuery("/chat/w/w-1/c/c-a", opened.split("?")[1] ?? "", false);
    expect(closed).toContain("thread=m-1");
    expect(closed).not.toContain("files");
  });

  it("closing over a preserved ?threads reveals the list (threads+files → close = threads)", () => {
    const opened = withFilesQuery("/chat/w/w-1/c/c-a", "?threads=1", true);
    expect(opened).toContain("threads=1");
    expect(opened).toContain("files=1");
    const closed = withFilesQuery("/chat/w/w-1/c/c-a", opened.split("?")[1] ?? "", false);
    expect(closed).toContain("threads=1");
    expect(closed).not.toContain("files");
  });

  it("clears ?files (and ?fullscreen defensively) when closing", () => {
    const closed = withFilesQuery("/chat/w/w-1/c/c-a", "?files=1&fullscreen=1", false);
    expect(closed).not.toContain("files");
    expect(closed).not.toContain("fullscreen");
  });

  it("preserves unrelated params but clears the task overlay when opening", () => {
    // files and task are both overlays → opening files clears the task
    // overlay (one overlay on top at a time). Unrelated params survive.
    const out = withFilesQuery("/chat/w/w-1/c/c-a", "?task=7&utm_source=test", true);
    expect(out).toContain("files=1");
    expect(out).toContain("utm_source=test");
    expect(out).not.toContain("task=7");
  });

  it("returns the bare path when the resulting query is empty", () => {
    // Falsifier: closing must not leave a dangling `?` on the path.
    expect(withFilesQuery("/chat/w/w-1/c/c-a", "?files=1", false)).toBe("/chat/w/w-1/c/c-a");
  });
});

describe("opening side/thread/threads closes the ?files overlay", () => {
  // The Attachments overlay does not coexist with a freshly-opened
  // conversation pane: navigating to side/thread/threads sweeps ?files so
  // the new pane is visible (the overlay would otherwise render ahead of it
  // by precedence). This is the reverse direction from withFilesQuery, which
  // PRESERVES an already-open thread/threads when Attachments opens.
  it("buildSideOpenUrl drops an open ?files", () => {
    const out = buildSideOpenUrl("/chat/w/w-1/c/c-a", "?files=1", "c-other");
    expect(out).toContain("side=c-other");
    expect(out).not.toContain("files");
  });

  it("withThreadQuery (open) drops an open ?files", () => {
    const out = withThreadQuery("/chat/w/w-1/c/c-a", "?files=1", "m-1");
    expect(out).toContain("thread=m-1");
    expect(out).not.toContain("files=1");
  });

  it("withThreadsQuery (open) drops an open ?files", () => {
    const out = withThreadsQuery("/chat/w/w-1/c/c-a", "?files=1", true);
    expect(out).toContain("threads=1");
    expect(out).not.toContain("files=1");
  });
});

describe("?agent + ?files — symmetric overlay mutual exclusion", () => {
  // Both are overlays; the right pane holds one overlay at a time. Opening
  // either clears the other (symmetric — no direction preserves the peer),
  // while both preserve the covered `?thread` / `?threads` restore stack.
  it("withAgentQuery (open) clears ?files, preserving the thread restore stack", () => {
    const out = withAgentQuery("/chat/w/w-1/c/c-a", "?thread=m-1&files=1", "a-9");
    expect(out).toContain("agent=a-9");
    expect(out).not.toContain("files=1");
    expect(out).toContain("thread=m-1");
    // Close agent → thread survives (files was already swept on open).
    const afterAgentClose = withAgentQuery("/chat/w/w-1/c/c-a", out.split("?")[1] ?? "", null);
    expect(afterAgentClose).not.toContain("agent");
    expect(afterAgentClose).not.toContain("files");
    expect(afterAgentClose).toContain("thread=m-1");
  });

  it("withFilesQuery (open) clears ?agent — Files is the single visible overlay", () => {
    const out = withFilesQuery("/chat/w/w-1/c/c-a", "?agent=a-9", true);
    expect(out).toContain("files=1");
    expect(out).not.toContain("agent");
  });
});

describe("overlay open — clears ?side (correctness) + keeps thread/threads (restore stack)", () => {
  // Two orthogonal invariants locked as a matrix (converged three-way with
  // @Cindy / @Alice, #task:1721). For EVERY overlay pane, opening it over a URL
  // that carries both `?side` and a `?thread` / `?threads` restore pane must:
  //   (a) CLEAR `?side` — a covered secondary conversation still feeds
  //       `useSecondaryPaneModel`'s visible/focused set (notification
  //       suppression + read-advance); leaving it set while invisible is data
  //       corruption. This is the hard gate on the registry's `side.
  //       survivesOverlay: false`; do not delete it.
  //   (b) PRESERVE `?thread` / `?threads` — the URL is the restore stack, so
  //       closing the overlay brings the covered thread pane back.
  // Both assertions run in a single transform; either alone leaves a back door.
  const overlays: Array<{
    name: string;
    open: (search: string) => string;
  }> = [
    { name: "task", open: (s) => buildTaskOpenUrl("/chat/w/w-1/c/c-a", s, "t-1") },
    { name: "agent", open: (s) => buildAgentPanelOpenUrl("/chat/w/w-1/c/c-a", s, "a-1") },
    {
      name: "settings",
      open: (s) => withChannelSettingsQuery("/chat/w/w-1/c/c-a", s, "c-other"),
    },
    { name: "files", open: (s) => withFilesQuery("/chat/w/w-1/c/c-a", s, true) },
  ];

  for (const overlay of overlays) {
    it(`open ${overlay.name} over ?side + ?thread → side cleared, thread kept`, () => {
      const out = overlay.open("?side=c-other&thread=m-1");
      expect(out).not.toContain("side=c-other");
      expect(out).toContain("thread=m-1");
    });

    it(`open ${overlay.name} over ?side + ?threads → side cleared, threads kept`, () => {
      const out = overlay.open("?side=c-other&threads=1");
      expect(out).not.toContain("side=c-other");
      expect(out).toContain("threads=1");
    });
  }
});

describe("buildSideOpenUrl — clears ?thread (side/thread mutual exclusion)", () => {
  it("opening a side overlay closes an open thread", () => {
    const out = buildSideOpenUrl("/chat/w/w-1/c/c-a", "?thread=m-1", "c-other");
    expect(out).toContain("side=c-other");
    expect(out).not.toContain("thread=m-1");
    expect(out).not.toContain("threads");
  });
});

// Agent side panel (`?agent=<agentId>`, #task:1189). Carries an agent id (not
// a conversation), rendered by `AgentView variant="side"` in the chat-section
// right pane. It OVERLAYS the thread panes: `?agent` coexists with `?thread` /
// `?threads` in the URL (App renders it ahead by precedence, closing it
// restores the covered pane — #task:1433). Only `?side` is cleared on open.
describe("parseRouteState — ?agent=<id> agent side panel", () => {
  it("parses ?agent as agentPanelId, distinct from the /members/:agentId route", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?agent=a-9");
    expect(r).toMatchObject({
      section: "chat",
      channelId: "c-a",
      agentPanelId: "a-9",
      agentId: null,
      sideConversationId: null,
      threadRootId: null,
      threadsListOpen: false,
    });
  });

  it("parses ?agent and ?thread together — overlay coexists at the RouteState layer (#task:1433)", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?agent=a-9&thread=m-1");
    expect(r).toMatchObject({
      agentPanelId: "a-9",
      threadRootId: "m-1",
    });
  });

  it("parses ?agent and ?threads together — overlay coexists with the threads-list (#task:1433)", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?agent=a-9&threads=1");
    expect(r).toMatchObject({
      agentPanelId: "a-9",
      threadsListOpen: true,
    });
  });

  it("defaults agentPanelId to null and treats empty as unset", () => {
    expect(parseRouteState("/chat/w/w-1/c/c-a", "").agentPanelId).toBeNull();
    expect(parseRouteState("/chat/w/w-1/c/c-a", "?agent=").agentPanelId).toBeNull();
  });
});

describe("withAgentQuery / buildAgentPanelOpenUrl — open / close the agent panel", () => {
  it("opens ?agent as an overlay: preserves thread / threads-list, clears ?side + fullscreen", () => {
    const out = buildAgentPanelOpenUrl(
      "/chat/w/w-1/c/c-a",
      "?side=c-other&thread=m-1&threads=1&fullscreen=1",
      "a-9",
    );
    expect(out).toContain("agent=a-9");
    // thread / threads-list are channel-context panes with no
    // secondary-model impact → preserved so closing the panel restores
    // them (#task:1433).
    expect(out).toContain("thread=m-1");
    expect(out).toContain("threads=1");
    // ?side is a secondary CONVERSATION (feeds notification suppression) →
    // cleared while covered. ?fullscreen is dropped so the panel opens at
    // its normal width.
    expect(out).not.toContain("side=c-other");
    expect(out).not.toContain("fullscreen=1");
  });

  it("round-trip: opening then closing the panel over a thread restores the thread pane (#task:1433)", () => {
    const opened = buildAgentPanelOpenUrl("/chat/w/w-1/c/c-a", "?thread=m-1", "a-9");
    const search = opened.slice(opened.indexOf("?"));
    const closed = withAgentQuery("/chat/w/w-1/c/c-a", search, null);
    expect(closed).not.toContain("agent");
    expect(closed).toContain("thread=m-1");
  });

  it("preserves unrelated params but clears the task overlay when opening", () => {
    // agent and task are both overlays → opening agent clears the task overlay.
    const out = buildAgentPanelOpenUrl("/chat/w/w-1/c/c-a", "?task=7&utm_source=test", "a-9");
    expect(out).toContain("agent=a-9");
    expect(out).toContain("utm_source=test");
    expect(out).not.toContain("task=7");
  });

  it("clears ?agent (and ?fullscreen) when closing", () => {
    const closed = withAgentQuery("/chat/w/w-1/c/c-a", "?agent=a-9&fullscreen=1", null);
    expect(closed).not.toContain("agent");
    expect(closed).not.toContain("fullscreen");
  });
});

describe("mutual exclusion — opening a conversation pane clears ?agent", () => {
  it("buildSideOpenUrl drops an open ?agent", () => {
    const out = buildSideOpenUrl("/chat/w/w-1/c/c-a", "?agent=a-9", "c-other");
    expect(out).toContain("side=c-other");
    expect(out).not.toContain("agent");
  });

  it("withThreadQuery (open) drops an open ?agent", () => {
    const out = withThreadQuery("/chat/w/w-1/c/c-a", "?agent=a-9", "m-1");
    expect(out).toContain("thread=m-1");
    expect(out).not.toContain("agent");
  });

  it("withThreadsQuery (open) drops an open ?agent", () => {
    const out = withThreadsQuery("/chat/w/w-1/c/c-a", "?agent=a-9", true);
    expect(out).toContain("threads=1");
    expect(out).not.toContain("agent");
  });
});

describe("buildChannelPathPreservingAgent — A=Activity-follows-channel (#task:1189)", () => {
  it("writes a fresh channel path carrying ?agent (panel survives the switch)", () => {
    expect(buildChannelPathPreservingAgent("w-1", "c-new", "a-9")).toBe(
      "/chat/w/w-1/c/c-new?agent=a-9",
    );
  });

  it("does NOT restore a remembered secondary — only ?agent rides along", () => {
    const out = buildChannelPathPreservingAgent("w-1", "c-new", "a-9");
    expect(out).not.toContain("side");
    expect(out).not.toContain("thread");
    expect(out).not.toContain("threads");
    expect(out).not.toContain("fullscreen");
    // round-trips back to the same agentPanelId, distinct from the route segment
    const r = parseRouteState("/chat/w/w-1/c/c-new", "?agent=a-9");
    expect(r.agentPanelId).toBe("a-9");
    expect(r.agentId).toBeNull();
  });
});

// Per-channel layout memory (#task:1081). `layoutMemoryFromRoute`
// projects the route's secondary-pane fields into the persisted shape;
// `buildChannelLayoutUrl` rebuilds the channel URL from that shape so a
// sidebar switch back into the channel restores the same pane.
describe("layoutMemoryFromRoute — RouteState → persisted layout", () => {
  function fields(overrides: Partial<SecondaryRouteFields>): SecondaryRouteFields {
    return {
      sideConversationId: null,
      threadRootId: null,
      threadsListOpen: false,
      ...overrides,
    };
  }

  it("maps a bare channel to { none }", () => {
    expect(layoutMemoryFromRoute(fields({}))).toEqual({ kind: "none" });
  });

  it("maps ?threads=1 to a threads-list layout", () => {
    expect(layoutMemoryFromRoute(fields({ threadsListOpen: true }))).toEqual({
      kind: "threads-list",
    });
  });

  it("maps ?thread=<id> to a thread layout", () => {
    expect(layoutMemoryFromRoute(fields({ threadRootId: "m-1" }))).toEqual({
      kind: "thread",
      rootId: "m-1",
    });
  });

  it("maps ?side=<id> to a side layout", () => {
    expect(layoutMemoryFromRoute(fields({ sideConversationId: "c-x" }))).toEqual({
      kind: "side",
      id: "c-x",
    });
  });

  it("does NOT persist the Attachments overlay: files-only route → { none }", () => {
    // The overlay is transient (see ChannelLayout doc): opening Attachments
    // on a bare channel and switching away must not resurrect it on re-entry.
    // `parseRouteState` still carries filesListOpen, but layoutMemoryFromRoute
    // ignores it — a files-only route memoizes as `none`.
    const filesOnly = parseRouteState("/chat/w/w-1/c/c-a", "?files=1");
    expect(filesOnly.filesListOpen).toBe(true);
    expect(layoutMemoryFromRoute(filesOnly)).toEqual({ kind: "none" });
  });

  it("persists the underlying pane, not the overlay, when files covers a thread", () => {
    // thread+files → the thread is the persisted layout, so channel re-entry
    // restores the thread (files overlay is dropped) rather than the overlay.
    const threadUnderFiles = parseRouteState("/chat/w/w-1/c/c-a", "?thread=m-1&files=1");
    expect(layoutMemoryFromRoute(threadUnderFiles)).toEqual({
      kind: "thread",
      rootId: "m-1",
    });
  });

  it("resolves side > thread > threads-list when a malformed URL sets several", () => {
    // Priority is the documented render-layer chain (Attachments overlay is
    // not part of it). A re-ordering would silently change persisted memory.
    expect(
      layoutMemoryFromRoute(
        fields({
          sideConversationId: "c-x",
          threadRootId: "m-1",
          threadsListOpen: true,
        }),
      ),
    ).toEqual({ kind: "side", id: "c-x" });
    expect(layoutMemoryFromRoute(fields({ threadsListOpen: true }))).toEqual({
      kind: "threads-list",
    });
  });
});

describe("buildChannelLayoutUrl — restore a channel with its layout", () => {
  it("yields the bare path for none / null memory", () => {
    expect(buildChannelLayoutUrl("w-1", "c-a", null)).toBe("/chat/w/w-1/c/c-a");
    expect(buildChannelLayoutUrl("w-1", "c-a", { kind: "none" })).toBe("/chat/w/w-1/c/c-a");
  });

  it("restores a threads-list pane", () => {
    expect(buildChannelLayoutUrl("w-1", "c-a", { kind: "threads-list" })).toBe(
      "/chat/w/w-1/c/c-a?threads=1",
    );
  });

  it("restores a thread pane", () => {
    expect(buildChannelLayoutUrl("w-1", "c-a", { kind: "thread", rootId: "m-1" })).toBe(
      "/chat/w/w-1/c/c-a?thread=m-1",
    );
  });

  it("restores a side overlay", () => {
    expect(buildChannelLayoutUrl("w-1", "c-a", { kind: "side", id: "c-x" })).toBe(
      "/chat/w/w-1/c/c-a?side=c-x",
    );
  });

  it("round-trips: parse → memory → build → parse is stable", () => {
    const built = buildChannelLayoutUrl("w-1", "c-a", { kind: "side", id: "c-x" });
    const [pathname, search] = built.split("?");
    const r = parseRouteState(pathname!, search ? `?${search}` : "");
    expect(layoutMemoryFromRoute(r)).toEqual({ kind: "side", id: "c-x" });
  });
});

/**
 * The WORKSPACE-LESS conversation route (#task:2035). Pure URL algebra, which is
 * where the interesting failure lives: the pane and the sidebar are driven off
 * `directConversationId`, and if the parser ever stops distinguishing "a pair is
 * open" from "nothing is open", App's workspace auto-select effect silently
 * bounces the user out of the conversation they just clicked.
 */
describe("parseRouteState — the workspace-less /chat/d/:id route", () => {
  it("populates channelId and marks the pane as having no workspace", () => {
    const r = parseRouteState("/chat/d/c-pair", "");
    expect(r).toMatchObject({
      section: "chat",
      chatView: "channel",
      workspaceId: null,
      channelId: "c-pair",
      directConversationId: "c-pair",
    });
  });

  it("leaves directConversationId null for every OTHER null-workspace URL", () => {
    // The whole reason the field exists: these all carry `workspaceId: null`
    // too, and the auto-select effect must keep treating them as "nowhere".
    for (const pathname of ["/chat", "/chat/search", "/chat/saved", "/activity"]) {
      expect(parseRouteState(pathname, "").directConversationId).toBeNull();
    }
    expect(parseRouteState("/chat/w/w-1/c/c-a", "").directConversationId).toBeNull();
  });

  it("carries the secondary panes unchanged — they resolve against channelId", () => {
    // The PARSER is placement-blind on purpose: `channelId` is populated the
    // same way, so every `?`-driven consumer needs no pair branch. It is NOT a
    // claim that these panes mount on a pair — App withholds the channel-like
    // indexes there (#task:2035), and no writer ever produces this URL for a
    // pair (see the deleted `buildChannelLayoutUrl(null, …)` arm). What is
    // pinned here is only that a hand-typed query does not corrupt the parse.
    const r = parseRouteState("/chat/d/c-pair", "?thread=m-1&files=1");
    expect(r).toMatchObject({
      channelId: "c-pair",
      directConversationId: "c-pair",
      threadRootId: "m-1",
      filesListOpen: true,
    });
  });

  it("does not collide with the workspace patterns", () => {
    // `/chat/d/:id` is one segment where `chatWorkspace` wants `/w/:id`, so a
    // pair id can never be read as a workspace id.
    expect(parseRouteState("/chat/d/c-pair", "").workspaceId).toBeNull();
    expect(parseRouteState("/chat/w/w-1", "").directConversationId).toBeNull();
  });
});

/**
 * The PRE-MINT direct route (#task:2039). `/chat/d/new/:agentId` names an AGENT,
 * not a conversation — the pair does not exist until a first send mints it — so
 * the load-bearing property is negative: the agent id must not leak into any
 * conversation coordinate. `channelId` drives conversation-keyed readers and
 * layout memory, `directConversationId` drives the pair pane; an agent id in
 * either would have those ask the backend about a conversation that does not
 * exist, and would record an agent id as a remembered channel (@Cindy's
 * #task:2039 boundary, previously only a read-the-code conclusion).
 */
describe("parseRouteState — the pre-mint /chat/d/new/:agentId route", () => {
  it("names the agent and NO conversation", () => {
    const r = parseRouteState("/chat/d/new/a-1", "");
    expect(r).toMatchObject({
      section: "chat",
      // The draft renders in the channel pane's slot, so the section's view is
      // the channel one — with nothing selected in it.
      chatView: "channel",
      directDraftTarget: { kind: "agent", id: "a-1" },
      channelId: null,
      directConversationId: null,
      workspaceId: null,
    });
  });

  it("is matched before the pair pattern — `new` is never read as a pair id", () => {
    // Both patterns start `/chat/d/`; if `chatDirect` won, the pane would open a
    // conversation literally called `new` and the agent id would be lost.
    expect(parseRouteState("/chat/d/new/a-1", "").directConversationId).toBeNull();
    expect(parseRouteState("/chat/d/new/a-1", "").channelId).toBeNull();
  });

  it("leaves the existing pair route unchanged — no drift", () => {
    const r = parseRouteState("/chat/d/c-pair", "");
    expect(r).toMatchObject({
      directConversationId: "c-pair",
      channelId: "c-pair",
      directDraftTarget: null,
      workspaceId: null,
    });
  });

  it("leaves directDraftTarget null for every other URL", () => {
    for (const pathname of [
      "/chat",
      "/chat/search",
      "/chat/saved",
      "/activity",
      "/chat/w/w-1",
      "/chat/w/w-1/c/c-a",
      "/chat/d/c-pair",
    ]) {
      expect(parseRouteState(pathname, "").directDraftTarget).toBeNull();
    }
  });

  it("records no layout memory for the draft — there is no channel to restore", () => {
    const r = parseRouteState("/chat/d/new/a-1", "");
    expect(layoutMemoryFromRoute(r)).toEqual({ kind: "none" });
  });
});

// No `buildChannelLayoutUrl` cases for a pair: layout memory is withheld for the
// workspace-less placement, and the builder's `workspaceId` is non-null by type.
// A test that drove the removed null arm was asserting the opposite of the
// ticket's own verdict, which is why it is gone rather than adapted.

describe("human profile overlay routing", () => {
  it("preserves the thread restore stack and removes covered conversations", () => {
    const url = withHumanQuery(
      "/chat/d/dm-a",
      "?thread=root&side=other&agent=a&fullscreen=1",
      "user-b",
    );
    const search = url.slice(url.indexOf("?"));
    const route = parseRouteState("/chat/d/dm-a", search);

    expect(route.humanPanelId).toBe("user-b");
    expect(route.agentPanelId).toBeNull();
    expect(route.sideConversationId).toBeNull();
    expect(route.threadRootId).toBe("root");
    expect(route.fullscreen).toBe(false);
    expect(withHumanQuery("/chat/d/dm-a", search, null)).toBe("/chat/d/dm-a?thread=root");
  });

  it("replaces the human pane when another inspector opens", () => {
    expect(withAgentQuery("/chat/d/dm-a", "?human=user-b&thread=root", "a")).not.toContain(
      "human=",
    );
    expect(parseRouteState("/chat/d/dm-a", "?human=").humanPanelId).toBeNull();
  });
});

describe("attachment preview routing", () => {
  it("previews inside the files list and returns to it", () => {
    const opened = withAttachmentQuery("/chat/w/w-1/c/c-a", "?files=1&thread=root", "att", true);
    const search = opened.slice(opened.indexOf("?"));
    const route = parseRouteState("/chat/w/w-1/c/c-a", search);

    expect(route.attachmentPanelId).toBe("att");
    expect(route.filesListOpen).toBe(true);
    expect(route.threadRootId).toBe("root");
    const back = withAttachmentQuery("/chat/w/w-1/c/c-a", search, null);
    expect(parseRouteState("/chat/w/w-1/c/c-a", back.slice(back.indexOf("?")))).toMatchObject({
      attachmentPanelId: null,
      filesListOpen: true,
    });
    expect(withFilesQuery("/chat/w/w-1/c/c-a", search, false)).toBe(
      "/chat/w/w-1/c/c-a?thread=root",
    );
  });

  it("opens on its own as an overlay that clears the files list", () => {
    const url = withAttachmentQuery("/chat/w/w-1/c/c-a", "?files=1&side=c-b", "att");

    expect(url).toBe("/chat/w/w-1/c/c-a?attachment=att");
  });
});

describe("human direct-message drafts", () => {
  it("keeps the human target separate from stored conversations", () => {
    const path = routes.chatDirectDraft({ kind: "human", id: "u-2" });
    expect(path).toBe("/chat/d/new/human/u-2");
    expect(parseRouteState(path, "")).toMatchObject({
      section: "chat",
      directDraftTarget: { kind: "human", id: "u-2" },
      channelId: null,
      directConversationId: null,
      workspaceId: null,
    });
  });
});

describe("channel settings overlay", () => {
  const path = "/chat/w/w-1/c/c-a";
  it("keeps an explicit target when replacing a secondary channel", () => {
    const url = withChannelSettingsQuery(path, "?side=c-b&fullscreen=1", "c-b");
    expect(parseRouteState(path, url.split("?")[1] ?? "")).toMatchObject({
      channelSettingsId: "c-b",
      sideConversationId: null,
      fullscreen: false,
    });
  });
  it("closes without losing the covered thread", () => {
    const settings = withChannelSettingsQuery(path, "?thread=root&files=1", "c-a");
    expect(settings).not.toContain("files=");
    expect(withChannelSettingsQuery(path, settings.split("?")[1] ?? "", null)).toBe(
      path + "?thread=root",
    );
  });
  it("opening another utility removes settings", () => {
    expect(withFilesQuery(path, "?settings=c-a&thread=root", true)).toBe(
      path + "?thread=root&files=1",
    );
  });
  it("ordinary channel navigation carries no old settings target", () => {
    expect(parseRouteState("/chat/w/w-1/c/c-b", "").channelSettingsId).toBeNull();
  });
});
