import { useMutation, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect } from "react";
import type {
  AgentCapabilitiesDto,
  AgentDto,
  ArchivedChannelListItemDto,
  ChannelConversationDto,
  ChannelDetailResponse,
  CreateWorkspaceInput,
  FixedRosterConversationDto,
  ListFavoritesDto,
  UpdateWorkspaceInput,
  ViewerWorkspaceDto,
  WorkspaceColdReadResponse,
  WorkspaceHumanMemberDto,
  WorkspaceMembersResponse,
  WorkspaceRole,
  SetSortModeInput,
  SortGroup,
  ViewerSortStateDto,
} from "@silverhand/shared";
import { apiPaths } from "@silverhand/shared";
import { api } from "@/lib/api";
import { favoritesKey } from "@/features/workspaces/favorites";
import { attentionBadgesKey } from "@/features/attention/hooks";
import {
  recordChannelWorkspace,
  recordChannelWorkspaceRows,
} from "@/features/channels/channel-workspace-index";
import { handleViewerWorkspaceRemoved } from "./viewer-removal-contract";
import { workspaceScopedQueryEnabled } from "./workspace-scoped-query";
import {
  onWsFrame,
  subscribeWsScope,
  subscribeViewerScope,
  isCurrentViewerScope,
} from "@/lib/ws-client";
import {
  channelsKey,
  archivedChannelsKey,
  workspacesKey,
  membersKey,
  channelDetailKey,
  archivedChannelsIndexKey,
  capabilitiesKey,
} from "./query-keys";

export type WorkspaceListItem = {
  id: string;
  name: string;
  description: string | null;
  workspace_scope_ids: string[];
  /** The agent this container belongs to, or `null` for a workspace people own.
   *  The renderer's ONLY spelling of the container-identity distinction since
   *  #task:2078 — classify through `isAgentSpace` (`./workspace-ownership`), not
   *  by checking this field inline; read the id itself only to resolve the
   *  owner's avatar / name. */
  owner_agent_id: string | null;
  dm_channel_ids: string[];
  archived_at: number | null;
  created_at: number;
  updated_at: number;
};

/**
 * A workspace channel-sidebar item. The list from `GET /api/workspaces/:id/channels`
 * is a per-viewer DISCOVERY UNION (joined channels + unjoined active PUBLIC
 * channels), so each item carries the route-computed, per-viewer `is_member`.
 * It is optional here for transitional safety: a payload lacking it is `unknown`
 * (see `deriveChannelMembership`), never silently treated as a member.
 */
export type ChannelListItem = (ChannelConversationDto | FixedRosterConversationDto) & {
  is_member?: boolean;
};

/** The two authoritative channel tables of one workspace, exported for the
 *  §12 removal contract (#task:1689): it must ask React Query whether each list
 *  has actually converged (`status` alone stays `success` through a background
 *  refetch), and it must invalidate them for a workspace the viewer is not
 *  currently looking at. Same keys the hooks below use — one definition, so the
 *  contract cannot read a different cache than the sidebar renders. */
export const workspaceChannelsQueryKey = channelsKey;
export const workspaceArchivedChannelsQueryKey = archivedChannelsKey;
/** Matcher for "any workspace's member list", for the one caller that has no
 *  workspace id: the identity-carrier registry reseats every cached roster on a
 *  profile write (`features/identity/identity-carrier-caches.ts`). A plain
 *  `['workspaces']` prefix would also drop the channel / archive lists, which
 *  carry no identity snapshot. */
export function isWorkspaceMembersQueryKey(key: readonly unknown[]): boolean {
  return key.length === 3 && key[0] === "workspaces" && key[2] === "members";
}

/** Human workspace member (canonical shared shape): `UserDto` display
 *  fields PLUS the graded-permissions membership `role`. */
export type WorkspaceHumanMember = WorkspaceHumanMemberDto;

export interface WorkspaceMemberLists {
  agents: AgentDto[];
  humans: WorkspaceHumanMember[];
  /** Current viewer's role in this workspace (request-scoped sugar from the
   *  members endpoint). `null` = not a member → read-only. */
  viewerRole: WorkspaceRole | null;
}

function normalizeWorkspaceMembers(response: WorkspaceMembersResponse): WorkspaceMemberLists {
  return {
    agents: response.members,
    humans: response.human_members,
    viewerRole: response.viewer_role,
  };
}

function workspaceToListItem(workspace: ViewerWorkspaceDto): WorkspaceListItem {
  // `name` is the row's own name, verbatim for both kinds. It used to gain a
  // `dm-` prefix for an agent space, which nothing consumed except the three
  // helpers that stripped it back off before display (#task:2066) — a private
  // spelling that made the SAME workspace arrive under two name shapes (prefixed
  // from this list, bare from the archived aggregate) and put a `dm-Norman` in
  // front of the user whenever a surface forgot to call the stripper.
  return {
    id: workspace.id,
    name: workspace.name,
    description: workspace.description,
    workspace_scope_ids: [workspace.id],
    // The container word is not carried into the domain object at all (#task:2078).
    // It was provably the same bit as this line — the schema pinned the kind word
    // to `owner_agent_id IS NOT NULL` — so keeping both gave every surface a
    // choice of two spellings and no way to prove it
    // picked the one the server would agree with. Minting a kind here from
    // ownership would just re-create that second authority (#task:2075 判据④):
    // classification goes through `isAgentSpace`.
    owner_agent_id: workspace.owner_agent_id,
    dm_channel_ids:
      workspace.dm_channel_ids ?? (workspace.dm_channel_id ? [workspace.dm_channel_id] : []),
    archived_at: workspace.archived_at,
    created_at: workspace.created_at,
    updated_at: workspace.updated_at,
  };
}

