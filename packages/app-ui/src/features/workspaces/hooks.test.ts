import { describe, expect, it } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import type {
  ActivityItemDto,
  ChannelConversationDto,
  FavoriteRowDto,
  ListActivityDto,
  ListFavoritesDto,
  ViewerSortStateDto,
  WorkspaceHumanMemberDto,
  WorkspaceMembersResponse,
} from "@silverhand/shared";
import {
  applyOrderToViewerSort,
  decideViewerSyncInvalidations,
  isViewerWorkspacesChangedPayload,
  patchHumanMemberRole,
  removeHumanMemberFromList,
  setChannelStarredOptimistic,
  sortCacheAdapter,
  sortConversationList,
  viewerRemovedWorkspaceId,
  type ChannelListItem,
  type ViewerSyncFrame,
} from "./hooks";
import {
  decideFavoritesInvalidation,
  favoritesKey,
  removeWorkspaceFromFavorites,
} from "./favorites";
import { activityKey, removeWorkspaceFromActivity } from "../attention/hooks";
import {
  messageUnreadByConversationKey,
  messageUnreadSummaryKey,
  removeWorkspaceFromMessageUnread,
} from "../channels/message-unread";

function channel(id: string, createdAt: number): ChannelConversationDto {
  return {
    id,
    workspace_id: "w1",
    placement_kind: "workspace",
    roster_policy: "open",
    title: id,
    status: "idle",
    spawned_by_agent_id: null,
    closed_at: null,
    close_summary: null,
    archived_at: null,
    created_at: createdAt,
    updated_at: createdAt,
    visibility: "private",
  };
}

describe("sortConversationList", () => {
  it("orders channels by created_at", () => {
    // Star is no longer a shared field on the conversation (#task:1209), so the
    // base sort is creation-time only. Under the exclusive model (#task:1254) a
    // starred channel is moved to STARRED and excluded from its workspace's
    // single `channels` home group (composed in WorkspaceChannels), not here.
    const first = channel("first", 1);
    const second = channel("second", 2);
    const third = channel("third", 3);

    const sorted = sortConversationList([third, first, second] satisfies ChannelListItem[]);

    expect(sorted.map((item) => item.id)).toEqual([first.id, second.id, third.id]);
  });
});

describe("setChannelStarredOptimistic", () => {
  const vs = (entries: ViewerSortStateDto["entries"]): ViewerSortStateDto => ({
    list_key: "channels:w1",
    modes: {},
    entries,
  });

  it("star PRESERVES the home rank (#task:1888 — STARRED is a different list)", () => {
    // A channel's star is an exclusive MOVE into `list_key='favorites'`, so its
    // `channels:*` rank survives as an orphan and unstar drops it back into the slot
    // it had. No caller clears rank any more — the backend's `SortRepo.clearRank` is
    // gone as well.
    const next = setChannelStarredOptimistic(
      vs([{ entity_id: "c", rank: 4, last_accessed_at: 100, starred_at: null }]),
      "c",
      true,
    );
    expect(next.entries[0]).toMatchObject({
      entity_id: "c",
      rank: 4,
      last_accessed_at: 100,
    });
    expect(next.entries[0]?.starred_at).not.toBeNull();
  });

  it("unstar restores the original home slot (rank intact)", () => {
    const next = setChannelStarredOptimistic(
      vs([{ entity_id: "c", rank: 2, last_accessed_at: 7, starred_at: 999 }]),
      "c",
      false,
    );
    expect(next.entries[0]).toEqual({
      entity_id: "c",
      rank: 2,
      last_accessed_at: 7,
      starred_at: null,
    });
  });

  it("creates a rank-null entry when the viewer had no prior state", () => {
    const next = setChannelStarredOptimistic(vs([]), "b", true);
    expect(next.entries).toHaveLength(1);
    expect(next.entries[0]).toMatchObject({
      entity_id: "b",
      rank: null,
      last_accessed_at: null,
    });
    expect(next.entries[0]?.starred_at).not.toBeNull();
  });

  it("leaves other entries untouched", () => {
    const next = setChannelStarredOptimistic(
      vs([
        { entity_id: "a", rank: 1, last_accessed_at: null, starred_at: null },
        { entity_id: "b", rank: 2, last_accessed_at: null, starred_at: null },
      ]),
      "a",
      true,
    );
    expect(next.entries[1]).toEqual({
      entity_id: "b",
      rank: 2,
      last_accessed_at: null,
      starred_at: null,
    });
  });
});

