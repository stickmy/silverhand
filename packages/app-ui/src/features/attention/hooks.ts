// Attention bounded context — renderer read/write hooks (#task:1227 ②a).
// Mirrors `@silverhand/shared` `api/attention.ts`; kept in its own feature
// folder (not `features/channels`) to respect the bounded-context boundary —
// Attention is the read-side projection of the human's "queue waiting on me",
// distinct from messaging/channels. Canonical: docs/activity-lifecycle-v1.md.
import { useEffect } from "react";
import { useMutation, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import type { QueryClient } from "@tanstack/react-query";
import { toast } from "@/lib/toast";
import type {
  ActivityItemDto,
  AttentionBadgeRollupDto,
  AttentionBadgeUpdatedPayload,
  AttentionPrefChangedPayload,
  ChannelAttentionMode,
  ChannelAttentionModeRowDto,
  ChannelAttentionPreferenceDto,
  ListActivityDto,
  ObserveSource,
  ServerFrame,
  WorkspaceAttentionModesDto,
  WorkspaceAttentionPreferenceDto,
} from "@silverhand/shared";
import { apiPaths } from "@silverhand/shared";
import { api } from "@/lib/api";
import { t } from "@/lib/i18n";
import { resolveEffectiveAttentionMode } from "@/features/attention/attention-mode";
import { activityItemSource, postObserve } from "./observe";
import {
  reconcileUnreadAfterObserveIfRealtimeUnavailable,
  settleStreamUnreadAfterObserve,
} from "./observe-cache-fallback";
import { onWsFrame, subscribeViewerScope, isCurrentViewerScope } from "@/lib/ws-client";

/** Query key for the viewer's mute SUPPRESSOR on one workspace. Workspace-KEYED
 *  in the `['workspaces', id, …]` form on purpose: that is the form layer 1 of the
 *  viewer-removal contract sweeps, so losing the workspace drops this with no
 *  registration of its own (see `features/workspaces/viewer-removal-contract.ts`). */
export const workspaceAttentionPreferenceKey = (workspaceId: string) =>
  ["workspaces", workspaceId, "attention-preference"] as const;

/** Query key for ONE workspace's batch of channel tiers. Kept under the
 *  `['attention','modes']` prefix — unlike the suppressor above — because the
 *  channel-tier mutation legitimately does not know its workspace and patches by
 *  scanning that prefix. The cost is that layer 1's `['workspaces', …]` sweep does
 *  NOT cover it, so {@link removeWorkspaceFromAttentionBadges} drops it explicitly. */
export const attentionModesKey = (workspaceId: string) =>
  ["attention", "modes", workspaceId] as const;

/** Query key for the viewer's cross-workspace badge roll-up. A single global
 *  entry — the roll-up itself spans every workspace. */
export const attentionBadgesKey = () => ["attention", "badges"] as const;

/**
 * Write one channel's new tier into every loaded tier batch, re-resolving
 * `effective_mode` through the SAME rule the server used
 * ({@link resolveEffectiveAttentionMode}). Patches by SCANNING the
 * `['attention','modes']` prefix rather than taking a `workspaceId`: the writer
 * is a channel-scoped mutation that legitimately does not know its workspace,
 * and a channel appears in at most one batch, so the scan is the cheap correct
 * shape rather than threading a second id through the call sites.
 *
 * PRESENT-ONLY: a batch that is not loaded, or that does not hold this channel,
 * is left alone — the authoritative GET brings the truth.
 */
function patchChannelModeInLoadedBatches(
  qc: QueryClient,
  conversationId: string,
  mode: ChannelAttentionMode,
): void {
  qc.setQueriesData<WorkspaceAttentionModesDto>({ queryKey: ["attention", "modes"] }, (prev) => {
    if (!prev) return prev;
    let hit = false;
    const channels = prev.channels.map((c) => {
      if (c.conversation_id !== conversationId) return c;
      hit = true;
      return {
        ...c,
        mode,
        effective_mode: resolveEffectiveAttentionMode(mode, prev.muted),
      };
    });
    return hit ? { ...prev, channels } : prev;
  });
}

/**
 * Set the viewer's attention tier for one channel (`PUT …`). This is one of the
 * two renderer surfaces that write attention state (§5.2) — it never touches
 * delivery/follow (`thread_followers`).
 *
 * The ONE cache this moves is the workspace tier batch: the sidebar row's badge
 * volume, the dimming and the menu check all read from there, so the per-channel
 * `GET …/attention-preference` is not a second carrier of the same fact (see
 * {@link useWorkspaceAttentionModes}). Optimistic so the write feels instant;
 * rollback is a re-read of the batch rather than a saved snapshot, because the
 * batch holds N rows and only the server can settle the rest.
 */
export function useSetChannelAttentionPreference(channelId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (mode: ChannelAttentionMode) => {
      if (!channelId) throw new Error("no channel");
      return api<ChannelAttentionPreferenceDto>(
        apiPaths.conversationAttentionPreference(channelId),
        { method: "PUT", body: JSON.stringify({ mode }) },
      );
    },
    onMutate: async (mode): Promise<void> => {
      if (!channelId) return;
      await qc.cancelQueries({ queryKey: ["attention", "modes"] });
      patchChannelModeInLoadedBatches(qc, channelId, mode);
    },
    onError: () => {
      // The optimistic re-tier must not survive the failure, and its pre-write
      // value isn't reconstructable from a snapshot of one row — re-read.
      void qc.invalidateQueries({ queryKey: ["attention", "modes"] });
      toast.error(t("common.updateFailed"));
    },
    onSuccess: (dto) => {
      patchChannelModeInLoadedBatches(qc, dto.conversation_id, dto.mode);
      // The tier is an `enter()` input, so flipping it adds/removes this
      // channel's ambient items from the Activity list — invalidate the (global)
      // activity query so the list reflects the new eligibility immediately, not
      // on the next unrelated unread/notification/task event. Backend treats this
      // as client-driven invalidation (no `unread_updated` for a pref change).
      void qc.invalidateQueries({ queryKey: ["activity"] });
      // The tier is a badge input too. The badge has its own absolute event, but
      // this writer must not depend on that frame arriving to stop showing a
      // stale count, so re-read the roll-up as well.
      void qc.invalidateQueries({ queryKey: attentionBadgesKey() });
    },
  });
}

