// AUTO-SPLIT from the former monolithic api/dto.ts (#task:963).
// Bounded context: host. Re-exported via api/index.ts → @silverhand/shared.
import { z } from 'zod';
import { DAEMON_ADMISSION } from '../daemon-admission.generated.js';
import {
  FastModeSchema,
  HostKindSchema,
  HostProvisioningSourceSchema,
  ReasoningEffortTokenSchema,
  RuntimeKindSchema,
} from '../db/schema.js';

export const HostLifecycleStatusSchema = z.enum(['pending', 'attached', 'active', 'revoked']);
export type HostLifecycleStatus = z.infer<typeof HostLifecycleStatusSchema>;

/**
 * Canonical length bound for a Computer's user-facing display label
 * (`hosts.display_label`), in UTF-16 code units — the unit `z.string()
 * .max()` counts.
 *
 * shiki 2026-08-06 (#task:1849) fixed it at 64 and ruled out silent
 * truncation. What this constant serves:
 *   - `HostClaimOrCreateInputSchema.label` and
 *     `UpdateHostInputSchema.display_label` — the two writers;
 *   - the repo's write invariant (`normalizeDisplayLabel` throws past
 *     the bound instead of the `slice(0, 64)` it used to do, so reaching
 *     that depth over-long means the API boundary was bypassed);
 *   - fitting SYSTEM-derived defaults at their producer (the desktop's
 *     hostname-derived claim label) — a user's own input is refused, but
 *     failing closed on a machine-supplied default would block claiming
 *     the local computer entirely;
 *   - the client-side rejection copy.
 *
 * It must NOT become a DOM `maxLength`. Both schemas judge the TRIMMED
 * string while a length attribute caps the RAW input: they are different
 * phases, so capping raw input at this number cuts a padded paste short
 * and persists a name the user never typed — the same silent rewrite the
 * server just stopped doing, one layer up. The name fields therefore
 * carry no length attribute and refuse with a reason instead
 * (`features/hosts/host-label`, and `features/common/display-name` for
 * the sibling field).
 *
 * Deliberately NOT `ENTITY_NAME_MAX_LENGTH`: that constant bounds
 * handles/entity names and is counted in code points
 * (`Array.from(name).length`). Equal numbers today, different units —
 * sharing them would make astral-plane input disagree across layers.
 */
export const HOST_DISPLAY_LABEL_MAX_LENGTH = 64;

//
// Single source-of-truth shape for every host the user owns. Both the
// owner-filtered `GET /api/hosts` list and the `POST /api/hosts/claim-
// or-create` response inline this DTO so renderer + desktop main share
// one parser. `revoked_at` and `last_seen_at` are owner-scoped
// metadata and are never returned outside the owning user (Stage B.4
// viewer filter at the route boundary).
export const HostDtoSchema = z.object({
  id: z.string(),
  name: z.string(),
  kind: HostKindSchema,
  created_at: z.number(),
  /** Soft-delete tombstone. Null while the host is active. After
   *  revoke the row stays around (audit history) but every credential
   *  and runtime placement using it fails closed. */
  revoked_at: z.number().nullable(),
  /** Last claim-or-create touch timestamp. Null only for legacy /
   *  migration-seeded hosts that have not been claimed yet. */
  last_seen_at: z.number().nullable(),
  /** First successful authenticated daemon attach. NULL means pending;
   *  non-null alone is not enough for placement until finalized_at is set. */
  activated_at: z.number().nullable().optional().default(null),
  /** User confirmed the Add computer flow. NULL means the row is still a
   *  setup draft, even if a daemon has already attached. */
  finalized_at: z.number().nullable().optional().default(null),
  /** Diagnostic-only live attachment name from the currently connected daemon.
   *  This is availability metadata, never identity or dedupe input. */
  connected_host_name: z.string().nullable().optional().default(null),
  /** Lifecycle/origin metadata for copy/badges/recovery messaging only. */
  provisioning_source: HostProvisioningSourceSchema.optional().default('system'),
  /** Stable lifecycle display state. Renderer uses this for rail/modal
   *  bucketing instead of deriving lifecycle states from raw timestamps. */
  status: HostLifecycleStatusSchema.optional().default('pending'),
  /** Backend-owned placement readiness predicate. Frontend pickers consume this
   *  boolean directly instead of re-deriving eligibility from lifecycle fields. */
  placeable: z.boolean().optional().default(false),
  /** User that claimed/created this computer row. Renderer uses this
   *  to gate host-local controls such as daemon proxy config. Optional
   *  receive-side default keeps older dev backends parseable. */
  creator_user_id: z.string().nullable().optional().default(null),
  /** v36 (#task:808): user-facing rename target. `name` stays the
   *  stable normalized identifier; renderer reads `display_label ?? name`.
   *  NULL on rows claimed before v36 and after the user clears the label.
   *  Length 1-64; CJK/spaces allowed. Set via `POST /api/hosts/claim-or-create`
   *  at claim time (input `label`) or mutated via `PATCH /api/hosts/:id`. */
  display_label: z.string().nullable(),
});
export type HostDto = z.infer<typeof HostDtoSchema>;

