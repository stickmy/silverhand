import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ChannelListItem, WorkspaceListItem } from "@/features/workspaces/hooks";
import type { RouteState } from "@/router";
import { makeRouteState } from "@/test-support/route-state";

const h = vi.hoisted(() => ({
  compact: false,
  route: {} as RouteState,

  effects: [] as Array<() => void>,
  removal: undefined as undefined | ((id: string) => void),
  go: vi.fn(),
  toChannel: vi.fn(),
  toWorkspace: vi.fn(),
  persist: vi.fn(),
  memory: new Map<string, string>(),
}));
vi.mock("@/navigation/layout", () => ({ useCompactLayout: () => h.compact }));
vi.mock("react", async (importOriginal) => ({
  ...(await importOriginal<typeof import("react")>()),
  useCallback: <T>(cb: T) => cb,
  useRef: <T>(current: T) => ({ current }),
  useEffect: (effect: () => void) => {
    h.effects.push(effect);
  },
}));
vi.mock("@/router", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/router")>()),
  useRouteState: () => h.route,
  useNav: () => ({ go: h.go, toChannel: h.toChannel, toWorkspace: h.toWorkspace }),
}));
vi.mock("@/features/channels/use-viewer-channel-removal-sync", () => ({
  useViewerChannelRemovalSync: (p: { onVisibilityLoss: (id: string) => void }) => {
    h.removal = p.onVisibilityLoss;
  },
  useForeignWorkspaceChannelRemovalSync: vi.fn(),
}));
vi.mock("@/features/app-shell/selection-persistence", () => ({
  useSelectionPersistenceEffects: vi.fn(),
}));
vi.mock("@/features/app-shell/place-memory", () => ({
  lastChannelByWorkspace: h.memory,
  persistLastChannel: h.persist,
  getLastWorkspaceId: () => null,
  getLastChatPlace: () => null,
  getChannelLayout: () => null,
  getCloseWatermark: () => null,
  recordChannelLayout: vi.fn(),
  recordCloseWatermark: vi.fn(),
}));
import { useConversationInvalidation } from "./use-conversation-invalidation";
import { useChatRouteRecovery } from "@/features/app-shell/use-chat-route-recovery";

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
const workspace: WorkspaceListItem = {
  id: "w",
  name: "Workspace",
  description: null,
  created_at: 0,
  updated_at: 0,
  archived_at: null,
  owner_agent_id: null,
  dm_channel_ids: [],
  workspace_scope_ids: ["w"],
};
function mountInvalidation(ids = ["c"]) {
  const channels = ids.map(channel);
  return useConversationInvalidation({
    activeChannels: channels,
    archivedChannels: [],
    memberActiveChannels: channels,
    activeSettled: true,
    archivedSettled: true,
    visibleWorkspaceIds: ["w"],
  });
}
function recover(
  removed: { current: { workspaceId: string; channelId: string } | null },
  ids = ["c"],
) {
  useChatRouteRecovery({
    workspaces: [workspace],
    channels: ids.map(channel),
    memberActiveChannels: ids.map(channel),
    messageUnreadData: undefined,
    directPairAuthority: { status: "ready", conversationIds: new Set() },
    jumpTarget: null,
    onJump: vi.fn(),
    recentlyRemovedPrimaryRef: removed,
    compact: h.compact,
  });
}
beforeEach(() => {
  h.compact = false;
  vi.clearAllMocks();
  h.effects = [];
  h.removal = undefined;
  h.memory.clear();
  Object.assign(h.route, makeRouteState("/chat/w/w/c/c"));
});

describe("shared conversation invalidation", () => {
  it.each(["settings-departure", "external"])(
    "uses the same empty fallback and stale guard for %s",
    async (origin) => {
      h.memory.set("w", "c");
      const owner = mountInvalidation();
      if (origin === "settings-departure") {
        owner.departPrimaryPane("c");
      } else {
        h.removal?.("c");
      }
      expect(owner.recentlyRemovedPrimaryRef.current).toEqual({ workspaceId: "w", channelId: "c" });
      expect(h.memory.has("w")).toBe(false);
      expect(h.persist).toHaveBeenCalledTimes(1);
      expect(h.toWorkspace).toHaveBeenCalledExactlyOnceWith("w");
      h.route.channelId = null;
      h.effects = [];
      recover(owner.recentlyRemovedPrimaryRef);
      h.effects.forEach((e) => e());
      expect(h.go).not.toHaveBeenCalled();
    },
  );
  it("selects one member neighbor without clearing unrelated memory", () => {
    const owner = mountInvalidation(["c", "next"]);
    owner.departPrimaryPane("c");
    expect(h.toChannel).toHaveBeenCalledExactlyOnceWith("w", "next");
    expect(h.persist).not.toHaveBeenCalled();
    expect(owner.recentlyRemovedPrimaryRef.current).toBeNull();
  });
  it("does not move the primary when a secondary channel departs", () => {
    const owner = mountInvalidation();
    owner.departPrimaryPane("other");
    expect(h.toChannel).not.toHaveBeenCalled();
    expect(h.toWorkspace).not.toHaveBeenCalled();
  });
});
describe("Chat recovery boundary", () => {
  it("keeps the compact conversation list open instead of restoring a desktop selection", () => {
    h.compact = true;
    h.route.channelId = null;
    recover({ current: null });
    h.effects.forEach((e) => e());
    expect(h.go).not.toHaveBeenCalled();
  });
  it("does not auto-select an Activity conversation", () => {
    h.route.section = "activity";
    h.route.channelId = null;
    recover({ current: null });
    h.effects.forEach((e) => e());
    expect(h.go).not.toHaveBeenCalled();
  });
  it("restores the same available channel in Chat", () => {
    h.route.channelId = null;
    recover({ current: null });
    h.effects.forEach((e) => e());
    expect(h.go).toHaveBeenCalledExactlyOnceWith("/chat/w/w/c/c", { replace: true });
  });
});
