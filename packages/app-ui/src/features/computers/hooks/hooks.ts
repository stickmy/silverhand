import type { ConnectionInfo, LocalHostInfo } from "@silverhand/client";
import { getPlatform, requireLocalHost } from "@/platform";
import { useCallback, useMemo, useState } from "react";
import { useMutation, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { z } from "zod";
import {
  HostCancelPendingResponseSchema,
  HostCreateResponseSchema,
  HostDeletePendingAgentsResponseSchema,
  HostDeleteResponseSchema,
  HostDisconnectResponseSchema,
  HostDtoSchema,
  HostPrivilegedGetResponseSchema,
  HostListResponseSchema,
  HostProxyConfigOfflineResponseSchema,
  HostProxyConfigResponseSchema,
  HostProxyConfigSchema,
  HostResourcesResponseSchema,
  HostRevokeResponseSchema,
  HostRotateTokenUserManagedResponseSchema,
  HostStatusResponseSchema,
  HOST_PROXY_CONFIG_OFFLINE_ERROR_CODE,
  RuntimeCatalogResponseSchema,
  UpdateHostInputSchema,
  apiPaths,
  type HostAvailability,
  type HostCancelPendingResponse,
  type HostCreateResponse,
  type HostDeleteResponse,
  type HostResourcesResponse,
  type HostDisconnectResponse,
  type HostDto,
  type HostListResponse,
  type HostProxyConfig,
  type HostStatusResponse,
  type RuntimeCatalogResponse,
  type UpdateHostInput,
} from "@silverhand/shared";
import type { LocalHostWsAttachment } from "@silverhand/client";
import { api, ApiError } from "@/lib/api";
import { rejectHostLabel } from "../model/host-label";

// Re-export the shared proxy DTO so feature consumers (HostDetailView) keep a
// single import surface off this hooks module without reaching past it — the
// *type* still originates from `@silverhand/shared` (single source), we just
// forward it.
export type { HostProxyConfig } from "@silverhand/shared";

export const hostsKey = ["hosts"] as const;
export const hostByIdKey = (hostId: string) => ["host", hostId] as const;
export const backendInfoKey = ["backend-info"] as const;
/** Per-host dynamic-status query key. Each host gets its own cache entry
 *  so the manual-refresh button on `HostDetailView` only revalidates that
 *  one host's snapshot (registration view + daemon-derived facts), never
 *  the whole host list. */
export const hostStatusKey = (hostId: string) => ["host-status", hostId] as const;
/** Prefixes of the two per-host caches that embed agent identity
 *  (`AgentOnHostDto.display_name` / `HostAgentResource.display_name`).
 *  Exported for the identity-carrier registry, which reseats every cached
 *  computer's copy on an `agent_changed` and has no host id in hand
 *  (`features/identity/identity-carrier-caches.ts`). */
export const hostStatusKeyPrefix = ["host-status"] as const;
export const hostProxyConfigKey = (hostId: string) => ["host-proxy-config", hostId] as const;
/** #task:1618 — per-computer PERSISTENT-OWNERSHIP query key (`/resources`).
 *  Deliberately separate from {@link hostStatusKey}: that one caches dynamic
 *  availability and is invalidated by attach/detach/liveness events, which say
 *  nothing about which agents and workspaces are rooted here. Sharing one key
 *  would make a disconnect event refetch ownership (pointless) and an agent
 *  creation leave the availability cache untouched (wrong). */
export const hostResourcesKey = (hostId: string) => ["host-resources", hostId] as const;
/** @see hostStatusKeyPrefix */
export const hostResourcesKeyPrefix = ["host-resources"] as const;
/** Per-host runtime-detection catalog query key. Each computer's installed
 *  runtimes are host-local truth (the daemon resolves them on that machine),
 *  so the catalog is cached per host and never shared across computers. */
export const hostRuntimeCatalogKey = (hostId: string) => ["host-runtime-catalog", hostId] as const;

export interface BackendInfoSnapshot extends ConnectionInfo, Partial<LocalHostInfo> {}

async function fetchHosts(): Promise<HostListResponse> {
  const raw = await api<unknown>("/api/hosts");
  return HostListResponseSchema.parse(raw);
}

/**
 * `#task:1630` (Cindy + Norman review of `e82f8a23`) — a missing
 * `BackendInfo` is a QUERY state, not a snapshot.
 *
 * `backend:info` answers `null` while desktop main has no backend handle
 * yet (window painted before bootstrap finished — reachable, and the
 * reason this code path exists). Synthesising a snapshot for that moment
 * is what re-opened the bug this task closes: any invented value is read
 * downstream as an observation. `{ observable: false }` in particular
 * tells every consumer "the attach timestamp is unreadable here, use
 * your remote authority", which in an embedded deployment means the
 * renderer's own local event WS — instantly `connected` — so a
 * pre-bootstrap frame renders Desktop→Server as Connected and could
 * dismiss a save banner on a purely local signal. It also silently
 * removed the exit path: neither poll gate matches
 * `{mode:'embedded', observable:false}`, so a single transient null
 * pinned the pane until some unrelated mutation invalidated the key.
 *
 * So: throw. The query surfaces `isPending`/`isError` with no data,
 * consumers render "unknown" (no status word) rather than guessing, and
 * `backendInfoRefetchIntervalMs` schedules a cheap local-IPC retry until
 * the first real snapshot lands. `meta.backendReachability: 'neutral'`
 * below keeps this failure out of the backend-unreachable snitch — it
 * says nothing about the server.
 */
export class BackendInfoUnavailableError extends Error {
  constructor() {
    super("backend:info returned no snapshot (no backend handle yet)");
    this.name = "BackendInfoUnavailableError";
  }
}

export async function fetchBackendInfo(): Promise<BackendInfoSnapshot> {
  const info = await getPlatform().client.getConnectionInfo();
  if (!info) throw new BackendInfoUnavailableError();
  const localHost = getPlatform().localHost;
  if (!localHost) return info;
  const snapshot = await localHost.getSnapshot();
  if (!snapshot) throw new BackendInfoUnavailableError();
  return { ...info, ...snapshot };
}

export function useBackendInfoSnapshot(options?: {
  /**
   * D3.d follow-up (Cindy `#task:636` review P1) — opt-in narrow
   * polling so the connection pill flips Connecting → Connected
   * without a user-triggered refetch.
   *
   * The base `useQuery` has no polling / no subscription; before this
   * opt-in, a renderer that mounted in `claimed && attached_at===null`
   * never saw `attached_at` flip to a number unless some other
   * mutation invalidated `backendInfoKey`. That left the pill stuck
   * on "Connecting…" indefinitely.
   *
   * When opted in we poll ONLY while the snapshot is actually in the
   * connecting window (`observable attachment + claimed +
   * attached_at===null`); the moment attached_at lands or the status moves
   * to a terminal arm (revoked / error / failed) we stop polling. In remote
   * mode we never start — the attachment is `{ observable: false }` there,
   * so the window has no exit (`#task:1628` / `#task:1630`). Callers that
   * don't render the connection pill should NOT opt in.
   */
  pollWhileConnecting?: boolean;
  /** Periodically re-check the remote server's advertised outbound protocol
   * version. This lets an already-running app notice a future server protocol
   * bump before the user retries an agent action. */
  pollProtocolCompatibility?: boolean;
}) {
  return useQuery({
    queryKey: backendInfoKey,
    queryFn: fetchBackendInfo,
    // `#task:1462` — backend:info is a local-IPC query that always
    // resolves (and its protocol probe swallows transient failure into
    // `unknown`), so its success/failure says nothing about remote-server
    // reachability. Mark it neutral so `useBackendUnreachableSnitch`
    // ignores it — otherwise the protocol-compatibility poll below would
    // clear a legitimate "Can't reach server" banner every cycle.
    meta: { backendReachability: "neutral" },
    refetchInterval: (query) => backendInfoRefetchIntervalMs(query.state.data ?? null, options),
    refetchOnWindowFocus: options?.pollProtocolCompatibility === true,
  });
}

/**
 * Pure helper that decides whether the next `refetchInterval` tick
 * fires — extracted so a unit test pins the "stop polling on
 * terminal / attached" matrix without spinning up React Query.
 *
 * Returns:
 *   - `false` (no refetch) for remote mode (no observable exit
 *     condition — see the gate below) and for terminal / attached /
 *     pre-claim states
 *   - `CONNECTING_POLL_MS` (number) for the transitional connecting
 *     window so the pill flips Connecting → Connected within ~1
 *     poll cycle after the daemon's outbound WS lands.
 *
 * Chosen interval (1500ms) is a balance: short enough that the user
 * doesn't sit on "Connecting…" for visibly long after the daemon
 * actually attaches; long enough that we don't hammer the embedded
 * backend on a slow / never-attaches branch. The attach watchdog's
 * own budget is 10s (`outbound-attach.ts`), so worst-case 6-7 polls
 * over the entire connecting window — cheap.
 */
export const CONNECTING_POLL_MS = 1500;
export const PROTOCOL_COMPATIBILITY_POLL_MS = 30_000;
/**
 * `#task:1630` — retry cadence while NO snapshot has been observed yet
 * (first fetch in flight, or `backend:info` answered `null` because main
 * has no backend handle). This is the self-heal path that replaces the
 * synthesised snapshot: without it a single pre-bootstrap `null` leaves
 * every consumer on "unknown" until an unrelated mutation invalidates the
 * key. Deliberately unconditional (not behind an option) — it is not a
 * feature poll, it is the query reaching its first value, and it is a
 * local IPC. It stops the moment data exists; a LATER failed refetch
 * keeps the last known snapshot in cache and is not re-tried here.
 */
export const UNKNOWN_SNAPSHOT_RETRY_MS = 1000;

export function backendInfoRefetchIntervalMs(
  data: BackendInfoSnapshot | null,
  options?: {
    pollWhileConnecting?: boolean;
    pollProtocolCompatibility?: boolean;
  },
): number | false {
  // No snapshot yet ⇒ keep asking, cheaply, until there is one. Ordered
  // FIRST because the option-gated polls below all read fields of a
  // snapshot that does not exist yet.
  if (data === null) return UNKNOWN_SNAPSHOT_RETRY_MS;
  if (options?.pollWhileConnecting) {
    const connectingInterval = connectingRefetchIntervalMs(data);
    if (connectingInterval !== false) return connectingInterval;
  }
  if (
    options?.pollProtocolCompatibility &&
    data?.mode === "remote" &&
    data.local_host_status !== undefined
  ) {
    return PROTOCOL_COMPATIBILITY_POLL_MS;
  }
  return false;
}

export function connectingRefetchIntervalMs(data: BackendInfoSnapshot | null): number | false {
  if (!data) return false;
  const attachment = data.local_host_ws_attachment;
  // `#task:1628` / `#task:1630` — this window only exists where its exit
  // condition is observable. `attached_at` comes from the backend's
  // attachment registry, which lives in the remote backend process on the
  // remote arm; there `local_host_ws_attachment` is `{ observable: false }`
  // and a "claimed, not attached yet" reading would be permanent even while
  // the daemon is genuinely attached and agents run fine. Before the field
  // carried this discriminant the poll ran every 1.5s for as long as the
  // window was open — which is what the user read as "the computer keeps
  // retrying" — and each tick also re-probed the remote `GET
  // /api/server-info`. `desktopServerConnectionStatus` learned the same
  // lesson in `#task:1010`: remote connectedness is proven by the renderer
  // event WS and host-status availability, never by this timestamp. The 30s
  // protocol-compatibility poll is a separate opt-in and still applies.
  if (!attachment?.observable) return false;
  // Only poll while we're explicitly waiting for the outbound WS
  // to land. Any other state — including `claimed + attached_at`
  // (already connected), `revoked` / `error` / `failed` (terminal),
  // and `unclaimed` (pre-claim, nothing to attach) — stops the
  // poll loop.
  if (data.local_host_status !== "claimed") return false;
  if (attachment.attached_at !== null) return false;
  return CONNECTING_POLL_MS;
}

export function useHostsQuery(options?: {
  enabled?: boolean;
  /** Poll the owner-scoped host list on an interval. Used by the Add-
   *  computer flow to watch a freshly-created row flip `status`
   *  pending → active once the daemon first attaches. Off by default. */
  refetchInterval?: number | false;
}) {
  return useQuery({
    queryKey: hostsKey,
    queryFn: fetchHosts,
    enabled: options?.enabled ?? true,
    refetchInterval: options?.refetchInterval ?? false,
  });
}

/**
 * #task:1348 — fetch a single host the viewer owns by id.
 *
 * Unlike `useHostsQuery` (the rail/picker list, which the backend filters
 * down to FINALIZED computers), `GET /api/hosts/:id` intentionally returns
 * un-finalized drafts too. That's exactly what the Add-computer modal needs:
 * it minted a draft that won't appear in the list, and must watch its
 * lifecycle `status` flip `pending → attached → active` (`attached` = daemon
 * connected but the user hasn't confirmed the setup yet). Poll with
 * `refetchInterval` while waiting for the first connect.
 */
export function useHostByIdQuery(
  hostId: string | null | undefined,
  options?: { enabled?: boolean; refetchInterval?: number | false },
) {
  return useQuery({
    queryKey: hostByIdKey(hostId ?? "__none__"),
    queryFn: async (): Promise<HostDto> => {
      const raw = await api<unknown>(apiPaths.host(hostId!));
      return HostPrivilegedGetResponseSchema.parse(raw).host;
    },
    enabled: (options?.enabled ?? true) && Boolean(hostId),
    refetchInterval: options?.refetchInterval ?? false,
  });
}

/**
 * #task:1588 slice 2 — the ONE freshness model for `hostStatusKey`.
 *
 * `/status` responses are cached fresh-forever and invalidated by
 * exactly three signals, all handled in `host-connection-sync.ts`:
 *   - `host_connection_changed` (attach/detach) → per-host invalidate;
 *   - `host_agent_liveness_changed` (an agent's runtime came up / its
 *     last one exited) → per-host status invalidate;
 *   - `snapshot_required` (event-loss recovery) → invalidate-all;
 * plus the explicit Refresh action on HostDetailView.
 *
 * History: this key used to carry FOUR divergent per-consumer policies
 * (staleTime Infinity / 0 / 2s+5s poll / poll-until-available), which
 * produced the #1010 stale-pill and #1376 dialog-jitter bugs. Backend-
 * side the read is now a zero-RPC memory snapshot and the backend
 * pushes the invalidation events above, so every consumer shares this
 * single policy and NOBODY polls.
 */
function hostStatusQueryOptions(hostId: string) {
  return {
    queryKey: hostStatusKey(hostId),
    queryFn: async (): Promise<HostStatusResponse> => {
      const raw = await api<unknown>(apiPaths.hostStatus(hostId));
      return HostStatusResponseSchema.parse(raw);
    },
    staleTime: Infinity,
    refetchOnMount: false,
    refetchOnWindowFocus: false,
  } as const;
}

/**
 * `#task:808` slice 4 — fetch the per-host dynamic snapshot powering
 * host detail surfaces. Returns the discriminated `HostStatusResponse`; the
 * detail view branches on `data.availability` for the snapshot card and
 * always renders `data.agents[]` (registration is a DB fact, survives
 * daemon disconnect — see Cindy `Mtf5zZL-Y7PauIDi`).
 *
 * Freshness: see {@link hostStatusQueryOptions} — event-driven, no
 * polling, one policy for every consumer of `hostStatusKey`.
 *
 * The hook is `enabled` only when `hostId` is provided so the master/
 * detail right pane mounting with no selection doesn't burn a request.
 */
export function useHostStatusQuery(
  hostId: string | null | undefined,
  options?: { enabled?: boolean },
) {
  return useQuery<HostStatusResponse>({
    ...hostStatusQueryOptions(hostId ?? "__none__"),
    enabled: (options?.enabled ?? true) && Boolean(hostId),
  });
}

/**
 * Live re-probe for the computer detail page: asks the backend to sample the
 * connected daemon now and writes the answer into the shared status key, so
 * every reader sees it. Only the detail page calls this (on open and on
 * Refresh) — the shared GET stays a zero-RPC cache read because sidebar dots
 * read every computer's status. A backend without the refresh route falls
 * back to re-reading the cache.
 */
export function useRefreshHostStatus(hostId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (): Promise<HostStatusResponse | null> => {
      try {
        const raw = await api<unknown>(`${apiPaths.hostStatus(hostId)}/refresh`, {
          method: "POST",
        });
        return HostStatusResponseSchema.parse(raw);
      } catch (err) {
        if (err instanceof ApiError && (err.status === 404 || err.status === 405)) {
          await qc.invalidateQueries({ queryKey: hostStatusKey(hostId) });
          return null;
        }
        throw err;
      }
    },
    onSuccess: (data) => {
      if (data) {
        qc.setQueryData(hostStatusKey(hostId), data);
      }
    },
  });
}

