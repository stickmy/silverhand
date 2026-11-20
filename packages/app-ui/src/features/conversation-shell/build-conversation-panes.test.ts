import { beforeEach, describe, expect, it, vi } from "vitest";
import { channelGateFor } from "@/features/channels/membership";
import { inheritedPaneCapabilities } from "@/features/conversations/pane-mode";
import type { ConversationPaneDescriptor } from "@/features/conversations/pane-descriptor";
import type { Pane } from "@/navigation/panes";
import type { ConversationColumn } from "./conversation-column";
import type { AttachedPaneModel } from "./pane-render-models";
import { buildAttachedPane, buildPrimaryPane, type PaneContext } from "./build-conversation-panes";

const openConversationTab = vi.hoisted(() => vi.fn());
vi.mock("@/features/conversations/use-conversation-tabs", () => ({ openConversationTab }));

const descriptor = (id: string, workspaceId: string | null): ConversationPaneDescriptor => ({
  id,
  title: id,
  workspace_id: workspaceId,
  placement_kind: workspaceId ? "workspace" : "direct_pair",
  roster_policy: "open",
  closed_at: null,
  archived_at: null,
  is_member: true,
});

let ctx: PaneContext;
const calls: string[] = [];

/** A conversation showing `id`, a member unless told otherwise. */
function column(
  id: string,
  workspaceId: string | null,
  membership: "member" | "nonmember" | "unknown" = "member",
): ConversationColumn {
  const d = membership === "unknown" ? null : descriptor(id, workspaceId);
  return {
    id,
    descriptor: d,
    status: d ? "resolved" : "loading",
    workspaceId,
    paneTitle: id,
    membership: { membership, gate: channelGateFor(membership) },
    hasChannelNamespace: workspaceId !== null,
    capabilities: inheritedPaneCapabilities(d),
    threadComposerDirectory: { kind: "withheld" },
    pair: null,
  };
}

beforeEach(() => {
  calls.length = 0;
  openConversationTab.mockReset();
  const primary = column("c", "w");
  ctx = {
    route: { workspaceId: "w", channelId: "c", directDraftTarget: null, panes: [] },
    read: {
      workspaceNameById: (id) => id,
      directPairLoadFailed: false,
      directPairNotFound: false,
      parentChannelResolving: false,
      conversationRefsById: new Map(),
      primaryPaneDescriptor: primary.descriptor,
      primary,
      channels: [],
      buildChannelDescriptor: () => null,
    },
    state: {
      channelIndexesAvailable: true,
      taskPaneId: null,
      parentJumpTarget: null,
      parentJumpPrimePending: false,
      jump: { jumpTarget: null, isPrimePendingFor: () => false, primePhaseFor: () => null },
    },
    commands: {
      departPrimaryPane: vi.fn(),
      requestJump: vi.fn(() => {
        calls.push("prime");
      }),
      handleOpenAdvancedSearch: vi.fn(),
      openTaskInPane: vi.fn(),
      setTaskPaneEditing: vi.fn(),
      requestCloseTaskPane: vi.fn(),
      handleOpenChannelByName: vi.fn(),
      handleJumpToMessage: vi.fn(() => true),
      handleMessageAgent: vi.fn(),
      handleDraftOpenedPair: vi.fn(),
      handleJumped: vi.fn(),
      handleOpenThreadFromChannel: vi.fn(),
      openConversationPane: vi.fn(),
      openThread: vi.fn(),
      toggleThreadsList: vi.fn(),
      toggleFilesList: vi.fn(),
      openAttachmentPanel: vi.fn(),
      openChannelMembers: vi.fn(),
      closePane: vi.fn(),
      backPane: vi.fn(() => {
        calls.push("back");
      }),
      closeRouteConversation: vi.fn(),
      movePane: vi.fn(),
    },
  };
});

function build(pane: Pane, owner: ConversationColumn | null = ctx.read.primary) {
  return buildAttachedPane(pane, owner, ctx);
}

