import { beforeEach, describe, expect, expectTypeOf, it, vi } from "vitest";
import { channelGateFor } from "@/features/channels/membership";
import { inheritedPaneCapabilities } from "@/features/conversations/pane-mode";
import { selectSecondaryViewInputs } from "@/features/conversation-shell/model/secondary-pane";
import type { ConversationPaneDescriptor } from "@/features/conversations/pane-descriptor";
import type {
  PaneContentRoute,
  PaneContentRead,
  PaneContentState,
  PaneContentCommands,
} from "./pane-content-inputs";
import type { SecondaryPaneModel } from "./pane-render-models";
import type { ConversationColumn } from "./conversation-column";
import { buildConversationPanes, resolveSecondaryPane } from "./build-conversation-panes";

const openConversationTab = vi.hoisted(() => vi.fn());
vi.mock("@/features/conversations/use-conversation-tabs", () => ({ openConversationTab }));

const descriptor: ConversationPaneDescriptor = {
  id: "foreign",
  title: "Foreign",
  workspace_id: "other-workspace",
  placement_kind: "workspace",
  roster_policy: "open",
  closed_at: null,
  archived_at: null,
  is_member: true,
};
let route: PaneContentRoute;
let read: PaneContentRead;
let state: PaneContentState;
let commands: PaneContentCommands;
const calls: string[] = [];

beforeEach(() => {
  calls.length = 0;
  openConversationTab.mockReset();
  route = {
    workspaceId: "w",
    channelId: "c",
    directDraftTarget: null,
    threadRootId: null,
    agentPanelId: null,
    humanPanelId: null,
    attachmentPanelId: null,
  };
  const primary = column("c", "w");
  read = {
    workspaceNameById: (id) => id,
    directPairLoadFailed: false,
    directPairNotFound: false,
    parentChannelResolving: false,
    conversationRefsById: new Map(),
    primaryPaneDescriptor: null,
    primary,
    split: null,
    paneOwner: primary,
  };
  state = {
    secondaryDescriptor: null,
    channelIndexesAvailable: true,
    filesListMounted: false,
    threadsListMounted: false,
    taskPaneId: null,
    secondaryViewInputs: selectSecondaryViewInputs({
      secondary: null,
      primaryHasChannelNamespace: false,
      primaryWorkspaceId: "w",
    }),
    secondaryConversationDescriptor: null,
    parentJumpTarget: null,
    secondaryJumpTarget: null,
    secondaryJumpPrimePending: false,
    threadJumpMessageId: null,
    parentJumpPrimePending: false,
    threadJumpPhase: null,
    splitJumpTarget: null,
    splitJumpPrimePending: false,
  };
  commands = {
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
    closeSecondary: vi.fn(),
    handleMessageAgent: vi.fn(),
    handleDraftOpenedPair: vi.fn(),
    handleJumped: vi.fn(),
    handleOpenThreadFromChannel: vi.fn(),
    closeColumn: vi.fn(),
    openSide: vi.fn(),
    toggleThreadsList: vi.fn(),
    toggleFilesList: vi.fn(),
    closeAgentPanel: vi.fn(),
    closeHumanPanel: vi.fn(),
    closeFilesList: vi.fn(),
    openAttachmentPanel: vi.fn(),
    closeAttachmentPanel: vi.fn(),
    closeThread: vi.fn(() => {
      calls.push("close");
    }),
    closeThreadsList: vi.fn(),
    openThread: vi.fn(),
  };
});

/** A column showing `id`, not yet resolved, with unknown membership. */
function column(id: string, workspaceId: string | null): ConversationColumn {
  return {
    column: "primary",
    id,
    descriptor: null,
    status: "loading",
    workspaceId,
    paneTitle: null,
    membership: { membership: "unknown", gate: channelGateFor("unknown") },
    hasChannelNamespace: workspaceId !== null,
    capabilities: inheritedPaneCapabilities(null),
    threadComposerDirectory: { kind: "withheld" },
    pair: null,
  };
}

/** Make the pane-owning column a member showing `descriptor`. */
function ownerShows(d: ConversationPaneDescriptor, membership: "member" | "nonmember") {
  read.paneOwner = {
    ...read.paneOwner!,
    id: d.id,
    descriptor: d,
    membership: { membership, gate: channelGateFor(membership) },
  };
}

