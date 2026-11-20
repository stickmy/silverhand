import { channelGateFor } from "@/features/channels/membership";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { DirectPairListItem } from "@silverhand/shared";
import type { ChannelListItem, WorkspaceListItem } from "@/features/workspaces/hooks";
import type { ConversationIdentityResolution } from "@/features/conversations/conversation-identity";
import type { PaneJumpModel, PaneReadModel } from "./conversation-shell-models";
import type { RouteState } from "@/router";
import { makeRouteState } from "@/test-support/route-state";

const h = vi.hoisted(() => ({
  compact: false,
  leaveGuard: undefined as (() => boolean) | undefined,
  route: {} as RouteState,

  workspaces: [] as WorkspaceListItem[],
  active: [] as ChannelListItem[],
  archived: [] as ChannelListItem[],
  fetching: false,
  loading: false,
  pairRows: [] as DirectPairListItem[] | undefined,
  pairError: false,
  deferred: undefined as string | null | undefined,
  side: null as ConversationIdentityResolution | null,
  reads: vi.fn(),
  detail: vi.fn(),
  activeQuery: vi.fn(),
  archivedQuery: vi.fn(),
  closeTask: vi.fn(),
  closeSide: vi.fn(),
  openTask: vi.fn(),
  openThread: vi.fn(),
  closeResponder: vi.fn(),
  slots: [] as unknown[],
  cursor: 0,
  effects: [] as Array<() => void>,
}));

vi.mock("@/navigation/provider", () => ({
  useNavigationDriver: () => ({
    registerLeaveGuard: (guard: () => boolean) => {
      h.leaveGuard = guard;
      return () => {
        h.leaveGuard = undefined;
      };
    },
  }),
}));
vi.mock("@/navigation/layout", () => ({ useCompactLayout: () => h.compact }));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useMemo: <T>(fn: () => T) => fn(),
  useCallback: <T>(fn: T) => fn,
  useDeferredValue: <T>(value: T) => (h.deferred === undefined ? value : h.deferred),
  useRef: <T>(value: T) => {
    const index = h.cursor++;
    h.slots[index] ??= { current: value };
    return h.slots[index];
  },
  useState: <T>(value: T) => {
    const index = h.cursor++;
    if (!(index in h.slots)) {
      h.slots[index] = value;
    }
    return [
      h.slots[index],
      (next: T) => {
        h.slots[index] = next;
      },
    ];
  },
  useEffect: (effect: () => void, deps: unknown[]) => {
    const index = h.cursor++;
    const previous = h.slots[index] as unknown[] | undefined;
    if (!previous || deps.some((v, i) => !Object.is(v, previous[i]))) {
      h.slots[index] = deps;
      h.effects.push(effect);
    }
  },
}));
vi.mock("@/router", async (original) => ({
  ...(await original<typeof import("@/router")>()),
  useRouteState: () => h.route,
  useNav: () => ({
    closeTask: h.closeTask,
    closeSide: h.closeSide,
    openTask: h.openTask,
    openThread: h.openThread,
  }),
}));
vi.mock("@/features/workspaces/hooks", () => ({
  useWorkspaces: () => ({ data: h.workspaces }),
  useChannelsForWorkspace: (id: string | null, enabled: boolean) => {
    h.activeQuery(id, enabled);
    return {
      data: h.active,
      status: "success",
      isLoading: h.loading,
      fetchStatus: h.fetching ? "fetching" : "idle",
    };
  },
  useArchivedChannelsForWorkspace: (id: string | null, enabled: boolean) => {
    h.archivedQuery(id, enabled);
    return { data: h.archived, status: "success", fetchStatus: "idle" };
  },
  useChannelDetail: (id: string | null, enabled: boolean) => {
    h.detail(id, enabled);
    return { data: undefined };
  },
}));
vi.mock("@/features/direct-pairs/hooks", async (original) => {
  const actual = await original<typeof import("@/features/direct-pairs/hooks")>();
  return {
    ...actual,
    useDirectPairsList: () => ({
      data: h.pairRows ? { direct_pairs: h.pairRows } : undefined,
      isFetching: h.fetching,
      isError: h.pairError,
    }),
    useDirectPairPane: (id: string | null) =>
      actual.projectDirectPairPane(id, {
        rows: h.pairRows,
        isPending: !h.pairRows && !h.pairError,
        isFetching: h.fetching,
        isError: h.pairError,
      }),
  };
});
vi.mock("@/features/conversations/conversation-identity", async (original) => ({
  ...(await original<typeof import("@/features/conversations/conversation-identity")>()),
  useConversationIdentity: (id: string | null) => (id === null ? { status: "loading" } : h.side),
}));
vi.mock("@/features/channels/hooks", () => ({
  useMarkMessageChannelRead: () => ({ mutate: h.reads }),
}));
vi.mock("@/features/app-shell/keyboard-shortcuts", () => ({
  useContextualCloseResponder: (...args: unknown[]) => h.closeResponder(...args),
}));