export function sortConversationList(items: ChannelListItem[]): ChannelListItem[] {
  // Channels and DM channels are all top-level (isolated-thread fossils were
  // retired in #task:1303), so ordering is purely by creation time. Star (the
  // personal ⭐ favorite) is not a shared conversation field — it lives in the
  // viewer's `viewer_sort.entries[].starred_at` and is applied downstream
  // (WorkspaceChannels via `excludeStarredChannels`, which moves the row into the
  // global STARRED list — #task:1888), not here.
  return [...items].sort((a, b) => a.created_at - b.created_at);
}

/** Workspace identity cache; sidebar ordering has its own read. */
export interface WorkspaceListPayload {
  items: WorkspaceListItem[];
}

async function fetchWorkspaces(): Promise<WorkspaceListPayload> {
  const res = await api<WorkspaceColdReadResponse>(apiPaths.workspaces);
  return { items: res.workspaces.map(workspaceToListItem) };
}

export function useWorkspaces(options?: { enabled?: boolean }) {
  return useQuery({
    queryKey: workspacesKey,
    queryFn: fetchWorkspaces,
    select: (r) => r.items,
    enabled: options?.enabled ?? true,
  });
}

/**
 * Wire payload for the viewer-scoped `viewer_workspaces_changed` event.
 * Narrowed locally from `unknown` (the bus payload type) — the backend
 * publisher owns the matching shape; the doc is the contract. Following
 * the established renderer pattern (e.g. `isMessageUnreadUpdatedPayload`),
 * we do NOT share this type — the wire stays `unknown` and each side
 * validates independently. See `docs/viewer-visibility-sync-v1.md`.
 */
interface ViewerWorkspacesChangedPayload {
  change: "added" | "removed" | "role_changed" | "metadata_changed";
  workspace_id: string;
  role?: WorkspaceRole;
}

export function isViewerWorkspacesChangedPayload(
  payload: unknown,
): payload is ViewerWorkspacesChangedPayload {
  if (typeof payload !== "object" || payload === null) return false;
  const o = payload as Record<string, unknown>;
  return (
    (o.change === "added" ||
      o.change === "removed" ||
      o.change === "role_changed" ||
      // A mutable workspace field the read model projects (today: `name`,
      // #task:2050 §4-B) moved with no membership/role change. Whitelisting it
      // is what makes the producer edge live: without it the frame fails this
      // guard and `['workspaces']` never refetches, so the F1 live-join of the
      // workspace label would inherit the exact staleness the embed had.
      o.change === "metadata_changed") &&
    typeof o.workspace_id === "string"
  );
}

/**
 * Minimal WS frame shape the decision below consumes. Both the live
 * `onWsFrame` listener and the unit test normalize into this so the
 * decision logic can be tested without React or a real socket (mirrors
 * the `decideNextVisible` pattern in `useBackendUnreachableSnitch`).
 */
export interface ViewerSyncFrame {
  type: string;
  scope: string;
  kind?: string;
  payload?: unknown;
}

/**
 * Pure decision: given a viewer-scope frame, return the query keys to
 * invalidate (empty = ignore the frame). The hook and tests share this so
 * the two cannot drift.
 *
 * - `snapshot_required` on the viewer scope → refetch `['workspaces']`
 *   (replay-gap safety, same as the other viewer consumers).
 * - `viewer_workspaces_changed` event → always `['workspaces']`; for
 *   `role_changed` additionally the workspace's members query (governance
 *   `viewerRole` is sourced there, not from the list); for `added` additionally
 *   the attention badge roll-up (below) and the favorites list. `metadata_changed`
 *   (#task:2050 §4-B — a projected label such as `name` moved, membership
 *   unchanged) needs only the list: the refetch carries the new label to every
 *   surface that joins it (Task/Saved place, Search), which is the whole point of
 *   F1 replacing frozen snapshots with a live join.
 * - anything else → no invalidation.
 *
 * Why `added` reaches FAVORITES (#task:1889, @Cindy MUST FIX): a star is stored on
 * the conversation and OUTLIVES the viewer's membership in its parent, exactly like
 * the mute preference below. Re-joining a workspace therefore makes starred rows
 * reachable again, and no `workspace_channels_changed` is emitted for it — without
 * this the STARRED section stays short until a remount.
 *
 * Why `added` reaches the roll-up (#task:1870 / #task:1877, Cindy review): a
 * workspace the viewer just joined has no unread of their own, so the old reading
 * was "its badge is necessarily 0, and absent ≡ 0, so the loaded roll-up is
 * already right". That discount died when the roll-up gained
 * `muted_workspace_ids`: the mute preference row deliberately OUTLIVES a
 * membership removal so a re-join restores the viewer's earlier choice, so a
 * re-join changes the roll-up while every number in it stays 0 — and the rail's
 * muted marker would stay missing until some unrelated attention event happened
 * along. `removed` needs nothing here: it is settled by the removal contract's
 * targeted local purge, which is cheaper than refetching a workspace that is gone.
 *
 * Caller is responsible for the `isCurrentViewerScope(frame.scope)` gate
 * before calling — kept out of the pure fn so it stays dependency-free.
 */