/**
 * `#task:1359` — batch availability probe for the placement pickers. Placement
 * needs BOTH axes: a computer must be `placeable` (lifecycle) AND its daemon
 * online (availability). `placeable` alone lets an offline-but-finalized
 * computer through, so the picker reads live availability per candidate and
 * disables the offline ones (shiki: "offline computer 应该 disabled").
 *
 * Returns `{ availabilityById, allSettled }`. `availabilityById` maps each host
 * to its last *settled* availability. It is `undefined` only while the FIRST
 * probe is unresolved (no settled data yet); a background refetch of already-
 * settled data KEEPS the last availability rather than blanking it — freshness
 * is an orthogonal axis and must never erase the settled fact (Alice/#1370; the
 * create-dialog jitter, Cindy CDP #task:1376). `allSettled` is false until every
 * probe first lands; the pickers gate default selection on it so staggered
 * `/status` resolution can't transiently make one host look like the only
 * available target and auto-select it (Cindy P1). A genuine offline still
 * propagates: the status query settles to a non-`available` value and the fold
 * flips authoritatively — this removes only the transient in-flight `undefined`,
 * never a real state.
 *
 * Each entry shares `hostStatusKey(id)` with `useHostStatusQuery` so the detail
 * view and the picker never keep divergent caches. The dialog is transient, so
 * we take a fresh mount probe (`staleTime: 0` + `refetchOnMount`). The result
 * has a stable identity keyed on a content signature so downstream memos only
 * recompute when a host's settle-state actually changes, not every render.
 */