// ───────────────── Workspace mute suppressor + tier batch (#task:1870) ─────────

/**
 * The viewer's mute suppressor for one PEOPLE-OWNED workspace
 * (`GET /api/workspaces/:id/attention-preference`). A BOOLEAN, not a fourth
 * tier: it silences every channel in the workspace without rewriting any
 * channel's own stored choice, so un-muting restores each one verbatim.
 *
 * People-owned workspaces only — a DB trigger refuses the row for an agent-owned
 * workspace, so an agent space is structurally un-mutable and callers must not
 * offer the control for one.
 */
export function useWorkspaceAttentionPreference(workspaceId: string | null) {
  return useQuery<WorkspaceAttentionPreferenceDto>({
    queryKey: workspaceAttentionPreferenceKey(workspaceId ?? "none"),
    enabled: !!workspaceId,
    queryFn: ({ signal }) =>
      api<WorkspaceAttentionPreferenceDto>(apiPaths.workspaceAttentionPreference(workspaceId!), {
        signal,
      }),
  });
}

/**
 * Flip the workspace mute suppressor (`PUT …`). Optimistic on BOTH faces it
 * moves — its own key and the workspace's tier batch — because muting is a
 * direct manipulation whose whole point is that N channel rows go quiet at once;
 * re-resolving them server-side first would show the rows lagging the menu.
 *
 * The optimistic tier re-resolution goes through
 * {@link resolveEffectiveAttentionMode}, the twin of the backend's
 * `effectiveChannelModeExpr`, so the instant state and the settled state are the
 * same function of the same two inputs.
 */
export function useSetWorkspaceAttentionPreference(workspaceId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (muted: boolean) => {
      if (!workspaceId) throw new Error("no workspace");
      return api<WorkspaceAttentionPreferenceDto>(
        apiPaths.workspaceAttentionPreference(workspaceId),
        { method: "PUT", body: JSON.stringify({ muted }) },
      );
    },
    onMutate: async (muted): Promise<{ prev?: WorkspaceAttentionPreferenceDto }> => {
      if (!workspaceId) return {};
      const key = workspaceAttentionPreferenceKey(workspaceId);
      await qc.cancelQueries({ queryKey: key });
      const prev = qc.getQueryData<WorkspaceAttentionPreferenceDto>(key);
      // Full DTO, not a spread of `prev`: the toggle must reflect even when the
      // click lands before the GET resolves (same reason as the channel arm).
      qc.setQueryData<WorkspaceAttentionPreferenceDto>(key, {
        workspace_id: workspaceId,
        muted,
      });
      patchWorkspaceMutedInLoadedBatch(qc, workspaceId, muted);
      return { prev };
    },
    onError: (_err, _muted, ctx) => {
      if (workspaceId) {
        const key = workspaceAttentionPreferenceKey(workspaceId);
        if (ctx?.prev) qc.setQueryData(key, ctx.prev);
        void qc.invalidateQueries({ queryKey: key });
        void qc.invalidateQueries({ queryKey: attentionModesKey(workspaceId) });
      }
      toast.error(t("common.updateFailed"));
    },
    onSuccess: (dto) => {
      qc.setQueryData(workspaceAttentionPreferenceKey(dto.workspace_id), dto);
      patchWorkspaceMutedInLoadedBatch(qc, dto.workspace_id, dto.muted);
      // The suppressor is an `enter()` input for every channel in the workspace,
      // and a badge input for the workspace itself.
      void qc.invalidateQueries({ queryKey: ["activity"] });
      void qc.invalidateQueries({ queryKey: attentionBadgesKey() });
    },
  });
}

/** Re-resolve every channel tier in ONE workspace's loaded batch against a new
 *  suppressor value. `mode` (the stored choice) is deliberately untouched — that
 *  is what makes un-muting restore each channel's own tier. PRESENT-ONLY. */
function patchWorkspaceMutedInLoadedBatch(
  qc: QueryClient,
  workspaceId: string,
  muted: boolean,
): void {
  qc.setQueryData<WorkspaceAttentionModesDto>(attentionModesKey(workspaceId), (prev) =>
    prev
      ? {
          ...prev,
          muted,
          channels: prev.channels.map((c) => ({
            ...c,
            effective_mode: resolveEffectiveAttentionMode(c.mode, muted),
          })),
        }
      : prev,
  );
}