export function decideViewerSyncInvalidations(
  frame: ViewerSyncFrame,
): readonly (readonly string[])[] {
  if (frame.type === "snapshot_required") return [workspacesKey];
  if (frame.type !== "event") return [];
  if (frame.kind !== "viewer_workspaces_changed") return [];
  if (!isViewerWorkspacesChangedPayload(frame.payload)) return [];
  if (frame.payload.change === "role_changed") {
    return [workspacesKey, membersKey(frame.payload.workspace_id)];
  }
  if (frame.payload.change === "added") {
    return [workspacesKey, attentionBadgesKey(), favoritesKey];
  }
  return [workspacesKey];
}

/**
 * The workspace_id whose cached message-unread must be PURGED for this frame, or
 * null. Only a `viewer_workspaces_changed` `removed` event qualifies — the viewer
 * lost access (removed / workspace deleted), so that workspace's unread
 * contribution has to drop locally (#task:1404).
 *
 * Kept pure and SEPARATE from {@link decideViewerSyncInvalidations} because it
 * drives a targeted cache PURGE (consume the event's workspace_id, no refetch of
 * a now-gone workspace) — a different mechanism than that fn's query
 * invalidations. The `['workspaces']` list refetch (from the sibling) drops the
 * row, but the message-unread summary / by_conversation caches are separate
 * queries it never touches — that gap is the ghost unread this closes. Caller
 * gates `isCurrentViewerScope(frame.scope)` first, same as the sibling.
 */
export function viewerRemovedWorkspaceId(frame: ViewerSyncFrame): string | null {
  if (frame.type !== "event") return null;
  if (frame.kind !== "viewer_workspaces_changed") return null;
  if (!isViewerWorkspacesChangedPayload(frame.payload)) return null;
  return frame.payload.change === "removed" ? frame.payload.workspace_id : null;
}

/**
 * Live-sync the viewer's workspace list when their cross-workspace
 * membership/role changes from ANY source — added to / removed from a
 * workspace, or role changed. See `docs/viewer-visibility-sync-v1.md`.
 *
 * Carrier is the `viewer:<userId>` scope which — unlike `workspace:<id>` —
 * is subscribable before the viewer belongs to any workspace, so this also
 * catches the "added to your first workspace" bootstrap case (there is no
 * workspace scope to have been listening on).
 *
 * Coarse trigger: any `viewer_workspaces_changed` invalidates
 * `['workspaces']`, and the refetch is the authority (the server returns
 * exactly this viewer's workspaces). The payload is a hint only —
 * `role_changed` additionally invalidates that workspace's members query
 * because the governance `viewerRole` is sourced from the members endpoint
 * (`useWorkspaceMemberLists`), not from the workspace list.
 *
 * Safe-downgrade on removal: App's existing "selected workspace no longer
 * exists → bounce to /chat" effect fires reactively once the refetched list
 * drops the row. Removal additionally settles the gone workspace's other
 * viewer-local projections through the removal-contract inventory
 * ({@link viewerRemovedWorkspaceId} → {@link handleViewerWorkspaceRemoved}): the
 * `['workspaces']` refetch above rebuilds the list but never touches the separate
 * message-unread / favorites / Activity caches, so without this the deleted
 * workspace lingers as a ghost badge, dead favorite row, or stale Activity item
 * (#task:1404 / #task:1409, unified under the #task:1411 contract).
 *
 * Mount ONCE in `RegisteredAppShell`: must be alive post-auth and
 * independent of any workspace selection (the sidebar may not be mounted
 * when the viewer's first workspace arrives).
 */
export function useViewerWorkspacesSync(): void {
  const qc = useQueryClient();
  useEffect(() => {
    const sub = subscribeViewerScope();
    const unsub = onWsFrame((frame) => {
      if (
        (frame.type !== "event" && frame.type !== "snapshot_required") ||
        !isCurrentViewerScope(frame.scope)
      ) {
        return;
      }
      for (const queryKey of decideViewerSyncInvalidations(frame)) {
        void qc.invalidateQueries({ queryKey });
      }
      // #task:1404 / #task:1409 / #task:1411: removal is the ONE change where the
      // `['workspaces']` refetch above is not enough — the same "viewer lost this
      // workspace" fact has further viewer-local caches a refetch never touches
      // (the `['workspaces', wsId, *]` keyed family + the item-scoped Activity /
      // favorites / message-unread-summary caches). All are settled through the
      // removal-contract chokepoint. NOTE — two distinct guarantees, don't conflate:
      // the contract makes a newly-DECLARED projection cleaned automatically
      // (handler-completeness); it does NOT by itself catch a cache never added to
      // the contract at all — that inventory-completeness half is the source-fence
      // / classification layer in `viewer-removal-contract` (+ future DTO marker).
      // `added` / `role_changed` need no purge (nothing removed).
      const removedWorkspaceId = viewerRemovedWorkspaceId(frame);
      if (removedWorkspaceId) {
        handleViewerWorkspaceRemoved(qc, removedWorkspaceId);
      }
    });
    return () => {
      sub.unsubscribe();
      unsub();
    };
  }, [qc]);
}