function expectKind<K extends AttachedPaneModel["kind"]>(
  model: AttachedPaneModel,
  kind: K,
): Extract<AttachedPaneModel, { kind: K }> {
  if (model.kind !== kind) throw new Error(`Expected ${kind}, got ${model.kind}`);
  return model as Extract<AttachedPaneModel, { kind: K }>;
}

describe("the route's conversation", () => {
  it("isolates a draft from conversation state", () => {
    ctx.route.directDraftTarget = { kind: "agent", agentId: "a" } as never;
    expect(buildPrimaryPane(ctx).kind).toBe("draft");
  });

  it("offers a close only beside a conversation pane", () => {
    const alone = buildPrimaryPane(ctx);
    expect(alone.kind === "conversation" && alone.props.onCloseChild).toBeFalsy();
    ctx.route.panes = [{ kind: "conversation", id: "c2" }];
    const beside = buildPrimaryPane(ctx);
    if (beside.kind !== "conversation") throw new Error("Expected conversation");
    beside.props.onCloseChild?.();
    expect(ctx.commands.closeRouteConversation).toHaveBeenCalledTimes(1);
  });

  it("marks its own index panes active, not another conversation's", () => {
    ctx.route.panes = [{ kind: "threads", of: "c2", thread: null }];
    const model = buildPrimaryPane(ctx);
    if (model.kind !== "conversation") throw new Error("Expected conversation");
    expect(model.props.threadsListActive).toBe(false);
    model.props.onToggleFilesList?.();
    expect(ctx.commands.toggleFilesList).toHaveBeenCalledExactlyOnceWith("c");
  });
});

