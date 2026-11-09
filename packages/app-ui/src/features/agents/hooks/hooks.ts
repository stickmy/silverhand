import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import type {
  AgentAvailability,
  AgentAvailabilityDto,
  AgentDto,
  AgentGetResponse,
  AgentPresenceDto,
  AgentRuntimeStateDto,
  AgentWorkspaceFileRead,
  AgentWorkspaceTree,
  CreateAgentInput,
  McpProbeRequest,
  McpProbeResult,
  ScheduleDto,
  ScheduleListStatusFilter,
  SwitchAgentRuntimeInput,
  SwitchAgentRuntimeResponse,
  UpdateAgentInput,
} from "@silverhand/shared";
import { AgentGetResponseSchema, AVATAR_DESIGN_FIELD, apiPaths } from "@silverhand/shared";
import {
  AGENT_AVAILABILITY_CHANGED_EVENT,
  AgentAvailabilityChangedEventSchema,
  AGENT_PRESENCE_CHANGED_EVENT,
  AgentPresenceChangedEventSchema,
} from "@silverhand/shared";
import { api, apiUpload, ApiError } from "@/lib/api";
import { avatarDesignKey } from "@/features/avatar-maker/hooks/use-avatar-design";
import { randomAvatarConfig } from "@/features/avatar-maker/model/avatar-config";
import { uploadComposedAvatar } from "@/features/avatar-maker/model/upload-composed-avatar";
import { isCurrentViewerScope, onWsFrame, subscribeViewerScope } from "@/lib/ws-client";

const agentsKey = ["agents"] as const;
const agentKey = (id: string) => ["agents", id] as const;
/**
 * Which directory an Agent Workspace read targets (Cindy #task:1190):
 *  - `home` → no `workspace_id` param → the agent home / L1 dir (full page);
 *  - `workspace` → `?workspace_id=<id>` → that workspace's L2 dir (side panel).
 * The resolver is source-blind on the wire — the frontend never builds a path,
 * only names a workspace. Passing an agent space's own id resolves back to
 * home (expected overlap), so the renderer never branches on source.
 */
export type AgentWorkspaceSource = { kind: "home" } | { kind: "workspace"; workspaceId: string };

/** Cache-scope segment so home vs each workspace get distinct query entries.
 *  The prefix `['agents', id, 'workspace']` still matches all of them, so the
 *  pane's coarse refresh-invalidate keeps working. */
function sourceScope(source: AgentWorkspaceSource): string {
  return source.kind === "workspace" ? `ws:${source.workspaceId}` : "home";
}

/** The `?workspace_id=` query fragment for a source (`''` for home → existing
 *  L1 behaviour). Exported for the contract test — the param name must match
 *  the backend exactly. */
export function workspaceScopeQuery(source: AgentWorkspaceSource): string {
  return source.kind === "workspace"
    ? `workspace_id=${encodeURIComponent(source.workspaceId)}`
    : "";
}

const agentWorkspaceTreeKey = (id: string, scope: string) =>
  ["agents", id, "workspace", "tree", scope] as const;
const agentWorkspaceFileKey = (id: string, scope: string, path: string) =>
  ["agents", id, "workspace", "file", scope, path] as const;
export const agentRuntimeStateKey = (id: string) => ["agents", id, "runtime-state"] as const;
const agentSchedulesKey = (id: string, status: ScheduleListStatusFilter) =>
  ["agents", id, "schedules", status] as const;
/**
 * List agents the viewer can see. Default = the full `canViewAgent` ACL set
 * (agents the viewer created OR participates in via a shared workspace).
 *
 * `scope: 'owned'` narrows to agents the LOGIN USER created — the backend
 * composes `created_by_user_id == viewer.userId` ON TOP of the ACL floor
 * (`GET /api/agents?scope=owned`, #task:1256). The "who counts as mine"
 * predicate lives ONLY in that backend contract; the renderer passes the flag
 * and never re-filters `created_by_user_id` client-side, so the CLI and every
 * other consumer share one definition. The scoped result gets its own cache
 * key (`['agents', 'owned']`) so it doesn't clobber the default list; both are
 * refreshed by the `['agents']`-prefixed invalidations on create/delete/avatar.
 */
export function useAgents(options?: { enabled?: boolean; scope?: "owned" }) {
  const scope = options?.scope;
  return useQuery({
    queryKey: scope ? ([...agentsKey, scope] as const) : agentsKey,
    queryFn: async () => {
      const res = await api<{ agents: AgentDto[] }>(
        scope ? `/api/agents?scope=${scope}` : "/api/agents",
      );
      return res.agents;
    },
    enabled: options?.enabled ?? true,
  });
}

/** One agent by id, tagged with the tier the viewer reads it at. */
export type AgentRead = AgentGetResponse;

/**
 * Window-focus policy for an agent's event-driven reads (presence,
 * availability). A public reader receives none of their realtime frames, so
 * for an agent the viewer reads at the public tier, focus re-reads them;
 * otherwise the frames keep them current and focus does nothing.
 */