export function useCreateWorkspace() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: CreateWorkspaceInput) => {
      const res = await api<{
        workspace: ViewerWorkspaceDto;
        onboarding_channel_id: string;
      }>(apiPaths.workspaces, { method: "POST", body: JSON.stringify(input) });
      return res;
    },
    onSuccess: async (res) => {
      const workspace = workspaceToListItem(res.workspace);
      qc.setQueryData<WorkspaceListPayload>(workspacesKey, (prev) => {
        if (!prev) return prev;
        const cur = prev.items;
        const items = cur.some((c) => c.id === workspace.id)
          ? cur.map((c) => (c.id === workspace.id ? workspace : c))
          : [...cur, workspace];
        return { ...prev, items };
      });
      await qc.invalidateQueries({ queryKey: workspacesKey });
    },
  });
}

export function useDeleteWorkspace() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      await api(apiPaths.workspace(id), { method: "DELETE" });
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: workspacesKey });
    },
  });
}

/**
 * Self-LEAVE a workspace (member | admin — the owner's exit is Delete). The
 * shape mirrors `useDeleteWorkspace`: fire the mutation, invalidate the
 * workspaces list, and let convergence do the rest. The backend
 * (`POST /leave`) removes the caller's workspace membership AND every channel
 * membership they hold there in one transaction, then publishes
 * `viewer_workspaces_changed {change:'removed'}` to the viewer scope — the
 * existing `useViewerWorkspacesSync` → `handleViewerWorkspaceRemoved` already
 * purges every viewer-local cache (favorites, unread, Activity), so no bespoke
 * cleanup lives here. Navigation away is REACTIVE: the invalidated
 * `['workspaces']` list drops the row and the selection effect bounces off it,
 * exactly as Delete does. Empty body — the caller is always the removed human,
 * so there is no target on the wire.
 */
export function useLeaveWorkspace() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      await api(apiPaths.workspaceLeave(id), { method: "POST" });
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: workspacesKey });
    },
  });
}

export function useRenameWorkspace() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (vars: { id: string } & UpdateWorkspaceInput) => {
      const { id, ...body } = vars;
      const res = await api<{ workspace: ViewerWorkspaceDto }>(apiPaths.workspace(id), {
        method: "PATCH",
        body: JSON.stringify(body),
      });
      return workspaceToListItem(res.workspace);
    },
    onSuccess: (workspace) => {
      qc.invalidateQueries({ queryKey: workspacesKey });
      // The workspace name shows up inside channel messages too (e.g.
      // the rename system broadcast we just emitted), so refresh
      // channels for this workspace as well.
      qc.invalidateQueries({ queryKey: channelsKey(workspace.id) });
      // STARRED snapshots the PARENT's title on every regular row
      // (`FavoriteRowDto.parent_title`, #task:1888) and a rename emits no
      // `workspace_channels_changed`, so nothing else re-reads it — without this
      // the old name survives in STARRED until a remount (@Cindy, #task:1889).
      // An agent-home parent is NOT covered here and must not be: its name is a
      // carrier of agent identity and already converges through the
      // identity-carrier registry.
      qc.invalidateQueries({ queryKey: favoritesKey });
    },
  });
}

export function useArchiveWorkspace() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (vars: { id: string; archived: boolean }) => {
      const res = await api<{ workspace: ViewerWorkspaceDto }>(
        `/api/workspaces/${vars.id}/archive`,
        {
          method: "PATCH",
          body: JSON.stringify({ archived: vars.archived }),
        },
      );
      return workspaceToListItem(res.workspace);
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: workspacesKey });
      // The archived-channel index carries each row's PARENT archived-ness
      // (the `archived workspace` marker), so archiving or restoring a
      // workspace changes rows the Archived view renders beside the workspace
      // rows (#task:1890).
      qc.invalidateQueries({ queryKey: ["archived-channels"] });
      // Favorites membership is the star set INTERSECTED with "not archived"
      // (`read-model/favorites.ts` filters `w.archived_at IS NULL`), so archive
      // and restore both change it — while `useFavorites` only refetches on the
      // viewer snapshot. Without this the STARRED list keeps a dead row after an
      // archive, and a restored workspace never comes back until remount
      // (#task:1890; the same seam #task:1889 converges for channels).
      qc.invalidateQueries({ queryKey: favoritesKey });
    },
  });
}

/** Channel sidebar list cache, now carrying the viewer's inline sort state
 *  (#task:1146). Mirrors `WorkspaceListPayload`. */
export interface ChannelListPayload {
  items: ChannelListItem[];
  viewerSort: ViewerSortStateDto;
}