describe("Favorites sort cache wiring (#task:1254)", () => {
  // Regression for Cindy's P1: the generic sort mutations resolve a list's cache
  // through `sortCacheAdapter`. Favorites was UNRESOLVED (sortListQueryKey →
  // null), so setMode/reorder wrote the backend but silently no-op'd the local
  // cache — the UI only caught up on a later refetch. The adapter also has to
  // bridge the cache SHAPE: channel caches store `viewerSort` (camelCase);
  // the Favorites cache (ListFavoritesDto) stores `viewer_sort` (snake_case).
  const vs = (
    entries: ViewerSortStateDto["entries"],
    modes: ViewerSortStateDto["modes"] = {},
  ): ViewerSortStateDto => ({ list_key: "favorites", modes, entries });

  it("resolves favorites to favoritesKey and reads/writes the snake_case viewer_sort field", () => {
    const adapter = sortCacheAdapter("favorites");
    expect(adapter).not.toBeNull();
    expect(adapter!.key).toEqual(favoritesKey);

    const cache = { items: [], viewer_sort: vs([], { "": "recent" }) };
    expect(adapter!.read(cache)).toEqual(cache.viewer_sort);

    const next = vs([], { "": "manual" });
    const written = adapter!.write(cache, next) as typeof cache;
    expect(written.viewer_sort).toEqual(next);
    // items preserved; no stray camelCase field leaks into the favorites shape.
    expect(written.items).toBe(cache.items);
    expect("viewerSort" in written).toBe(false);
  });

  it("keeps channel sort caching and retires workspace sorting", () => {
    for (const listKey of ["channels:w1"]) {
      const adapter = sortCacheAdapter(listKey);
      expect(adapter).not.toBeNull();
      const cache = { items: [], viewerSort: vs([], { "": "recent" }) };
      expect(adapter!.read(cache)).toEqual(cache.viewerSort);
      const next = vs([], { "": "manual" });
      const written = adapter!.write(cache, next) as typeof cache;
      expect(written.viewerSort).toEqual(next);
      expect("viewer_sort" in written).toBe(false);
    }
    expect(sortCacheAdapter("workspaces")).toBeNull();
    expect(sortCacheAdapter("channels:w1")!.key).toEqual(["workspaces", "w1", "channels"]);
  });

  it("applyOrderToViewerSort writes dense 0..n ranks and flips the list-wide scope to manual", () => {
    // Favorites keys are `conversation:<conversation_id>` only — a container
    // can never appear here (#task:1888 / #task:1900).
    const next = applyOrderToViewerSort(
      vs(
        [
          { entity_id: "conversation:c1", rank: null, last_accessed_at: 5, starred_at: 1 },
          { entity_id: "conversation:c2", rank: null, last_accessed_at: 9, starred_at: 2 },
        ],
        { "": "recent" },
      ),
      ["conversation:c2", "conversation:c1"],
    );
    expect(next.modes[""]).toBe("manual");
    const byId = new Map(next.entries.map((e) => [e.entity_id, e]));
    expect(byId.get("conversation:c2")?.rank).toBe(0);
    expect(byId.get("conversation:c1")?.rank).toBe(1);
    // non-rank fields ride through untouched.
    expect(byId.get("conversation:c2")?.last_accessed_at).toBe(9);
  });

  it("a favorites cache write lands on favoritesKey and never touches workspaces/channels caches", () => {
    const client = new QueryClient();
    const workspacesCache = { items: [] };
    const channelsCache = { items: [], viewerSort: vs([], { "": "recent" }) };
    client.setQueryData(["workspaces"], workspacesCache);
    client.setQueryData(["workspaces", "w1", "channels"], channelsCache);
    client.setQueryData(favoritesKey, {
      items: [],
      viewer_sort: vs(
        [{ entity_id: "conversation:c1", rank: 0, last_accessed_at: 5, starred_at: 1 }],
        { "": "recent" },
      ),
    });

    // setMode write (mirrors useSetSortMode.onMutate's write body).
    const modeAdapter = sortCacheAdapter("favorites")!;
    const prevMode = client.getQueryData(modeAdapter.key);
    const prevModeVs = modeAdapter.read(prevMode)!;
    client.setQueryData(
      modeAdapter.key,
      modeAdapter.write(prevMode, {
        ...prevModeVs,
        modes: { ...prevModeVs.modes, [""]: "manual" },
      }),
    );

    // reorder write (mirrors useApplySortOrder).
    const orderAdapter = sortCacheAdapter("favorites")!;
    const prevOrder = client.getQueryData(orderAdapter.key);
    const prevOrderVs = orderAdapter.read(prevOrder)!;
    client.setQueryData(
      orderAdapter.key,
      orderAdapter.write(prevOrder, applyOrderToViewerSort(prevOrderVs, ["conversation:c1"])),
    );

    const favAfter = client.getQueryData<{ viewer_sort: ViewerSortStateDto }>(favoritesKey)!;
    expect(favAfter.viewer_sort.modes[""]).toBe("manual");
    expect(favAfter.viewer_sort.entries.find((e) => e.entity_id === "conversation:c1")?.rank).toBe(
      0,
    );

    // Independent axis: the home caches are byte-identical (same object ref) —
    // favorites reorder/mode can't perturb a workspace/channel's home rank.
    expect(client.getQueryData(["workspaces"])).toBe(workspacesCache);
    expect(client.getQueryData(["workspaces", "w1", "channels"])).toBe(channelsCache);
  });
});

