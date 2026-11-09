// AUTO-SPLIT from the former monolithic api/dto.ts (#task:963).
// Bounded context: agent. Re-exported via api/index.ts → @silverhand/shared.
import { z } from 'zod';
import {
  AgentSessionScopeSchema,
  AgentMaterializationStatusSchema,
  ConversationAgentStatusSchema,
  ConversationPlacementKindSchema,
  ConversationRosterPolicySchema,
  FastModeSchema,
  ReasoningEffortTokenSchema,
  RuntimeKindSchema,
} from '../db/schema.js';
import { DisplayNameSchema } from './user.js';
import { ParticipantNameSchema } from './name.js';
import { reasoningEffortValidForRuntime, runtimeSupportsFastMode } from '../runtime-catalog.js';

export const AgentDtoSchema = z.object({
  id: z.string(),
  name: z.string(),
  display_name: z.string(),
  runtime: RuntimeKindSchema,
  model: z.string().nullable(),
  reasoning_effort: ReasoningEffortTokenSchema.nullable(),
  fast_mode: FastModeSchema.nullable(),
  host_id: z.string(),
  // Server-resolved computer name (`hosts.display_label ?? hosts.name`), null
  // only when the host row is gone. Denormalized here because `GET /api/hosts`
  // is owner-scoped, so a non-owner viewer has no other face to resolve it. As
  // a denormalized read face it REQUIRES an invalidation edge: a host rename
  // fires `agent_changed` for every agent on that host, or this goes stale (see
  // the `last_activity_at` note below for the rule this satisfies).
  host_label: z.string().nullable(),
  description: z.string().nullable(),
  /** Immutable runtime-session identity selected when the Agent is created. */
  session_scope: AgentSessionScopeSchema,
  /** v26: id of the user who created this agent. The renderer uses it
   *  to decide whether to surface profile-edit affordances (Edit
   *  button, avatar upload). Backend `canEditAgent` is the source of
   *  truth; this field is the renderer's hint. */
  created_by_user_id: z.string(),
  /** v26: resolved avatar URL or null. Matches `UserDto.avatar_url` —
   *  cache-busted by `updated_at`. PR-1 always returns null (no
   *  storage wired); PR-2 flips this on once the storage / route
   *  pair lands. */
  avatar_url: z.string().nullable(),
  /** v33 (D4.d): agent-home materialization lifecycle. Required
   *  because every v33 row has a non-null status — default-list
   *  callers see `'active'`; the failed-row management view
   *  (Q6.a, §7.3) opts in via `?include_failed=1` and reads
   *  `'failed'` here together with the two diagnostic fields
   *  below. `'pending_materialization'` is reserved for the
   *  in-flight window between row reserve and L3a/L3b — readers
   *  see it only when the materialize wire round-trip is still
   *  pending. Note: `idempotency_key` is intentionally NOT
   *  exposed on the default DTO — same-key retry uses a
   *  route-local management payload (Cindy D1 round-2 P2). */
  materialization_status: AgentMaterializationStatusSchema,
  /** v33 (D4.d): populated only when
   *  `materialization_status='failed'`. Mirrors the §3.4 wire→HTTP
   *  error-code mapping plus server-side codes. Kept as
   *  `string | null` (not a narrow enum) so the row column / DTO
   *  / migration do not need a coordinated schema change when a
   *  new server-side code is added — the route layer pins the
   *  exact code set it produces. */
  materialization_failed_code: z.string().nullable(),
  /** v33 (D4.d): epoch ms set together with
   *  `materialization_failed_code`; NULL while the row is in
   *  `pending_materialization` or `active`. */
  materialization_failed_at: z.number().nullable(),
  /** Durable terminal-deletion intent. Non-null means host cleanup/DB commit
   *  is pending or retrying; management readers must not present it as active. */
  deletion_started_at: z.number().nullable(),
  // #task:2049 — `last_activity_at` is DELIBERATELY NOT on this DTO. The
  // presence clock has exactly ONE wire read face: `GET /api/agents/:id/presence`
  // (`away_at`), invalidated by `agent_presence_changed`. `stampLastActivity`
  // intentionally does NOT bump `updated_at` (avoids the IDENTITY_CARRIERS
  // `agent_changed` ripple), so `agent_changed` never fires for it — putting the
  // raw stamp here would be a second read face with NO invalidation edge (a
  // client would cache a forever-stale value). Same discipline that kept
  // `away_at` off the availability DTO. Do not re-add; presence readers hit
  // `/presence`.
  created_at: z.number(),
  updated_at: z.number(),
});
export type AgentDto = z.infer<typeof AgentDtoSchema>;