async function fetchWorkspaceChannels(workspaceId: string): Promise<ChannelListPayload> {
  const res = await api<{
    channels: ChannelListItem[];
    viewer_sort: ViewerSortStateDto;
  }>(apiPaths.workspaceChannels(workspaceId));
  // Write-through of the immutable channel → workspace binding (#task:1689): this
  // listing IS the statement that these channels belong to this workspace.
  recordChannelWorkspaceRows(res.channels, workspaceId);
  return {
    items: sortConversationList(res.channels),
    viewerSort: res.viewer_sort,
  };
}

/**
 * One workspace's live channel list, plus the `workspace:<id>` WS subscription
 * the whole channel family shares.
 *
 * `workspaceResolved` is the caller's assertion that the container ROW behind
 * `workspaceId` has resolved — an id alone is not enough (see
 * {@link workspaceScopedQueryEnabled}). It carries NO default: this used to be a
 * nullable container-kind field defaulting to `'regular'` whose only read was
 * `!== null`, i.e. two different absences on one axis, and any default
 * (`'regular'` then, `true` now) silently turns "unresolved" into "go ahead".
 * Absence must be a compile error (@Paul, #task:2078).
 */
export function useChannelsForWorkspace(workspaceId: string | null, workspaceResolved: boolean) {
  const qc = useQueryClient();
  useEffect(() => {
    if (!workspaceScopedQueryEnabled(workspaceId, workspaceResolved)) return;
    const scope = `workspace:${workspaceId}`;
    const sub = subscribeWsScope(scope);
    const unsub = onWsFrame((frame) => {
      if (frame.type === "event") {
        if (frame.scope !== scope || frame.kind !== "workspace_channels_changed") return;
        void qc.invalidateQueries({ queryKey: channelsKey(workspaceId) });
        void qc.invalidateQueries({ queryKey: archivedChannelsKey(workspaceId) });
        void qc.invalidateQueries({ queryKey: ["archived-channels"] });
      } else if (frame.type === "snapshot_required" && frame.scope === scope) {
        void qc.invalidateQueries({ queryKey: channelsKey(workspaceId) });
        void qc.invalidateQueries({ queryKey: archivedChannelsKey(workspaceId) });
        void qc.invalidateQueries({ queryKey: ["archived-channels"] });
      }
    });
    return () => {
      sub.unsubscribe();
      unsub();
    };
  }, [qc, workspaceId, workspaceResolved]);

  return useQuery({
    queryKey: workspaceId ? channelsKey(workspaceId) : ["workspaces", "none", "channels"],
    enabled: workspaceScopedQueryEnabled(workspaceId, workspaceResolved),
    queryFn: () => fetchWorkspaceChannels(workspaceId!),
    select: (r) => r.items,
  });
}

/**
 * Authoritative per-viewer channel detail from `GET /api/channels/:id`, carrying
 * `is_member`. This is the DEEP-LINK / UNKNOWN fallback: the sidebar list item
 * already carries `is_member`, so this only runs when a channel is reached with
 * no authoritative carrier (e.g. a `#conv:` chip or conversation-pane deep link) — pass `enabled`
 * = "membership is still unknown" so member channels opened from the list never
 * fire an extra fetch (zero flicker on the hot path).
 */
export function useChannelDetail(channelId: string | null, enabled = true) {
  return useQuery<ChannelDetailResponse>({
    queryKey: channelId ? channelDetailKey(channelId) : ["channel-detail", "none"],
    enabled: !!channelId && enabled,
    queryFn: async () => {
      const res = await api<ChannelDetailResponse>(apiPaths.conversationChannel(channelId!));
      // The deep-link path is often the ONLY place this client learns a channel's
      // workspace, and it is exactly the path that leaves channel-keyed caches
      // behind (#task:1689).
      recordChannelWorkspace(res.channel.id, res.channel.workspace_id);
      return res;
    },
  });
}

/** The viewer's sort state (`mode` + sparse `entries`) for one workspace's
 *  channel sidebar, read off the same `useChannelsForWorkspace` cache — so it
 *  takes the same `workspaceResolved` gate (see that hook for why there is no
 *  default). */
export function useChannelSortState(workspaceId: string | null, workspaceResolved: boolean) {
  return useQuery({
    queryKey: workspaceId ? channelsKey(workspaceId) : ["workspaces", "none", "channels"],
    enabled: workspaceScopedQueryEnabled(workspaceId, workspaceResolved),
    queryFn: () => fetchWorkspaceChannels(workspaceId!),
    select: (r) => r.viewerSort,
  });
}

/** Archived channel-like conversations for a workspace.
 *  Backed by `GET /api/workspaces/:id/channels/archived`. Runs for EVERY
 *  container kind — the endpoint is channel-like and returns archived
 *  `dm_channel`s for an agent space just as it returns archived `channel`s for a
 *  regular workspace (#task:1144). This union feeds `channels` in app.tsx, so an
 *  archived DM opened from the global Archived panel resolves to a real
 *  `selectedChannel` instead of a blank pane (#task:1145). The gate that
 *  remains is `workspaceResolved` — the one cell of the old kind predicate with
 *  independent content ("unresolved ⇒ do not fetch"); the kind arms were an OR
 *  over the whole alphabet, so a regular-only gate is no longer expressible
 *  (#task:2078). WS invalidation lives on `useChannelsForWorkspace` — both
 *  queries share the same trigger. */