describe("isViewerWorkspacesChangedPayload", () => {
  it("accepts the four documented change variants with a workspace_id", () => {
    for (const change of ["added", "removed", "role_changed", "metadata_changed"] as const) {
      expect(isViewerWorkspacesChangedPayload({ change, workspace_id: "w-1" })).toBe(true);
    }
    // role is optional and not required for the guard to pass
    expect(
      isViewerWorkspacesChangedPayload({
        change: "role_changed",
        workspace_id: "w-1",
        role: "admin",
      }),
    ).toBe(true);
  });

  it("rejects unknown change values and missing/typed-wrong workspace_id", () => {
    expect(isViewerWorkspacesChangedPayload({ change: "mutated", workspace_id: "w-1" })).toBe(
      false,
    );
    expect(isViewerWorkspacesChangedPayload({ change: "added" })).toBe(false);
    expect(isViewerWorkspacesChangedPayload({ change: "added", workspace_id: 42 })).toBe(false);
  });

  it("rejects non-object payloads (no duck-typing)", () => {
    expect(isViewerWorkspacesChangedPayload(null)).toBe(false);
    expect(isViewerWorkspacesChangedPayload(undefined)).toBe(false);
    expect(isViewerWorkspacesChangedPayload("added")).toBe(false);
    expect(isViewerWorkspacesChangedPayload(123)).toBe(false);
  });
});