/** The public tier of `GET /api/agents/{id}`: what any signed-in human on the
 *  server may read. Restricted fields are absent, not null. */
export const AgentPublicDtoSchema = AgentDtoSchema.pick({
  id: true,
  name: true,
  display_name: true,
  runtime: true,
  model: true,
  reasoning_effort: true,
  fast_mode: true,
  host_id: true,
  host_label: true,
  description: true,
  created_by_user_id: true,
  avatar_url: true,
  created_at: true,
});
export type AgentPublicDto = z.infer<typeof AgentPublicDtoSchema>;

/** `GET /api/agents/{id}`. `view` names the tier; a body without it is the
 *  privileged shape older backends returned. */
export const AgentPrivilegedGetResponseSchema = z.object({
  view: z.literal('privileged').default('privileged'),
  agent: AgentDtoSchema,
});
export const AgentPublicGetResponseSchema = z.object({
  view: z.literal('public'),
  agent: AgentPublicDtoSchema,
});
export const AgentGetResponseSchema = z.union([
  AgentPrivilegedGetResponseSchema,
  AgentPublicGetResponseSchema,
]);
export type AgentGetResponse = z.infer<typeof AgentGetResponseSchema>;

export const CreateAgentInputSchema = z
  .object({
    name: ParticipantNameSchema,
    runtime: RuntimeKindSchema,
    model: z.string().optional(),
    reasoning_effort: ReasoningEffortTokenSchema.optional(),
    fast_mode: FastModeSchema.optional(),
    host_id: z.string().optional(), // defaults to the current user's owned local host
    description: z.string().optional(),
    /** Defaults to the established one-runtime-per-conversation model. */
    session_scope: AgentSessionScopeSchema.optional(),
  })
  // reasoning_effort is a bounded opaque token (see ReasoningEffortTokenSchema):
  // the schema accepts ANY structurally-valid token, and the SOLE legality gate
  // is per-runtime/model via `reasoningEffortValidForRuntime`. So a token like
  // Qoder's `none` parses structurally but is simply not in Codex's catalog set,
  // while Codex `max`/`ultra` are legal only on a verified 5.6 model. All fields
  // are present in the create payload, so the authoritative gate closes here.
  .superRefine((val, ctx) => {
    if (!reasoningEffortValidForRuntime(val.runtime, val.reasoning_effort, val.model)) {
      ctx.addIssue({
        code: z.ZodIssueCode.custom,
        path: ['reasoning_effort'],
        message: `reasoning_effort '${val.reasoning_effort}' is not valid for runtime '${val.runtime}' and model '${val.model ?? 'driver default'}'`,
      });
    }
    if (!runtimeSupportsFastMode(val.runtime) && val.fast_mode !== undefined) {
      ctx.addIssue({
        code: z.ZodIssueCode.custom,
        path: ['fast_mode'],
        message: `fast_mode is not supported by runtime '${val.runtime}'`,
      });
    }
  });
export type CreateAgentInput = z.infer<typeof CreateAgentInputSchema>;

// PATCH /api/agents/:id — every field optional, only provided keys
// are written. `name`, `host_id`, and `runtime` are deliberately excluded:
// `name` is the immutable identifier (see migrations.ts v5 comment),
// `host_id` swap is out of v1 scope, and runtime changes require the dedicated
// transition command rather than an ordinary metadata patch.
//
// Unlike create/switch, this schema has NO reasoning superRefine: PATCH is
// partial, so the effective (runtime, model, token) tuple isn't fully known
// here (runtime is fixed, model may be absent). `reasoning_effort` is only
// bounded structurally by the token schema; its per-runtime/model legality is
// enforced downstream at the service boundary (`normalizeRuntimeTuning`), which
// resolves the token against the agent's current runtime + effective model.
export const UpdateAgentInputSchema = z
  .object({
    display_name: DisplayNameSchema.optional(),
    description: z.string().max(2000).nullable().optional(),
    runtime: z.never().optional(),
    model: z.string().nullable().optional(),
    reasoning_effort: ReasoningEffortTokenSchema.nullable().optional(),
    fast_mode: FastModeSchema.nullable().optional(),
  })
  .refine((v) => Object.keys(v).length > 0, { message: 'no fields to update' });
