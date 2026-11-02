import { useMutation, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { t } from "@/lib/i18n";
import type {
  ChannelDetailResponse,
  ChannelLikeDto,
  ConversationVisibility,
  CreateConversationChannelInput,
  ViewerSortStateDto,
} from "@silverhand/shared";
import { apiPaths } from "@silverhand/shared";
import { api } from "@/lib/api";
import { favoritesKey } from "@/features/workspaces/favorites";
import type { ChannelListPayload } from "./hooks";
import { archivedChannelsKey, channelDetailKey, channelsKey } from "./query-keys";

/** Optimistically flip one CHANNEL's `starred_at` in its `channels:<ws>` viewer-sort
 *  cache: star → stamp now, unstar → null. Star belongs to conversations only —
 *  containers have none (#task:1900) — so this is the whole star surface.
 *  The home manual `rank` is always PRESERVED: a channel's star is an exclusive MOVE
 *  into the global STARRED list, a DIFFERENT `list_key='favorites'`, so its
 *  `channels:*` rank survives as an orphan and unstar drops it back into its original
 *  home slot. The favorites list's own rank is cleared server-side by the unstar
 *  route, not here. A brand-new entry has no prior rank, so its rank starts null.
 *  Pure. (#task:1209, #task:1252, #task:1254, #task:1888) */
export function setChannelStarredOptimistic(
  viewerSort: ViewerSortStateDto,
  entityId: string,
  starred: boolean,
): ViewerSortStateDto {
  const starredAt = starred ? Date.now() : null;
  const entries = viewerSort.entries.some((e) => e.entity_id === entityId)
    ? viewerSort.entries.map((e) =>
        e.entity_id === entityId ? { ...e, starred_at: starredAt } : e,
      )
    : [
        ...viewerSort.entries,
        { entity_id: entityId, rank: null, last_accessed_at: null, starred_at: starredAt },
      ];
  return { ...viewerSort, entries };
}

/**
 * Self-join a public channel: `POST /api/channels/:id/join` (empty body — the
 * caller is always the joiner, no forgeable target). Idempotent; first-join and
 * retry both return `{ channel, is_member: true }`. On success we backfill the
 * channel-detail cache from the AUTHORITATIVE response (never an optimistic
 * client-side flip) so membership resolves to `member`, and invalidate the
 * workspace channel list so the joined channel's `is_member` converges to true
 * (the server also fires `workspace_channels_changed`, subscribed by
 * `useChannelsForWorkspace`; invalidating here just avoids the round-trip wait).
 * On failure the caches are untouched → the viewer stays a nonmember.
 */
export function useJoinChannel(workspaceId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (channelId: string) =>
      api<ChannelDetailResponse>(apiPaths.channelJoin(channelId), { method: "POST" }),
    onSuccess: (res, channelId) => {
      qc.setQueryData(channelDetailKey(channelId), res);
      if (workspaceId) {
        // Converge the CURRENT list carrier immediately. The join's
        // 200 IS the authority (`is_member: true`), not an optimistic guess, and
        // `deriveChannelMembership` prefers the carrier — so without this in-place
        // flip the row (and any ChannelView reading it) would stay `nonmember`
        // until the list refetch lands, and a slow/failed refetch would strand
        // it. The invalidate below still runs for full reconciliation.
        qc.setQueryData<ChannelListPayload>(channelsKey(workspaceId), (prev) =>
          applyChannelJoinToList(prev, channelId),
        );
        void qc.invalidateQueries({ queryKey: channelsKey(workspaceId) });
      }
    },
  });
}

/**
 * Pure: flip ONE channel's list carrier to `is_member: true` after a successful
 * self-join. Kept a pure function (not inlined into the mutation) so the
 * convergence contract is unit-testable without a query client. A row not in
 * the loaded list (a deep-link to a channel the workspace list hasn't fetched)
 * is left untouched — there is nothing to converge. Referentially stable when
 * nothing changes (already a member, or id absent), so an idempotent retry
 * writes no new cache object. Failure has NO handler and therefore never calls
 * this — a failed join leaves every carrier as `nonmember`.
 */