/** `PATCH /api/hosts/:id` rename input. Currently only `display_label`
 *  is mutable; `name` is the stable identifier and never changes after
 *  claim. Pass `null` to clear the custom label (renderer falls back to
 *  `name`). Empty/whitespace-only strings are coerced to `null` server-side. */
export const UpdateHostInputSchema = z
  .object({
    /** v36 rename input, bounded by `HOST_DISPLAY_LABEL_MAX_LENGTH`.
     *
     *  The cap used to be deliberately looser than the repo's normalizer
     *  (schema 256 vs a `slice(0, 64)` on write) so that typing past the
     *  visible limit auto-truncated in one round-trip instead of 400-ing.
     *  shiki reversed that in #task:1849: silently returning 200 with a
     *  value the user did not type is worse than a rejection, so every
     *  layer now shares this bound and over-long input is refused.
     *  `.trim()` stays — trimming plus empty-string → `null` is the
     *  documented clear-on-blank path, not a rewrite of a valid value. */
    display_label: z.string().trim().max(HOST_DISPLAY_LABEL_MAX_LENGTH).nullable(),
  })
  .strict();
export type UpdateHostInput = z.infer<typeof UpdateHostInputSchema>;

/** `PATCH /api/hosts/:id` response. Returns the updated DTO so the
 *  caller can refresh local UI without a follow-up GET. */
export const UpdateHostResponseSchema = z.object({
  host: HostDtoSchema,
});
export type UpdateHostResponse = z.infer<typeof UpdateHostResponseSchema>;

//
// `GET /api/hosts/:id/status` answers "what is this host running right
// now". Permission: 404 for missing / cross-user (no enumeration via
// id-probing). 200 with `availability` tagged-union once owner ACL
// passes. Renderer NEVER branches on local vs remote — same shape for
// every host.
//
// availability:
//   - 'available'    — daemon connected + responsive; `snapshot` populated.
//   - 'disconnected' — host row OK but no live daemon connection. Renderer
//                      shows persistent fields + "Computer is offline".
//                      `admission_failure` (#task:1631) tells the renderer
//                      whether that offline state can self-heal.
//   - 'unreachable'  — connected but RPC failed (timeout / protocol error).
//                      `reason` carries the typed failure.
//   - 'revoked'      — host has `revoked_at` set. Explicit so the renderer
//                      can show "device revoked" instead of pretending it's
//                      just temporarily offline.
//
// agents[] is a registration view — one row per `agents.host_id == hostId`
// row, NOT one row per running process. `online` is derived backend-side
// from the daemon's runtime snapshot (active agent_id set intersected with
// the registered set). pid/started_at/conversation are intentionally
// absent: this pane describes who's bound to this host, not process state.
export const HostAvailabilitySchema = z.enum([
  'available',
  'disconnected',
  'unreachable',
  'revoked',
]);
export type HostAvailability = z.infer<typeof HostAvailabilitySchema>;