export type UpdateAgentInput = z.infer<typeof UpdateAgentInputSchema>;

/**
 * Dedicated runtime transition command. Every tuning key is required so a
 * missing field cannot silently select a default. `null` is the caller's
 * explicit "use the target driver's default" choice.
 */
export const SwitchAgentRuntimeInputSchema = z
  .object({
    runtime: RuntimeKindSchema,
    model: z.string().trim().min(1).nullable(),
    reasoning_effort: ReasoningEffortTokenSchema.nullable(),
    fast_mode: FastModeSchema.nullable(),
  })
  .strict()
  .superRefine((val, ctx) => {
    if (!reasoningEffortValidForRuntime(val.runtime, val.reasoning_effort, val.model)) {
      ctx.addIssue({
        code: z.ZodIssueCode.custom,
        path: ['reasoning_effort'],
        message: `reasoning_effort '${val.reasoning_effort}' is not valid for runtime '${val.runtime}' and model '${val.model ?? 'driver default'}'`,
      });
    }
    if (!runtimeSupportsFastMode(val.runtime) && val.fast_mode !== null) {
      ctx.addIssue({
        code: z.ZodIssueCode.custom,
        path: ['fast_mode'],
        message: `fast_mode is not supported by runtime '${val.runtime}'`,
      });
    }
  });
export type SwitchAgentRuntimeInput = z.infer<typeof SwitchAgentRuntimeInputSchema>;

export const SwitchAgentRuntimeResponseSchema = z.object({
  agent: AgentDtoSchema,
  cleared_sessions: z.number().int().nonnegative(),
  restarted_conversations: z.number().int().nonnegative(),
  restart_failures: z.number().int().nonnegative(),
});
export type SwitchAgentRuntimeResponse = z.infer<typeof SwitchAgentRuntimeResponseSchema>;

// ── Self-profile (`silverhand profile get|update`, #task:1178) ─────────
//
// The executing subject's view of its OWN profile metadata. This is a
// SUBSET of AgentDto (identity + the editable text fields + the derived
// avatar_url) — deliberately not the full row, because `profile get` is
// "what can I see/change about myself", not the agent-admin DTO.
//
// `avatar_url` is the SAME derived read field as AgentDto: always our
// internal `/api/agents/:id/avatar?v=<updated_at>` route, never a foreign
// URL. The write side stores a blob; this read side only ever points at
// that blob (or null). See the avatar service for the storage truth.
export const AgentProfileDtoSchema = z.object({
  id: z.string(),
  /** Immutable mention handle — shown for context, not editable here. */
  name: z.string(),
  display_name: z.string(),
  description: z.string().nullable(),
  avatar_url: z.string().nullable(),
});
export type AgentProfileDto = z.infer<typeof AgentProfileDtoSchema>;

// Text fields of `silverhand profile update`. Avatar is intentionally
// ABSENT — bytes ride the multipart avatar route, never base64 in JSON.
// Mirrors the display_name / description subset of UpdateAgentInputSchema
// so the two write paths share one validation contract (single source of
// truth for the field rules).
export const UpdateProfileInputSchema = z
  .object({
    display_name: DisplayNameSchema.optional(),
    description: z.string().max(2000).nullable().optional(),
  })
  .refine((v) => Object.keys(v).length > 0, { message: 'no fields to update' });
export type UpdateProfileInput = z.infer<typeof UpdateProfileInputSchema>;