describe("decideViewerSyncInvalidations", () => {
  const frame = (over: Partial<ViewerSyncFrame>): ViewerSyncFrame => ({
    type: "event",
    scope: "viewer:u-1",
    ...over,
  });

  it("invalidates the workspace list on snapshot_required (replay-gap safety)", () => {
    expect(decideViewerSyncInvalidations(frame({ type: "snapshot_required" }))).toEqual([
      ["workspaces"],
    ]);
  });

  it("also invalidates the attention roll-up and STARRED on add (re-join restores a mute and a star)", () => {
    // #task:1870 — the badge NUMBER of a just-joined workspace is necessarily 0,
    // which is why this used to be list-only. But the roll-up also carries
    // `muted_workspace_ids`, and a mute preference outlives a membership removal
    // so a re-join brings the id back with every number still 0. Without this the
    // rail's muted marker stays missing until an unrelated attention event lands.
    // #task:1889 — a STAR outlives the membership the same way, and a re-join emits
    // no `workspace_channels_changed`, so the favorites list is the third
    // projection of this one fact (workspace remove → re-add).
    expect(
      decideViewerSyncInvalidations(
        frame({
          kind: "viewer_workspaces_changed",
          payload: { change: "added", workspace_id: "w-1" },
        }),
      ),
    ).toEqual([["workspaces"], ["attention", "badges"], favoritesKey]);
  });

  it("invalidates only the workspace list on remove — the purge is the removal contract's", () => {
    expect(
      decideViewerSyncInvalidations(
        frame({
          kind: "viewer_workspaces_changed",
          payload: { change: "removed", workspace_id: "w-1" },
        }),
      ),
    ).toEqual([["workspaces"]]);
  });

  it("invalidates the workspace list on metadata_changed so the joined label refreshes (§4-B, #task:2050)", () => {
    // The producer (`routes/workspaces.ts` rename) fans this to every member;
    // F1 replaced Saved/Task's frozen `workspace_name` with a live join off
    // `['workspaces']`, so this frame MUST refetch the list or the joined label
    // stays stale for every NON-actor member — the exact liveness the embed
    // lacked, moved to the join. If a future edit folds `metadata_changed` back
    // into the guard's reject (→ `[]`), this reddens.
    expect(
      decideViewerSyncInvalidations(
        frame({
          kind: "viewer_workspaces_changed",
          payload: { change: "metadata_changed", workspace_id: "w-1" },
        }),
      ),
    ).toEqual([["workspaces"]]);
  });

  it("also invalidates the members query on role_changed (governance viewerRole source)", () => {
    expect(
      decideViewerSyncInvalidations(
        frame({
          kind: "viewer_workspaces_changed",
          payload: { change: "role_changed", workspace_id: "w-1", role: "admin" },
        }),
      ),
    ).toEqual([["workspaces"], ["workspaces", "w-1", "members"]]);
  });

  it("ignores other event kinds and malformed payloads", () => {
    expect(
      decideViewerSyncInvalidations(
        frame({ kind: "unread_updated", payload: { conversation_id: "c-1" } }),
      ),
    ).toEqual([]);
    expect(
      decideViewerSyncInvalidations(
        frame({ kind: "viewer_workspaces_changed", payload: { change: "nope" } }),
      ),
    ).toEqual([]);
    expect(
      decideViewerSyncInvalidations(frame({ kind: "viewer_workspaces_changed", payload: null })),
    ).toEqual([]);
  });

  it("ignores frame types that are neither event nor snapshot_required", () => {
    expect(decideViewerSyncInvalidations(frame({ type: "subscribed" }))).toEqual([]);
    expect(decideViewerSyncInvalidations(frame({ type: "ping" }))).toEqual([]);
  });
});

// #task:1404 — delete-workspace ghost unread. The `['workspaces']` refetch above
// rebuilds the list but never touches the SEPARATE message-unread caches, so the
// removed workspace's slice lingers as a ghost badge. `viewerRemovedWorkspaceId`
// is the pure decision (which frames trigger the purge); `removeWorkspaceFromMessageUnread`
// is the targeted cache op — tested directly since the node env can't render the hook.
describe("viewerRemovedWorkspaceId (#task:1404)", () => {
  const frame = (over: Partial<ViewerSyncFrame>): ViewerSyncFrame => ({
    type: "event",
    scope: "viewer:u-1",
    ...over,
  });

  it("returns the workspace_id only for a removed viewer_workspaces_changed event", () => {
    expect(
      viewerRemovedWorkspaceId(
        frame({
          kind: "viewer_workspaces_changed",
          payload: { change: "removed", workspace_id: "w-1" },
        }),
      ),
    ).toBe("w-1");
  });

  it("returns null for added / role_changed / metadata_changed (nothing removed)", () => {
    for (const change of ["added", "role_changed", "metadata_changed"] as const) {
      expect(
        viewerRemovedWorkspaceId(
          frame({
            kind: "viewer_workspaces_changed",
            payload: { change, workspace_id: "w-1" },
          }),
        ),
      ).toBeNull();
    }
  });

  it("returns null for snapshot_required, other kinds, and malformed payloads", () => {
    expect(viewerRemovedWorkspaceId(frame({ type: "snapshot_required" }))).toBeNull();
    expect(
      viewerRemovedWorkspaceId(
        frame({ kind: "unread_updated", payload: { conversation_id: "c-1" } }),
      ),
    ).toBeNull();
    expect(
      viewerRemovedWorkspaceId(
        frame({ kind: "viewer_workspaces_changed", payload: { change: "nope" } }),
      ),
    ).toBeNull();
    expect(
      viewerRemovedWorkspaceId(frame({ kind: "viewer_workspaces_changed", payload: null })),
    ).toBeNull();
  });
});