function refetchOnFocusWhenPublic(qc: QueryClient, agentId: string | null | undefined) {
  return () =>
    agentId && qc.getQueryData<AgentRead>(agentKey(agentId))?.view === "public"
      ? ("always" as const)
      : false;
}

/**
 * One agent by id. Any signed-in human can read an existing agent: viewers
 * with access get the full record (`view: "privileged"`), everyone else the
 * public identity (`view: "public"`). Public reads receive no realtime frames,
 * so they refresh on mount and on window focus instead.
 */
export function useAgent(agentId: string | null) {
  return useQuery({
    queryKey: agentId ? agentKey(agentId) : ["agents", "none"],
    enabled: !!agentId,
    queryFn: async (): Promise<AgentRead> => {
      const raw = await api<unknown>(`/api/agents/${agentId}`);
      return AgentGetResponseSchema.parse(raw);
    },
    refetchOnMount: (query) => (query.state.data?.view === "public" ? "always" : true),
    refetchOnWindowFocus: (query) => query.state.data?.view === "public",
  });
}

const agentPresenceKey = (id: string) => ["agents", id, "presence"] as const;

/**
 * The activity deadline in the CLIENT's wall-clock domain, or a sentinel when
 * there is none:
 *  - a `number` — the calibrated instant (client clock) the agent crosses to
 *    Away; the projection compares it against the same clock's `now`.
 *  - `null` — the backend reported `away_at === null` (never active).
 *  - `'unknown'` — the `/presence` read is pending / cold-errored (no deadline).
 * The RAW server-epoch `away_at` deliberately is NOT this type and never leaves
 * {@link useAgentPresence}: comparing a server-epoch deadline against a client
 * `now` silently misprojects under clock skew (#task:2049).
 */
export type PresenceDeadline = number | null | "unknown";

/**
 * Shift the backend's ABSOLUTE server-epoch `away_at` into the CLIENT's
 * wall-clock domain so a local `now` / timer comparison doesn't misfire under
 * client↔server clock skew (#task:2049). The backend emits `server_now` (its
 * clock at response time) beside `away_at`; the client measures skew as
 * `offset = dataUpdatedAt − server_now` (its own clock at the SAME response)
 * and adds it to the deadline. `null` (never active) has no deadline to shift
 * and passes through unchanged.
 *
 * `dataUpdatedAt` and `server_now` MUST come from the SAME fetch — this is the
 * reason the `['agents', id, 'presence']` cache is refetch-ONLY and NEVER
 * written by `setQueryData` (see {@link useAgentPresence}): an optimistic write
 * or an id-only-frame back-fill would advance `dataUpdatedAt` to now while
 * `server_now` stayed at the prior fetch, poisoning `offset` with the cache
 * entry's AGE — reviving skew, not staleness. This is the same discipline as
 * "an id-only invalidation frame carries no value, so apply it only as a
 * refetch, never as a snapshot". Pure — exported for unit tests.
 *
 * Residual (accepted, #task:2049): the offset is recomputed only on each fetch,
 * so across a long-lived cache entry an NTP correction or sleep/wake drifts the
 * deadline until the next refetch — a second reason nav/mount must truly refetch
 * `/presence`, not merely read a warm entry.
 */
export function calibratePresenceDeadline(
  awayAt: number | null,
  serverNow: number,
  dataUpdatedAt: number,
): number | null {
  if (awayAt === null) return null;
  return awayAt + (dataUpdatedAt - serverNow);
}

/**
 * Pure decision: does this frame invalidate the given agent's presence read?
 * Same shape as {@link decideAgentAvailabilityInvalidation} on the presence
 * event: `snapshot_required` counts unconditionally (a replay gap may have
 * swallowed the id-only `agent_presence_changed` frame, and the only recovery
 * for a dropped viewer-scoped delta is to refetch), and an
 * `agent_presence_changed` event counts only when its validated `agent_id`
 * matches this agent (the frame is per-viewer, not per-agent). The caller gates
 * `isCurrentViewerScope(frame.scope)` first. Pure — exported for unit tests.
 */
export function decideAgentPresenceInvalidation(
  frame: AgentAvailabilitySyncFrame,
  agentId: string,
): boolean {
  if (frame.type === "snapshot_required") return true;
  if (frame.type !== "event" || frame.kind !== AGENT_PRESENCE_CHANGED_EVENT) {
    return false;
  }
  const parsed = AgentPresenceChangedEventSchema.safeParse(frame.payload);
  return parsed.success && parsed.data.agent_id === agentId;
}