/** WS event kind published on `viewer:{ownerUserId}` when the backend's
 *  outbound host-attachment registry changes. The event is an invalidation
 *  signal only: consumers refetch `/api/hosts/:id/status` for the authoritative
 *  availability projection instead of trusting a delta payload. */
export const HOST_CONNECTION_CHANGED_EVENT = 'host_connection_changed';

export const HOST_WS_CLOSE_REASON_REVOKED = 'host_revoked';
export const HOST_WS_CLOSE_REASON_DISCONNECTED = 'host_disconnected';
/** #task:1966 — the daemon's wire protocol is not the one this server speaks, so
 *  no retry against this server can ever succeed. Sent as the close reason for a
 *  well-formed `ready` on a wrong `protocol_version`, and TERMINAL on the daemon
 *  side: it suppresses reconnect for the current connection config instead of
 *  burning the ~1s reconnect floor forever. A malformed `ready` is NOT this — it
 *  is a corrupt dialer, which keeps the generic protocol-error close and stays
 *  retryable. */
export const HOST_WS_CLOSE_REASON_UPDATE_REQUIRED = DAEMON_ADMISSION.updateRequiredCloseReason;

export const HostConnectionChangedEventSchema = z
  .object({
    host_id: z.string(),
  })
  .strict();
export type HostConnectionChangedEvent = z.infer<typeof HostConnectionChangedEventSchema>;

/** #task:1588 slice 2 — WS event kind published on `viewer:{ownerUserId}`
 *  when a host's active-agent set actually changes (an agent's first
 *  runtime comes up, or its last one exits). Invalidation signal only:
 *  consumers refetch `/api/hosts/:id/status` (a zero-RPC memory read
 *  backend-side) — the payload never carries liveness state, so it can
 *  never become a second source of truth. */
export const HOST_AGENT_LIVENESS_CHANGED_EVENT = 'host_agent_liveness_changed';

export const HostAgentLivenessChangedEventSchema = z
  .object({
    host_id: z.string(),
  })
  .strict();
export type HostAgentLivenessChangedEvent = z.infer<typeof HostAgentLivenessChangedEventSchema>;

export const HostUnreachableReasonSchema = z.enum([
  'daemon_unreachable',
  'timeout',
  'protocol_error',
]);
export type HostUnreachableReason = z.infer<typeof HostUnreachableReasonSchema>;

/**
 * #task:1631 — why the LAST authenticated connection attempt from this
 * computer's daemon was refused, projected onto the `disconnected` arm.
 *
 * This is **not** a fifth availability value and not a second liveness
 * source. `availability` still answers "is there a live connection right
 * now" (`disconnected`); this answers the different question "is the
 * offline state self-healing?". A daemon whose wire protocol the server
 * refuses will never come back on its own, so the renderer must not
 * render the generic "will appear when the host is back online" promise
 * for it — the only effective user action is upgrading Silverhand on
 * that machine.
 *
 * Recorded only AFTER host-token authentication, so it is always
 * attributable to a real Computer (the version is read from the daemon's
 * `ready` frame, which arrives post-auth). Cleared by the next valid
 * `ready` — presence therefore means "still unresolved as of now",
 * never a historical log. Absent means the offline state carries no
 * known terminal cause: treat it as ordinary, possibly-self-healing
 * offline.
 *
 * Both version integers are carried (not a boolean) because the
 * actionable copy needs them: which version the machine offers vs which
 * one this server requires.
 */
export const HostAdmissionFailureSchema = z
  .object({
    kind: z.literal('protocol_mismatch'),
    offered_protocol_version: z.number().int().nonnegative(),
    expected_protocol_version: z.number().int().nonnegative(),
    /** Epoch ms of the refused attempt. */
    observed_at: z.number(),
  })
  .strict();