export interface HostAvailabilitySnapshot {
  availabilityById: Map<string, HostAvailability | undefined>;
  allSettled: boolean;
}

/**
 * Pure fold from per-host probe state to the picker's availability snapshot.
 * A `checking` host is reported as `undefined` even if it carries a stale
 * `available`, and flips `allSettled` false. Exported so the "staggered probes
 * never yield a premature single-available default" contract is unit-tested
 * without rendering React Query.
 */
export function foldHostAvailability(
  entries: {
    id: string;
    checking: boolean;
    availability: HostAvailability | undefined;
  }[],
): HostAvailabilitySnapshot {
  const availabilityById = new Map<string, HostAvailability | undefined>();
  let allSettled = true;
  for (const entry of entries) {
    availabilityById.set(entry.id, entry.checking ? undefined : entry.availability);
    if (entry.checking) allSettled = false;
  }
  return { availabilityById, allSettled };
}

export interface HostAvailabilityQueryState {
  id: string;
  /** `true` only while the FIRST probe is unresolved (React Query `isPending`)
   *  — no settled data yet. A background refetch of already-settled data is NOT
   *  pending, so it must not be treated as `checking`. */
  isPending: boolean;
  /** Last settled availability from the query cache (survives a background
   *  refetch); `undefined` when the probe has no data (pending or errored). */
  availability: HostAvailability | undefined;
}