import { useConversationReadModel } from "./use-conversation-read-model";
import { useConversationPanes } from "./use-conversation-panes";

const channel = (id: string): ChannelListItem => ({
  id,
  workspace_id: "w",
  placement_kind: "workspace",
  roster_policy: "open",
  title: id,
  status: "active",
  spawned_by_agent_id: null,
  closed_at: null,
  close_summary: null,
  archived_at: null,
  created_at: 0,
  updated_at: 0,
  visibility: "private",
  is_member: true,
});
const pair: DirectPairListItem = {
  conversation_id: "dm",
  created_at: 0,
  read_only: true,
  unread_count: 0,
  counterpart: {
    id: "u",
    name: "u",
    display_name: "User",
    kind: "human",
    runtime: null,
    avatar_url: null,
    status: "active",
  },
};
const jump: PaneJumpModel = {
  jumpTarget: null,
  isPrimePendingFor: () => false,
  primePhaseFor: () => null,
  handleJumpToMessage: vi.fn(() => true),
};
function render(
  read: PaneReadModel = {
    primaryPointer: { id: "c", rosterPolicy: "open" },
    hasChannelNamespace: true,
    deferredChannelId: "c",
    paired: null,
  },
  nextJump = jump,
) {
  h.cursor = 0;
  const model = useConversationPanes(read, nextJump, { compact: h.compact });
  h.effects.splice(0).forEach((effect) => effect());
  return model;
}
beforeEach(() => {
  h.compact = false;
  vi.clearAllMocks();
  Object.assign(h.route, makeRouteState("/chat/w/w/c/c"));
  h.workspaces = [
    {
      id: "w",
      name: "Workspace",
      description: null,
      created_at: 0,
      updated_at: 0,
      archived_at: null,
      owner_agent_id: null,
      dm_channel_ids: [],
      workspace_scope_ids: ["w"],
    },
  ];
  h.active = [channel("c")];
  h.archived = [];
  h.fetching = false;
  h.loading = false;
  h.pairRows = [];
  h.pairError = false;
  h.deferred = undefined;
  h.side = null;
  h.slots = [];
  h.cursor = 0;
  h.effects = [];
});