export function useArchivedChannelsForWorkspace(
  workspaceId: string | null,
  workspaceResolved: boolean,
) {
  return useQuery<ChannelListItem[]>({
    queryKey: workspaceId
      ? archivedChannelsKey(workspaceId)
      : ["workspaces", "none", "channels", "archived"],
    enabled: workspaceScopedQueryEnabled(workspaceId, workspaceResolved),
    queryFn: () => fetchWorkspaceArchivedChannels(workspaceId!),
  });
}

/** Archived twin of {@link fetchWorkspaceChannels}. Since #task:1691 the route
 *  returns `WorkspaceChannelListItem` rows — the SAME shape as the active list,
 *  each carrying the viewer-scoped `is_member` — so a consumer never has to know
 *  which of the two tables a row came from to read membership off it. */
async function fetchWorkspaceArchivedChannels(workspaceId: string): Promise<ChannelListItem[]> {
  const res = await api<{ channels: ChannelListItem[] }>(
    apiPaths.workspaceArchivedChannels(workspaceId),
  );
  recordChannelWorkspaceRows(res.channels, workspaceId);
  return sortConversationList(res.channels);
}

/** Authoritative read of BOTH channel tables of one workspace, bypassing the
 *  mounted queries (#task:1689). The §12 removal contract needs this for a
 *  NON-SELECTED workspace: neither list is mounted there, so invalidation alone
 *  fetches nothing and the domain fact would wait for navigation. Endpoint
 *  knowledge stays in this feature; the contract consumes rows. */
export async function fetchWorkspaceChannelTables(workspaceId: string): Promise<{
  active: ChannelListItem[];
  archived: ChannelListItem[];
}> {
  const [active, archived] = await Promise.all([
    fetchWorkspaceChannels(workspaceId),
    fetchWorkspaceArchivedChannels(workspaceId),
  ]);
  return { active: active.items, archived };
}

/** Active channels (+ threads) for every supplied workspace, fetched in
 *  parallel. Reuses the per-workspace `channelsKey` + queryFn of
 *  `useChannelsForWorkspace`, so results share that hook's cache rather
 *  than duplicating fetches. Returns one query result per input id,
 *  index-aligned with `workspaceIds`. Used by the global Tasks pane, which
 *  spans every member workspace and has no single-workspace channel list.
 *  (Per-workspace WS invalidation is owned by `useChannelsForWorkspace`;
 *  this hook is a read fan-out and intentionally adds no extra subscription.) */
export function useChannelsAcrossWorkspaces(workspaceIds: readonly string[]) {
  return useQueries({
    queries: workspaceIds.map((id) => ({
      queryKey: channelsKey(id),
      queryFn: () => fetchWorkspaceChannels(id),
      select: (r: ChannelListPayload) => r.items,
    })),
  });
}

/** Archived twin of {@link useChannelsAcrossWorkspaces}, index-aligned with
 *  `workspaceIds` the same way and sharing `archivedChannelsKey` with
 *  `useArchivedChannelsForWorkspace`.
 *
 *  A consumer asking "is the viewer a member of conversation X" needs BOTH
 *  tables: since #task:1691 the archived route returns the same
 *  `WorkspaceChannelListItem` rows carrying the same viewer-scoped `is_member`,
 *  and the archived set is itself membership-filtered — so a joined channel that
 *  has since been archived lives only here. Reading the active list alone would
 *  answer "not a member" for it, which is a different claim from the truth
 *  (#task:2040, @Cindy). No kind gating anywhere: the endpoint is channel-like
 *  for every container kind (see {@link useArchivedChannelsForWorkspace}). */
export function useArchivedChannelsAcrossWorkspaces(workspaceIds: readonly string[]) {
  return useQueries({
    queries: workspaceIds.map((id) => ({
      queryKey: archivedChannelsKey(id),
      queryFn: () => fetchWorkspaceArchivedChannels(id),
    })),
  });
}

// ---- Per-user list sort mutations (#task:1146 / #task:1147) ----

type SortCachePayload = { items: unknown; viewerSort: ViewerSortStateDto };

function sortListQueryKey(listKey: string): readonly unknown[] | null {
  if (listKey.startsWith("channels:")) {
    return channelsKey(listKey.slice("channels:".length));
  }
  return null;
}

/** A list's sort cache: its query key + how to READ/WRITE the inline viewer-sort
 *  state, abstracting the per-feature cache SHAPE. The channel cache
 *  store it camelCase (`viewerSort`, the `SortCachePayload` shape); the global
 *  Favorites cache (`ListFavoritesDto`, `list_key='favorites'`) stores it
 *  snake_case (`viewer_sort`) under `favoritesKey`. ALL sort mutations (mode /
 *  reorder) go through this adapter so their optimistic + reconcile writes land
 *  on whichever list owns `list_key` — Favorites was previously unresolved by
 *  `sortListQueryKey` (→ null), so its mode/reorder silently no-op'd the cache
 *  and only a later refetch reflected the change (#task:1254, Cindy P1). */
type SortCacheAdapter = {
  key: readonly unknown[];
  read: (cache: unknown) => ViewerSortStateDto | undefined;
  write: (cache: unknown, next: ViewerSortStateDto) => unknown;
};