/**
 * Pure bridge from per-host React Query state to the picker snapshot.
 * `checking` is driven by `isPending` (no settled data yet), NOT by in-flight
 * `isFetching`: availability is a single-source "last settled fact" and
 * freshness is an orthogonal axis that must never blank it (Alice/#1370). This
 * is the exact seam the create-dialog jitter fix lives on (Cindy CDP
 * #task:1376) — the four cases the review gates on:
 *   - initial pending           → checking (undefined + allSettled false)
 *   - background refetch w/data  → last-settled availability preserved
 *   - settled disconnected/etc.  → flips authoritatively (offline propagates)
 *   - errored no-data (settled)  → undefined + settled (no stuck "Checking…")
 * Exported so those cases are unit-tested without rendering React Query.
 */
export function foldHostAvailabilityQueries(
  results: HostAvailabilityQueryState[],
): HostAvailabilitySnapshot {
  return foldHostAvailability(
    results.map((result) => ({
      id: result.id,
      checking: result.isPending,
      availability: result.availability,
    })),
  );
}

export function useHostAvailabilityMap(hostIds: string[]): HostAvailabilitySnapshot {
  const results = useQueries({
    queries: hostIds.map((id) => hostStatusQueryOptions(id)),
  });
  const entries = hostIds.map((id, i) => {
    const result = results[i];
    return {
      id,
      // `isPending` (NOT `isFetching`): "no settled data yet" = the true initial
      // probe. A BACKGROUND refetch (event-driven invalidation) of already-
      // settled data has `isPending===false` (data exists) even while
      // `isFetching===true`, so it keeps exposing the last availability
      // instead of blanking it. Using `isFetching` here conflated the two
      // and caused the #1376 create-dialog jitter.
      isPending: result?.isPending ?? true,
      availability: result?.data?.availability,
    };
  });
  // Signature keys on the SETTLED fact, not freshness: `isPending` ('c') / each
  // settled availability / errored-no-data ('x'). A background refetch that
  // doesn't change the settled availability leaves the signature (and the memo)
  // untouched — no re-render churn, no flicker.
  const signature = entries
    .map((entry) => `${entry.id}=${entry.isPending ? "c" : (entry.availability ?? "x")}`)
    .join(";");
  return useMemo(
    () => foldHostAvailabilityQueries(entries),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [signature],
  );
}

export interface UpdateHostMutationVars {
  display_label: string | null;
}

/**
 * `#task:1361` — per-agent, HOST-level liveness for the computer detail view's
 * agent list (today's only consumer: `HostDetailView`), sourced from each
 * computer's host status (`/status.agents[].online`, derived backend-side from
 * the daemon's active-runtime set) rather than the conversation runtime-state.
 * A daemon-connected agent that is spawned & reachable but not mid-turn reads
 * `online` here yet `idle` in `/runtime-state`, so the dot — which means "is
 * this agent up on its computer" — must read this source, not the conversation
 * runtime-state projection (Cindy #task:1362 P1).
 *
 * This is NOT the agent-presence axis: #task:2049 made presence
 * `(availability, away_at)` and forbade process-aliveness as an input, and
 * #task:2056 deleted the per-agent liveness read face entirely. "Is this agent's
 * process up on that machine" survives only HERE, as a host-scoped operational
 * detail of the computer view — never as a person's presence.
 *
 * Shares `hostStatusKey(id)` with the detail view / picker (one cache per
 * computer, N computers ≤ N agents). #task:1588 slice 2 removed the 5s
 * poll: host-level liveness changes now arrive as `host_agent_liveness_changed`
 * invalidations pushed by the backend (see `host-connection-sync.ts`), so
 * the dots stay live without any interval. Returns `Map<agentId, boolean>`
 * (true = online); agents whose computer has no status (deleted / unowned
 * host) are absent → caller treats them as offline.
 */