describe("conversation authority read model", () => {
  it("keeps current membership separate from the deferred descriptor and settled list facts", () => {
    h.active.push({ ...channel("new"), is_member: false });
    h.route.channelId = "new";
    h.deferred = "c";
    h.fetching = true;
    const model = useConversationReadModel();
    expect(model.selectedChannel?.id).toBe("new");
    expect(model.parentChannelMembership).toBe("nonmember");
    expect(model.primaryPaneDescriptor?.id).toBe("c");
    expect(model.activeSettled).toBe(false);
    expect(model.archivedSettled).toBe(true);
    expect(model.memberActiveChannels.map((c) => c.id)).toEqual(["c"]);
    expect(h.activeQuery).toHaveBeenCalledExactlyOnceWith("w", true);
    expect(h.reads).not.toHaveBeenCalled();
  });
  it("uses the workspace-less pair for pointer, permissions and composer directory", () => {
    h.route.workspaceId = null;
    h.route.channelId = "dm";
    h.route.directConversationId = "dm";
    h.pairRows = [pair];
    const model = useConversationReadModel();
    expect(model.primaryPointer).toEqual({ id: "dm", rosterPolicy: "fixed" });
    expect(model.primaryPaneDescriptor?.workspace_id).toBeNull();
    expect(model.primary?.hasChannelNamespace).toBe(false);
    expect(model.primary?.threadComposerDirectory.kind).toBe("viewer");
    expect(model.parentMembership.gate).toEqual({
      allowMutations: false,
      allowMemberEffects: true,
      showComposer: true,
      showJoin: false,
    });
    expect(h.detail).toHaveBeenCalledWith("dm", false);
    expect(h.activeQuery).toHaveBeenCalledWith(null, false);
    expect(h.reads).not.toHaveBeenCalled();
  });
  it.each(["loading", "error", "absent"] as const)(
    "preserves pair %s as a distinct load result",
    (state) => {
      h.route.workspaceId = null;
      h.route.channelId = "dm";
      h.route.directConversationId = "dm";
      h.pairRows = state === "absent" ? [] : undefined;
      h.pairError = state === "error";
      const model = useConversationReadModel();
      expect(model.parentChannelResolving).toBe(state === "loading");
      expect(model.directPairLoadFailed).toBe(state === "error");
      expect(model.directPairNotFound).toBe(state === "absent");
      expect(model.primaryPointer).toBeNull();
    },
  );
});

describe("conversation pane coordination", () => {
  it("shows every conversation pane and open thread side by side", () => {
    h.route.panes = [
      { kind: "threads", of: "c", thread: "root" },
      { kind: "conversation", id: "dm" },
      { kind: "agent", id: "a" },
    ];
    const { screen } = render();
    expect(screen.conversations).toEqual(["c", "dm"]);
    expect(screen.threads).toEqual([{ conversationId: "c", rootId: "root" }]);
    expect(h.reads.mock.calls).toEqual([["c"]]);
  });

  it("on compact, only the last pane is on screen", () => {
    h.compact = true;
    h.route.panes = [
      { kind: "threads", of: "c", thread: "root" },
      { kind: "agent", id: "a" },
    ];
    const { screen } = render();
    expect(screen).toEqual({ conversations: [], threads: [] });
    expect(h.reads).not.toHaveBeenCalled();
  });

  it("marks the route conversation once while it stays on screen", () => {
    render();
    render();
    expect(h.reads.mock.calls).toEqual([["c"]]);
  });

  it("guards task close while editing", () => {
    h.route.panes = [{ kind: "task", id: "task" }];
    const model = render();
    model.setTaskPaneEditing(true);
    expect(h.leaveGuard?.()).toBe(false);
    model.requestCloseTaskPane();
    expect(h.closeTask).not.toHaveBeenCalled();
    model.setTaskPaneEditing(false);
    expect(h.leaveGuard?.()).toBe(true);
    model.requestCloseTaskPane();
    expect(h.closeTask).toHaveBeenCalledTimes(1);
    h.route.section = "tasks";
    expect(render().taskPaneId).toBeNull();
  });

  it("jumps to a reply of the route conversation, and opens one elsewhere in place", () => {
    const model = render();
    model.handleOpenThreadFromChannel("root", "reply", "c");
    expect(jump.handleJumpToMessage).toHaveBeenCalledExactlyOnceWith({
      workspaceId: "w",
      channelId: "c",
      threadRootId: "root",
      messageId: "reply",
    });
    model.handleOpenThreadFromChannel("root", "reply", "dm");
    expect(h.openThread).toHaveBeenCalledExactlyOnceWith("root", "dm");
  });
});