/** The (key, fetcher) pair for ONE workspace's tier batch. Both consumers below
 *  compose it so the key and the request can't drift — that identity is what
 *  makes them share one cache entry rather than issue two reads. */
function attentionModesQuery(workspaceId: string): {
  queryKey: readonly unknown[];
  queryFn: (ctx: { signal?: AbortSignal }) => Promise<WorkspaceAttentionModesDto>;
} {
  return {
    queryKey: attentionModesKey(workspaceId),
    queryFn: ({ signal }) =>
      api<WorkspaceAttentionModesDto>(
        `${apiPaths.attentionModes}?workspace_id=${encodeURIComponent(workspaceId)}`,
        { signal },
      ),
  };
}

/**
 * Every channel tier in ONE workspace in a single read
 * (`GET /api/attention/modes?workspace_id=…`) — one request for the whole
 * sidebar instead of N per-row GETs. Each row carries its own `mode`, the
 * suppressed `effective_mode`, and `directed_unread_count`.
 *
 * Live sources: the tier itself moves on `attention_pref_changed`, and
 * `directed_unread_count` moves on every `notification` insert/read and on
 * positional reads — all three are handled by the single subscription in
 * {@link useAttentionRealtime}, which is mounted for the app's lifetime, so this
 * hook adds no second listener.
 */
export function useWorkspaceAttentionModes(workspaceId: string | null) {
  return useQuery<WorkspaceAttentionModesDto>({
    ...attentionModesQuery(workspaceId ?? "none"),
    enabled: !!workspaceId,
  });
}

/**
 * The SAME per-workspace tier batches as {@link useWorkspaceAttentionModes},
 * fanned out over several workspaces at once (#task:1889). The global STARRED
 * list holds conversations from ANY workspace, so a starred row's tier cannot
 * come from the selected workspace's batch alone — but it must still come from
 * the same authority, because the acceptance contract is that a conversation
 * paints the IDENTICAL {@link channelRowBadge} result before and after it moves
 * into STARRED.
 *
 * Deliberately a `useQueries` over `attentionModesKey(id)` — the very key
 * {@link useWorkspaceAttentionModes} uses — so React Query dedupes: the selected
 * workspace's sidebar and this fan-out share one cache entry and there is still
 * exactly ONE authority for a tier. It likewise adds no listener (realtime is
 * the single app-lifetime {@link useAttentionRealtime}), and every arm that
 * invalidates a single batch already keys off the payload's workspace rather
 * than the selection, so a non-selected workspace's batch stays live with no new
 * event or subscription.
 *
 * `byChannel` is keyed by `conversation_id` across all batches (ids are globally
 * unique). A conversation ABSENT from it has no tier — the caller must pass
 * `effective_mode: undefined` and render no badge, never a fabricated tier. The
 * read is intentionally NOT widened to cover those absences: the server's
 * population is member `channel`s only, so a starred-but-unjoined public channel
 * and a `dm_channel` are absent BY DESIGN, exactly as they are in the channel
 * sidebar today (a `dm_channel`'s rule is the DM row's own: always `'all'`).
 */
export function useAttentionModesAcrossWorkspaces(workspaceIds: readonly string[]): {
  byChannel: Map<string, ChannelAttentionModeRowDto>;
  mutedByWorkspace: Map<string, boolean>;
} {
  const results = useQueries({
    queries: workspaceIds.map((id) => attentionModesQuery(id)),
  });
  const byChannel = new Map<string, ChannelAttentionModeRowDto>();
  const mutedByWorkspace = new Map<string, boolean>();
  for (const r of results) {
    const batch = r.data;
    if (!batch) continue;
    mutedByWorkspace.set(batch.workspace_id, batch.muted);
    for (const row of batch.channels) byChannel.set(row.conversation_id, row);
  }
  return { byChannel, mutedByWorkspace };
}

/**
 * The viewer's cross-workspace badge roll-up (`GET /api/attention/badges`) — the
 * ONE carrier of "how loudly should this workspace shout" for every rail and
 * STARRED row. Do not give another read-model a second badge field for the same
 * fact (#task:1875 review): one fact reaching a cache two ways has to be kept in
 * sync forever, which is the drift source the STARRED "exclusive home" rule
 * already exists to avoid. A row's own `unread_count` is a DIFFERENT question
 * ("how many are unread", raw positional) and stays where it is.
 *
 * Since #task:1939 the workspace row PAINTS none of this: it reads its entry as an
 * invisible wake gate (poke that branch's `loud` rows out of whatever is collapsed
 * around them — the branch itself or the whole section) and as the presence dot's
 * suppressor. Still one carrier — one fewer face
 * (docs/activity-lifecycle-v1.md §8「呈现」).
 *
 * `by_workspace` omits zeros (absent ≡ 0) and `total` is their sum — the
 * app-level badge (macOS dock included: a muted workspace that still bumped the
 * dock would defeat muting).
 *
 * Query ONLY, so any number of rows may call it (React Query dedupes by key).
 * Its live updates come from {@link useAttentionRealtime}, which must be mounted
 * ONCE at the app level — without it this roll-up never moves.
 */