describe("attached panes", () => {
  it("closes each pane by its own key", () => {
    expectKind(build({ kind: "agent", id: "a1" }, null), "agent").props.onClose?.();
    expectKind(build({ kind: "human", id: "u1" }, null), "human").props.onClose?.();
    expect(ctx.commands.closePane).toHaveBeenNthCalledWith(1, "agent");
    expect(ctx.commands.closePane).toHaveBeenNthCalledWith(2, "human");
  });

  it("shows the task only as a pane, closing through the editing guard", () => {
    expect(build({ kind: "task", id: "t" }, null).kind).toBe("none");
    ctx.state.taskPaneId = "t";
    expectKind(build({ kind: "task", id: "t" }, null), "task").props.onClose?.();
    expect(ctx.commands.requestCloseTaskPane).toHaveBeenCalledTimes(1);
  });

  it("renders nothing for a pane whose conversation has not resolved", () => {
    expect(build({ kind: "threads", of: "c2", thread: null }, null).kind).toBe("none");
  });

  it("withholds Threads and Files from a non-member", () => {
    const browsing = column("c", "w", "nonmember");
    expect(build({ kind: "files", of: "c", attachment: null }, browsing).kind).toBe("none");
    expect(build({ kind: "threads", of: "c", thread: null }, browsing).kind).toBe("none");
  });

  it("settings steps back from members to the overview", () => {
    const overview = expectKind(build({ kind: "settings", of: "c", page: null }), "settings");
    expect(overview.props.onBack).toBeUndefined();
    const members = expectKind(build({ kind: "settings", of: "c", page: "members" }), "settings");
    members.props.onBack?.();
    members.props.onOpenMembers(true);
    expect(ctx.commands.backPane).toHaveBeenCalledExactlyOnceWith("settings:c");
    expect(ctx.commands.openChannelMembers).toHaveBeenCalledExactlyOnceWith("c", true);
  });

  it("previews inside Files, backs to the list, and expands into the owner's tab", () => {
    const model = expectKind(build({ kind: "files", of: "c", attachment: "a" }), "files");
    if (!model.preview?.onExpand) throw new Error("Expected expandable preview");
    model.preview.onExpand({ id: "a", filename: "a.md", size: 1, content_type: "text/markdown" });
    expect(openConversationTab).toHaveBeenCalledExactlyOnceWith("c", {
      kind: "attachment",
      attachmentId: "a",
      filename: "a.md",
      contentType: "text/markdown",
    });
    expect(ctx.commands.backPane).toHaveBeenCalledExactlyOnceWith("files:c");
    model.preview.onClose();
    expect(ctx.commands.closePane).toHaveBeenCalledExactlyOnceWith("files:c");
  });

  it("binds a Files row to the conversation that owns the pane", () => {
    const other = column("c2", "w2");
    const model = expectKind(build({ kind: "files", of: "c2", attachment: null }, other), "files");
    expect(model.props).toMatchObject({ channelId: "c2", workspaceId: "w2" });
    model.props.onOpenAttachment?.({ id: "a" } as never);
    expect(ctx.commands.openAttachmentPanel).toHaveBeenCalledExactlyOnceWith("a", "c2");
  });

  it("opens a thread from the list inside the same conversation's pane", () => {
    const model = expectKind(build({ kind: "threads", of: "c", thread: null }), "threads");
    model.props.onOpenThread("root");
    expect(ctx.commands.openThread).toHaveBeenCalledExactlyOnceWith("root", "c");
  });

  it("keeps DM thread coordinates, inherited permissions and prime-before-back ordering", () => {
    const dm = column("dm", null);
    ctx.state.jump = {
      jumpTarget: { channelId: "dm", threadRootId: "root", messageId: "reply" },
      isPrimePendingFor: () => false,
      primePhaseFor: () => "committed",
    };
    const model = expectKind(build({ kind: "threads", of: "dm", thread: "root" }, dm), "thread");
    expect(model.capabilities).toBe(dm.capabilities);
    expect(model.props.gate).toBe(dm.membership.gate);
    expect(model.props).toMatchObject({
      channelId: "dm",
      workspaceId: null,
      rootMessageId: "root",
      jumpMessageId: "reply",
      jumpPhase: "committed",
    });
    model.props.onViewInChannel?.();
    expect(ctx.commands.requestJump).toHaveBeenCalledExactlyOnceWith({
      workspaceId: undefined,
      channelId: "dm",
      messageId: "root",
    });
    expect(calls).toEqual(["prime", "back"]);
  });

  it("ignores a thread jump aimed at another conversation", () => {
    ctx.state.jump = {
      jumpTarget: { channelId: "other", threadRootId: "root", messageId: "reply" },
      isPrimePendingFor: () => false,
      primePhaseFor: () => null,
    };
    const model = expectKind(build({ kind: "threads", of: "c", thread: "root" }), "thread");
    expect(model.props.jumpMessageId).toBeNull();
  });
});

describe("conversation panes", () => {
  it("shows a placeholder until the conversation resolves", () => {
    const model = build({ kind: "conversation", id: "c2" }, column("c2", "w", "unknown"));
    if (model.kind !== "conversation" || model.status !== "unresolved") {
      throw new Error("Expected placeholder");
    }
    model.props.onClose();
    expect(ctx.commands.closePane).toHaveBeenCalledExactlyOnceWith("conversation:c2");
  });

  it("gives a resolved pane its own descriptor, workspace and panes", () => {
    const other = column("c2", "w2");
    const model = build({ kind: "conversation", id: "c2" }, other);
    if (model.kind !== "conversation" || model.status !== "resolved") {
      throw new Error("Expected resolved");
    }
    expect(model.props.conversation).toBe(other.descriptor);
    expect(model.props.workspaceName).toBe("w2");
    model.props.onToggleThreadsList?.();
    model.props.onOpenAttachmentPanel?.("a");
    expect(ctx.commands.toggleThreadsList).toHaveBeenCalledExactlyOnceWith("c2");
    expect(ctx.commands.openAttachmentPanel).toHaveBeenCalledExactlyOnceWith("a", "c2");
  });
});