/**
 * `agent_changed` — WS event on the **`app`** scope, published by the
 * backend on any identity write that changes what the agent directory
 * shows — UPDATE, CREATION, and DELETION:
 *   - update: human `PATCH /api/agents/:id`, human avatar upload/delete,
 *     the agent's own `silverhand profile update` (display_name /
 *     description / avatar), and a completed runtime transition — the
 *     callers of `AgentService.publishChanged`.
 *   - create: a genuine agent provisioning, published once as a
 *     post-commit effect of `createAgent`'s `created` result (after the
 *     dependent-row commit). An idempotent echo or a failed materialization
 *     builds no such effect, so it publishes nothing.
 *   - delete: a hard delete's terminal identity change, published as a
 *     post-commit effect of `deleteAgent` / the `deleteComputer` cascade
 *     (`AgentService.publishDeleted`) once the `agents` row is gone. A
 *     refused/rolled-back delete publishes nothing, so clients are never told
 *     "deleted" for an agent that still exists. On this signal the historical
 *     message/thread/saved/search/activity author carriers refetch and resolve
 *     the author to its server-authoritative `deleted` snapshot (#task:1791)
 *     instead of collapsing to a directory miss; a direct AgentDto refetch 404s
 *     and the roster simply drops the id.
 * `user_changed` has the same create+update producer boundary (#task:1707).
 *
 * Clients treat it as an invalidation signal and reseat every cache that
 * denormalizes an agent's identity — the agent-kind entries of the
 * renderer's carrier registry (`features/identity/identity-carrier-caches`),
 * which is `['agents']` plus DM favorites and the host status / resources
 * trees, and the both-kind entries shared with `user_changed` (rosters,
 * workspace members, Activity, reaction detail, the task trees). #task:1704
 * replaced the earlier `['agents']`-prefix-only consumer. Avatars converge
 * on their own because a refetched DTO carries a bumped `avatar_url` `?v=`
 * cache-buster.
 *
 * Payload is intentionally minimal — just the id, no private data — so it
 * is safe on the global `app` scope (mirrors `user_presence`). The backend
 * is the single source of refresh; renderer mutation `onSuccess` may keep
 * a local fast-path but is no longer the sole propagation mechanism. The
 * kind string is `'agent_changed'`. (#task:1178)
 */
export const AgentChangedEventSchema = z.object({
  agent_id: z.string(),
});
export type AgentChangedEvent = z.infer<typeof AgentChangedEventSchema>;

// GET /api/agents/:id/workspace/tree + GET /api/agents/:id/workspace/file —
// see "agent home workspace files (tree + read pair)" below for the wire
// contract that backs the two-column workspace pane in the renderer.

// Pair of read-only protocols that together back the two-column workspace
// pane. The renderer pulls a flat tree of every file under the agent's
// home directory ("agent home workspace files" — the agent's L1 dir, whose
// host-local coordinate only the daemon resolves), then reads any
// path the user clicks. This intentionally does NOT segment by
// `system_prompt` / `MEMORY` / `notes/`: the protocol speaks one virtual
// path namespace rooted at the agent home dir, and the UI categorizes
// (or doesn't) however it wants.
//
// Forward-compat: a future expansion that exposes a different file root
// (e.g. an L2 workspace dir) will introduce ADDITIVE selector params
// (`scope`, `workspace_id`) — it MUST NOT reinterpret the current path
// semantics: today `path: "MEMORY.md"` is unambiguously the agent-home
// `MEMORY.md`, and that meaning is frozen.
//
// Security (enforced daemon-side, mirrored in the route): every input
// path is parsed segment-by-segment, dot/dot-dot/dotfile segments are
// rejected, every step is `lstat`-ed to refuse symlinks (intermediate
// or leaf), and the realpath of the resolved target must stay strictly
// inside the realpath of the agent home base — a missing or out-of-base
// target is `not_found`, never a leak.

/**
 * One entry in the flat tree response. `path` is a `/`-joined virtual
 * path relative to the agent home (e.g. `system_prompt.md`,
 * `notes/work-log.md`); empty / leading-slash / `..` / dotfile
 * segments are rejected upstream so any value here is safe to feed
 * back into the read endpoint verbatim.
 */
export const AgentWorkspaceTreeNodeSchema = z
  .object({
    path: z.string().min(1),
    kind: z.enum(['file', 'directory']),
    size: z.number(),
    mtime: z.number(),
  })
  .strict();
export type AgentWorkspaceTreeNode = z.infer<typeof AgentWorkspaceTreeNodeSchema>;