export function useAttentionBadges() {
  return useQuery<AttentionBadgeRollupDto>({
    queryKey: attentionBadgesKey(),
    queryFn: ({ signal }) => api<AttentionBadgeRollupDto>(apiPaths.attentionBadges, { signal }),
  });
}

/** Extract the workspaceId from a `workspace:<id>` transport scope, or null.
 *  Local rather than in `ws-frames.ts`'s scope grammar because only this
 *  consumer parses it back today; the prefix is the same literal the renderer
 *  subscribes with. Deliberately exact-prefix (never `includes`), so no other
 *  scope family can masquerade as a workspace. */
function parseWorkspaceScope(scope: string): string | null {
  return scope.startsWith("workspace:") ? scope.slice("workspace:".length) : null;
}

/**
 * Apply ONE server frame to the attention caches. Extracted from
 * {@link useAttentionRealtime} so the frame→invalidation policy is unit-testable
 * without a DOM: it is a pure function of the frame plus the one fact the module
 * singleton would otherwise supply (`isViewerScope` — whether this frame's scope
 * is the current viewer's), which the hook passes in.
 *
 * Live events and replayed events flow through here identically; every write is
 * either an invalidation or the idempotent absolute patch, so a re-delivered
 * frame cannot accumulate error.
 */
export function applyAttentionFrame(
  qc: QueryClient,
  frame: ServerFrame,
  isViewerScope: boolean,
): void {
  if (frame.type === "snapshot_required") {
    // Reconnect / gap catch-all: "an unknown amount happened in this scope".
    // BOTH scope families that carry an attention input must re-read, because
    // either can swallow the event that would otherwise settle the caches:
    //   - the VIEWER scope carries the badge's own absolute event, the pref
    //     change and the notification/unread frames the tier batch rides;
    //   - a WORKSPACE scope carries `workspace_channels_changed`, which is the
    //     ONLY carrier of the membership input — nothing viewer-scoped restates
    //     it, so a swallowed one is permanently wrong without this arm.
    // A workspace gap is confined to that workspace, so only its tier batch is
    // re-read; the roll-up is one global entry, so it re-reads whole either way.
    // Any other scope family (conversation / conversation-side / app) carries no
    // attention input — invalidating on those would turn every unrelated gap
    // into an attention refetch.
    if (isViewerScope) {
      void qc.invalidateQueries({ queryKey: attentionBadgesKey() });
      void qc.invalidateQueries({ queryKey: ["attention", "modes"] });
      return;
    }
    const workspaceId = parseWorkspaceScope(frame.scope);
    if (workspaceId) {
      void qc.invalidateQueries({ queryKey: attentionBadgesKey() });
      void qc.invalidateQueries({ queryKey: attentionModesKey(workspaceId) });
    }
    return;
  }
  if (frame.type !== "event") return;
  // Channel membership is a badge input the badge has no event for: both arms
  // of the projection join `conversation_users`, so a self-leave or a
  // third-party removal changes the absolute value.
  // `workspace_channels_changed` is the existing domain fact for that
  // transition (§12 — no new event needed, and emitting a badge-specific one
  // would encode the same fact twice). It is WORKSPACE-scoped, yet it reaches
  // EVERY workspace the viewer belongs to, not just the selected one:
  // `useForeignWorkspaceChannelRemovalSync` is mounted for the app's lifetime
  // and retains `workspace:<id>` for all `visibleWorkspaceIds` (it skips only
  // the selected workspace's RECONCILE, never the subscription), so this
  // global listener sees the frame on any of those scopes. That subscription
  // set is therefore load-bearing for the badge; the fence in
  // `attention-badge-scope-coverage.test.ts` keeps a later subscription
  // optimization from narrowing it silently.
  if (frame.kind === "workspace_channels_changed") {
    void qc.invalidateQueries({ queryKey: attentionBadgesKey() });
    void qc.invalidateQueries({ queryKey: ["attention", "modes"] });
    return;
  }
  if (!isViewerScope) return;
  if (frame.kind === "direct_pairs_changed") {
    // The DM term of the roll-up has no absolute frame; re-read so out-of-order
    // signals converge on the committed value.
    void qc.invalidateQueries({ queryKey: attentionBadgesKey() });
    return;
  }
  if (frame.kind === "attention_badge_updated") {
    applyBadgeUpdate(qc, frame.payload as AttentionBadgeUpdatedPayload);
    return;
  }
  if (frame.kind === "attention_pref_changed") {
    // Identity only, by design — the tier read face is the single authority,
    // so re-read rather than patch. A null `conversation_id` means the
    // workspace suppressor moved (which re-resolves every channel in it), so
    // its own key needs the re-read too; a channel tier lives only in the
    // batch, so the batch is the whole answer there.
    const p = frame.payload as AttentionPrefChangedPayload;
    void qc.invalidateQueries({ queryKey: attentionModesKey(p.workspace_id) });
    if (p.conversation_id === null) {
      void qc.invalidateQueries({
        queryKey: workspaceAttentionPreferenceKey(p.workspace_id),
      });
      // …and the roll-up, which now projects the suppressor as
      // `muted_workspace_ids` for the rail's muted marker (#task:1877). This is
      // an OBLIGATION of that field, not an optimisation: muting a workspace
      // that sits at 0 changes no number, so the backend has no badge frame to
      // send and `applyBadgeUpdate` would early-return on the same value anyway
      // — miss this and the marker stays missing until some unrelated attention
      // event happens along: the roll-up is app-lifetime and the client disables
      // `refetchOnWindowFocus`, so nothing re-reads it on its own.
      void qc.invalidateQueries({ queryKey: attentionBadgesKey() });
    }
    return;
  }
  if (frame.kind === "notification" || frame.kind === "unread_updated") {
    // `directed_unread_count` on the tier batch moves with both, so the modes batch
    // always needs the re-read.
    void qc.invalidateQueries({ queryKey: ["attention", "modes"] });
    // The two `unread_updated` variants are discriminated by `conversation_id`
    // presence (the canonical `isMessageUnreadUpdatedPayload` discriminator). The
    // ENRICHED message variant (has `conversation_id`) rides its own absolute
    // `attention_badge_updated` frame in the same transaction, so the badge roll-up
    // is already covered. The THIN notification-clear variant (source #3: a directed
    // notification going read) deliberately carries NO absolute badge frame — the
    // migrated backend publishes a refetch signal instead of a standalone absolute
    // count, because an absolute count published out of commit order would let an
    // earlier clear's stale value win under this cache's last-write-wins apply. So
    // for the thin variant the roll-up is authoritative-refetched HERE; two clears
    // delivered out of order each just re-read the current committed roll-up and
    // converge on it. A bare `notification` insert frame likewise carries no absolute
    // count and needs the refetch. (See scope_events.rs NotificationUnreadCleared.)
    const isEnrichedMessageVariant =
      frame.kind === "unread_updated" &&
      typeof (frame.payload as { conversation_id?: unknown } | undefined)?.conversation_id ===
        "string";
    if (!isEnrichedMessageVariant) {
      void qc.invalidateQueries({ queryKey: attentionBadgesKey() });
    }
  }
}