export function applyChannelJoinToList(
  payload: ChannelListPayload | undefined,
  channelId: string,
): ChannelListPayload | undefined {
  if (!payload) return payload;
  let changed = false;
  const items = payload.items.map((item) => {
    if (item.id !== channelId || item.is_member === true) return item;
    changed = true;
    return { ...item, is_member: true };
  });
  return changed ? { ...payload, items } : payload;
}

export function useCreateChannel(workspaceId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: CreateConversationChannelInput) => {
      if (!workspaceId) throw new Error("no workspace selected");
      const res = await api<{ channel: ChannelLikeDto }>(apiPaths.workspaceChannels(workspaceId), {
        method: "POST",
        body: JSON.stringify(input),
      });
      return res.channel;
    },
    onSuccess: () => {
      if (workspaceId) qc.invalidateQueries({ queryKey: channelsKey(workspaceId) });
    },
  });
}

/** Star / unstar a channel for the current viewer (#task:1209). Personal favorite.
 *  Since #task:1888 a channel star is an EXCLUSIVE MOVE into the global STARRED
 *  list, so it reconciles TWO surfaces: this sidebar's inline `viewerSort` (which
 *  drives the exclusion, so the row leaves/returns instantly) and the favorites
 *  list itself. Optimistic + reconciled from the server's full `viewer_sort`. No-op
 *  on the local cache when no workspace is selected (no cache key to reconcile);
 *  the favorites invalidation still runs. */
export function useStarChannel(workspaceId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ channelId, starred }: { channelId: string; starred: boolean }) => {
      const res = await api<{ viewer_sort: ViewerSortStateDto }>(apiPaths.channelStar(channelId), {
        method: starred ? "POST" : "DELETE",
      });
      return res.viewer_sort;
    },
    onMutate: async ({ channelId, starred }) => {
      if (!workspaceId) return { prev: undefined };
      const key = channelsKey(workspaceId);
      await qc.cancelQueries({ queryKey: key });
      const prev = qc.getQueryData<ChannelListPayload>(key);
      if (prev) {
        qc.setQueryData<ChannelListPayload>(key, {
          ...prev,
          // Home `rank` is preserved (#task:1888): STARRED is a different list, so
          // unstar must drop the channel back into the slot it had here.
          viewerSort: setChannelStarredOptimistic(prev.viewerSort, channelId, starred),
        });
      }
      return { prev };
    },
    onError: (_e, _v, ctx) => {
      if (workspaceId && ctx?.prev) qc.setQueryData(channelsKey(workspaceId), ctx.prev);
    },
    onSuccess: (viewerSort) => {
      // The moved-to / moved-from surface. Membership of the global list is a
      // projection of `starred_at`, so it can only be refetched — there is no local
      // row to synthesize (the favorites row carries server-resolved parent identity
      // and a server-scored unread this cache doesn't have).
      void qc.invalidateQueries({ queryKey: favoritesKey });
      if (!workspaceId) return;
      qc.setQueryData<ChannelListPayload>(channelsKey(workspaceId), (prev) =>
        prev ? { ...prev, viewerSort } : prev,
      );
    },
  });
}

export function useArchiveChannel(workspaceId: string | null) {
  const qc = useQueryClient();
  return useMutation(channelArchiveMutationOptions(qc, workspaceId));
}