/**
 * The agent's user-facing presence (Online / Away / Offline / no-status),
 * composed from the two orthogonal reads this hook owns (#task:2049):
 *  - availability ({@link useAgentAvailability}) — host reachable × agent valid,
 *    the PRIMARY axis and the only source of Offline;
 *  - the presence CLOCK (`GET /api/agents/:id/presence` → `{ away_at, server_now }`)
 *    — the absolute deadline past which a reachable agent reads Away.
 *
 * Liveness is NO LONGER an input (the old availability × liveness composition is
 * gone): a one-shot runtime that replies and then cleanly exits is not evidence
 * it crossed the inactivity boundary, so presence is judged by the durable
 * `away_at` deadline, not by "is a process alive this instant".
 *
 * Two things this hook OWNS so every consumer stays a one-liner:
 *  1. Clock-domain calibration ({@link calibratePresenceDeadline}) — done ONCE
 *     here; the raw server-epoch `away_at` never leaves this hook, so no second
 *     site can compare it against a local clock and silently misfire under skew.
 *  2. The boundary re-render: the backend emits `agent_presence_changed` only on
 *     an Away→ONLINE crossing (real activity), NEVER when an idle agent passively
 *     crosses its deadline to Away — so a resident surface would sit on a stale
 *     Online forever without a client timer. A single `setTimeout` at the
 *     calibrated deadline bumps a tick to re-project exactly once, at the flip.
 *
 * The presence cache is refetch-ONLY (`staleTime: Infinity`, event-invalidated,
 * NEVER written by `setQueryData`). Beyond the usual "don't apply an id-only
 * frame as a snapshot" rule, an optimistic write here would also poison the skew
 * offset — see {@link calibratePresenceDeadline}. A retained value (invalidated
 * but not yet refetched) is used as-is rather than folded to unknown: its
 * `away_at`/`server_now` are still a matched pair, and the deadline degrades
 * safely (a past deadline reads Away; the availability axis, which folds stale →
 * unknown, is the safety valve for "should we claim anything at all").
 */
export function agentPresenceQueryOptions(qc: QueryClient, agentId: string | null | undefined) {
  return {
    queryKey: agentId ? agentPresenceKey(agentId) : ["agents", "none", "presence"],
    enabled: !!agentId,
    // Route is the single source of the value; `agent_presence_changed` drives
    // every refresh, so no timer-based staleness (the boundary timer below
    // re-PROJECTS, it never refetches). Same retry ceiling as availability.
    staleTime: Infinity,
    // …but a mount MUST hit the wire, not read a warm entry. The WS listener is
    // unbuffered and lives on this hook's effect, so any `agent_presence_changed`
    // / reconnect `snapshot_required` that arrives while every presence surface
    // is unmounted is dropped with no compensation; within the default gcTime a
    // remount would otherwise reuse a never-stale warm entry and paint a working
    // agent Away for as long as its next real crossing is away. `'always'` closes
    // that gap and re-pairs `(dataUpdatedAt, server_now)` each mount, bounding the
    // skew residual (see {@link calibratePresenceDeadline}) to mount granularity.
    refetchOnMount: "always" as const,
    refetchOnWindowFocus: refetchOnFocusWhenPublic(qc, agentId),
    retry: agentAvailabilityRetry,
    queryFn: async (): Promise<AgentPresenceDto> => {
      const res = await api<AgentPresenceDto>(apiPaths.agentPresence(agentId!));
      return res;
    },
  };
}

export function useAgentPresence(agentId: string | null | undefined): GlobalPresence | null {
  const qc = useQueryClient();
  const availability = useAgentAvailability(agentId);
  const query = useQuery(agentPresenceQueryOptions(qc, agentId));
  useEffect(() => {
    if (!agentId) return;
    const sub = subscribeViewerScope();
    const unsub = onWsFrame((frame) => {
      if (
        (frame.type !== "event" && frame.type !== "snapshot_required") ||
        !isCurrentViewerScope(frame.scope)
      ) {
        return;
      }
      if (decideAgentPresenceInvalidation(frame, agentId)) {
        void qc.invalidateQueries({ queryKey: agentPresenceKey(agentId) });
      }
    });
    return () => {
      sub.unsubscribe();
      unsub();
    };
  }, [agentId, qc]);

  const deadline: PresenceDeadline =
    query.data === undefined
      ? "unknown"
      : calibratePresenceDeadline(query.data.away_at, query.data.server_now, query.dataUpdatedAt);

  // Re-project once at the deadline: `agent_presence_changed` fires only for an
  // Away→Online crossing, never for the passive Online→Away one, so without a
  // timer a resident Online would stick past the deadline. A single timer per
  // mount; every mount shares the one presence cache entry, computes the same
  // deadline, and flips together. A deadline already in the past needs no timer
  // (the projection already reads Away).
  const [, forceTick] = useState(0);
  useEffect(() => {
    if (typeof deadline !== "number") return;
    let timer: ReturnType<typeof setTimeout>;
    // `setTimeout` may fire a hair EARLY under engine clock granularity; a
    // forceTick then would re-render while `Date.now() < deadline` still reads
    // Online, and `deps: [deadline]` wouldn't re-arm — stranding this mount on a
    // stale Online. So the callback re-checks: only flip once the deadline has
    // truly passed, otherwise re-arm for the remainder.
    const tick = (): void => {
      if (Date.now() >= deadline) {
        forceTick((t) => t + 1);
        return;
      }
      timer = setTimeout(tick, deadline - Date.now());
    };
    timer = setTimeout(tick, deadline - Date.now());
    return () => clearTimeout(timer);
  }, [deadline]);

  return projectGlobalPresence(availability, deadline, Date.now());
}

const agentAvailabilityKey = (id: string) => ["agents", id, "availability"] as const;