/**
 * Flat tree shape — every visible file/directory under the agent home,
 * ordered directories-first then alphabetical within a level. The
 * renderer is responsible for reconstructing the parent/child layout
 * via the `path` segments. Hidden entries (anything starting with `.`)
 * are filtered server-side and never appear here.
 */
export const AgentWorkspaceTreeSchema = z
  .object({
    nodes: z.array(AgentWorkspaceTreeNodeSchema),
  })
  .strict();
export type AgentWorkspaceTree = z.infer<typeof AgentWorkspaceTreeSchema>;

/**
 * Single-file read result. `truncated` flips when the file exceeds the
 * daemon's preview cap (`MAX_PREVIEW_BYTES`); the renderer just labels the
 * preview accordingly.
 * `path` is the canonical virtual path the caller asked for, echoed for
 * cache-key stability.
 */
export const AgentWorkspaceFileReadSchema = z
  .object({
    path: z.string().min(1),
    content: z.string(),
    size: z.number(),
    mtime: z.number(),
    truncated: z.boolean(),
  })
  .strict();
export type AgentWorkspaceFileRead = z.infer<typeof AgentWorkspaceFileReadSchema>;

// One placement-aware participation projection for the Agent detail surface.
// It carries display/navigation coordinates only; runtime ownership and process
// state are represented independently by `AgentRuntimeSeatDtoSchema` below.
//
// It carried a `workspace_kind` until #task:2069, and the removal is a MODELLING
// fix rather than a dead-field cleanup. Its only reader asked a CONVERSATION-level
// question ("draw this row as a private line or a channel?") but keyed part of the
// answer on the CONTAINER's word — the same mis-keying the retired `channelKind`
// prompt field was, mirrored. The keying rule (stated once on
// `ConversationPlacementKindSchema`, db/schema.ts) settles it: identity is a
// function of PLACEMENT, and neither the container word nor `roster_policy` is a
// sufficient stand-in for it. In particular `fixed` does NOT mean "private line":
// an agent home's DM AND Onboarding channels are workspace-placed with a fixed
// roster — fixed CHANNELS, not private lines. `direct_pair` vs a workspace
// placement is the real cut, and `conversation_placement_kind` carries exactly it;
// that is why THIS face — which genuinely admits `direct_pair` —
// keeps the placement + roster axes and drops the container word.
//
// The word was also redundant on top of being mis-keyed: within an owned workspace
// every workspace-placed row is fixed-roster — the only open-roster creation path
// rejects a workspace with an owner agent (`channel-service.ts`
// `isAgentHomeWorkspace(workspace)` pre-check, `isAgentHomeWorkspace(current)`
// in-lock recheck; `create-workspace.ts` `workspaces.insert` builds only a no-owner
// workspace, never an agent home) and ownership never changes after creation, so
// `agent_home ⇒ fixed` holds. That reachability fact is the half kept from
// #task:2069; the "fixed ⇒ private line" sufficiency claim is the half corrected
// here (#task:2074).
export const AgentRuntimeParticipationSchema = z.object({
  conversation_id: z.string(),
  workspace_id: z.string().nullable(),
  workspace_name: z.string().nullable(),
  conversation_placement_kind: ConversationPlacementKindSchema,
  conversation_roster_policy: ConversationRosterPolicySchema,
  channel_id: z.string(),
  channel_title: z.string(),
  closed_at: z.number().nullable(),
});
export type AgentRuntimeParticipation = z.infer<typeof AgentRuntimeParticipationSchema>;

export const AgentRuntimeSeatScopeSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('agent') }),
  z.object({ kind: z.literal('conversation'), conversation_id: z.string() }),
]);
export type AgentRuntimeSeatScope = z.infer<typeof AgentRuntimeSeatScopeSchema>;

export const AgentRuntimeFocusSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('conversation'), conversation_id: z.string() }),
  z.object({ kind: z.literal('other') }),
]);
export type AgentRuntimeFocus = z.infer<typeof AgentRuntimeFocusSchema>;

/** One semantic runtime identity. Agent scope remains one row no matter how
 * many conversations it serves; `focus` is a viewer-redacted presentation
 * coordinate, never part of ownership. */