export function channelArchiveMutationOptions(qc: QueryClient, workspaceId: string | null) {
  return {
    mutationFn: async ({
      channelId,
      archived,
    }: {
      channelId: string;
      archived: boolean;
      workspaceId?: string;
    }) => {
      const res = await api<{ channel: ChannelLikeDto }>(apiPaths.channelArchive(channelId), {
        method: "PATCH",
        body: JSON.stringify({ archived }),
      });
      return res.channel;
    },
    onSuccess: (channel: ChannelLikeDto, vars: { workspaceId?: string }) => {
      const targetWorkspaceId = vars.workspaceId ?? channel.workspace_id ?? workspaceId;
      reconcileChannelArchive(qc, channel.id, targetWorkspaceId);
    },
    onError: (_err: unknown, vars: { archived: boolean }) => {
      toast.error(t(vars.archived ? "workspaces.archiveFailed" : "workspaces.unarchiveFailed"));
    },
  };
}

/** Archive ACK invalidates channel lists and the detail used by row menus. */
export function reconcileChannelArchive(
  qc: QueryClient,
  channelId: string,
  workspaceId: string | null,
): void {
  if (workspaceId) {
    void qc.invalidateQueries({ queryKey: channelsKey(workspaceId) });
    void qc.invalidateQueries({ queryKey: archivedChannelsKey(workspaceId) });
  }
  void qc.invalidateQueries({ queryKey: ["archived-channels"] });
  void qc.invalidateQueries({ queryKey: channelDetailKey(channelId) });
}

export function useRenameChannel(workspaceId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      channelId,
      title,
    }: {
      channelId: string;
      title: string;
      workspaceId?: string;
    }) => {
      const res = await api<{ channel: ChannelLikeDto }>(apiPaths.conversationChannel(channelId), {
        method: "PATCH",
        body: JSON.stringify({ title }),
      });
      return res.channel;
    },
    onSuccess: (channel, vars) => {
      const targetWorkspaceId = vars.workspaceId ?? channel.workspace_id ?? workspaceId;
      // Rename keeps the row in place (unlike archive), but the title is
      // mirrored into several read models, so refresh all of them rather
      // than only the active list — a rename on an archived channel (the
      // header settings menu is reachable while archived) or in a foreign
      // secondary pane would otherwise show a stale title until an
      // unrelated WS event happened to refetch:
      //   - active + archived channel lists (sidebar groups)
      //   - the global archived-channels index
      //   - the conversation-ref cache (#conv:<id> chips / secondary pane)
      if (targetWorkspaceId) {
        qc.invalidateQueries({ queryKey: channelsKey(targetWorkspaceId) });
        qc.invalidateQueries({ queryKey: archivedChannelsKey(targetWorkspaceId) });
      }
      qc.invalidateQueries({ queryKey: ["archived-channels"] });
      qc.invalidateQueries({ queryKey: ["conversation-ref", channel.id] });
      qc.invalidateQueries({ queryKey: channelDetailKey(channel.id) });
    },
  });
}

/**
 * Self-LEAVE a channel: `POST /api/channels/:id/leave` (empty body — the caller
 * always removes THEMSELVES, no forgeable target). The 200 returns the SAME
 * channel-detail envelope as join/detail with the authoritative
 * `is_member: false`; the cache handling forks on the just-left channel's
 * visibility, and the current workspace list converges in place so the open
 * view reacts without waiting for the refetch:
 *   - PUBLIC → the row stays (still discoverable) with `is_member: false`, and
 *     we backfill the detail cache from the authoritative envelope, so the open
 *     ChannelView collapses to `ChannelJoinBar` (rejoin anytime).
 *   - PRIVATE → the row drops from the discovery union AND we EVICT the detail
 *     + conversation-ref cache entries — a private channel is invisible to a
 *     non-member and must not stay resolvable on any read path (§12 read
 *     authority); the primary pane is navigated away by the caller.
 * Rejects on the typed 403/409 (`not_member` / `channel_archived` /
 * `channel_owner` / `last_human_member`). The settings modal disables Leave from
 * the carrier, so a rejection is defensive only; on failure the caches are
 * untouched → the viewer stays a member.
 */