export function useAgentOnlineByHost(hostIds: string[]): Map<string, boolean> {
  const results = useQueries({
    queries: hostIds.map((id) => hostStatusQueryOptions(id)),
  });
  // Content signature over every agent's online flag, so the memo (and Map
  // identity) only changes when a liveness bit actually flips.
  const signature = results
    .map((r) => (r.data?.agents ?? []).map((a) => `${a.agent_id}:${a.online}`).join(","))
    .join("|");
  return useMemo(() => {
    const map = new Map<string, boolean>();
    for (const r of results) {
      for (const a of r.data?.agents ?? []) {
        map.set(a.agent_id, a.online === "online");
      }
    }
    return map;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [signature]);
}

/**
 * `#task:808` slice 4 — rename a host (only `display_label` is mutable;
 * `name` stays the stable identifier set at claim time). On success we
 * invalidate `hostsKey` so the sidebar list re-renders the new label.
 *
 * Guard the input against the shared schema before the request so a
 * refusal reaches the user as a sentence instead of a 400 (`./host-label`
 * owns the wording; the rule stays in `UpdateHostInputSchema`, which the
 * server applies too). Over-long labels are REJECTED, not reshaped
 * (#task:1849 — the repo used to silently truncate to 64 and return
 * 200 with a value the user never typed). Empty → null is still mapped
 * server-side, so a clear-on-blank flow works without a separate
 * "Clear" mutation: send `{ display_label: null }` to clear.
 */
export function useUpdateHostMutation(hostId: string) {
  const qc = useQueryClient();
  return useMutation<HostDto, Error, UpdateHostMutationVars>({
    mutationFn: async (vars: UpdateHostMutationVars): Promise<HostDto> => {
      // `safeParse` + our own sentence, not `parse`: a raw ZodError
      // message is a JSON blob, and both write faces render this
      // rejection to the user (the inline rename error row and the
      // create dialog's naming step). The rule itself stays in the
      // schema — `rejectHostLabel` only picks the wording.
      const rejection = rejectHostLabel(vars.display_label);
      if (rejection) throw new Error(rejection);
      const body: UpdateHostInput = UpdateHostInputSchema.parse(vars);
      const raw = await api<unknown>(apiPaths.host(hostId), {
        method: "PATCH",
        body: JSON.stringify(body),
      });
      const parsed = z.object({ host: HostDtoSchema }).parse(raw);
      return parsed.host;
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: hostsKey });
    },
  });
}

/** True when an `api()` failure carries the shared `computer_offline`
 *  envelope. This classifies a *raced* offline error at the UI layer (the
 *  computer went unreachable between the availability check and the GET/PUT
 *  landing) so the form can show the connect hint once — it is NOT a second
 *  source of the "offline" fact. The durable offline signal lives solely on
 *  the availability axis (`useHostStatusQuery`); this query and mutation are
 *  config-or-throw and never encode offline as data.
 *
 *  Exported so a unit test pins the contract-boundary parse (typed
 *  `error_code` discriminator via the shared schema) without rendering the
 *  hook — mirrors `interpretDeleteComputerError`. */
export function isHostProxyOfflineError(err: unknown): boolean {
  if (!(err instanceof ApiError)) return false;
  const parsed = HostProxyConfigOfflineResponseSchema.safeParse(safeJsonParse(err.body));
  return parsed.success && parsed.data.error_code === HOST_PROXY_CONFIG_OFFLINE_ERROR_CODE;
}

/**
 * Proxy config for a computer is **host-local truth** — it lives in the
 * target daemon's per-computer profile, never in the control-plane DB. The
 * backend reads/writes it by dispatching to the live daemon over the composite
 * outbound adapter, so both GET and SET require the computer to be **online**
 * (a live attachment). When it isn't, the route fails closed with a typed
 * `computer_offline` envelope (503).
 *
 * **config-or-throw** (Alice/Cindy #1350 single-source review): "offline" is
 * modeled in exactly ONE place — the availability axis (`useHostStatusQuery`,
 * which drives the section gate + hint). So this query resolves to the bare
 * `HostProxyConfig` on success and lets *every* failure — including a raced
 * `computer_offline` — propagate as a normal query error. It never encodes
 * offline as a `{ ok: false }` data sentinel: doing so duplicated the
 * availability fact and, cached under `staleTime: Infinity`, pinned a stale
 * offline hint that never cleared after the computer recovered (the P1 Cindy
 * caught). Recovery is now by construction — availability flips back to
 * `available` → the gate re-enables this query → it refetches.
 *
 * `HostProxyConfigSchema` / `HostProxyConfigResponseSchema` are the single
 * shared source (`@silverhand/shared`); we never reshape the DTO here.
 */