describe("removeWorkspaceFromMessageUnread (#task:1404)", () => {
  it("drops the removed workspace slice from the by_workspace summary, keeps the rest", () => {
    const qc = new QueryClient();
    qc.setQueryData(messageUnreadSummaryKey, {
      by_workspace: { "w-1": 3, "w-2": 5 },
    });
    removeWorkspaceFromMessageUnread(qc, "w-1");
    expect(qc.getQueryData(messageUnreadSummaryKey)).toEqual({
      by_workspace: { "w-2": 5 },
    });
  });

  it("removes the workspace by_conversation query, not refetch", () => {
    const qc = new QueryClient();
    qc.setQueryData(messageUnreadByConversationKey("w-1"), {
      by_conversation: { "c-1": 2 },
      latest_by_conversation: {},
      total: 2,
    });
    // An unrelated workspace's by_conversation must survive.
    qc.setQueryData(messageUnreadByConversationKey("w-2"), {
      by_conversation: { "c-9": 1 },
      latest_by_conversation: {},
      total: 1,
    });
    removeWorkspaceFromMessageUnread(qc, "w-1");
    expect(qc.getQueryData(messageUnreadByConversationKey("w-1"))).toBeUndefined();
    expect(qc.getQueryData(messageUnreadByConversationKey("w-2"))).toEqual({
      by_conversation: { "c-9": 1 },
      latest_by_conversation: {},
      total: 1,
    });
  });

  it("is idempotent: absent summary key / unloaded by_conversation is a no-op", () => {
    const qc = new QueryClient();
    // Summary loaded but the removed workspace is not present → left untouched.
    qc.setQueryData(messageUnreadSummaryKey, { by_workspace: { "w-2": 5 } });
    expect(() => removeWorkspaceFromMessageUnread(qc, "w-1")).not.toThrow();
    expect(qc.getQueryData(messageUnreadSummaryKey)).toEqual({
      by_workspace: { "w-2": 5 },
    });
  });
});