export function useLeaveChannel(workspaceId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (channelId: string) =>
      api<ChannelDetailResponse>(apiPaths.channelLeave(channelId), { method: "POST" }),
    onSuccess: (res, channelId) => {
      const visibility = res.channel.roster_policy === "open" ? res.channel.visibility : "public";
      if (visibility === "private") {
        // Private leave = "back to before you joined": the channel is invisible
        // to a non-member and must NOT stay resolvable in any read-path cache
        // (§12 read authority). EVICT the detail + conversation-ref entries
        // rather than caching a `{ is_member:false }` object a detail/ref read
        // could still surface as a non-member-visible channel.
        qc.removeQueries({ queryKey: channelDetailKey(channelId) });
        qc.removeQueries({ queryKey: ["conversation-ref", channelId] });
      } else {
        // Public leave: the channel stays browsable, so backfill the
        // authoritative `is_member:false` detail — the open view collapses to
        // the Join bar off the same envelope join/detail return.
        qc.setQueryData(channelDetailKey(channelId), res);
      }
      if (workspaceId) {
        qc.setQueryData<ChannelListPayload>(channelsKey(workspaceId), (prev) =>
          applyChannelLeaveToList(prev, channelId, visibility),
        );
        void qc.invalidateQueries({ queryKey: channelsKey(workspaceId) });
      }
    },
  });
}

/**
 * Pure: converge ONE channel's list carrier after a successful self-leave. Kept
 * pure (not inlined) so the two-visibility contract is unit-testable without a
 * query client, and referentially stable when nothing changes (idempotent). A
 * PRIVATE channel drops from the list (a non-member can't discover it); a PUBLIC
 * channel keeps its row with `is_member: false` (browse-and-Join). Mirror of
 * `applyChannelJoinToList`. Failure has no handler and never calls this.
 */
export function applyChannelLeaveToList(
  payload: ChannelListPayload | undefined,
  channelId: string,
  visibility: ConversationVisibility,
): ChannelListPayload | undefined {
  if (!payload) return payload;
  if (visibility === "private") {
    const items = payload.items.filter((item) => item.id !== channelId);
    return items.length === payload.items.length ? payload : { ...payload, items };
  }
  let changed = false;
  const items = payload.items.map((item) => {
    if (item.id !== channelId || item.is_member === false) return item;
    changed = true;
    return { ...item, is_member: false };
  });
  return changed ? { ...payload, items } : payload;
}

/**
 * Toggle a top-level channel's visibility (private ↔ public):
 * `PATCH /api/channels/:id` with `{ visibility }` — the SAME endpoint as rename,
 * but the v1 schema is exactly-one-field so visibility is its OWN mutation, never
 * bundled into the rename Save (no half-committed title+visibility state).
 * Admin/human governance is gated server-side; a same-value request is a no-op.
 * A visibility change keeps the viewer a member, so — unlike leave — there is no
 * membership convergence; we invalidate the read models that mirror the
 * private/public glyph + discovery membership: the channel lists, the global
 * archived index, the channel-detail carrier, and the conversation-ref cache
 * (`#conv:` chips / secondary pane).
 */
export function useSetChannelVisibility(workspaceId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      channelId,
      visibility,
    }: {
      channelId: string;
      visibility: ConversationVisibility;
      workspaceId?: string;
    }) => {
      const res = await api<{ channel: ChannelLikeDto }>(apiPaths.conversationChannel(channelId), {
        method: "PATCH",
        body: JSON.stringify({ visibility }),
      });
      return res.channel;
    },
    onSuccess: (channel, vars) => {
      const targetWorkspaceId = vars.workspaceId ?? channel.workspace_id ?? workspaceId;
      if (targetWorkspaceId) {
        qc.invalidateQueries({ queryKey: channelsKey(targetWorkspaceId) });
        qc.invalidateQueries({ queryKey: archivedChannelsKey(targetWorkspaceId) });
      }
      qc.invalidateQueries({ queryKey: ["archived-channels"] });
      qc.invalidateQueries({ queryKey: channelDetailKey(channel.id) });
      qc.invalidateQueries({ queryKey: ["conversation-ref", channel.id] });
    },
  });
}