export type HostAdmissionFailure = z.infer<typeof HostAdmissionFailureSchema>;

/** Per-agent registration entry returned at the top level of every
 *  `HostStatusResponse` 200 arm. This is a DB-persisted **registration**
 *  view, NOT a process list — agents registered to this host stay visible
 *  even when the daemon is disconnected / unreachable / revoked, just
 *  with `online: 'offline'` (Cindy `Mtf5zZL-Y7PauIDi` P1: registration is
 *  the primary fact; only `online` is daemon-derived).
 *  v1 UI renders only `display_name` / `handle` / `runtime` / `online`;
 *  `agent_id` stays in the DTO so future row-click / runtime-control
 *  affordances don't need a wire change. */
export const AgentOnHostDtoSchema = z.object({
  agent_id: z.string(),
  display_name: z.string(),
  /** Mention handle (`@<agent.name>`). Prefixed so the renderer doesn't
   *  reassemble it from a raw name. */
  handle: z.string(),
  /** CLI runtime kind (claude / codex / ...) — straight from `agents.runtime`. */
  runtime: z.string(),
  /** Backend-stored avatar URL (null when the user hasn't uploaded one).
   *  Routed through the renderer's `useAgentAvatarObjectUrls` cache so
   *  the host detail surface shows the same avatar a user picked in
   *  #Members instead of falling back to the runtime tile.
   *
   *  **Skew-tolerant**: declared `.nullable().optional().default(null)`
   *  so a response from a backend that predates this field (dev hot-
   *  reload skew, or any future producer/consumer version drift) parses
   *  successfully with `avatar_url = null` rather than failing the
   *  entire `HostStatusResponse` and turning the detail pane into an
   *  error banner. The default normalizes `undefined → null`, so the
   *  inferred output type stays `string | null` and consumers don't
   *  need to handle `undefined`. New backend code emits the field
   *  unconditionally (`buildAgentRows` in routes.ts); the tolerance
   *  here is only the receive-side safety net. */
  avatar_url: z.string().nullable().optional().default(null),
  /** Same values as `AgentDto`; the viewer may not have this agent in its
   *  cache (someone else's computer). Skew-tolerant like `avatar_url`. */
  model: z.string().nullable().optional().default(null),
  reasoning_effort: ReasoningEffortTokenSchema.nullable().optional().default(null),
  fast_mode: FastModeSchema.nullable().optional().default(null),
  /** Narrow UI state derived backend-side: 'online' iff this agent's id
   *  appears in the daemon's active-runtime set for this host. Raw
   *  daemon phase strings stay daemon-side; renderer never branches on
   *  them. Always 'offline' on non-`available` arms (no daemon to ask). */
  online: z.enum(['online', 'offline']),
});
export type AgentOnHostDto = z.infer<typeof AgentOnHostDtoSchema>;

/** Dynamic host facts. Present only when `availability === 'available'`.
 *  Every field is read from the live daemon RPC — never persisted to db.
 *  Registered `agents` live at the response arm root, NOT here — the
 *  registration list must survive daemon disconnect. */
export const HostStatusSnapshotSchema = z.object({
  /** Human-readable OS string composed daemon-side (e.g. "macOS 14.5",
   *  "Linux Ubuntu 22.04"). Renderer displays verbatim — does not parse.
   *  `.min(1)` matches the daemon helper's never-empty contract; an empty
   *  string would otherwise reach the renderer as a blank cell. */
  os: z.string().min(1),
  /** Architecture token from `os.arch()` ('arm64' | 'x64' | ...).
   *  Optional so older daemons that don't advertise it still validate. */
  os_arch: z.string().optional(),
  daemon_version: z.string(),
  daemon_pid: z.number().int(),
  started_at: z.number().int(),
  /** Daemon process uptime. */
  uptime_ms: z.number().int(),
  /** Machine facts read live per status RPC. All optional: older daemons
   *  omit them, and a fact the daemon cannot read is omitted, not zeroed. */
  hostname: z.string().optional(),
  /** IPv4 of the default-route interface (not the egress/public IP). */
  ip: z.string().optional(),
  memory_total_bytes: z.number().int().nonnegative().optional(),
  memory_used_bytes: z.number().int().nonnegative().optional(),
  /** System volume (`/`), not the daemon's working directory. */
  disk_total_bytes: z.number().int().nonnegative().optional(),
  disk_used_bytes: z.number().int().nonnegative().optional(),
  /** Machine uptime since boot. */
  system_uptime_ms: z.number().int().nonnegative().optional(),
});
export type HostStatusSnapshot = z.infer<typeof HostStatusSnapshotSchema>;