const secondary = () => resolveSecondaryPane(route, read, state, commands);
function resolveSide() {
  state.secondaryDescriptor = {
    source: "side",
    status: "resolved",
    id: descriptor.id,
    descriptor,
    hasChannelNamespace: true,
  };
  state.secondaryViewInputs = selectSecondaryViewInputs({
    secondary: state.secondaryDescriptor,
    primaryHasChannelNamespace: false,
    primaryWorkspaceId: "w",
  });
  state.secondaryConversationDescriptor = descriptor;
}

describe("single secondary priority resolver", () => {
  it("selects each overlay in order without invoking any command during construction", () => {
    route.agentPanelId = "a";
    route.threadRootId = "root";
    state.filesListMounted = true;
    state.taskPaneId = "task";
    state.threadsListMounted = true;
    resolveSide();
    expect(secondary().kind).toBe("agent");
    route.agentPanelId = null;
    expect(secondary().kind).toBe("files");
    state.filesListMounted = false;
    expect(secondary().kind).toBe("task");
    state.taskPaneId = null;
    expect(secondary().kind).toBe("conversation");
    state.secondaryDescriptor = null;
    expect(secondary().kind).toBe("thread");
    route.threadRootId = null;
    expect(secondary().kind).toBe("threads");
    state.threadsListMounted = false;
    expect(secondary()).toEqual({ kind: "none" });
    for (const command of Object.values(commands)) {
      expect(command).not.toHaveBeenCalled();
    }
  });
  it.each(["loading", "unauthorized", "not_found", "unresolved"] as const)(
    "keeps an unresolved %s side above thread and threads",
    (status) => {
      route.threadRootId = "root";
      state.threadsListMounted = true;
      state.secondaryDescriptor = { source: "side", status, id: "side-id" };
      const model = secondary();
      expect(model).toMatchObject({
        kind: "conversation",
        status: "unresolved",
        props: { conversationId: "side-id", status },
      });
      if (model.kind !== "conversation" || model.status !== "unresolved") {
        throw new Error("Expected unresolved side");
      }
      model.props.onClose();
      expect(commands.closeSecondary).toHaveBeenCalledTimes(1);
    },
  );
  it("requires a conversation for files/task/thread/threads but not the agent overlay", () => {
    route.channelId = null;
    route.threadRootId = "root";
    state.filesListMounted = true;
    state.taskPaneId = "task";
    state.threadsListMounted = true;
    expect(secondary().kind).toBe("none");
    route.agentPanelId = "a";
    expect(secondary()).toMatchObject({
      kind: "agent",
      props: { agentId: "a", contextConversationId: null },
    });
  });
});

