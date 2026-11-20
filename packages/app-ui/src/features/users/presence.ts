import { useCallback, useEffect, useMemo } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  apiPaths,
  UserPresenceEventSchema,
  type PresenceResponse,
  type ServerFrame,
  type UserPresence,
  type UserPresenceDto,
} from "@silverhand/shared";
import { api } from "@/lib/api";
import { onWsFrame, subscribeWsScope } from "@/lib/ws-client";
import { t } from "@/lib/i18n";

const presenceKey = ["presence"] as const;
const STALE_MS = 30 * 1000;
const GC_MS = 5 * 60 * 1000;

export function usePresence() {
  return useQuery<PresenceResponse, Error, UserPresenceDto[]>({
    queryKey: presenceKey,
    queryFn: () => api<PresenceResponse>(apiPaths.presence),
    select: (res) => res.users,
    staleTime: STALE_MS,
    gcTime: GC_MS,
  });
}

/**
 * One person's presence. Selects that person's row out of the shared snapshot,
 * so a change to anyone else (the upsert keeps other rows' identity) does not
 * re-render the reader. A miss reads as offline.
 */
export function usePresenceOf(userId: string): UserPresenceDto | undefined {
  const select = useCallback(
    (res: PresenceResponse) => res.users.find((u) => u.user_id === userId),
    [userId],
  );
  return useQuery<PresenceResponse, Error, UserPresenceDto | undefined>({
    queryKey: presenceKey,
    queryFn: () => api<PresenceResponse>(apiPaths.presence),
    select,
    staleTime: STALE_MS,
    gcTime: GC_MS,
  }).data;
}

/**
 * Presence keyed by `user_id`. A miss means "not yet known" — callers
 * treat the absence as offline (no live connection observed). Rebuilt
 * only when the underlying snapshot changes.
 */
export function usePresenceById(): Map<string, UserPresenceDto> {
  const { data: users } = usePresence();
  return useMemo(() => {
    const map = new Map<string, UserPresenceDto>();
    if (!users) return map;
    for (const u of users) map.set(u.user_id, u);
    return map;
  }, [users]);
}

/**
 * Subscribe to the global `app` scope and reconcile the `['presence']`
 * cache on each frame. **Mount ONCE in the app shell** (next to
 * {@link useViewerWorkspacesSync}) — it must outlive any single view so
 * the presence map stays fresh regardless of which surface reads it.
 *
 * Two reconciliation paths (see {@link decidePresenceFrameAction}):
 *  - `user_presence` transition → **upsert in place** (the delta carries
 *    the full {@link UserPresenceDto}, so no refetch per online/offline
 *    flip).
 *  - `snapshot_required` (app-scope epoch mismatch / replay gap, e.g.
 *    backend restart) → **invalidate** so the REST snapshot re-syncs the
 *    authoritative set — without this the cache could show pre-restart
 *    state until `staleTime`. This is the recovery path from
 *    `docs/human-presence-v1.md` §3.4.
 */
export function usePresenceSync(): void {
  const qc = useQueryClient();
  useEffect(() => {
    const sub = subscribeWsScope("app");
    const unsub = onWsFrame((frame) => {
      const action = decidePresenceFrameAction(frame);
      if (!action) return;
      if (action.kind === "invalidate") {
        void qc.invalidateQueries({ queryKey: presenceKey });
        return;
      }
      qc.setQueryData<PresenceResponse>(presenceKey, (prev) => upsertPresence(prev, action.delta));
    });
    return () => {
      sub.unsubscribe();
      unsub();
    };
  }, [qc]);
}

/**
 * How an incoming WS frame should reconcile the `['presence']` cache:
 *  - `upsert` — an app-scope `user_presence` event with a valid payload.
 *  - `invalidate` — an app-scope `snapshot_required` (drop & refetch).
 *  - `null` — anything else (other scopes/kinds, ping, malformed payload).
 *
 * Pure — exported for unit tests. Mirrors the `decideViewerSyncInvalidations`
 * idiom so the hook body stays a thin dispatcher.
 */
export type PresenceFrameAction =
  | { kind: "upsert"; delta: UserPresenceDto }
  | { kind: "invalidate" };

export function decidePresenceFrameAction(frame: ServerFrame): PresenceFrameAction | null {
  if (frame.type === "snapshot_required") {
    return frame.scope === "app" ? { kind: "invalidate" } : null;
  }
  if (frame.type !== "event" || frame.scope !== "app" || frame.kind !== "user_presence") {
    return null;
  }
  const parsed = UserPresenceEventSchema.safeParse(frame.payload);
  return parsed.success ? { kind: "upsert", delta: parsed.data } : null;
}

/**
 * Upsert a single presence delta into the snapshot, preserving order
 * (existing rows keep their slot; unknown users append). Pure — exported
 * for unit tests.
 */
export function upsertPresence(
  prev: PresenceResponse | undefined,
  delta: UserPresenceDto,
): PresenceResponse {
  if (!prev) return { users: [delta] };
  const idx = prev.users.findIndex((u) => u.user_id === delta.user_id);
  if (idx === -1) return { users: [...prev.users, delta] };
  const users = prev.users.slice();
  users[idx] = delta;
  return { users };
}

/**
 * Map human presence onto the shared {@link SidebarRow} status dot:
 * online → `active` (filled accent), offline / unknown → `idle` (muted).
 * Reuses the agent dot vocabulary so AGENTS / HUMANS rows render an
 * identical pip. Pure — exported for unit tests.
 */
export function presenceDotStatus(presence: UserPresence | undefined): "active" | "idle" {
  return presence === "online" ? "active" : "idle";
}

/**
 * Human presence label for the info popover (the human counterpart to
 * `AgentInfoPopover`'s `PresenceInline`): online → "Online"; offline with
 * a recorded last-seen → "Last seen X ago"; offline with none → "Offline".
 * The relative string is injected so this stays pure / testable — callers
 * pass `dayjs(lastSeenAt).fromNow()`.
 */
export function humanPresenceText(
  presence: UserPresence | undefined,
  lastSeenAt: number | null,
  relativeLastSeen: string,
): string {
  if (presence === "online") return t("common.online");
  if (lastSeenAt != null) return t("common.lastSeen", { time: relativeLastSeen });
  return t("common.offline");
}