/**
 * `GET /api/hosts/:id/status` response.
 *
 * Discriminated on `availability` (Cindy `oJ-mzKP7kbXtHUTe` slice-1 review
 * P1 #1): the previous wide-object shape let incoherent combinations like
 * `available + reason` or `disconnected + snapshot` pass parse, which would
 * force the renderer to guess + risk routes accidentally emitting them.
 * Each arm pins exactly which fields are legal for that availability.
 *
 * **Registration vs runtime** (Cindy `Mtf5zZL-Y7PauIDi` slice-3 review P1):
 * `agents[]` is at the **arm root** of every 200 arm, NOT inside
 * `snapshot`. Registration is a DB fact, not a daemon fact — when the
 * daemon disconnects we still know which agents the user registered on
 * this host; we just can't tell which are running. `snapshot` keeps ONLY
 * dynamic daemon-derived fields (OS / daemon_version / pid / uptime). On
 * non-`available` arms every `agents[].online === 'offline'`.
 *
 * - `available`    — snapshot REQUIRED; agents at root with live online
 *                    enrichment; no reason; last_seen_at optional.
 * - `disconnected` — host row OK, no live daemon connection. No snapshot,
 *                    no reason; agents all offline; `last_seen_at` for
 *                    "last seen 5m ago" UX. `admission_failure` present
 *                    only when the last authenticated attempt was refused
 *                    for a terminal reason (#task:1631) — that is the
 *                    "offline, and it will NOT self-heal" signal.
 * - `unreachable`  — connected but RPC failed. No snapshot; typed `reason`
 *                    REQUIRED; agents all offline; `last_seen_at` optional.
 * - `revoked`      — host has `revoked_at` set. No snapshot, no reason;
 *                    agents all offline (registrations stay visible until
 *                    the user deletes the host); `last_seen_at` optional.
 */
const HostStatusAvailableArmSchema = z
  .object({
    host_id: z.string(),
    availability: z.literal('available'),
    snapshot: HostStatusSnapshotSchema,
    agents: z.array(AgentOnHostDtoSchema),
    last_seen_at: z.number().nullable().optional(),
  })
  .strict();

const HostStatusDisconnectedArmSchema = z
  .object({
    host_id: z.string(),
    availability: z.literal('disconnected'),
    agents: z.array(AgentOnHostDtoSchema),
    last_seen_at: z.number().nullable().optional(),
    /** #task:1631 — present only when the last authenticated connection
     *  attempt was refused for a known terminal reason (today: wire
     *  protocol mismatch). Absent = ordinary offline. See
     *  {@link HostAdmissionFailureSchema}. */
    admission_failure: HostAdmissionFailureSchema.optional(),
  })
  .strict();

const HostStatusUnreachableArmSchema = z
  .object({
    host_id: z.string(),
    availability: z.literal('unreachable'),
    reason: HostUnreachableReasonSchema,
    agents: z.array(AgentOnHostDtoSchema),
    last_seen_at: z.number().nullable().optional(),
  })
  .strict();

const HostStatusRevokedArmSchema = z
  .object({
    host_id: z.string(),
    availability: z.literal('revoked'),
    agents: z.array(AgentOnHostDtoSchema),
    last_seen_at: z.number().nullable().optional(),
  })
  .strict();