export function isViewerAttentionFrame(frame: ServerFrame): boolean {
  return (
    (frame.type === "event" || frame.type === "snapshot_required") &&
    isCurrentViewerScope(frame.scope)
  );
}

/**
 * The SINGLE attention realtime subscription — mount once, at the app level,
 * beside the other app-lifetime subscriptions. Split out from
 * {@link useAttentionBadges} so the roll-up query can be read by many rows
 * without forking that many identical listeners.
 *
 * It must cover every source the badge and the tier batch are functions of,
 * because a missed frame is not a transient window: these caches are only
 * written by these frames, and the client sets `refetchOnWindowFocus: false`, so
 * a miss FREEZES a wrong number until some unrelated event happens to arrive.
 *
 * It subscribes only the VIEWER scope; the workspace scopes it also reacts to are
 * held by `useForeignWorkspaceChannelRemovalSync` for the app's lifetime, and the
 * transport already drops any frame for a scope no one desires, so a
 * `workspace:<id>` frame reaching this listener is by construction a held one.
 */
export function useAttentionRealtime(): void {
  const qc = useQueryClient();

  useEffect(() => {
    const sub = subscribeViewerScope();
    const unsub = onWsFrame((frame) => {
      applyAttentionFrame(qc, frame, isViewerAttentionFrame(frame));
    });
    return () => {
      sub.unsubscribe();
      unsub();
    };
  }, [qc]); // eslint-disable-line react-hooks/exhaustive-deps
}

/** The server's `total`: direct pairs plus every workspace. */
function rollupTotal(directPairs: number | undefined, byWorkspace: Record<string, number>): number {
  return Object.values(byWorkspace).reduce((a, b) => a + b, directPairs ?? 0);
}

/**
 * Patch one workspace's ABSOLUTE badge into the roll-up. Never a delta — the
 * event carries the settled value precisely so a dropped or reordered frame
 * cannot accumulate error.
 *
 * `total` is RECOMPUTED from the map rather than adjusted, so `total =
 * direct_pairs + Σ by_workspace` holds by construction instead of by two
 * matching arithmetic paths. A zero deletes its entry, keeping the server's "absent ≡ 0" shape (a
 * stored `0` would make `Object.keys` a wrong answer to "which workspaces have
 * something waiting").
 *
 * Roll-up not loaded → no-op: the GET is the authority, and inventing a
 * one-entry map would claim every OTHER workspace is at zero.
 */
export function applyBadgeUpdate(qc: QueryClient, payload: AttentionBadgeUpdatedPayload): void {
  const prev = qc.getQueryData<AttentionBadgeRollupDto>(attentionBadgesKey());
  if (!prev) return;
  const by_workspace = { ...prev.by_workspace };
  if (payload.badge_count > 0) by_workspace[payload.workspace_id] = payload.badge_count;
  else delete by_workspace[payload.workspace_id];
  const total = rollupTotal(prev.direct_pairs, by_workspace);
  if (
    total === prev.total &&
    by_workspace[payload.workspace_id] === prev.by_workspace[payload.workspace_id]
  ) {
    return; // idempotent replay — no write, no re-render
  }
  qc.setQueryData<AttentionBadgeRollupDto>(attentionBadgesKey(), {
    by_workspace,
    direct_pairs: prev.direct_pairs,
    total,
    // The frame carries the NUMBER only; mute is a different fact with its own
    // event (#task:1877), so carry the held value through untouched rather than
    // letting a badge patch imply anything about it.
    muted_workspace_ids: prev.muted_workspace_ids,
  });
}