/**
 * Transport / 5xx retries for the availability read are BOUNDED, and a 4xx is
 * TERMINAL: a 404 is the endpoint's anti-enumeration answer for a non-viewer,
 * which no amount of retrying turns into an availability bit. A persistently-
 * failing read has nothing to deliver by hammering an unreachable authority, so
 * it settles into the renderer's `unknown` (no-status) instead. The presence
 * read shares this retry (`retry: agentAvailabilityRetry`), the same 4xx-terminal
 * shape.
 */
const AGENT_AVAILABILITY_MAX_RETRIES = 2;

export function agentAvailabilityRetry(failureCount: number, err: unknown): boolean {
  if (err instanceof ApiError && err.status >= 400 && err.status < 500) {
    return false;
  }
  return failureCount < AGENT_AVAILABILITY_MAX_RETRIES;
}

/**
 * Pure projection of the availability query state onto the tri-state a consumer
 * composes. This axis is EVENT-invalidated, not timer-polled (#task:2043): the
 * route is the single source of the value and `agent_availability_changed` drives
 * every refresh, so with `staleTime: Infinity` a value only becomes `isStale` when
 * an event (or a `snapshot_required` recovery) explicitly invalidates it — never
 * on a timer. A value the client holds is therefore authoritative ONLY while it is
 * a fresh successful read; every other state folds to `unknown` (which composes to
 * no-status), NEVER continues to assert a possibly-wrong online/offline:
 *  - `data === undefined` — first load in flight, disabled, or a read that failed
 *    with no prior value (incl. the 404 anti-enum for a non-viewer).
 *  - `isStale` — an INVALIDATED value not yet replaced by a successful refetch.
 *    React Query retains the prior `data` (status can stay `'success'`, or go
 *    `'error'` after a failed refetch) but keeps the query invalidated, so the old
 *    value must not be re-asserted. This is the case @Cindy's #task:2039 review
 *    pinned: after a disconnect/revoke frame invalidates and the refetch then 5xx's,
 *    the retained `online` would otherwise paint Online indefinitely under
 *    `staleTime: Infinity` with no guaranteed next frame. From invalidation until a
 *    successful replacement the projection reads `unknown`; the brief no-status
 *    blink on a genuine change is acceptable because availability changes are rare
 *    events, not a poll: this fold keys on `isStale`, which an
 *    in-flight-but-not-invalidated read never sets, so a rare event's replacement
 *    is the only thing that clears it.
 */
export function projectAgentAvailability(state: {
  data: AgentAvailability | undefined;
  isStale: boolean;
}): AgentAvailability | "unknown" {
  if (state.data === undefined || state.isStale) return "unknown";
  return state.data;
}

/**
 * A roster's answer to "is this agent a member" is a THREE-state projection, not
 * a boolean over a default-`[]` list (@Cindy, #task:2039). The presence line uses
 * it only for the MEMBERSHIP gate — Online/Offline is Availability's job — so the
 * one thing it must never do is let an un-settled read impersonate absence:
 *
 *  - `undefined` (loading, or errored with no prior successful snapshot) →
 *    `unknown`: no membership verdict, so the caller draws no `Not here`. A
 *    default `[]` here was the bug — it painted a real member as `Not here` on the
 *    first popover open, the same "un-settled must not impersonate absent" rule
 *    the availability fold enforces.
 *  - a successful snapshot (even an empty one) that CONTAINS the agent → `member`.
 *  - a successful snapshot that does NOT contain it → `not_here`.
 *
 * A failed refetch RETAINS the last successful snapshot (React Query keeps
 * `data`), and a failure states nothing new about membership, so a retained
 * snapshot is used as-is: this keys on `data` being defined, never on fetch/error
 * status. Callers must pass the RAW `query.data` — defaulting it to `[]` reopens
 * the exact hole this closes.
 */
export type RosterMembership = "unknown" | "member" | "not_here";
export function projectRosterMembership(
  members: readonly { agent_id: string }[] | undefined,
  agentId: string,
): RosterMembership {
  if (members === undefined) return "unknown";
  return members.some((m) => m.agent_id === agentId) ? "member" : "not_here";
}

/** Minimal WS frame shape {@link decideAgentAvailabilityInvalidation} consumes,
 *  so the decision is exercised without React or a socket (mirrors
 *  `DirectPairsSyncFrame`). */
export interface AgentAvailabilitySyncFrame {
  type: string;
  kind?: string;
  payload?: unknown;
}

/**
 * Pure decision: does this frame invalidate the given agent's availability?
 *
 * `snapshot_required` counts unconditionally (for the one mounted agent): a
 * replay gap may have swallowed the `agent_availability_changed` frame, and the
 * only recovery for a dropped viewer-scoped delta is to refetch — same reasoning
 * as every other viewer-scoped list (see `decideDirectPairsInvalidation`).
 * An `agent_availability_changed` event counts only when its validated
 * `agent_id` matches — the event is per-viewer but not per-agent, so a mounted
 * surface refetches solely its own agent.
 *
 * The caller gates `isCurrentViewerScope(frame.scope)` first — kept out of here
 * so the decision stays dependency-free and unit-testable.
 */