export const AgentRuntimeSeatDtoSchema = z.object({
  scope: AgentRuntimeSeatScopeSchema,
  generation: z.number().int().nonnegative(),
  status: ConversationAgentStatusSchema,
  pid: z.number().nullable(),
  cli_session_id: z.string().nullable(),
  focus: AgentRuntimeFocusSchema.nullable(),
  queue_depth: z.number().int().nonnegative(),
});
export type AgentRuntimeSeatDto = z.infer<typeof AgentRuntimeSeatDtoSchema>;

// GET /api/agents/:id/runtime-state deliberately returns two orthogonal read
// models. `participations`/`visible_participations` answer where the agent is a
// participant; `seats` answer which semantic runtimes exist. Neither is derived
// from the other, so an absent slot cannot erase membership and AgentSlot cannot
// fan out into one fake process row per conversation.
export const AgentRuntimeStateDtoSchema = z.object({
  participations: z.array(AgentRuntimeParticipationSchema),
  visible_participations: z.array(AgentRuntimeParticipationSchema),
  seats: z.array(AgentRuntimeSeatDtoSchema),
});
export type AgentRuntimeStateDto = z.infer<typeof AgentRuntimeStateDtoSchema>;

// GET /api/agents/:id/availability (#task:2043) — the ONE reachability bit a
// viewer authorized on the agent may read. Read it against the concept it is
// NOT: process-aliveness — "does a live process exist right now?" — which
// #task:2049 retired as a presence input and #task:2056 deleted as a read face
// outright (no per-agent liveness endpoint exists; the surviving
// `host_agent_liveness_changed` frame is a HOST-level, owner-scoped
// invalidation, not a viewer read face). Availability projects whether the
// agent's host is *reachable* right now (an authenticated outbound attachment
// is connected and the host is not revoked), which is a total function of
// transport + agent validity — independent of any runtime snapshot, hydration,
// or active-agent set. An idle-reaped or not-yet-hydrated agent on a connected
// host is AVAILABLE (the next inbound message re-spawns it), which is why
// availability must not be derived from the active-runtime snapshot at all.
//
// Deliberately TWO values, not three. The source is a total in-process fact:
// the attachment is either present or absent; "the authority could not read"
// is an infrastructure FAULT (HTTP 5xx), never a successful `{availability}`.
// Folding a fault into a domain value would license callers to disguise a
// backend failure as a normal answer. "We cannot tell right now" is a RENDERER
// query-projection state (pending / error / cached-stale ⇒ its own `unknown`),
// not a backend value. The UI composes THIS axis with the presence deadline
// (`away_at`, the #task:2049 activity axis — NOT liveness, which #task:2049
// dropped from the projection) into the user-facing Online / Away / Offline /
// no-status projection.
export const AgentAvailabilitySchema = z.enum(['online', 'offline']);
export type AgentAvailability = z.infer<typeof AgentAvailabilitySchema>;

export const AgentAvailabilityDtoSchema = z.object({
  availability: AgentAvailabilitySchema,
});
export type AgentAvailabilityDto = z.infer<typeof AgentAvailabilityDtoSchema>;

// GET /api/agents/:id/presence (#task:2049). The activity-deadline axis, kept
// SEPARATE from availability on purpose: availability's invalidation contract
// (below) explicitly excludes runtime-liveness edges, so folding `away_at` onto
// that DTO would re-weld the two axes #task:2043 split. `away_at` is the
// absolute epoch-ms deadline at which a currently-available agent projects to
// Away: last_activity_at + PRESENCE_AWAY_TTL_MS, or null when never active (no
// fabricated deadline — see below, null projects to Away, NOT Online). The
// renderer projects presence = availability × away_at, and Online requires
// POSITIVE evidence of a live window (an away_at number still in the future):
//   offline                                    ⇒ Offline
//   online + away_at is number ∧ now <  away_at ⇒ Online
//   online + away_at is number ∧ now >= away_at ⇒ Away
//   online + away_at === null                  ⇒ Away   (never active; v82 has
//       no backfill, so this is the DOMINANT post-deploy state — Online here
//       would be a deploy-day mass lie)
//   online + away_at unknown (fetch pending/errored) ⇒ Away
// This read face is invalidated ONLY by
// `agent_presence_changed`, NEVER by `agent_availability_changed`.
//
// `server_now` is the backend clock at response time. Because `away_at` is an
// ABSOLUTE server-epoch deadline, a renderer comparing it against its own wall
// clock would misproject under client/server skew; the consumer calibrates at
// the read boundary (offset = dataUpdatedAt − server_now; away_at_client =
// away_at + offset). The backend's whole skew responsibility is to EMIT
// `server_now` alongside `away_at` — no calibration logic lives here.
export const AgentPresenceDtoSchema = z.object({
  away_at: z.number().nullable(),
  server_now: z.number(),
});
export type AgentPresenceDto = z.infer<typeof AgentPresenceDtoSchema>;