describe("branch-specific data and commands", () => {
  it("isolates draft identity and preserves conversation loading/index gates", () => {
    route.directDraftTarget = { kind: "agent", id: "agent" };
    let model = buildConversationPanes(route, read, state, commands).primary;
    expect(model).toEqual({
      kind: "draft",
      props: { target: { kind: "agent", id: "agent" }, onOpenPair: commands.handleDraftOpenedPair },
    });
    route.directDraftTarget = null;
    read.directPairNotFound = true;
    read.directPairLoadFailed = true;
    read.parentChannelResolving = true;
    model = buildConversationPanes(route, read, state, commands).primary;
    if (model.kind !== "conversation") {
      throw new Error("Expected conversation");
    }
    expect(model.props).toMatchObject({ resolving: true, loadFailed: true, notFound: true });
    expect(model.props.onToggleThreadsList).toBeUndefined();
    expect(model.props.onToggleFilesList).toBeUndefined();
    read.primaryPaneDescriptor = descriptor;
    const ready = buildConversationPanes(route, read, state, commands).primary;
    if (ready.kind !== "conversation") {
      throw new Error("Expected conversation");
    }
    expect(ready.props.conversation).toBe(descriptor);
    ready.props.onToggleThreadsList?.();
    expect(commands.toggleThreadsList).toHaveBeenCalledTimes(1);
  });
  it("gives a foreign side its own descriptor and workspace", () => {
    resolveSide();
    const model = secondary();
    if (model.kind !== "conversation" || model.status !== "resolved") {
      throw new Error("Expected resolved side");
    }
    expect(model.props.conversation).toBe(descriptor);
    expect(model.props.workspaceName).toBe("other-workspace");
    expect(model.props).not.toHaveProperty("nav");
  });
  it("binds files attachments to the selected conversation at model creation", () => {
    state.filesListMounted = true;
    const model = secondary();
    if (model.kind !== "files") {
      throw new Error("Expected files");
    }
    route.channelId = "later";
    const attachment = {
      id: "a",
      filename: "a.md",
      size: 1,
      content_type: "text/markdown",
      created_at: 0,
      channel_id: "c",
      source_conversation_id: null,
      storage: "local" as const,
      content_revision: 1,
      created_by_kind: "user" as const,
      created_by_id: "u",
    };
    model.props.onOpenAttachment?.(attachment);
    expect(commands.openAttachmentPanel).toHaveBeenCalledExactlyOnceWith("a", true);
    model.props.onClose();
    expect(commands.closeFilesList).toHaveBeenCalledTimes(1);
  });
  it("previews inside the files list, backs to it, and expands into the issuing conversation", () => {
    state.filesListMounted = true;
    route.attachmentPanelId = "a";
    const model = secondary();
    if (model.kind !== "files" || !model.preview) {
      throw new Error("Expected files preview");
    }
    route.channelId = "later";
    model.preview.onBack?.();
    expect(commands.closeAttachmentPanel).toHaveBeenCalledTimes(1);
    model.preview.onClose();
    expect(commands.closeFilesList).toHaveBeenCalledTimes(1);
    expect(model.preview.onExpand).toBeUndefined();
  });
  it("expands into the owning column's tab only while the viewer is a member there", () => {
    route.attachmentPanelId = "a";
    ownerShows({ ...descriptor, id: "c", workspace_id: "w" }, "member");
    const model = secondary();
    if (model.kind !== "attachment" || !model.props.onExpand) {
      throw new Error("Expected expandable attachment");
    }
    model.props.onExpand({ id: "a", filename: "a.md", size: 1, content_type: "text/markdown" });
    expect(openConversationTab).toHaveBeenCalledExactlyOnceWith("c", {
      kind: "attachment",
      attachmentId: "a",
      filename: "a.md",
      contentType: "text/markdown",
    });
    expect(commands.closeAttachmentPanel).toHaveBeenCalledTimes(1);
    ownerShows({ ...descriptor, id: "c", workspace_id: "w" }, "nonmember");
    const browsing = secondary();
    expect(browsing.kind === "attachment" && browsing.props.onExpand).toBeFalsy();
  });
  it("shows a standalone attachment preview that closes only itself", () => {
    route.attachmentPanelId = "a";
    route.threadRootId = "root";
    const model = secondary();
    if (model.kind !== "attachment") {
      throw new Error("Expected attachment");
    }
    expect(model.props.attachmentId).toBe("a");
    expect(model.props.onBack).toBeUndefined();
    model.props.onClose();
    expect(commands.closeAttachmentPanel).toHaveBeenCalledTimes(1);
  });
  it("keeps DM thread coordinates, inherited permissions and prime-before-close ordering", () => {
    route.workspaceId = null;
    route.channelId = "dm";
    read.paneOwner = column("dm", null);
    route.threadRootId = "root";
    state.threadJumpMessageId = "reply";
    state.threadJumpPhase = "committed";
    const model = secondary();
    if (model.kind !== "thread") {
      throw new Error("Expected thread");
    }
    expect(model.capabilities).toBe(read.paneOwner!.capabilities);
    expect(model.props.gate).toBe(read.paneOwner!.membership.gate);
    expect(model.props.directory).toBe(read.paneOwner!.threadComposerDirectory);
    expect(model.props).toMatchObject({
      channelId: "dm",
      workspaceId: null,
      rootMessageId: "root",
      jumpMessageId: "reply",
      jumpPhase: "committed",
    });
    model.props.onViewInChannel?.();
    expect(commands.requestJump).toHaveBeenCalledExactlyOnceWith({
      workspaceId: undefined,
      channelId: "dm",
      messageId: "root",
    });
    expect(calls).toEqual(["prime", "close"]);
    const attachment = {
      id: "a",
      filename: "a.md",
      size: 1,
      content_type: "text/markdown",
      created_at: 0,
      channel_id: "c",
      source_conversation_id: null,
      storage: "local" as const,
      content_revision: 1,
      created_by_kind: "user" as const,
      created_by_id: "u",
    };
    model.props.onOpenAttachment?.(attachment);
    expect(commands.openAttachmentPanel).toHaveBeenCalledExactlyOnceWith("a");
    expect(model.props.onExpandAttachment).toBeUndefined();
  });
  it("routes task closing through the editing guard and lists through their own close/open commands", () => {
    state.taskPaneId = "task";
    const task = secondary();
    if (task.kind !== "task") {
      throw new Error("Expected task");
    }
    expect(task.props.onClose).toBe(commands.requestCloseTaskPane);
    task.props.onEditingChange?.(true);
    expect(commands.setTaskPaneEditing).toHaveBeenCalledExactlyOnceWith(true);
    state.taskPaneId = null;
    state.threadsListMounted = true;
    const list = secondary();
    if (list.kind !== "threads") {
      throw new Error("Expected threads");
    }
    list.props.onOpenThread("root");
    list.props.onClose();
    expect(commands.openThread).toHaveBeenCalledExactlyOnceWith("root");
    expect(commands.closeThreadsList).toHaveBeenCalledTimes(1);
  });
  it("does not expose unrelated pane fields through the branch contract", () => {
    type Task = Extract<SecondaryPaneModel, { kind: "task" }>;
    type Files = Extract<SecondaryPaneModel, { kind: "files" }>;
    expectTypeOf<Task["props"]>().not.toHaveProperty("channelId");
    expectTypeOf<Task["props"]>().not.toHaveProperty("onMessageAgent");
    expectTypeOf<Files["props"]>().not.toHaveProperty("taskId");
    expectTypeOf<Files["props"]>().not.toHaveProperty("nav");
  });
});