// #task:1404 — favorites is the THIRD viewer-local projection of the same
// `viewer_workspaces_changed(removed)` fact (Cindy elevated this from a flag to
// blocking): a starred row under a lost workspace must drop, else "list gone +
// unread gone + favorites still there" becomes the next ghost. Since #task:1888 a
// row is a CONVERSATION and `workspace_id` is its PARENT, so one lost workspace can
// drop SEVERAL rows. Row membership + their orphan sort entries only, no refetch.
describe("removeWorkspaceFromFavorites (#task:1404, conversation rows since #task:1888)", () => {
  const favRow = (conversationId: string, workspaceId: string): FavoriteRowDto => ({
    roster_policy: "open",
    favorite_key: `conversation:${conversationId}`,
    entity_id: conversationId,
    workspace_id: workspaceId, // the PARENT, not the row itself
    title: conversationId,
    parent_title: workspaceId,
    starred_at: 1,
    created_at: 1,
    unread_count: 3,
    visibility: "public",
  });

  it("drops every row under the removed parent, plus their orphan sort entries", () => {
    const qc = new QueryClient();
    qc.setQueryData<ListFavoritesDto>(favoritesKey, {
      items: [favRow("c-1", "w-1"), favRow("c-2", "w-1"), favRow("c-3", "w-2")],
      viewer_sort: {
        list_key: "favorites",
        modes: {},
        entries: [
          { entity_id: "conversation:c-1", rank: 1, last_accessed_at: null, starred_at: 1 },
          { entity_id: "conversation:c-3", rank: 2, last_accessed_at: null, starred_at: 1 },
        ],
      },
    });
    removeWorkspaceFromFavorites(qc, "w-1");
    const next = qc.getQueryData<ListFavoritesDto>(favoritesKey);
    // Both of w-1's conversations gone; the other parent's row survives.
    expect(next?.items.map((i) => i.entity_id)).toEqual(["c-3"]);
    // A sort entry keyed by a removed row's `favorite_key` would let the id survive
    // in cache — the whole-cache removal invariant #task:1412 closed.
    expect(next?.viewer_sort.entries.map((e) => e.entity_id)).toEqual(["conversation:c-3"]);
  });

  it("is idempotent: list not loaded / no matching row → no write", () => {
    const qc = new QueryClient();
    // Not loaded → no-op, no throw.
    expect(() => removeWorkspaceFromFavorites(qc, "w-1")).not.toThrow();
    expect(qc.getQueryData(favoritesKey)).toBeUndefined();
    // Loaded but no matching row → the cached object is left in place (no write).
    const loaded: ListFavoritesDto = {
      items: [favRow("c-3", "w-2")],
      viewer_sort: { list_key: "favorites", modes: {}, entries: [] },
    };
    qc.setQueryData<ListFavoritesDto>(favoritesKey, loaded);
    removeWorkspaceFromFavorites(qc, "w-1");
    expect(qc.getQueryData(favoritesKey)).toBe(loaded);
  });
});

describe("decideFavoritesInvalidation (#task:1889)", () => {
  // The scope set is what this pins. STARRED used to derive its watched
  // workspaces from the parents of the rows it was currently showing, so hiding
  // the LAST favorite under a parent (archive the channel, leave a private one)
  // silently disarmed that parent — and the restore event that should bring the
  // star back had nobody listening. The set now comes from the consumer: every
  // workspace the viewer can see, rows or not.
  const chFrame = (scope: string) => ({
    type: "event",
    scope,
    kind: "workspace_channels_changed",
  });

  it("invalidates for a watched workspace that currently shows NO favorite (channel hide → restore)", () => {
    // `w-2` holds a star whose channel is archived, so the list shows nothing
    // under it. The restore arrives here; with a result-derived scope set this
    // frame was dropped and the star never came back short of a remount.
    expect(decideFavoritesInvalidation(chFrame("workspace:w-2"), ["w-1", "w-2"])).toBe(true);
    expect(decideFavoritesInvalidation(chFrame("workspace:w-2"), [])).toBe(false);
  });

  it("ignores an unwatched workspace, other event kinds, and other frame types", () => {
    expect(decideFavoritesInvalidation(chFrame("workspace:w-9"), ["w-1"])).toBe(false);
    expect(
      decideFavoritesInvalidation(
        { type: "event", scope: "workspace:w-1", kind: "message_created" },
        ["w-1"],
      ),
    ).toBe(false);
    expect(decideFavoritesInvalidation({ type: "ack", scope: "workspace:w-1" }, ["w-1"])).toBe(
      false,
    );
  });

  it("treats a replay gap on a watched workspace as an invalidation", () => {
    expect(
      decideFavoritesInvalidation({ type: "snapshot_required", scope: "workspace:w-1" }, ["w-1"]),
    ).toBe(true);
    // The viewer-scope half of the gate stays in the hook (`isCurrentViewerScope`).
    expect(
      decideFavoritesInvalidation({ type: "snapshot_required", scope: "viewer:u-1" }, ["w-1"]),
    ).toBe(false);
  });
});