export function useHostProxyConfigQuery(
  hostId: string | null | undefined,
  options?: { enabled?: boolean },
) {
  return useQuery<HostProxyConfig>({
    queryKey: hostProxyConfigKey(hostId ?? "__none__"),
    queryFn: async () => {
      const raw = await api<unknown>(apiPaths.hostProxyConfig(hostId!));
      return HostProxyConfigResponseSchema.parse(raw).config;
    },
    enabled: (options?.enabled ?? true) && Boolean(hostId),
    // Successful configs are cached (renderer is the sole writer; a computer's
    // proxy doesn't drift underneath us) — no blanket `staleTime: 0` that would
    // re-fetch happy-path config on every remount. Offline is NOT cached here:
    // it propagates as an error and recovers when availability re-enables us.
    staleTime: Infinity,
    // Don't retry-hammer a raced `computer_offline`; recovery rides the
    // availability transition, not query retries.
    retry: false,
    refetchOnWindowFocus: false,
  });
}

export function useSaveHostProxyConfigMutation(hostId: string) {
  const qc = useQueryClient();
  return useMutation<HostProxyConfig, Error, HostProxyConfig>({
    mutationFn: async (config: HostProxyConfig) => {
      const body = HostProxyConfigSchema.parse(config);
      const raw = await api<unknown>(apiPaths.hostProxyConfig(hostId), {
        method: "PUT",
        body: JSON.stringify(body),
      });
      return HostProxyConfigResponseSchema.parse(raw).config;
    },
    // config-or-throw: a raced `computer_offline` rejects the mutation (the
    // caller shows the connect hint once); it is never written into the query
    // cache. Only a confirmed write updates the cached config.
    onSuccess: (config) => {
      qc.setQueryData(hostProxyConfigKey(hostId), config);
    },
  });
}

/**
 * #task:1348 / #task:1428 — create a user-managed computer. The backend mints a
 * PENDING host + a reveal-once connect token and returns the bootstrap payload
 * (`connect_token` + `daemon_npm_spec`); the desktop assembles the connect
 * command locally from those + its own connected server URL. Naming happens
 * AFTER the first successful connect, so this only starts the flow; the row
 * shows up in the sidebar as pending until a daemon attaches.
 *
 * **Reveal-once boundary**: `connect_token` is the cleartext credential shown
 * exactly once. The caller keeps it in local component state only — never write
 * it into the query cache. The bootstrap payload carries no server reach URL:
 * the command's `--server-url` is always this desktop's own connected origin.
 */
export function useCreateComputerMutation() {
  const qc = useQueryClient();
  return useMutation<HostCreateResponse, Error, void>({
    mutationFn: async () => {
      const raw = await api<unknown>(apiPaths.hosts, {
        method: "POST",
        body: JSON.stringify({}),
      });
      return HostCreateResponseSchema.parse(raw);
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: hostsKey });
    },
  });
}

export type RotateConnectTokenResult = z.infer<typeof HostRotateTokenUserManagedResponseSchema>;

/**
 * #task:1348 / #task:1354 — delete the user-managed computer behind an
 * abandoned Add-computer draft.
 *
 * A pending computer is a modal-scoped DRAFT, not a permanent lifecycle
 * event (shiki: "这个连接只是一个临时的生命周期"). Closing the Add-computer
 * dialog before the setup is finalized deletes the draft row and
 * hard-invalidates its reveal-once token, so no orphan pending card lingers
 * in the rail and the never-copied command can't later connect.
 *
 * ⚠️ Post-#task:1355 this route is the UNIFIED empty-computer delete:
 * `DELETE /api/hosts/:id` hard-deletes an owner's `remote + user_invite`
 * host whenever it has no dependents — INCLUDING a *finalized* empty
 * computer. The old `finalized_at → 409 host_already_active` guard that made
 * a close-time cancel a benign no-op is GONE. So this mutation is NOT safe
 * to fire on a computer that has been finalized — the caller
 * (`NewComputerDialog`) owns that decision via its categorical
 * "finalized → never delete" guard and only fires this for a genuine draft
 * abandon. It stays best-effort (404/409 just mean "nothing to clean up"),
 * but "best-effort" no longer implies "harmless on a real computer".
 */
export function useCancelPendingComputerMutation() {
  const qc = useQueryClient();
  return useMutation<HostCancelPendingResponse, Error, string>({
    mutationFn: async (hostId: string) => {
      const raw = await api<unknown>(apiPaths.hostCancelPending(hostId), {
        method: "DELETE",
      });
      return HostCancelPendingResponseSchema.parse(raw);
    },
    onSettled: () => {
      // Refresh the rail whether the delete landed or the row turned out
      // to be a real (already-attached) computer — either way the list
      // may have changed.
      void qc.invalidateQueries({ queryKey: hostsKey });
    },
  });
}

/**
 * #task:1348 / #task:1428 — rotate a user-managed computer's connect token.
 * Mints a fresh reveal-once credential, invalidates the old token, and
 * force-drops any live connection using it (no grace — Cindy/Alice contract).
 * Same bootstrap-payload shape as create; the new `connect_token` lands once in
 * the response and is kept in local state only, and the card assembles the
 * command locally from it + this desktop's connected server URL.
 */
export function useRotateConnectTokenMutation(hostId: string) {
  const qc = useQueryClient();
  return useMutation<RotateConnectTokenResult, Error, void>({
    mutationFn: async () => {
      const raw = await api<unknown>(apiPaths.hostRotateConnectToken(hostId), {
        method: "POST",
      });
      return HostRotateTokenUserManagedResponseSchema.parse(raw);
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: hostsKey });
    },
  });
}

/**
 * #task:1348 / #task:1351 — disconnect a user-managed computer's CURRENT
 * live connection WITHOUT touching credentials or lifecycle. This is a
 * pure availability-axis kick: the connect token stays valid and the same
 * connection command reconnects later (contrast rotate/revoke, which
 * invalidate the token). The backend returns a benign `disconnected:false`
 * no-op when there was no live connection, so callers don't special-case
 * races.
 *
 * Invalidate both the host list (rail availability pips) and this host's
 * status (the detail-view pill + the Disconnect button's own gate) so the
 * UI flips online → offline right after the kick; liveness also arrives
 * via host-connection-sync, this just makes it immediate.
 */