export function sortCacheAdapter(listKey: string): SortCacheAdapter | null {
  const key = sortListQueryKey(listKey);
  if (key) {
    return {
      key,
      read: (c) => (c as SortCachePayload | undefined)?.viewerSort,
      write: (c, next) => (c ? { ...(c as SortCachePayload), viewerSort: next } : c),
    };
  }
  if (listKey === "favorites") {
    return {
      key: favoritesKey,
      read: (c) => (c as ListFavoritesDto | undefined)?.viewer_sort,
      write: (c, next) => (c ? { ...(c as ListFavoritesDto), viewer_sort: next } : c),
    };
  }
  return null;
}

/** Toggle the channel/favorites list mode and reconcile its cache. */
export function useSetSortMode() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: SetSortModeInput) => {
      const res = await api<{ viewer_sort: ViewerSortStateDto }>(apiPaths.sortMode, {
        method: "PUT",
        body: JSON.stringify(input),
      });
      return res.viewer_sort;
    },
    onMutate: async (input) => {
      const adapter = sortCacheAdapter(input.list_key);
      if (!adapter) return undefined;
      await qc.cancelQueries({ queryKey: adapter.key });
      const prev = qc.getQueryData(adapter.key);
      const prevVs = adapter.read(prev);
      if (prevVs) {
        qc.setQueryData(
          adapter.key,
          adapter.write(prev, {
            ...prevVs,
            modes: { ...prevVs.modes, "": input.mode },
          }),
        );
      }
      return { adapter, prev };
    },
    onError: (_e, _v, ctx) => {
      if (ctx?.adapter) qc.setQueryData(ctx.adapter.key, ctx.prev);
    },
    onSuccess: (viewerSort, input) => {
      const adapter = sortCacheAdapter(input.list_key);
      if (adapter) {
        qc.setQueryData(adapter.key, (prev) => (prev ? adapter.write(prev, viewerSort) : prev));
      }
    },
  });
}

/** Apply dense ranks and the list-wide manual mode for channel/favorites lists. */
export function applyOrderToViewerSort(
  prev: ViewerSortStateDto,
  orderedIds: string[],
): ViewerSortStateDto {
  const byId = new Map(prev.entries.map((e) => [e.entity_id, e] as const));
  orderedIds.forEach((id, rank) => {
    const e = byId.get(id);
    byId.set(
      id,
      e ? { ...e, rank } : { entity_id: id, rank, last_accessed_at: null, starred_at: null },
    );
  });
  return {
    ...prev,
    modes: { ...prev.modes, "": "manual" },
    entries: [...byId.values()],
  };
}

/** Apply a drag's new order to the inline viewer-sort cache, synchronously —
 *  the deterministic CACHE/persistence mirror of the drop (dense ranks +
 *  mode=manual). It does NOT carry frame-level sync: a `setQueryData` here does
 *  NOT re-render in the same frame dnd-kit clears the drag transform, because
 *  React Query's observer notify is microtask-batched, so the cache-driven
 *  reorder lands a frame later (CDP-confirmed B-type desync — the row would
 *  snap to its old slot then jump). Same-frame reordering is owned by the
 *  renderer's `pendingDrop` local state (WorkspaceChannels), set in the same drop
 *  event — do NOT delete `pendingDrop` on the assumption this cache write covers
 *  the frame; it doesn't. See docs/list-sort-v1.md §Renderer composition. The
 *  mutation then persists (no success reconcile — see useReorderSort); errors
 *  refetch. */
export function useApplySortOrder() {
  const qc = useQueryClient();
  return (listKey: string, orderedIds: string[]): void => {
    const adapter = sortCacheAdapter(listKey);
    if (!adapter) return;
    const prev = qc.getQueryData(adapter.key);
    const prevVs = adapter.read(prev);
    if (prevVs) {
      qc.setQueryData(adapter.key, adapter.write(prev, applyOrderToViewerSort(prevVs, orderedIds)));
    }
  };
}

/** Persist a manual drag order for ONE live group. The renderer always sends
 *  the COMPLETE group order (`ordered_ids` set-equal to the live group); the
 *  backend rejects partial/duplicate/cross-group ids and flips mode to manual
 *  atomically (#task:1147). The optimistic cache write happens synchronously
 *  in the drop handler via `useApplySortOrder` and is DETERMINISTIC — identical
 *  to what the backend computes (mode=manual + dense 0..n ranks for the same
 *  ordered_ids). So we intentionally do NOT reconcile the success response onto
 *  the cache: a second `setQueryData` a network-RTT after the drop re-renders
 *  the list (the server's `entries` array order can differ from ours, defeating
 *  structural sharing) and shows as a post-drop flicker. The optimistic state
 *  stands; only errors refetch the authoritative order.
 *
 *  BOUNDARY (Cindy): the no-reconcile shortcut is valid for `order` ONLY,
 *  because its success response is a deterministic function of the request —
 *  the backend validated the complete live group and can only return that same
 *  group's 0..n ranks + mode=manual; there is no server-side sort/normalization
 *  the client must absorb. Do NOT copy this to setMode/touch (or any future
 *  mutation): those touch more than one local group or lack an isomorphic local
 *  truth, so they still need to reconcile. The error path keeps refetching, so
 *  stale/race still recovers. */