/**
 * Settle the attention caches when the viewer LOSES a workspace — removed from it,
 * or it was deleted (#task:1412's contract, the same fact
 * `removeWorkspaceFrom{Activity,Favorites,MessageUnread}` consume).
 *
 * Mandatory, not hygiene: the roll-up feeds the dock badge and the nav-rail dot,
 * and it is only ever written by its own events. Leaving the lost workspace's
 * entry behind would keep the OS dock badge counting a workspace the viewer can no
 * longer open, with no focus refetch to correct it.
 *
 * The map delete is the part only this cleanup does; `total` is recomputed as
 * `direct_pairs + Σ by_workspace` — the value a refetch returns — never
 * `prev.total - dropped`, which would carry a stale offset onto the survivors.
 * The tier batch is dropped whole (it is per-workspace and can simply go).
 * PRESENT-ONLY and idempotent, like its siblings.
 */
export function removeWorkspaceFromAttentionBadges(qc: QueryClient, workspaceId: string): void {
  const prev = qc.getQueryData<AttentionBadgeRollupDto>(attentionBadgesKey());
  if (
    prev &&
    (workspaceId in prev.by_workspace || prev.muted_workspace_ids.includes(workspaceId))
  ) {
    const by_workspace = { ...prev.by_workspace };
    delete by_workspace[workspaceId];
    qc.setQueryData<AttentionBadgeRollupDto>(attentionBadgesKey(), {
      by_workspace,
      direct_pairs: prev.direct_pairs,
      total: rollupTotal(prev.direct_pairs, by_workspace),
      // A lost workspace's mute marker goes with its count, for the same reason
      // (#task:1877): nothing refetches this entry, so a leftover id would keep
      // marking a workspace the viewer can no longer open.
      muted_workspace_ids: prev.muted_workspace_ids.filter((id) => id !== workspaceId),
    });
  }
  qc.removeQueries({ queryKey: attentionModesKey(workspaceId) });
}

// ───────────────────────────── Activity list (②c) ─────────────────────────────

/** Query key for the viewer's Activity list. v1 is a single GLOBAL list (the
 *  human's whole "queue waiting on me" across every workspace), so there is one
 *  cache entry; `['activity']` is its prefix for broad invalidation. */
export const activityKey = () => ["activity", "all"] as const;

/** Whether an Activity item is attributed to ONE workspace — the single reading
 *  of the `location` union that both the removal filter and the held-channel scan
 *  go through, so they cannot disagree about attribution.
 *
 *  A `direct_pair` item belongs to NO workspace (#task:2115). A canonical pair is
 *  workspace-LESS, so there is no id to compare and nothing a workspace-removal
 *  fact can say about it: it is never attributed to a workspace and therefore never
 *  removed with one. This is why the discriminant is tested rather than a nullable
 *  id compared — `undefined !== workspaceId` would silently return the right answer
 *  for the wrong reason, and would flip the moment a DM row's coordinate were
 *  defaulted. */
function isInWorkspace(item: ActivityItemDto, workspaceId: string): boolean {
  return item.location.kind === "workspace_channel" && item.location.workspace_id === workspaceId;
}

/** Drop every Activity item for a workspace the viewer lost access to — removed
 *  from it, or the workspace deleted (#task:1409). Activity is the FOURTH
 *  viewer-local projection of the single `viewer_workspaces_changed(removed)`
 *  fact (after #task:1404's workspaces list / message-unread / favorites): a
 *  `workspace_channel` item carries its `workspace_id`, so a removed workspace's
 *  items must drop or they linger as ghosts (and keep the #task:1408 rail dot
 *  falsely lit). Consume the event's `workspace_id` and filter in place — NOT a
 *  refetch: the list is GLOBAL (`['activity','all']`, so a refetch wouldn't 404),
 *  but a whole-list refetch re-scores and re-orders every SURVIVING item — a
 *  needless reflow for a pure membership drop. This is a straight membership
 *  filter, never a re-score, so it stays a pure projection: survivors keep their
 *  server order, and a `direct_pair` survivor keeps its very identity — the
 *  filter cannot touch a placement that has no workspace axis to lose.
 *  PRESENT-ONLY / idempotent, mirroring the sibling `removeWorkspaceFrom*`
 *  cleanups: list not loaded → no-op; no matching item → no write.
 *
 *  Archive is deliberately NOT handled here — an archived channel stays
 *  visible-to-owner and its directed items self-heal via the existing
 *  `unread_updated` → refetch path once the backend `enter()` filter excludes
 *  archived (#task:1409 axis 1). Only workspace REMOVAL (a visibility-loss fact
 *  the Activity subscription does not otherwise observe) needs this local
 *  settle. */