export function useDisconnectHostMutation(hostId: string) {
  const qc = useQueryClient();
  return useMutation<HostDisconnectResponse, Error, void>({
    mutationFn: async () => {
      const raw = await api<unknown>(apiPaths.hostDisconnect(hostId), {
        method: "POST",
      });
      return HostDisconnectResponseSchema.parse(raw);
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: hostsKey });
      void qc.invalidateQueries({ queryKey: hostStatusKey(hostId) });
    },
  });
}

/**
 * App-managed credential rotation. Wraps the main-process
 * `requireLocalHost().rotateHostToken` IPC, which updates the local
 * daemon-config's host token in place. Token bytes NEVER enter the
 * renderer — the IPC returns `LocalHostMutationResult` with fresh
 * `BackendInfo` fields but no credential payload.
 *
 * This is the app-managed counterpart to `useRotateConnectTokenMutation`
 * (user-managed reveal-once command). Do not merge the two flows —
 * they target different daemon layers (local config vs. server-issued
 * connect-token) and their success/failure copy is different.
 *
 * The mutation surfaces the typed `ok:false` branch as an Error so
 * consumers can render one banner path without special-casing the
 * discriminant. Invalidates the host list + backend snapshot so the
 * detail view re-reads whatever changed (e.g. `local_host_status`).
 */
export function useRotateHostTokenMutation(hostId: string) {
  const qc = useQueryClient();
  return useMutation<void, Error, void>({
    mutationFn: async () => {
      const result = await requireLocalHost().rotateHostToken({ hostId });
      if (!result.ok) {
        throw new Error(result.message ?? result.code);
      }
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: hostsKey });
      void qc.invalidateQueries({ queryKey: backendInfoKey });
      void qc.invalidateQueries({ queryKey: hostStatusKey(hostId) });
    },
  });
}

/**
 * App-managed soft revoke uses the server API, which
 * writes `revoked_at`, invalidates the credential, force-drops any
 * live WS, and preserves agents/workspaces (per doc §5.3). NOT a
 * lexical alias of Delete — Delete is the user-managed terminal verb
 * with dependency-check + cascade semantics.
 *
 * Revoking the current local computer does NOT log the user out —
 * user sessions are user-level, independent of Computer lifecycle.
 * The renderer's session stays valid; the detail view flips to a
 * revoked-terminal read-only shape via the invalidated host queries.
 */
export async function revokeComputer(hostId: string): Promise<void> {
  const raw = await api<unknown>(apiPaths.hostRevoke(hostId), { method: "POST" });
  HostRevokeResponseSchema.parse(raw);
}

export function useRevokeComputerMutation(hostId: string) {
  const qc = useQueryClient();
  return useMutation<void, Error, void>({
    mutationFn: async () => {
      await revokeComputer(hostId);
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: hostsKey });
      void qc.invalidateQueries({ queryKey: backendInfoKey });
      void qc.invalidateQueries({ queryKey: hostStatusKey(hostId) });
    },
  });
}

/**
 * #task:1618 — `GET /api/hosts/:id/resources`: the agents placed on a computer
 * and the `regular` workspaces whose files live there, each with the viewer's
 * `viewer_role`.
 *
 * An ordinary Computer-domain ownership query, not a delete-impact endpoint —
 * the delete dialog is only its first consumer, and it is the consumer (not this
 * hook, not the backend) that reads the list as "what deleting this destroys".
 * Ownership is orthogonal to `/status` availability, so an unreachable computer
 * still answers this in full: that computer is precisely the one being deleted.
 *
 * Freshness: `staleTime: 0` and `enabled` gated on the dialog being open, so
 * every open takes a fresh read rather than confirming against a list that could
 * be minutes old. `retry: false` — a failed read is a *decision point* for the
 * caller (fail closed: no destructive confirm), not something to silently
 * re-attempt behind a spinner.
 */
export function useHostResourcesQuery(
  hostId: string | null | undefined,
  options?: { enabled?: boolean },
) {
  return useQuery<HostResourcesResponse>({
    queryKey: hostResourcesKey(hostId ?? "__none__"),
    queryFn: async () => {
      const raw = await api<unknown>(apiPaths.hostResources(hostId!));
      return HostResourcesResponseSchema.parse(raw);
    },
    enabled: (options?.enabled ?? true) && Boolean(hostId),
    staleTime: 0,
    retry: false,
    refetchOnWindowFocus: false,
  });
}

/**
 * #task:1380 — a `409 has_pending_agents` block, surfaced as its OWN typed
 * error because it is the ONLY path on which the user can learn this state: an
 * agent on this computer is still materializing, which `/resources` deliberately
 * does not predict (a read model lists objects, it does not forecast whether a
 * command will succeed — shiki 2026-07-25). Transient and self-clearing, so the
 * correct UX is wait-and-retry.
 *
 * Every other refusal is NOT typed here. `409 has_dependents` (a DELETE that
 * skipped the confirmation) describes a fact the preview already showed, so from
 * this dialog it is unreachable-by-construction and gets the generic failure
 * line — giving it a named screen would rebuild the discovery-through-error path
 * this task removed.
 */
export class ComputerHasPendingAgentsError extends Error {
  constructor(readonly pendingAgents: number) {
    super("has_pending_agents");
    this.name = "ComputerHasPendingAgentsError";
  }
}