export function useReorderSort() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: { list_key: string; group: SortGroup; ordered_ids: string[] }) => {
      const res = await api<{ viewer_sort: ViewerSortStateDto }>(apiPaths.sortOrder, {
        method: "PUT",
        body: JSON.stringify(input),
      });
      return res.viewer_sort;
    },
    onMutate: async (input) => {
      // Cancel in-flight list refetches so they can't clobber the
      // already-applied synchronous optimistic order.
      const adapter = sortCacheAdapter(input.list_key);
      if (adapter) await qc.cancelQueries({ queryKey: adapter.key });
    },
    onError: (_e, input) => {
      // The optimistic order was applied synchronously and we didn't snapshot
      // the pre-drag state here; refetch the authoritative order instead.
      const adapter = sortCacheAdapter(input.list_key);
      if (adapter) void qc.invalidateQueries({ queryKey: adapter.key });
    },
  });
}

/** Stamp recent-opened on selection. Fire-and-forget, non-blocking; the
 *  reorder applies on the next list fetch (no jump under the cursor). The
 *  caller dedups/throttles per (viewer, target); the upsert is idempotent. */
export function useTouchSort() {
  return useMutation({
    mutationFn: async (input: { list_key: string; entity_id: string }) => {
      await api(apiPaths.sortTouch, { method: "POST", body: JSON.stringify(input) });
    },
  });
}

export function useArchivedChannelsIndex({
  workspaceId,
  since,
  workspaceScopeIds,
}: {
  workspaceId: string | null;
  since: number | null;
  workspaceScopeIds: string[];
}) {
  const qc = useQueryClient();
  useEffect(() => {
    const scopeIds = workspaceId ? [workspaceId] : workspaceScopeIds;
    if (scopeIds.length === 0) return;
    const subscriptions = scopeIds.map((id) => subscribeWsScope(`workspace:${id}`));
    const unsub = onWsFrame((frame) => {
      if (frame.type === "event") {
        if (frame.kind !== "workspace_channels_changed") return;
        if (!scopeIds.some((id) => frame.scope === `workspace:${id}`)) return;
        void qc.invalidateQueries({ queryKey: ["archived-channels"] });
      } else if (
        frame.type === "snapshot_required" &&
        scopeIds.some((id) => frame.scope === `workspace:${id}`)
      ) {
        void qc.invalidateQueries({ queryKey: ["archived-channels"] });
      }
    });
    return () => {
      for (const sub of subscriptions) sub.unsubscribe();
      unsub();
    };
  }, [qc, workspaceId, workspaceScopeIds]);

  return useQuery<ArchivedChannelListItemDto[]>({
    queryKey: archivedChannelsIndexKey(workspaceId, since),
    queryFn: async () => {
      const params = new URLSearchParams();
      if (workspaceId) params.set("workspace_id", workspaceId);
      if (since !== null) params.set("since", String(since));
      const suffix = params.toString();
      const res = await api<{ channels: ArchivedChannelListItemDto[] }>(
        suffix ? `${apiPaths.archivedChannels}?${suffix}` : apiPaths.archivedChannels,
      );
      // Attribution write-through (#task:1689): this cross-workspace index is the
      // one carrier for an archived channel of a workspace whose own lists were
      // never mounted, and each row states its workspace.
      for (const row of res.channels) {
        recordChannelWorkspace(row.channel.id, row.workspace.id);
      }
      return res.channels;
    },
  });
}

export function useWorkspaceMemberLists(workspaceId: string | null) {
  return useQuery({
    queryKey: workspaceId ? membersKey(workspaceId) : ["workspaces", "none", "members"],
    enabled: !!workspaceId,
    queryFn: async () => api<WorkspaceMembersResponse>(apiPaths.workspaceMembers(workspaceId!)),
    select: normalizeWorkspaceMembers,
  });
}

/** The effective capability set of one agent in one workspace (GET).
 *  Any workspace viewer may read; editing is gated on the human role. */
export function useAgentCapabilities(workspaceId: string | null, agentId: string | null) {
  return useQuery({
    queryKey:
      workspaceId && agentId
        ? capabilitiesKey(workspaceId, agentId)
        : ["workspaces", "none", "agents", "none", "capabilities"],
    enabled: !!workspaceId && !!agentId,
    queryFn: () =>
      api<AgentCapabilitiesDto>(apiPaths.workspaceAgentCapabilities(workspaceId!, agentId!)),
  });
}

export {
  applyChannelJoinToList,
  applyChannelLeaveToList,
  channelArchiveMutationOptions,
  reconcileChannelArchive,
  setChannelStarredOptimistic,
  useArchiveChannel,
  useCreateChannel,
  useJoinChannel,
  useLeaveChannel,
  useRenameChannel,
  useSetChannelVisibility,
  useStarChannel,
} from "./channel-mutations";
export {
  patchHumanMemberRole,
  removeHumanMemberFromList,
  useAddWorkspaceMember,
  useRemoveWorkspaceHumanMember,
  useRemoveWorkspaceMember,
  useUpdateAgentCapabilities,
  useUpdateWorkspaceHumanMemberRole,
} from "./member-mutations";