export function removeWorkspaceFromActivity(qc: QueryClient, workspaceId: string): void {
  const prev = qc.getQueryData<ListActivityDto>(activityKey());
  if (!prev) return; // list not loaded → no-op
  const items = prev.items.filter((it) => !isInWorkspace(it, workspaceId));
  if (items.length === prev.items.length) return; // no matching item → no write
  qc.setQueryData<ListActivityDto>(activityKey(), { ...prev, items });
}

/**
 * The conversation-scoped sibling of {@link removeWorkspaceFromActivity}
 * (#task:1689): drop every Activity item anchored in ONE conversation the viewer
 * lost. A third party removing the viewer from a channel is the same CLASS of
 * fact as losing a workspace — the items keep pointing at a channel whose
 * membership ended, so they linger as ghosts (and keep the #task:1408 rail dot
 * falsely lit) until some unrelated refetch. Same discipline as the workspace
 * arm: consume the id, filter in place (no refetch — the list is global, so a
 * whole-list refetch would re-score and reorder every SURVIVOR for a pure
 * membership drop), PRESENT-ONLY and idempotent.
 *
 * A thread item carries its PARENT channel's `conversation_id` (thread-ness
 * rides `thread_root_id`), so one match drops the channel's mainline and thread
 * items together.
 */
export function removeConversationFromActivity(qc: QueryClient, conversationId: string): void {
  const prev = qc.getQueryData<ListActivityDto>(activityKey());
  if (!prev) return; // list not loaded → no-op
  const items = prev.items.filter((it) => it.conversation_id !== conversationId);
  if (items.length === prev.items.length) return; // no matching item → no write
  qc.setQueryData<ListActivityDto>(activityKey(), { ...prev, items });
}

/** Which channels of ONE workspace the Activity list still holds items for
 *  (#task:1689). Attribution is read off the payload's `location` (via the shared
 *  {@link isInWorkspace}), never inferred from the key — the list is global, and it
 *  now spans both placements, so a `direct_pair` item answers no workspace and
 *  contributes no channel here. Feeds the foreign-workspace convergence path: only
 *  a channel the viewer actually holds state for is worth an authoritative
 *  re-read. */
export function activityHeldChannelIds(qc: QueryClient, workspaceId: string): string[] {
  const data = qc.getQueryData<ListActivityDto>(activityKey());
  if (!data) return [];
  return data.items.filter((it) => isInWorkspace(it, workspaceId)).map((it) => it.conversation_id);
}

/**
 * The viewer-scoped frame kinds a live Activity re-read hangs on — the whole set,
 * named once so the list has ONE definition instead of a condition inlined in a
 * closure no test can reach.
 *
 * ONE ENTRY PER CARRIER, not per source family, and that distinction is the bug
 * this set was widened to fix (#task:2115). Activity has two source families
 * (positional unread, directed notifications) but THREE carriers, because the
 * positional family's frame is chosen by PLACEMENT on the emit side:
 * `MessageDeliveryCoordinator.publishUnreadUpdated` sends `unread_updated` (+ the
 * badge it is paired with) for a workspace conversation and
 * `direct_pairs_changed` for a `direct_pair` — deliberately, since the unread
 * read model's unit is (human viewer, WORKSPACE) and a pair has no coordinate in
 * it. So for a DM there is no `unread_updated` to wait for, ever. Before this
 * ticket that cost nothing because Activity could not show a DM at all; now that
 * it can, a two-kind set means the rail goes stale for exactly the item class the
 * person is most obliged to answer — a first DM arrives and the list, already
 * mounted, never re-reads.
 *
 * The CLEAR direction is the same shape and is why this is not a one-way patch:
 * `application/observe.ts` publishes `direct_pairs_changed` to the observing
 * viewer when a pair's cursor moves, where the workspace arm publishes
 * `unread_updated`. Without the third kind a DM read in the DM pane would leave
 * its row sitting in Activity.
 *
 * What this is NOT is "invalidate on any viewer frame". The viewer scope also
 * carries `attention_badge_updated`, `attention_pref_changed` and `task_changed`,
 * none of which can change this list (`kind=task` is out of Activity v1), and
 * swallowing them would turn every badge tick into a refetch of the app's hottest
 * read. The set is exhaustive on purpose: a fourth carrier has to be a decision
 * recorded here.
 */
export const ACTIVITY_INVALIDATING_EVENT_KINDS = [
  "unread_updated",
  "notification",
  "direct_pairs_changed",
] as const;

/**
 * Decide what one server frame does to the Activity cache — extracted from
 * {@link useActivity}'s subscription so the decision is a pure function of
 * (frame, isViewerScope), testable without a DOM, exactly as
 * {@link applyAttentionFrame} is.
 *
 * `isViewerScope` is passed in rather than derived here for the same reason it is
 * in the attention applier: the caller owns the identity of "this viewer", and a
 * frame's scope string is only meaningful against it.
 */
export function applyActivityFrame(
  qc: QueryClient,
  frame: ServerFrame,
  isViewerScope: boolean,
): void {
  if (frame.type === "snapshot_required") {
    // A gap on the viewer scope may have swallowed any of the three carriers, so
    // the only honest answer is a re-read — the list is app-lifetime and the
    // client disables `refetchOnWindowFocus`, so nothing else would.
    if (isViewerScope) void qc.invalidateQueries({ queryKey: activityKey() });
    return;
  }
  if (frame.type !== "event") return;
  if (!isViewerScope) return;
  if ((ACTIVITY_INVALIDATING_EVENT_KINDS as readonly string[]).includes(frame.kind)) {
    void qc.invalidateQueries({ queryKey: activityKey() });
  }
}