export const HostStatusResponseSchema = z.discriminatedUnion('availability', [
  HostStatusAvailableArmSchema,
  HostStatusDisconnectedArmSchema,
  HostStatusUnreachableArmSchema,
  HostStatusRevokedArmSchema,
]);
export type HostStatusResponse = z.infer<typeof HostStatusResponseSchema>;

export const HostProxyConfigSchema = z.object({
  proxy_url: z.string().nullable(),
  proxy_runtimes: z.array(RuntimeKindSchema),
});
export type HostProxyConfig = z.infer<typeof HostProxyConfigSchema>;

export const HostProxyConfigResponseSchema = z
  .object({
    config: HostProxyConfigSchema,
  })
  .strict();
export type HostProxyConfigResponse = z.infer<typeof HostProxyConfigResponseSchema>;

export const HOST_PROXY_CONFIG_OFFLINE_ERROR_CODE = 'computer_offline';

export const HostProxyConfigOfflineResponseSchema = z
  .object({
    error: z.string(),
    error_code: z.literal(HOST_PROXY_CONFIG_OFFLINE_ERROR_CODE),
  })
  .strict();
export type HostProxyConfigOfflineResponse = z.infer<typeof HostProxyConfigOfflineResponseSchema>;

export const HostClaimOrCreateInputSchema = z
  .object({
    installation_id: z.string().uuid(),
    /** Claim-time display label — the same column as
     *  `UpdateHostInputSchema.display_label`, so it shares the same bound.
     *  Callers that derive this from a system value (the desktop main
     *  process defaults it to the OS hostname) must fit it themselves;
     *  claiming a computer must not fail because the machine name is long. */
    label: z.string().trim().min(1).max(HOST_DISPLAY_LABEL_MAX_LENGTH).optional(),
    current_host_id: z.string().min(1).optional(),
  })
  .strict();
export type HostClaimOrCreateInput = z.infer<typeof HostClaimOrCreateInputSchema>;

export const HostClaimOrCreateResponseSchema = z.object({
  host_id: z.string(),
  host: HostDtoSchema,
  host_token: z.string().min(1).optional(),
});
export type HostClaimOrCreateResponse = z.infer<typeof HostClaimOrCreateResponseSchema>;

/** Owner-scoped list response from `GET /api/hosts`. The route filters
 *  to the current viewer's owned hosts only. */
export const HostListResponseSchema = z.object({
  hosts: z.array(HostDtoSchema),
});
export type HostListResponse = z.infer<typeof HostListResponseSchema>;

/** `POST /api/hosts` user-managed computer create. Naming happens after the
 *  first successful daemon connect; creation only mints the reveal-once
 *  connect credential + launch recipe and leaves the host pending. The
 *  connect command is assembled at the edge (desktop) via shared
 *  `assembleConnectCommand` — the server never mints a command string, so it
 *  cannot leak its own bind address as the daemon's reach URL. See
 *  `docs/orchestration-boundary-v1.md §4.3`. */
export const HostCreateInputSchema = z.object({}).strict();
export type HostCreateInput = z.infer<typeof HostCreateInputSchema>;

export const HostCreateResponseSchema = z.object({
  host_id: z.string(),
  host: HostDtoSchema,
  connect_token: z.string().min(1),
  /** Server-owned launch recipe (npm spec) the desktop passes to
   *  `assembleConnectCommand`. Per-deploy configurable via
   *  `SILVERHAND_DAEMON_NPM_SPEC`. */
  daemon_npm_spec: z.string().min(1),
});
export type HostCreateResponse = z.infer<typeof HostCreateResponseSchema>;

/** `GET /api/hosts/connect-token/resolve` daemon bootstrap. The copied command
 *  carries only a connect token; the daemon resolves this id at runtime and
 *  still uses the same token as the websocket bearer credential. */