// Invalidation event for GET /api/agents/:id/availability (#task:2043). A pure
// CACHE-INVALIDATION frame, NOT a state patch: it carries only the agent id, so
// a subscriber's sole reaction is to refetch `/availability` for that id — the
// route stays the single source of the value. This mirrors `agent_changed`
// (identity) and `direct_pairs_changed` (list): the wire never ships the new
// value, only the id whose derived read is now stale.
//
// Fan-out is per-VIEWER, not global: unlike `agent_changed` (safe on `app`
// because the id alone leaks nothing), availability is a `canViewAgent`-gated
// read, so the backend publishes to `viewer:{userId}` for exactly the users who
// can currently see the agent (creator ∪ DirectPair human endpoints ∪
// shared-workspace members, each filtered through `canViewAgent`). The renderer
// composes the refetched availability with the presence deadline (`away_at`;
// #task:2049 dropped the liveness axis from the projection) into Online / Away /
// Offline / no-status. Emitted only on transport-reachability transitions
// (attachment connect / disconnect, host revoke) — never on runtime-liveness
// changes, which drove the retired process axis, not this one.
export const AGENT_AVAILABILITY_CHANGED_EVENT = 'agent_availability_changed';
export const AgentAvailabilityChangedEventSchema = z
  .object({
    agent_id: z.string(),
  })
  .strict();
export type AgentAvailabilityChangedEvent = z.infer<typeof AgentAvailabilityChangedEventSchema>;

// Invalidation event for GET /api/agents/:id/presence (#task:2049). A pure,
// id-only CACHE-INVALIDATION frame — same discipline as
// `agent_availability_changed` above: the wire never ships the new value, only
// the agent id whose derived `/presence` read is now stale, so a subscriber's
// SOLE reaction is to refetch `/presence` for that id.
//
// Fires on an Away→Online transition, judged at the presence-clock write edge,
// in TWO cases: (a) an agent already past its away deadline (a currently-Away
// projection) producing real runtime activity again; and (b) a never-active
// agent (prev null — which the projection renders Away, NOT Online) producing
// its first-ever activity. Both are real Away→Online crossings a
// cross-conversation viewer cannot observe on its own — a viewer not on the
// active conversation has no other signal that the silent (or never-yet-seen)
// agent came alive — so both are worth a per-viewer push. With v82's
// no-backfill, case (b) is the DOMINANT post-deploy transition, not an edge
// case. No frame is emitted while the agent stays within its window (continuous
// activity inside the deadline).
//
// Fan-out is per-VIEWER via the SAME `agentViewerUserIds` set availability uses
// (creator ∪ DirectPair human endpoints ∪ shared-workspace members, each
// through `canViewAgent`): publish to `viewer:{userId}` for exactly the users
// who can currently see the agent.
//
// This must NOT reuse `agent_availability_changed`: that event's contract
// explicitly FORBIDS runtime-liveness triggers (it fires only on
// transport-reachability transitions), whereas this event's whole purpose is a
// runtime-activity edge. They are distinct axes with distinct read faces.
export const AGENT_PRESENCE_CHANGED_EVENT = 'agent_presence_changed';
export const AgentPresenceChangedEventSchema = z
  .object({
    agent_id: z.string(),
  })
  .strict();
export type AgentPresenceChangedEvent = z.infer<typeof AgentPresenceChangedEventSchema>;

export const DeleteAgentInputSchema = z.object({
  confirm: z.literal(true),
});
export type DeleteAgentInput = z.infer<typeof DeleteAgentInputSchema>;