/**
 * The viewer's GLOBAL Activity list (`GET /api/activity` with NO `workspace_id`,
 * #task:1227 §9.2). v1 UI scope = global (the user's single attention inbox
 * across all workspaces). The server returns it fully
 * `enter()`-filtered (§6), per-(viewer,message) de-duped (§7) and salience-sorted
 * (§13.2) — the renderer renders it as-is and NEVER re-scores. The three filter
 * chips slice this purely client-side on `kind` / `thread_root_id`.
 *
 * Activity is fed by TWO source families, both converging on the viewer scope, so
 * it refreshes on a single subscription (same no-poll model as
 * `useConversationUnread`, #task:1226) — but on THREE frame kinds, because the
 * positional family's carrier is chosen by placement on the emit side. The set and
 * the reasoning live on {@link ACTIVITY_INVALIDATING_EVENT_KINDS}.
 *
 * `kind=task` is intentionally OUT of Activity v1 (#task:1227: a status mirror,
 * not an event), so THIS hook adds no `task_changed` subscription — one
 * `viewer:<id>` subscription covers every Activity live source. (The task
 * board's own `useTaskBoardRealtime` does subscribe `task_changed` on the same
 * viewer scope; that is a separate consumer.) Task re-enters Activity in v2 only
 * via a notification-backed `task_needs_review`, which already rides the
 * `notification` source above.
 */
export function useActivity() {
  const qc = useQueryClient();
  const queryKey = activityKey();

  useEffect(() => {
    const sub = subscribeViewerScope();
    const unsub = onWsFrame((frame) => {
      applyActivityFrame(qc, frame, isViewerAttentionFrame(frame));
    });
    return () => {
      sub.unsubscribe();
      unsub();
    };
  }, [qc]); // eslint-disable-line react-hooks/exhaustive-deps

  return useQuery<ListActivityDto>({
    queryKey,
    queryFn: ({ signal }) => api<ListActivityDto>(apiPaths.activity, { signal }),
  });
}

/**
 * Clear ONE Activity item through the single eliminate primitive (#task:1231).
 * The caller resolves the item to a `source_ref` via {@link activityItemSource}
 * (the kind-based routing chokepoint in `observe.ts`), so this hook is
 * source-shape agnostic — it just posts and refreshes. Used by the per-row
 * hover-✓ and the on-open directed dismiss. Invalidates both the activity list
 * and the notification caches so the row and any badge drop together.
 */
export function useClearActivityItem() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (source: ObserveSource) => postObserve([source]),
    onSuccess: (result, source) => {
      reconcileUnreadAfterObserveIfRealtimeUnavailable(qc, result);
      // A cleared stream item (mainline / thread / DM positional) changes this
      // conversation's per-stream `conversationUnread` — settle it via the single
      // seam so a co-visible channel chip re-settles at once, not on the async
      // `unread_updated` round-trip (#task:1402: same window as #task:1400, from
      // the Activity entry point). Notification-kind sources are ignored by the
      // seam. #task:1284 global maps are untouched.
      settleStreamUnreadAfterObserve(qc, [source], result);
      void qc.invalidateQueries({ queryKey: ["activity"] });
      void qc.invalidateQueries({ queryKey: ["notifications"] });
    },
    onError: () => {
      toast.error(t("attention.markReadFailed"));
    },
  });
}

/**
 * Mark everything in Activity read: every listed item through {@link activityItemSource}
 * in one `observe()` call (directed by `notification_id`, ambient by an explicit
 * stream-head clear), then every remaining unread directed notification. The second
 * step reaches notices Activity cannot list, such as a start failure whose
 * conversation was archived; without it their sticky toasts would return on the next
 * launch. `observe()` caps a batch at 1000 sources, far above a real queue.
 */
export function useMarkActivityAllRead() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (items: ActivityItemDto[]) => {
      const sources = items.map(activityItemSource).filter((s): s is ObserveSource => s != null);
      const result = sources.length > 0 ? await postObserve(sources) : undefined;
      await api(apiPaths.notificationsRead, {
        method: "POST",
        body: JSON.stringify({ all: true }),
      });
      return result;
    },
    onSuccess: (result, items) => {
      reconcileUnreadAfterObserveIfRealtimeUnavailable(qc, result);
      // Settle every distinct stream conversation this batch cleared via the
      // single seam (#task:1402) — dedup + `changed`-gated inside it. Rebuild the
      // sources from `items` (pure routing) since onSuccess gets the variables,
      // not the mutationFn's local sources. #task:1284 global maps untouched.
      const sources = items.map(activityItemSource).filter((s): s is ObserveSource => s != null);
      settleStreamUnreadAfterObserve(qc, sources, result);
      void qc.invalidateQueries({ queryKey: ["activity"] });
      void qc.invalidateQueries({ queryKey: ["notifications"] });
    },
    onError: () => {
      toast.error(t("activity.markAllReadFailed"));
    },
  });
}