export const HostConnectTokenResolveResponseSchema = z.object({
  host_id: z.string().min(1),
});
export type HostConnectTokenResolveResponse = z.infer<typeof HostConnectTokenResolveResponseSchema>;

/** `GET /api/hosts/{id}` owner-scoped host read. Unlike the bulk list, this
 *  may return a modal-scoped setup draft so the Add computer flow can observe
 *  `pending → attached → active` without exposing drafts in the sidebar. */
export const HostGetResponseSchema = z.object({
  host: HostDtoSchema,
});
export type HostGetResponse = z.infer<typeof HostGetResponseSchema>;

/** `DELETE /api/hosts/{id}` user-managed computer delete. The same route covers
 *  unfinalized Add-computer drafts, finalized empty computers, and — after an
 *  explicit two-phase confirmation — a computer whose agents are cascade-deleted
 *  with it. relocation-v1 §7-4: a `regular` workspace is not rooted on a
 *  computer, so nothing workspace-shaped is in the cascade. The row is never
 *  soft-revoked by this user-facing delete action. */
export const HostCancelPendingResponseSchema = z.object({
  host_id: z.string(),
  deleted: z.literal(true),
});
export type HostCancelPendingResponse = z.infer<typeof HostCancelPendingResponseSchema>;
export const HostDeleteResponseSchema = HostCancelPendingResponseSchema;
export type HostDeleteResponse = HostCancelPendingResponse;

/**
 * Delete-computer cascade confirmation.
 *
 * ONE boolean, deliberately: the user is confirming "delete this computer and
 * everything on it", not a per-object consent list. Everything the cascade can
 * reach is rooted on the computer the user just decided to destroy, so a
 * dependency that appears between the dialog and the request cannot change the
 * CLASS of what is deleted — and an exact-id snapshot would have bought only
 * that, at the price of a second consent model (set equality, a
 * `dependents_changed` refusal, a re-confirmation screen).
 *
 * The flag is a guard, not an authorization: the server re-reads the current
 * dependency set and the pending-materialization state inside its delete
 * transaction, and refuses there.
 *
 * Discovery is not this route's job: the dialog reads the impact from
 * `GET /api/hosts/{id}/resources` first and shows it, then sends this flag once.
 */
export const HostDeleteInputSchema = z
  .object({
    cascade_dependents: z.boolean().optional().default(false),
  })
  .optional()
  .default({});
export type HostDeleteInput = z.infer<typeof HostDeleteInputSchema>;

export const HostDeleteDependentsSchema = z.object({
  agents: z.number().int().nonnegative(),
});
export type HostDeleteDependents = z.infer<typeof HostDeleteDependentsSchema>;

/** One agent rooted on a computer. Named for the resource, not for the action:
 *  the same shape describes "the agents on this computer" wherever it is asked,
 *  and the delete refusal below is only one consumer.
 *
 *  Plain facts only. It deliberately does NOT carry
 *  `materialization_status`: a read model lists the affected objects, it does not
 *  predict whether a later command will succeed. A materialization racing this
 *  delete is rare and already answered by the DELETE's own
 *  `has_pending_agents`, so mirroring it here would only buy a UI state that
 *  almost never renders. */
export const HostAgentResourceSchema = z.object({
  id: z.string(),
  name: z.string(),
  display_name: z.string(),
  runtime: RuntimeKindSchema,
});
export type HostAgentResource = z.infer<typeof HostAgentResourceSchema>;

/**
 * `GET /api/hosts/{id}/resources` — everything persistently rooted on one
 * computer: the agents placed on it. relocation-v1 §7-4 removed the workspace
 * dimension: a `regular` workspace has no home computer and keeps no files on
 * one (each member agent's bundle lives on that agent's OWN computer, at a
 * deterministic agent-centric coordinate), so no workspace can be this
 * computer's resource.
 *
 * A general Computer-domain query, not a delete-flavoured read model. It answers
 * "what belongs to this computer" and carries no action fields; deciding what
 * that means (deletion impact, a move, a diagnostic) is the caller's job.
 * Distinct from `GET /api/hosts/{id}/status`, which reports the DYNAMIC
 * availability of the daemon and its live runtimes — availability vs ownership.
 */
