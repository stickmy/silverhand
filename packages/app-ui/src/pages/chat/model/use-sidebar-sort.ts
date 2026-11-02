import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  apiPaths,
  SidebarSortResponseSchema,
  SIDEBAR_SORT_CHANGED_EVENT,
  SIDEBAR_SORT_GROUP,
  SIDEBAR_SORT_LIST_KEY,
} from "@silverhand/shared";
import { toast } from "sonner";
import { t } from "@/lib/i18n";
import { api } from "@/lib/api";
import { useViewerUser } from "@/features/session/hooks";
import { isCurrentViewerScope, onWsFrame, subscribeViewerScope } from "@/lib/ws-client";

// Workspace invalidation also refreshes the authorized complete branch set.

export function useSidebarSort() {
  const qc = useQueryClient();
  const viewer = useViewerUser().data;
  const viewerId = viewer && "id" in viewer ? viewer.id : null;
  const sidebarSortKey = useMemo(
    () => ["workspaces", "sidebar-sort", viewerId] as const,
    [viewerId],
  );
  const [pendingOrder, setPendingOrder] = useState<{ viewerId: string; ids: string[] } | null>(
    null,
  );
  const saving = useRef<AbortController | null>(null);
  const query = useQuery({
    queryKey: sidebarSortKey,
    enabled: viewerId !== null,
    queryFn: async ({ signal }) =>
      SidebarSortResponseSchema.parse(await api(apiPaths.sidebarSort, { signal })),
  });

  useEffect(() => {
    const subscription = subscribeViewerScope();
    const unsubscribe = onWsFrame((frame) => {
      if (
        frame.type === "authenticated" ||
        (frame.type === "snapshot_required" && isCurrentViewerScope(frame.scope)) ||
        (frame.type === "event" &&
          isCurrentViewerScope(frame.scope) &&
          (frame.kind === SIDEBAR_SORT_CHANGED_EVENT || frame.kind === "viewer_workspaces_changed"))
      ) {
        saving.current?.abort();
        setPendingOrder(null);
        void qc.invalidateQueries({ queryKey: sidebarSortKey });
      }
    });
    return () => {
      saving.current?.abort();
      unsubscribe();
      subscription.unsubscribe();
    };
  }, [qc, sidebarSortKey]);

  const reorder = async (orderedIds: string[]) => {
    if (saving.current || !query.data || !viewerId) {
      return;
    }
    const controller = new AbortController();
    saving.current = controller;
    // Local state keeps the drop and transform reset in the same render.
    setPendingOrder({ viewerId, ids: orderedIds });
    try {
      await qc.cancelQueries({ queryKey: sidebarSortKey });
      SidebarSortResponseSchema.parse(
        await api(apiPaths.sortOrder, {
          method: "PUT",
          signal: controller.signal,
          body: JSON.stringify({
            list_key: SIDEBAR_SORT_LIST_KEY,
            group: SIDEBAR_SORT_GROUP,
            ordered_ids: orderedIds,
          }),
        }),
      );
    } catch {
      if (!controller.signal.aborted) {
        toast.error(t("chat.sortSaveFailed"));
      }
    } finally {
      // Always read the current complete set; a late write ACK never replaces it.
      if (!controller.signal.aborted) {
        await qc.invalidateQueries({ queryKey: sidebarSortKey });
        setPendingOrder(null);
      }
      saving.current = null;
    }
  };

  return {
    order:
      (pendingOrder?.viewerId === viewerId ? pendingOrder?.ids : null) ??
      query.data?.viewer_sort.entries.map((entry) => entry.entity_id),
    ready: query.data !== undefined,
    reorder,
    pending: pendingOrder !== null && pendingOrder.viewerId === viewerId,
  };
}