describe("human profile pane", () => {
  it("keeps the exact subject and closes only the profile overlay", () => {
    route.humanPanelId = "other-human";
    route.threadRootId = "thread-root";
    const model = secondary();

    expect(model.kind).toBe("human");
    if (model.kind !== "human") {
      throw new Error("Expected human pane");
    }

    expect(model.props.userId).toBe("other-human");
    model.props.onClose?.();
    expect(commands.closeHumanPanel).toHaveBeenCalledTimes(1);
    expect(commands.closeThread).not.toHaveBeenCalled();
  });
});

it("binds settings to the explicit channel and clears them on ordinary navigation", () => {
  route.channelSettingsId = "foreign";
  route.threadRootId = "root";
  expect(secondary()).toEqual({
    kind: "settings",
    channelId: "foreign",
    onDepartChannel: commands.departPrimaryPane,
  });
  route.channelId = "next";
  route.channelSettingsId = null;
  route.threadRootId = null;
  expect(secondary()).toEqual({ kind: "none" });
});

describe("split view columns", () => {
  const splitDescriptor = { ...descriptor, id: "s", workspace_id: "w2", title: "Second" };
  beforeEach(() => {
    read.primaryPaneDescriptor = { ...descriptor, id: "c", workspace_id: "w" };
    read.split = {
      ...column("s", "w2"),
      column: "split",
      descriptor: splitDescriptor,
      status: "resolved",
      paneTitle: "Second",
    };
  });

  it("gives both columns a close button and opens each column's panes in that column", () => {
    const model = buildConversationPanes(route, read, state, commands);
    if (model.primary.kind !== "conversation" || model.split?.kind !== "conversation") {
      throw new Error("Expected two columns");
    }
    model.split.props.onOpenThread?.("root");
    expect(commands.handleOpenThreadFromChannel).toHaveBeenCalledWith("root", undefined, "split");
    model.split.props.onOpenAttachmentPanel?.("a");
    expect(commands.openAttachmentPanel).toHaveBeenCalledWith("a", false, "split");
    model.primary.props.onCloseChild?.();
    model.split.props.onCloseChild?.();
    expect(vi.mocked(commands.closeColumn).mock.calls).toEqual([["primary"], ["split"]]);
  });

  it("marks the open index pane active only on the column that owns it", () => {
    state.threadsListMounted = true;
    read.paneOwner = read.split;
    const model = buildConversationPanes(route, read, state, commands);
    if (model.primary.kind !== "conversation" || model.split?.kind !== "conversation") {
      throw new Error("Expected two columns");
    }
    expect(model.primary.props.threadsListActive).toBe(false);
    expect(model.split.props.threadsListActive).toBe(true);
  });

  it("resolves the right pane against the owning column", () => {
    route.threadRootId = "root";
    read.paneOwner = read.split;
    const model = secondary();
    if (model.kind !== "thread") {
      throw new Error("Expected thread");
    }
    expect(model.props).toMatchObject({ channelId: "s", workspaceId: "w2", parentTitle: "Second" });
  });

  it("shows a placeholder for a second column that has not resolved", () => {
    read.split = { ...read.split!, descriptor: null, status: "not_found" };
    const model = buildConversationPanes(route, read, state, commands).split;
    expect(model).toMatchObject({ kind: "unresolved", props: { status: "not_found" } });
  });
});