export const HostResourcesResponseSchema = z.object({
  agents: z.array(HostAgentResourceSchema),
});
export type HostResourcesResponse = z.infer<typeof HostResourcesResponseSchema>;

/**
 * `409 has_dependents` — a DELETE arrived WITHOUT `cascade_dependents` while the
 * computer still has agents. This is a guard for callers that skipped the
 * confirmation step (the dialog reads the list itself), not the UI's discovery
 * path; it still names the objects so a CLI/API caller learns what it would
 * destroy. Guarantees zero deletion.
 */
export const HostDeleteBlockedResponseSchema = z.object({
  error: z.string(),
  error_code: z.literal('has_dependents'),
  dependents: HostDeleteDependentsSchema,
  agents: z.array(HostAgentResourceSchema).optional().default([]),
});
export type HostDeleteBlockedResponse = z.infer<typeof HostDeleteBlockedResponseSchema>;

export const HostDeletePendingAgentsResponseSchema = z.object({
  error: z.string(),
  error_code: z.literal('has_pending_agents'),
  pending_agents: z.number().int().positive(),
});
export type HostDeletePendingAgentsResponse = z.infer<typeof HostDeletePendingAgentsResponseSchema>;

/** `POST /api/hosts/{id}/revoke` response. Returns the updated DTO so
 *  the caller can refresh local UI without a follow-up GET. */
export const HostRevokeResponseSchema = z.object({
  host: HostDtoSchema,
});
export type HostRevokeResponse = z.infer<typeof HostRevokeResponseSchema>;

/** Embedded `POST /api/hosts/{id}/rotate-token` response.
 *
 *  IMPORTANT: this shape is consumed by Electron main only —
 *  `host_token` is cleartext and must be written to `daemon-config.json`
 *  before any return value crosses the IPC boundary. The renderer
 *  never sees this schema; the renderer-facing IPC return strips
 *  the token and only echoes `{ ok, host_id, status }`. */
export const HostRotateTokenEmbeddedResponseSchema = z.object({
  host_id: z.string(),
  host: HostDtoSchema,
  host_token: z.string().min(1),
});

/** Standalone D2 outbound rotation response.
 *
 *  The cleartext replacement token is delivered only over the authenticated
 *  host WebSocket `rotate_token` command; the HTTP caller only learns that the
 *  rotation completed. */
export const HostRotateTokenStandaloneResponseSchema = z.object({
  host_id: z.string(),
  host: HostDtoSchema,
  status: z.literal('rotated'),
});

/** User-managed connect-token rotation response. Cleartext is reveal-once and
 *  is intentionally returned to the renderer for the command card. */
export const HostRotateTokenUserManagedResponseSchema = z.object({
  host_id: z.string(),
  host: HostDtoSchema,
  status: z.literal('rotated'),
  connect_token: z.string().min(1),
  /** Server-owned launch recipe (npm spec); see `HostCreateResponseSchema`. */
  daemon_npm_spec: z.string().min(1),
});

/** `POST /api/hosts/{id}/disconnect` user-managed connection kick. This only
 *  affects the live availability axis: credentials and lifecycle fields are not
 *  changed, so the same connection command can reconnect later. */
export const HostDisconnectResponseSchema = z.object({
  host_id: z.string(),
  host: HostDtoSchema,
  disconnected: z.boolean(),
});
export type HostDisconnectResponse = z.infer<typeof HostDisconnectResponseSchema>;

export const HostRotateTokenResponseSchema = z.union([
  HostRotateTokenEmbeddedResponseSchema,
  HostRotateTokenStandaloneResponseSchema,
  HostRotateTokenUserManagedResponseSchema,
]);
export type HostRotateTokenResponse = z.infer<typeof HostRotateTokenResponseSchema>;