export function decideAgentAvailabilityInvalidation(
  frame: AgentAvailabilitySyncFrame,
  agentId: string,
): boolean {
  if (frame.type === "snapshot_required") return true;
  if (frame.type !== "event" || frame.kind !== AGENT_AVAILABILITY_CHANGED_EVENT) {
    return false;
  }
  const parsed = AgentAvailabilityChangedEventSchema.safeParse(frame.payload);
  return parsed.success && parsed.data.agent_id === agentId;
}

/**
 * The agent's reachability — the ONE viewer-authorized bit
 * `GET /api/agents/:id/availability` answers (#task:2043). TWO values on the
 * wire (`online` | `offline`), projected here to a tri-state with the renderer's
 * own `unknown` (query pending / errored with no prior value). Composes with the
 * presence deadline via {@link projectGlobalPresence}.
 *
 * This endpoint HAS a push channel: the viewer-scoped
 * `agent_availability_changed {agent_id}` frame is a pure invalidation signal, so
 * the query is `staleTime: Infinity` with NO polling — a mounted surface
 * subscribes the `viewer:<userId>` scope and refetches only when a matching event
 * (or a `snapshot_required` replay recovery) arrives. The subscription lives with
 * the hook, not app-shell-global: availability only matters while a presence line
 * for that agent is on screen, so there is nothing to keep live for an agent no
 * view is reading (contrast `useDirectPairsSync`, which must catch a stranger's
 * first DM while the viewer looks elsewhere). That "no keep-alive while unmounted"
 * stance is only sound because a mount truly refetches (`refetchOnMount: 'always'`
 * below) — any frame dropped while every surface was unmounted is recovered on the
 * next mount rather than left painting a stale value from a never-stale warm entry.
 */
export function agentAvailabilityQueryOptions(qc: QueryClient, agentId: string | null | undefined) {
  return {
    queryKey: agentId ? agentAvailabilityKey(agentId) : ["agents", "none", "availability"],
    enabled: !!agentId,
    // The route is the single source of the value; the event drives every
    // refresh, so there is no timer-based staleness (no poll, no age-out).
    staleTime: Infinity,
    // A mount must hit the wire (see docblock): the unbuffered WS listener lives
    // on this hook, so a frame dropped while unmounted is only recovered by a
    // real mount refetch — never by rereading a never-stale warm entry.
    refetchOnMount: "always" as const,
    refetchOnWindowFocus: refetchOnFocusWhenPublic(qc, agentId),
    retry: agentAvailabilityRetry,
    queryFn: async (): Promise<AgentAvailability> => {
      const res = await api<AgentAvailabilityDto>(apiPaths.agentAvailability(agentId!));
      return res.availability;
    },
  };
}

export function useAgentAvailability(
  agentId: string | null | undefined,
): AgentAvailability | "unknown" {
  const qc = useQueryClient();
  const query = useQuery(agentAvailabilityQueryOptions(qc, agentId));
  useEffect(() => {
    if (!agentId) return;
    const sub = subscribeViewerScope();
    const unsub = onWsFrame((frame) => {
      if (
        (frame.type !== "event" && frame.type !== "snapshot_required") ||
        !isCurrentViewerScope(frame.scope)
      ) {
        return;
      }
      if (decideAgentAvailabilityInvalidation(frame, agentId)) {
        void qc.invalidateQueries({ queryKey: agentAvailabilityKey(agentId) });
      }
    });
    return () => {
      sub.unsubscribe();
      unsub();
    };
  }, [agentId, qc]);
  return projectAgentAvailability({ data: query.data, isStale: query.isStale });
}

/** The user-facing presence a consumer draws once availability and the presence
 *  deadline are composed. */
export type GlobalPresence = "online" | "away" | "offline";

/**
 * Compose the agent's availability with its (client-domain) presence deadline
 * into the one word the UI shows (#task:2049, frozen — supersedes the old
 * availability × liveness composition). Availability is the PRIMARY axis and the
 * only source of Offline; Online requires POSITIVE evidence — an away deadline
 * still in the future:
 *  - availability `unknown` (read pending / errored) → `null`: no presence to
 *    claim yet, so draw nothing — never a speculative status.
 *  - availability `offline` → Offline: host unreachable / agent invalid — the
 *    honest Offline regardless of any deadline (a reaped process on a reachable
 *    host is NOT offline; that path reads Away below).
 *  - availability `online` + deadline a FUTURE instant (`now < deadline`) →
 *    Online.
 *  - availability `online` + deadline in the PAST (`now >= deadline`) → Away.
 *  - availability `online` + deadline `null` (never active) → Away. v82 has NO
 *    backfill, so null is the DOMINANT post-deploy state; Online-on-null would be
 *    a deploy-day mass lie.
 *  - availability `online` + deadline `'unknown'` (presence read pending/errored)
 *    → Away.
 *
 * `now` is INJECTED (the hook owns the clock + the boundary timer), so this stays
 * a pure, testable time function with no `Date.now()` inside. The transitions it
 * admits move one way only (no-status → Away → Online), never a backward flip to
 * a louder claim.
 */
