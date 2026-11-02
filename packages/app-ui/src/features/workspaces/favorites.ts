import { useEffect } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "@/lib/toast";
import { t } from "@/lib/i18n";
import type { QueryClient } from "@tanstack/react-query";
import type {
  FavoriteRowDto,
  ListFavoritesDto,
  UnreadUpdatedMessagePayload,
} from "@silverhand/shared";
import { apiPaths } from "@silverhand/shared";
import { api } from "@/lib/api";
import type { FavoritesWatchScope, StarMembership } from "@/features/workspaces/home-grouping";
import {
  isCurrentViewerScope,
  onWsFrame,
  subscribeViewerScope,
  subscribeWsScope,
} from "@/lib/ws-client";

export const favoritesKey = ["favorites", "all"] as const;

export function patchFavoritesUnread(
  qc: QueryClient,
  payload: UnreadUpdatedMessagePayload,
): "patched" | "unchanged" | "absent" | "cold" {
  const prev = qc.getQueryData<ListFavoritesDto>(favoritesKey);
  if (!prev) return "cold"; // list not loaded → no-op
  const count = payload.conversation_unread_count;
  const idx = prev.items.findIndex((it) => it.entity_id === payload.conversation_id);
  const row = prev.items[idx];
  if (row) {
    if (row.unread_count === count) return "unchanged";
    const items = prev.items.slice();
    items[idx] = { ...row, unread_count: count };
    qc.setQueryData<ListFavoritesDto>(favoritesKey, { ...prev, items });
    return "patched";
  }
  const pairs = prev.direct_pairs ?? [];
  const pairIdx = pairs.findIndex((it) => it.conversation_id === payload.conversation_id);
  const pair = pairs[pairIdx];
  if (!pair) return "absent"; // no such favorite row → no-op (never insert)
  if (pair.unread_count === count) return "unchanged";
  const directPairs = pairs.slice();
  directPairs[pairIdx] = { ...pair, unread_count: count };
  qc.setQueryData<ListFavoritesDto>(favoritesKey, { ...prev, direct_pairs: directPairs });
  return "patched";
}

export function removeWorkspaceFromFavorites(qc: QueryClient, workspaceId: string): void {
  const prev = qc.getQueryData<ListFavoritesDto>(favoritesKey);
  if (!prev) return;

  const removedKeys = new Set(
    prev.items.filter((it) => it.workspace_id === workspaceId).map((it) => it.favorite_key),
  );
  const items = prev.items.filter((it) => it.workspace_id !== workspaceId);
  const entries = prev.viewer_sort.entries.filter((e) => !removedKeys.has(e.entity_id));
  const changed =
    items.length !== prev.items.length || entries.length !== prev.viewer_sort.entries.length;
  if (!changed) return; // nothing matched → no write
  qc.setQueryData<ListFavoritesDto>(favoritesKey, {
    ...prev,
    items,
    viewer_sort: { ...prev.viewer_sort, entries },
  });
}

export function useFavorites(watchWorkspaceIds: FavoritesWatchScope) {
  const qc = useQueryClient();
  const query = useFavoritesList();

  // Joined into a stable string so the effect re-subscribes only when the SET
  // changes, not on every render of the caller's array literal.
  const scopeKey = Array.from(new Set(watchWorkspaceIds)).sort().join(",");

  useEffect(() => {
    const scopeIds = scopeKey ? scopeKey.split(",") : [];
    const subs = scopeIds.map((id) => subscribeWsScope(`workspace:${id}`));
    const sub = subscribeViewerScope();
    const unsub = onWsFrame((frame) => {
      const viewerSnapshot =
        frame.type === "snapshot_required" && isCurrentViewerScope(frame.scope);
      void applyFavoritesSync(qc, frame, scopeIds, viewerSnapshot);
    });
    return () => {
      for (const s of subs) s.unsubscribe();
      sub.unsubscribe();
      unsub();
    };
  }, [qc, scopeKey]);

  return query;
}

export function useFavoritesList() {
  return useQuery<ListFavoritesDto>({
    queryKey: favoritesKey,
    queryFn: ({ signal }) => api<ListFavoritesDto>(apiPaths.favorites, { signal }),
  });
}

export function useStarMembership(conversationId: string | null): StarMembership {
  const { data, status } = useFavoritesList();
  if (conversationId === null) return "unknown";
  if (!data) return status === "error" ? "unstarred" : "unknown";
  return starredConversationIds(data).has(conversationId) ? "starred" : "unstarred";
}

/** Every starred conversation id, channels and direct pairs alike. */
export function starredConversationIds(data: ListFavoritesDto | undefined): Set<string> {
  const ids = new Set<string>();
  for (const item of data?.items ?? []) ids.add(item.entity_id);
  for (const pair of data?.direct_pairs ?? []) ids.add(pair.conversation_id);
  return ids;
}

/** Star / unstar a direct pair. It has no workspace list to patch, so only the
 *  favorites projection is refetched. */
export function useStarDirectPair() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      conversationId,
      starred,
    }: {
      conversationId: string;
      starred: boolean;
    }) => {
      await api<unknown>(apiPaths.channelStar(conversationId), {
        method: starred ? "POST" : "DELETE",
      });
    },
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: favoritesKey });
    },
    onError: (_e, { starred }) => {
      toast.error(t(starred ? "chat.starFailed" : "chat.unstarFailed"));
    },
  });
}

/** Minimal WS frame shape {@link decideFavoritesInvalidation} consumes, so the
 *  decision can be exercised without React or a socket (mirrors
 *  `ViewerSyncFrame` in `features/workspaces/hooks`). */
export interface FavoritesSyncFrame {
  type: string;
  /** Optional so a raw `ServerFrame` (whose non-scoped variants carry none) can
   *  be passed straight in — a frame with no scope matches no watched workspace. */
  scope?: string;
  kind?: string;
}

export function decideFavoritesInvalidation(
  frame: FavoritesSyncFrame,
  scopeIds: readonly string[],
): boolean {
  const watched = scopeIds.some((id) => frame.scope === `workspace:${id}`);
  if (frame.type === "snapshot_required") return watched;
  if (frame.type !== "event") return false;
  if (frame.kind !== "workspace_channels_changed") return false;
  return watched;
}

export function useUnstarFavorite() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (item: FavoriteRowDto) => {
      await api<unknown>(apiPaths.channelStar(item.entity_id), { method: "DELETE" });
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: favoritesKey });
      void qc.invalidateQueries({ queryKey: ["workspaces"] });
    },
    onError: () => {
      toast.error(t("chat.unstarFailed"));
    },
  });
}

/** The subscribed workspace set survives an empty favorites projection. */
export function applyFavoritesSync(
  qc: QueryClient,
  frame: FavoritesSyncFrame,
  scopeIds: readonly string[],
  viewerSnapshot = false,
): Promise<void> {
  if (viewerSnapshot || decideFavoritesInvalidation(frame, scopeIds)) {
    return qc.invalidateQueries({ queryKey: favoritesKey });
  }
  return Promise.resolve();
}