/**
 * #task:1354 / #task:1618 — delete a user-managed computer (the single removal
 * verb; there is no user-facing revoke).
 *
 * Two steps, and this is the second one. The dialog first reads
 * `GET /api/hosts/:id/resources` (see {@link useHostResourcesQuery}) and shows
 * what is rooted on the computer; this mutation then sends the ONE
 * `cascade_dependents` boolean the user confirmed. The flag is consent to an
 * intent — "delete this computer and what it holds" — not an id snapshot, so the
 * backend deletes whatever belongs at execution time (it re-reads the set, the
 * pending-materialization state and the per-workspace owner gate inside its
 * delete transaction, #task:1617).
 *
 * Which means: this can still reject after a confirm, and the caller must not
 * assume success. Only `409 has_pending_agents` gets a typed branch — see
 * {@link ComputerHasPendingAgentsError} for why the other refusals do not.
 *
 * On success we invalidate `hostsKey`; the app-shell then drops the deleted
 * row from `computerHosts`, which auto-clears the detail selection — no
 * explicit navigation needed here.
 */
export interface DeleteComputerVars {
  /** The user's single confirmation bit, sent verbatim as `cascade_dependents`.
   *  Set when the preview listed anything AND when it listed nothing: consent is
   *  over the computer, not over the rows that happened to be on screen (Cindy
   *  2026-07-25 — gating the flag on "preview was non-empty" would smuggle a
   *  1-bit staleness check back into the protocol). */
  cascadeDependents?: boolean;
}

export function useDeleteComputerMutation(hostId: string) {
  const qc = useQueryClient();
  return useMutation<HostDeleteResponse, Error, DeleteComputerVars | void>({
    mutationFn: async (vars) => {
      try {
        const raw = await api<unknown>(apiPaths.host(hostId), {
          method: "DELETE",
          ...(vars?.cascadeDependents
            ? { body: JSON.stringify({ cascade_dependents: true }) }
            : {}),
        });
        return HostDeleteResponseSchema.parse(raw);
      } catch (err) {
        throw interpretDeleteComputerError(err);
      }
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: hostsKey });
    },
  });
}

/**
 * Per-host runtime-detection catalog (#task:1376). Asks the *target*
 * computer's daemon (composite dispatch by hostId — not app-local
 * `/api/runtimes`) which offerable runtimes (catalog `selectable`)
 * are installed, returning an explicit state per runtime.
 *
 * - `enabled` gates the probe on the caller's availability check: an offline
 *   computer can't be probed, so the picker collapses into the availability
 *   gate rather than a bogus all-absent result.
 * - `retry: false` — a dispatch failure (host unreachable/timeout) is a real
 *   "unknown-for-all" signal the UI surfaces as *enabled + can't-confirm*, not
 *   silently retried into a spinner.
 * - `reprobe()` is the manual "Recheck" force path: it hits `?force=1` to
 *   bypass the daemon's short TTL (a user who just installed a runtime
 *   shouldn't have to reopen the dialog) and writes the fresh result back into
 *   the same cache key. It never throws — a probe hiccup must not break the
 *   dialog; the last-known catalog stays in place.
 */
export function useHostRuntimeCatalog(
  hostId: string | null | undefined,
  options?: { enabled?: boolean },
): {
  data: RuntimeCatalogResponse | undefined;
  isLoading: boolean;
  isError: boolean;
  reprobe: () => Promise<void>;
  isReprobing: boolean;
} {
  const qc = useQueryClient();
  const [isReprobing, setIsReprobing] = useState(false);
  const enabled = (options?.enabled ?? true) && Boolean(hostId);
  const query = useQuery({
    queryKey: hostRuntimeCatalogKey(hostId ?? "__none__"),
    enabled,
    // Installed-runtime sets change rarely; cache within a dialog session but
    // refetch on remount so reopening after an install picks it up. `reprobe`
    // (force) is the immediate-refresh escape hatch.
    staleTime: 30_000,
    retry: false,
    queryFn: async () => {
      const raw = await api<unknown>(apiPaths.hostRuntimeCatalog(hostId!));
      return RuntimeCatalogResponseSchema.parse(raw);
    },
  });
  const reprobe = useCallback(async () => {
    if (!hostId) return;
    setIsReprobing(true);
    try {
      const raw = await api<unknown>(`${apiPaths.hostRuntimeCatalog(hostId)}?force=1`);
      qc.setQueryData(hostRuntimeCatalogKey(hostId), RuntimeCatalogResponseSchema.parse(raw));
    } catch {
      // Swallow: a forced re-probe failure leaves the last-known catalog (or
      // the unknown-for-all fallback) in place. Never break the dialog.
    } finally {
      setIsReprobing(false);
    }
  }, [hostId, qc]);
  return {
    data: query.data,
    isLoading: enabled && query.isLoading,
    isError: query.isError,
    reprobe,
    isReprobing,
  };
}

/**
 * Pure: interpret a delete failure. A `409 has_pending_agents` envelope (parsed
 * via the shared `HostDeletePendingAgentsResponseSchema` — single source, never a
 * renderer-local reshape) becomes a typed `ComputerHasPendingAgentsError`; every
 * other failure passes through unchanged and the dialog shows its generic
 * failure line.
 *
 * #task:1618 — that is the whole mapping now. The named `has_dependents` /
 * `dependents_changed` branches are gone with the discovery-through-error
 * flow: the dialog learns the impact from
 * `/resources` BEFORE asking, so those refusals are unreachable from here and a
 * dedicated screen for each would be code with no trigger.
 *
 * Exported so a unit test pins the contract-boundary parse (`error_code`
 * discriminator, non-JSON body) without rendering the hook.
 */
export function interpretDeleteComputerError(err: unknown): Error {
  if (err instanceof ApiError && err.status === 409) {
    const pending = HostDeletePendingAgentsResponseSchema.safeParse(safeJsonParse(err.body));
    if (pending.success) {
      return new ComputerHasPendingAgentsError(pending.data.pending_agents);
    }
  }
  return err instanceof Error ? err : new Error(String(err));
}

/** Best-effort JSON parse of an `ApiError.body` string; returns `null` on
 *  non-JSON so a schema `safeParse` cleanly falls through to the generic
 *  error path instead of throwing. */
function safeJsonParse(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}