describe("removeWorkspaceFromActivity (#task:1409)", () => {
  // Activity is the FOURTH viewer-local projection of a workspace-removed fact
  // (after list / message-unread / favorites). A `workspace_channel` item carries
  // its workspace_id, so a removed workspace's items are filtered out locally (no
  // refetch → no reorder reflow of survivors).
  const baseItem = (id: string): Omit<ActivityItemDto, "location"> => ({
    id,
    kind: "unread",
    salience: "ambient",
    conversation_id: `c-${id}`,
    thread_root_id: null,
    message_id: `m-${id}`,
    notification_id: null,
    thread_root_preview: null,
    preview: null,
    preview_sender: null,
    timestamp: 1,
    unread_count: 2,
    mention_count: 0,
  });
  const activityItem = (id: string, workspaceId: string): ActivityItemDto => ({
    ...baseItem(id),
    location: { kind: "workspace_channel", workspace_id: workspaceId, title: `#${id}` },
  });
  // A canonical workspace-less pair (#task:2115): it has no workspace axis, so no
  // workspace-removal fact can address it.
  const dmItem = (id: string): ActivityItemDto => ({
    ...baseItem(id),
    location: {
      kind: "direct_pair",
      counterpart: {
        status: "active",
        kind: "agent",
        id: `agent-${id}`,
        name: `agent-${id}`,
        display_name: `Agent ${id}`,
        avatar_url: null,
        runtime: "claude",
      },
    },
  });

  it("drops every item of the removed workspace, keeps survivors in order", () => {
    const qc = new QueryClient();
    qc.setQueryData<ListActivityDto>(activityKey(), {
      items: [
        activityItem("a", "w-1"),
        activityItem("b", "w-2"),
        activityItem("c", "w-1"),
        activityItem("d", "w-3"),
      ],
    });
    removeWorkspaceFromActivity(qc, "w-1");
    const next = qc.getQueryData<ListActivityDto>(activityKey());
    // Both w-1 items gone; w-2 / w-3 survivors keep their relative server order.
    expect(next?.items.map((i) => i.id)).toEqual(["b", "d"]);
  });

  // The fence is on the PRESERVED side (#task:2115). A workspace-less DM has no
  // workspace to lose, so it must come through a removal untouched — and "untouched"
  // is pinned as REFERENCE EQUALITY plus relative order, not as a re-derived equal
  // value: that is what distinguishes a FILTER from a rebuild. A rebuild would also
  // be the shape of a re-score, which this cleanup must never be.
  //
  // Structural sharing is OFF for this client on purpose. React Query's
  // `replaceEqualDeep` walks the new array INDEX-WISE against the old one and returns
  // a merged copy for any position whose neighbour differs — so once an earlier item
  // is dropped, every shifted survivor gets a fresh object identity no matter what
  // this function did. That is the cache's behaviour, not ours, and it masks the
  // distinction in BOTH directions (it would also hand a genuine rebuild the old
  // references back). Turning it off makes the assertion measure the one thing under
  // test: what `removeWorkspaceFromActivity` itself put in the array.
  it("preserves direct_pair items by identity — a workspace fact cannot address them", () => {
    const qc = new QueryClient({
      defaultOptions: { queries: { structuralSharing: false } },
    });
    const dmEarly = dmItem("dm-early");
    const dmLate = dmItem("dm-late");
    qc.setQueryData<ListActivityDto>(activityKey(), {
      items: [dmEarly, activityItem("a", "w-1"), dmLate, activityItem("d", "w-3")],
    });
    removeWorkspaceFromActivity(qc, "w-1");
    const next = qc.getQueryData<ListActivityDto>(activityKey());
    expect(next?.items.map((i) => i.id)).toEqual(["dm-early", "dm-late", "d"]);
    // Same objects, not equal copies.
    expect(next?.items[0]).toBe(dmEarly);
    expect(next?.items[1]).toBe(dmLate);
  });

  it("is idempotent: list not loaded / no matching item → no write", () => {
    const qc = new QueryClient();
    // Not loaded → no-op, no throw.
    expect(() => removeWorkspaceFromActivity(qc, "w-1")).not.toThrow();
    expect(qc.getQueryData(activityKey())).toBeUndefined();
    // Loaded but no matching item → the cached object is left in place (no write),
    // so a whole-list refetch/reorder is never triggered for an unaffected delete.
    const loaded: ListActivityDto = { items: [activityItem("b", "w-2")] };
    qc.setQueryData<ListActivityDto>(activityKey(), loaded);
    removeWorkspaceFromActivity(qc, "w-1");
    expect(qc.getQueryData(activityKey())).toBe(loaded);
  });

  // A DM-ONLY list is the sharper half of the same rule: with nothing addressable
  // in it, a workspace removal is not just order-preserving but a NO-WRITE — the
  // whole cached object keeps its identity, so no consumer re-renders at all.
  it("leaves a DM-only list byte-identical (no write)", () => {
    const qc = new QueryClient();
    const loaded: ListActivityDto = { items: [dmItem("dm-1"), dmItem("dm-2")] };
    qc.setQueryData<ListActivityDto>(activityKey(), loaded);
    removeWorkspaceFromActivity(qc, "w-1");
    expect(qc.getQueryData(activityKey())).toBe(loaded);
  });
});