export function projectGlobalPresence(
  availability: AgentAvailability | "unknown",
  deadline: PresenceDeadline,
  now: number,
): GlobalPresence | null {
  if (availability === "unknown") return null;
  if (availability === "offline") return "offline";
  if (deadline === "unknown" || deadline === null) return "away";
  return now < deadline ? "online" : "away";
}

const agentProfileMcpKey = (id: string) => ["agents", id, "profile-mcp"] as const;

/**
 * Read the agent's raw profile MCP config (`mcp.json`, or `null` when unset).
 * The endpoint is `canEditAgent`-gated — the raw can carry secrets, so a
 * non-editor viewer gets a 403 (surfaced as a "private" state, not retried).
 * 4xx responses are terminal: retrying can't turn a forbidden/not-found into
 * bytes, it only delays the locked/empty state.
 */
export function useAgentProfileMcp(agentId: string | null) {
  return useQuery({
    queryKey: agentId ? agentProfileMcpKey(agentId) : ["agents", "none", "profile-mcp"],
    enabled: !!agentId,
    queryFn: async () => {
      const res = await api<{ raw: string | null }>(apiPaths.agentProfileMcp(agentId!));
      return res.raw;
    },
    retry: (count, err) =>
      !(err instanceof ApiError && err.status >= 400 && err.status < 500) && count < 2,
  });
}

/**
 * Live MCP **probe** — connect to one server's real endpoint, run the protocol
 * `initialize` handshake and, when `include_tools`, `tools/list`. A mutation,
 * not a query: it is an explicit, side-effectful, user-triggered diagnostic
 * (never on mount, never polled). The host owns the whole connection
 * lifecycle (one-shot connect → close in `finally`); the renderer just fires
 * the request and renders the returned snapshot. The endpoint is
 * `canEditAgent`-gated, same as the raw read.
 *
 * One mutation instance per ServerRow so each row's pending/result state is
 * isolated. `server_name` is the RAW name (the host connects to the real
 * endpoint, not the `silverhand_profile__` runtime alias).
 */
export function useMcpProbe(agentId: string) {
  return useMutation({
    mutationFn: async (body: McpProbeRequest) => {
      // Route returns the bare `McpProbeResult` (200), canEditAgent-gated;
      // protocol failures are `{ok:false, phase}` at 200, only host-transport
      // faults are HTTP errors. (#task:1137 backend, agents.ts:603.)
      return api<McpProbeResult>(apiPaths.agentProfileMcpProbe(agentId), {
        method: "POST",
        body: JSON.stringify(body),
      });
    },
  });
}

export function useCreateAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: CreateAgentInput) => {
      // D4.d D3 — `POST /api/agents` now REQUIRES an
      // `Idempotency-Key` header (1..64, `[A-Za-z0-9_-]`) per
      // design §14 Q1. The key is generated inside this
      // `mutationFn` closure and lives for exactly the single
      // `api()` HTTP attempt below.
      //
      // Scope note (Cindy D3 round-2 P2): D3 ONLY commits to
      // "every HTTP attempt carries an Idempotency-Key". We do
      // NOT commit to key reuse across React Query's builtin
      // retry surface — each invocation of `mutationFn`
      // (whether from a fresh `mutate()` call or from React
      // Query's automatic retry) re-enters this scope and
      // generates a new key. If a future UX wants a "Retry"
      // button to round-trip the original create's idempotent
      // state, the key must be lifted out of `mutationFn` into
      // mutation context / component state — that's an explicit
      // future change, not an emergent property of the current
      // code.
      //
      // `crypto.randomUUID()` is RFC 4122 v4 — 36 chars,
      // [0-9a-f-] only, well within the route's 1..64
      // `[A-Za-z0-9_-]` window. No need for any extra encoding.
      const idempotencyKey = crypto.randomUUID();
      const res = await api<{
        agent: AgentDto;
        agent_home_workspace_id: string | null;
        dm_channel_id: string | null;
        onboarding_message_id: string | null;
      }>("/api/agents", {
        method: "POST",
        body: JSON.stringify(input),
        headers: { "Idempotency-Key": idempotencyKey },
      });
      return res;
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: agentsKey });
      qc.invalidateQueries({ queryKey: ["channels"] });
    },
  });
}

export function useUpdateAgent(agentId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: UpdateAgentInput) => {
      if (!agentId) throw new Error("no agent id");
      const res = await api<{ agent: AgentDto }>(`/api/agents/${agentId}`, {
        method: "PATCH",
        body: JSON.stringify(input),
      });
      return res.agent;
    },
    onSuccess: () => {
      if (!agentId) return;
      qc.invalidateQueries({ queryKey: agentsKey });
      qc.invalidateQueries({ queryKey: agentKey(agentId) });
    },
  });
}

/**
 * Cache-invalidation side effects for a successful runtime switch. Extracted
 * from `useSwitchAgentRuntime.onSuccess` so it can be unit-tested against a
 * real QueryClient with a seeded run-state query (Cindy #1470 review).
 *
 * The run-state keys are `['conversations', conversationId, 'agent-run-states']`
 * — see `features/agents/model/agent-run-state-cache.ts::agentRunStatesKey` (the single authority;
 * `features/channels/hooks.ts::useAgentRunStates` calls into it). A bare
 * `['conversations']` prefix would over-fire against every conversation-scoped
 * query, so we use a structured predicate that matches only run-state slots.
 */
