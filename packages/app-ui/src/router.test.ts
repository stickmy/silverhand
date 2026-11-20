import { describe, expect, it } from "vitest";
import {
  channelUrlKeeping,
  closePaneUrl,
  parseRouteState,
  promotedPaneUrl,
  resolveNavigationTarget,
  routes,
  besideUrl,
} from "./router";

const path = "/chat/w/w1/c/c1";
const query = (url: string) => (url.includes("?") ? url.slice(url.indexOf("?")) : "");
const open = (target: Parameters<typeof resolveNavigationTarget>[0], search = "") =>
  resolveNavigationTarget(target, path, search);

describe("pane routing — attached panes beside the route's conversation", () => {
  it("opens panes side by side, in the order they open", () => {
    let url = open({ kind: "threads" });
    url = open({ kind: "attachments" }, query(url));
    url = open({ kind: "agent", agentId: "a1" }, query(url));
    expect(url).toBe(`${path}?pane=threads&pane=files&pane=agent.a1`);
  });

  it("a thread opens inside its conversation's Threads pane, where it stands", () => {
    const url = open({ kind: "thread", rootMessageId: "r1" }, "?pane=threads&pane=files");
    expect(url).toBe(`${path}?pane=threads.r1&pane=files`);
  });

  it("each conversation keeps its own Threads and Files", () => {
    const url = open({ kind: "threads", of: "c2" }, "?pane=conversation.c2&pane=threads");
    expect(url).toBe(`${path}?pane=conversation.c2&pane=threads&pane=threads%7Ec2`);
    expect(parseRouteState(path, query(url)).panes).toContainEqual({
      kind: "threads",
      of: "c2",
      thread: null,
    });
  });

  it("a pane opened without a column goes to the conversation whose pane is last", () => {
    const url = open({ kind: "attachments" }, "?pane=conversation.c2&pane=threads.r2%7Ec2");
    expect(url).toBe(`${path}?pane=conversation.c2&pane=threads.r2%7Ec2&pane=files%7Ec2`);
  });

  it("settings and members are pages of one Settings pane per conversation", () => {
    const settings = open({ kind: "conversation-settings", conversationId: "c1" });
    expect(settings).toBe(`${path}?pane=settings`);
    const members = open(
      { kind: "conversation-members", conversationId: "c1", add: true },
      query(settings),
    );
    expect(members).toBe(`${path}?pane=settings.add`);
    expect(parseRouteState(path, query(members)).panes).toEqual([
      { kind: "settings", of: "c1", page: "add" },
    ]);
  });

  it("task, agent and person panes are one each, replaced in place", () => {
    const url = open({ kind: "task", taskId: "t2" }, "?pane=task.t1&pane=threads");
    expect(url).toBe(`${path}?pane=task.t2&pane=threads`);
  });

  it("closing a conversation pane closes the panes it owns", () => {
    expect(
      closePaneUrl(path, "?pane=conversation.c2&pane=files%7Ec2&pane=threads", "conversation:c2"),
    ).toBe(`${path}?pane=threads`);
    expect(besideUrl(path, "?pane=conversation.c2&pane=files%7Ec2", null)).toBe(path);
  });

  it("closing the route's conversation hands the route to the pane beside it", () => {
    expect(
      promotedPaneUrl(
        path,
        "?pane=threads&pane=conversation.c2&pane=threads.r2%7Ec2",
        "/chat/w/w1/c/c2",
      ),
    ).toBe("/chat/w/w1/c/c2?pane=threads.r2");
  });

  it("keeps every conversation pane in display order", () => {
    const route = parseRouteState(path, "?pane=conversation.c2&pane=conversation.c3&pane=agent.a1");
    expect(route.panes.map((p) => p.kind)).toEqual(["conversation", "conversation", "agent"]);
  });

  it("keeps params outside the pane list", () => {
    expect(open({ kind: "threads" }, "?utm=1")).toBe(`${path}?utm=1&pane=threads`);
  });

  it("keeps one pane across a channel switch", () => {
    expect(channelUrlKeeping("w1", "c3", { agent: "a1" })).toBe("/chat/w/w1/c/c3?pane=agent.a1");
    expect(channelUrlKeeping("w1", "c3", { side: "c4" })).toBe(
      "/chat/w/w1/c/c3?pane=conversation.c4",
    );
  });

  it("a thread in another conversation opens on that conversation's route", () => {
    expect(
      open({ kind: "thread", rootMessageId: "r9", conversationId: "c9", workspaceId: "w1" }),
    ).toBe("/chat/w/w1/c/c9?pane=threads.r9");
  });
});

describe("parseRouteState — chat URL matrix", () => {
  it("parses workspace + channel without secondary", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "");
    expect(r).toMatchObject({
      section: "chat",
      chatView: "channel",
      workspaceId: "w-1",
      channelId: "c-a",
      panes: [],
    });
  });

  it("does not read the old pane params", () => {
    const r = parseRouteState("/chat/w/w-1/c/c-a", "?side=c-other&thread=r&split=c-b");
    expect(r.panes).toEqual([]);
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

  it("parses a thread pane on an activity channel route (panes shared with chat)", () => {
    const r = parseRouteState("/activity/w/w-1/c/c-a", "?pane=threads.root-1");
    expect(r.section).toBe("activity");
    expect(r.workspaceId).toBe("w-1");
    expect(r.channelId).toBe("c-a");
    expect(r.panes).toEqual([{ kind: "threads", of: "c-a", thread: "root-1" }]);
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

  it("carries a thread pane on the activity DM route (same pane list as chat)", () => {
    const r = parseRouteState("/activity/d/conv-1", "?pane=threads.root-1");
    expect(r).toMatchObject({
      section: "activity",
      directConversationId: "conv-1",
      channelId: "conv-1",
      panes: [{ kind: "threads", of: "conv-1", thread: "root-1" }],
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

  it("owns its panes like a channel does — they resolve against channelId", () => {
    const r = parseRouteState("/chat/d/c-pair", "?pane=threads.m-1&pane=files");
    expect(r).toMatchObject({ channelId: "c-pair", directConversationId: "c-pair" });
    expect(r.panes).toEqual([
      { kind: "threads", of: "c-pair", thread: "m-1" },
      { kind: "files", of: "c-pair", attachment: null },
    ]);
  });

  it("does not collide with the workspace patterns", () => {
    // `/chat/d/:id` is one segment where `chatWorkspace` wants `/w/:id`, so a
    // pair id can never be read as a workspace id.
    expect(parseRouteState("/chat/d/c-pair", "").workspaceId).toBeNull();
    expect(parseRouteState("/chat/w/w-1", "").directConversationId).toBeNull();
  });
});

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