describe("patchHumanMemberRole", () => {
  const human = (
    userId: string,
    role: WorkspaceHumanMemberDto["role"],
  ): WorkspaceHumanMemberDto => ({
    id: userId,
    name: userId,
    display_name: userId,
    avatar_url: null,
    created_at: 0,
    updated_at: 0,
    workspace_id: "w-1",
    user_id: userId,
    joined_at: 0,
    role,
  });

  const base: WorkspaceMembersResponse = {
    members: [],
    human_members: [human("u-owner", "owner"), human("u-a", "member")],
    viewer_role: "owner",
  };

  it("sets only the targeted member role and leaves the rest untouched", () => {
    const next = patchHumanMemberRole(base, "u-a", "admin");
    expect(next.human_members.map((h) => [h.user_id, h.role])).toEqual([
      ["u-owner", "owner"],
      ["u-a", "admin"],
    ]);
  });

  it("does not mutate the input (new objects for changed rows)", () => {
    const next = patchHumanMemberRole(base, "u-a", "admin");
    expect(base.human_members[1]?.role).toBe("member");
    expect(next).not.toBe(base);
    expect(next.human_members[1]).not.toBe(base.human_members[1]);
    // Unchanged row keeps its identity (cheap referential equality).
    expect(next.human_members[0]).toBe(base.human_members[0]);
  });

  it("is a no-op shape when the userId is not present", () => {
    const next = patchHumanMemberRole(base, "nobody", "admin");
    expect(next.human_members).toEqual(base.human_members);
  });
});

describe("removeHumanMemberFromList", () => {
  const human = (userId: string): WorkspaceHumanMemberDto => ({
    id: userId,
    name: userId,
    display_name: userId,
    avatar_url: null,
    created_at: 0,
    updated_at: 0,
    workspace_id: "w-1",
    user_id: userId,
    joined_at: 0,
    role: "member",
  });

  const base: WorkspaceMembersResponse = {
    members: [],
    human_members: [human("u-owner"), human("u-a"), human("u-b")],
    viewer_role: "owner",
  };

  it("drops only the targeted human and keeps the rest", () => {
    const next = removeHumanMemberFromList(base, "u-a");
    expect(next.human_members.map((h) => h.user_id)).toEqual(["u-owner", "u-b"]);
  });

  it("does not mutate the input", () => {
    const next = removeHumanMemberFromList(base, "u-a");
    expect(base.human_members).toHaveLength(3);
    expect(next).not.toBe(base);
    expect(next.human_members).not.toBe(base.human_members);
  });

  it("is a no-op shape when the userId is not present", () => {
    const next = removeHumanMemberFromList(base, "nobody");
    expect(next.human_members.map((h) => h.user_id)).toEqual(["u-owner", "u-a", "u-b"]);
  });
});