export function applySwitchAgentRuntimeSideEffects(qc: QueryClient, agentId: string): void {
  qc.invalidateQueries({ queryKey: agentsKey });
  qc.invalidateQueries({ queryKey: agentKey(agentId) });
  qc.invalidateQueries({ queryKey: agentRuntimeStateKey(agentId) });
  qc.invalidateQueries({ queryKey: ["channels"] });
  qc.invalidateQueries({
    predicate: (q) => q.queryKey[0] === "conversations" && q.queryKey[2] === "agent-run-states",
  });
}

/**
 * PUT /api/agents/:id/runtime — transition an agent to a different runtime
 * (#task:1470). Distinct from `useUpdateAgent`'s PATCH: runtime changes carry
 * transition semantics (stop child processes, clear CLI sessions, reset
 * per-runtime state) and MUST go through the dedicated backend command, not
 * the ordinary field-patch route.
 *
 * Contract (`SwitchAgentRuntimeInputSchema` in shared): all four keys
 * required; `null` on model / reasoning_effort / fast_mode is the explicit
 * "Use driver default" sentinel. The schema also enforces `(runtime, value)`
 * legality — the UI's per-runtime catalog gating mirrors this, so a legal UI
 * state is always accepted.
 *
 * Response (`SwitchAgentRuntimeResponse`) carries transition side-effects.
 * The renderer keys on `restart_failures > 0` to surface partial success as
 * a warning (not a green toast), so a "runtime committed but N conversations
 * couldn't restart" outcome isn't misread as clean success. `restart_failures`
 * is a nonneg count (not a list of details).
 */
export function useSwitchAgentRuntime(agentId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: SwitchAgentRuntimeInput) => {
      if (!agentId) throw new Error("no agent id");
      const res = await api<SwitchAgentRuntimeResponse>(apiPaths.agentRuntime(agentId), {
        method: "PUT",
        body: JSON.stringify(input),
      });
      return res;
    },
    onSuccess: () => {
      if (!agentId) return;
      applySwitchAgentRuntimeSideEffects(qc, agentId);
    },
  });
}

/**
 * POST /api/agents/:id/avatar (multipart). Mirrors `useUploadAvatar` in
 * `features/session/hooks.ts` but for agents.
 *
 * The route is gated by `canEditAgent` (creator-or-system) — non-creator
 * humans get 403, which `apiUpload` surfaces as `ApiError`. The renderer
 * UI hides the upload control via `canEditAgentProfile` so the user
 * normally never reaches this hook without permission, but the
 * mutation still has to handle 403 in case the cached `created_by_user_id`
 * is stale or the agent was just transferred.
 *
 * On success the response carries the updated agent dto with a new
 * `avatar_url = ${apiPaths.agentAvatar(id)}?v=${updated_at}` cache-buster;
 * invalidating the list + detail keys re-renders every consumer with the
 * new URL, which `useAgentAvatarObjectUrls` picks up via its
 * `(id, avatar_url)` cache key.
 */
export function useUploadAgentAvatar(agentId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ file, design }: { file: File; design?: string }) => {
      if (!agentId) throw new Error("no agent id");
      const form = new FormData();
      if (design) {
        form.set(AVATAR_DESIGN_FIELD, design);
      }
      form.set("file", file);
      const res = await apiUpload<{ avatar_url: string; agent: AgentDto }>(
        apiPaths.agentAvatar(agentId),
        form,
      );
      return res;
    },
    onSuccess: () => {
      if (!agentId) return;
      qc.invalidateQueries({ queryKey: agentsKey });
      qc.invalidateQueries({ queryKey: agentKey(agentId) });
      qc.removeQueries({ queryKey: avatarDesignKey(agentId) });
    },
  });
}

/** A new agent starts with a random composed face instead of the runtime mark.
 *  Best effort: a failure leaves the runtime mark, which is still a valid avatar. */
export function useGiveAgentDefaultAvatar() {
  const qc = useQueryClient();

  return (agentId: string) => {
    void uploadComposedAvatar(apiPaths.agentAvatar(agentId), randomAvatarConfig())
      .then(() => {
        qc.invalidateQueries({ queryKey: agentsKey });
        qc.invalidateQueries({ queryKey: agentKey(agentId) });
      })
      .catch(() => {});
  };
}

/**
 * DELETE /api/agents/:id/avatar. Idempotent on the backend — clearing
 * an already-clear pointer returns 200 with `avatar_url: null`, so the
 * renderer doesn't have to special-case "nothing to clear".
 */
export function useClearAgentAvatar(agentId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async () => {
      if (!agentId) throw new Error("no agent id");
      const res = await api<{ agent: AgentDto }>(apiPaths.agentAvatar(agentId), {
        method: "DELETE",
      });
      return res.agent;
    },
    onSuccess: () => {
      if (!agentId) return;
      qc.invalidateQueries({ queryKey: agentsKey });
      qc.invalidateQueries({ queryKey: agentKey(agentId) });
      qc.removeQueries({ queryKey: avatarDesignKey(agentId) });
    },
  });
}

export function useAgentWorkspaceTree(
  agentId: string | null,
  source: AgentWorkspaceSource = { kind: "home" },
) {
  const scope = sourceScope(source);
  return useQuery({
    queryKey: agentId
      ? agentWorkspaceTreeKey(agentId, scope)
      : ["agents", "none", "workspace", "tree"],
    enabled: !!agentId,
    queryFn: async () => {
      const query = workspaceScopeQuery(source);
      const url = query
        ? `${apiPaths.agentWorkspaceTree(agentId!)}?${query}`
        : apiPaths.agentWorkspaceTree(agentId!);
      const res = await api<{ tree: AgentWorkspaceTree }>(url);
      return res.tree;
    },
    // The tree is essentially static for the human's purposes — files
    // appear/disappear when the agent edits them via tools. 30s stale
    // window plus a manual Refresh button on the pane header.
    staleTime: 30_000,
  });
}

/**
 * Read one file by virtual path, scoped to `source` (home or a workspace L2).
 * The (scope, path) pair is the cache key suffix — switching the selected file
 * or the source is a fresh query, and React Query's structural-share keeps the
 * previous content rendered until the new one resolves so the preview never
 * blanks.
 *
 * Pass `null` path to disable the query (no file selected → preview shows an
 * empty-state).
 */
export function useAgentWorkspaceFile(
  agentId: string | null,
  source: AgentWorkspaceSource,
  path: string | null,
) {
  const scope = sourceScope(source);
  return useQuery({
    queryKey:
      agentId && path
        ? agentWorkspaceFileKey(agentId, scope, path)
        : ["agents", "none", "workspace", "file"],
    enabled: !!agentId && !!path,
    queryFn: async () => {
      const params = new URLSearchParams({ path: path! });
      if (source.kind === "workspace") {
        params.set("workspace_id", source.workspaceId);
      }
      const url = `${apiPaths.agentWorkspaceFile(agentId!)}?${params.toString()}`;
      const res = await api<{ file: AgentWorkspaceFileRead }>(url);
      return res.file;
    },
    staleTime: 30_000,
  });
}

export function useAgentRuntimeState(agentId: string | null, options?: { enabled?: boolean }) {
  return useQuery({
    queryKey: agentId ? agentRuntimeStateKey(agentId) : ["agents", "none", "runtime-state"],
    enabled: !!agentId && (options?.enabled ?? true),
    queryFn: async () => {
      const res = await api<AgentRuntimeStateDto>(`/api/agents/${agentId}/runtime-state`);
      return res;
    },
    // Status / pid change as soon as the user clicks Stop or a conversation
    // spawns, so refetch on focus + every 5s while mounted.
    staleTime: 2_000,
    refetchInterval: 5_000,
  });
}

export function useStopAgent(agentId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async () => {
      if (!agentId) throw new Error("no agent id");
      return api<{ stopped: number }>(`/api/agents/${agentId}/stop`, {
        method: "POST",
      });
    },
    onSuccess: () => {
      if (!agentId) return;
      qc.invalidateQueries({ queryKey: agentRuntimeStateKey(agentId) });
    },
  });
}

export function useDeleteAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (agentId: string) => {
      await api<void>(`/api/agents/${agentId}`, {
        method: "DELETE",
        body: JSON.stringify({ confirm: true }),
      });
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: agentsKey });
      qc.invalidateQueries({ queryKey: ["channels"] });
    },
  });
}

/** Per-agent schedule list for the panel. Default filter `active`
 *  matches the common case "what does this agent still have on the
 *  books"; users can flip to `all` to inspect terminal rows.
 *
 *  Polling cadence: 30s. The scheduler tick + sweep operate at 60s,
 *  so 30s gives at-most-30s lag between a fire and the row's
 *  fired_count / next_fire_at refreshing in the UI without burning
 *  HTTP traffic on the much slower human read cadence. */
export function useAgentSchedules(
  agentId: string | null,
  status: ScheduleListStatusFilter = "active",
) {
  return useQuery({
    queryKey: agentId
      ? agentSchedulesKey(agentId, status)
      : ["agents", "none", "schedules", status],
    enabled: !!agentId,
    queryFn: async () => {
      const res = await api<{ schedules: ScheduleDto[] }>(
        `${apiPaths.agentSchedules(agentId!)}?status=${status}`,
      );
      return res.schedules;
    },
    staleTime: 5_000,
    refetchInterval: 30_000,
  });
}

/** User-initiated cancel from the panel. Invalidates every status
 *  filter for the agent because the row's status is what changed —
 *  active list shrinks, cancelled list grows. */
export function useCancelAgentSchedule(agentId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (scheduleId: string) => {
      if (!agentId) throw new Error("no agent id");
      const res = await api<{ schedule: ScheduleDto }>(
        apiPaths.agentScheduleCancel(agentId, scheduleId),
        { method: "POST" },
      );
      return res.schedule;
    },
    onSuccess: () => {
      if (!agentId) return;
      qc.invalidateQueries({ queryKey: ["agents", agentId, "schedules"] });
    },
  });
}
