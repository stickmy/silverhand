/**
 * Wire protocol between `silverhand-daemon` (process boundary) and
 * `HostClient` (callers inside `backend` / `electron main`).
 *
 * Why this is its own module (and not buried inside `client` or
 * `daemon`): the wire format is the contract. Two sides of an IPC
 * cannot share TypeScript types but a different parser — the only way
 * to keep them honest is to make the schema the single source of truth
 * that both sides import. This file MUST stay free of node:net /
 * node:child_process / fs imports so it can be unit-tested as a pure
 * encode/decode.
 *
 * ## Authority layering
 *
 * Two layers govern the contract — they are NOT redundant:
 *
 *   1. **This file (zod schemas)** — authoritative for what *shape*
 *      a frame may have. TypeScript daemon and TypeScript clients
 *      import these directly.
 *   2. **`tests/fixtures/wire/wire-fixtures.ts` (NDJSON byte
 *      fixtures)** — authoritative for what *bytes* a valid frame
 *      looks like on the socket. Locks the contract at the byte
 *      level so future re-implementations (Rust daemon, remote host
 *      agent, etc.) have an acceptance gate that does not depend on
 *      zod. See `tests/fixtures/wire/README.md` for the consumer
 *      recipe.
 *
 * When zod and the fixture catalogue disagree, the disagreement is a
 * bug — fix it in the same change. Adding a new schema arm here
 * without adding a fixture variant leaves cross-language consumers
 * blind to the new shape; adding a fixture without widening the
 * schema leaves the TS daemon refusing its own canonical wire bytes.
 *
 * ## Framing
 *
 * Newline-delimited JSON over a duplex byte stream. One JSON object
 * per `\n`-terminated chunk. Both sides decode by buffering bytes,
 * splitting on `\n`, and JSON.parse'ing each frame. We do NOT use
 * length-prefixed framing because (a) the message sizes for PR-1's
 * intent surface are < 1KB each and (b) NDJSON gives us trivial
 * tail-of-log debuggability.
 *
 * ## Three frame shapes
 *
 * - **Request** (`client → daemon`): correlation id + intent name + params.
 * - **Response** (`daemon → client`): same correlation id + result OR
 *   typed error.
 * - **Event** (`daemon → client`, unsolicited): no correlation id;
 *   discriminated by `event` field. PR-1 only emits `ready` (on
 *   socket bind) and `shutting_down` (on SIGTERM/SIGINT). Future
 *   PRs add `runtime_state` events here.
 *
 * Request ids are correlation handles, not security tokens — generated
 * by the client side via `randomBytes`. The daemon echoes them back on
 * the matching response so multiple in-flight requests on one socket
 * can be demultiplexed.
 */
import { z } from 'zod';
import {
  AgentRunStateSchema,
  AgentSendStaleResolutionReasonSchema,
  AgentWorkspaceFileReadSchema,
  AgentWorkspaceTreeSchema,
  ConversationPlacementKindSchema,
  ConversationRosterPolicySchema,
  HostProxyConfigSchema as SharedHostProxyConfigSchema,
  HostKindSchema,
  McpProbeResultSchema,
  MemberDtoSchema,
  MessageReferenceSchema,
  ParsedEventKindSchema,
  ParsedEventSchema,
  PROTOCOL_VERSIONS,
  ReasoningEffortTokenSchema,
  RUNTIME_DELIVERY_INSTRUCTION_SIGNALS,
  RuntimeCatalogResponseSchema,
  RuntimeKindSchema,
} from '@silverhand/shared';

// ── Protocol handshake ─────────────────────────────────────────────
//
// `HOST_PROTOCOL_VERSION` is an EQUALITY SENTINEL on the CLI/backend ↔ local
// daemon boundary: clients fail loud during `hello` if the daemon speaks a
// different version. It is not a package-version bump. The NUMBER lives in the
// single protocol-version authority (`@silverhand/shared` →
// `protocol-versions.ts`); this module owns the framing/contract, not the digit.
//
// Why exact-equality (no skew window, unlike the desktop API epoch): a breaking
// change here is a framing / ordering / custody / addressing break whose
// mixed-pair failure is NOT a parse error. The daemon composes each agent's
// platform prompt and renders every delivery header itself, so ITS generation
// decides what every agent it spawns is TAUGHT while the backend decides what is
// EMITTED — a version-mismatched pair would answer the right message in the WRONG
// stream with no error on either side. No shim is possible (a prompt is composed
// fresh at every spawn, a header rendered fresh at every delivery), so the only
// safe move is to refuse the pair at hello before any agent is spawned.
// `OUTBOUND_PROTOCOL_VERSION` moves with it whenever the same cutover also
// crosses the daemon↔server wire.
//
// Two peers built from the same tree are never a mixed pair, so multiple
// unreleased breaking changes fold into ONE generation rather than minting a
// number per ticket. What each generation changed, and why it must fail loud,
// lives in its owning task/design doc — not as a number ledger here.
//
// Disjoint from the desktop API epoch (`desktop-server-api-epoch.ts`): that gate
// guards `/api/*` (renderer ↔ server) and never classifies a daemon, and this
// constant never classifies a renderer. Two boundaries, two admission rules —
// one numeric source (`protocol-versions.ts`) for both.
export const HOST_PROTOCOL_VERSION = PROTOCOL_VERSIONS.host;

// ── capability tokens ────────────────────────────────────────────────
//
// Two-layer compatibility (the pattern large wire protocols use — HTTP/2
// SETTINGS, TLS extensions, SSH algorithm negotiation, Git protocol caps):
//   • PROTOCOL EPOCH (`HOST_PROTOCOL_VERSION`) — breaking framing/ordering/
//     custody/security changes. Rare. Mismatch fails the handshake.
//   • FEATURE CAPABILITY (this list) — additive, OPTIONAL runtime-behavior
//     flags. A peer that doesn't know a token MUST ignore it (the `capabilities`
//     wire field is an open `string[]`, never an enum), so advertising a new
//     token never breaks an old peer. Unadvertised = unsupported.
//
// `runtime.codex.reasoning.v1` means: this daemon's `runtime_start` accepts an
// OPEN reasoning-effort token (`ReasoningEffortTokenSchema`) rather than a closed
// enum, so the backend may safely send values beyond the historical baseline
// (e.g. Codex `ultra`). A daemon binary predating this token has the closed
// enum and would strict-reject such a value, so the backend must withhold
// non-baseline tokens from it. Bump to `.v2` only if the token's *contract*
// changes; adding a new effort VALUE does not.

/**
 * #task:1614 — this daemon serves the `agent_stale_receive` intent
 * (deferred-pull second consumer). The CLI gates the command on this token so
 * a new CLI against an old daemon fails with a clear message.
 */
export const KNOWN_CAPABILITIES_V1 = [
  'runtime.claude',
  'runtime.codex',
  'runtime.cursor',
  'runtime.opencode',
  'runtime.pi',
  'runtime.qoder',
  'runtime.grok',
  'runtime.codex.reasoning.v1',
  'transport.unix_socket',
  'fs.local',
  'proxy.env_inherit',
  'agent.freshness_authority',
  // #task:1614 — this daemon serves the `agent_stale_receive` intent
  // (deferred-pull second consumer). The CLI gates the command on this
  // token so a new CLI against an old daemon fails with a clear message
  // instead of `unknown_intent`.
  'agent.stale_receive.v1',
  // #task:2991 — `host_status` answers machine facts when asked with
  // `include_machine: true`. Servers ask only daemons advertising this, because
  // older servers decode the result with unknown fields refused.
  'host_status.machine.v1',
] as const;
export type KnownCapabilityV1 = (typeof KNOWN_CAPABILITIES_V1)[number];

export const HelloParamsSchema = z.object({}).strict();
export type HelloParams = z.infer<typeof HelloParamsSchema>;

export const HelloResultSchema = z
  .object({
    protocol_version: z.number().int().positive(),
    daemon_version: z.string(),
    /**
     * Stable daemon-installation identity. For the bootstrap local
     * daemon this is `local`; future multi-daemon installs will each
     * report their own durable daemon_id.
     */
    daemon_id: z.string().min(1),
    /**
     * Ephemeral process-generation identity. Changes on every daemon
     * boot and lets backend drop stale per-daemon runtime snapshots
     * without confusing that with daemon placement.
     */
    instance_id: z.string().min(1),
    /**
     * Human-readable host claim. This is display / registry input,
     * not a placement policy by itself; placement must also consider
     * capabilities and backend-side assignment.
     */
    host_descriptor: z
      .object({
        name: z.string().min(1),
        kind: HostKindSchema,
      })
      .strict(),
    /**
     * Deliberately `string[]`, not an enum. Unknown capabilities must
     * remain wire-valid so older clients can ignore future daemon
     * feature flags instead of rejecting the whole handshake.
     */
    capabilities: z.array(z.string()),
  })
  .strict();
export type HelloResult = z.infer<typeof HelloResultSchema>;

// ── Intent registry ─────────────────────────────────────────────────
//
// PR-1 intent surface is intentionally tiny: hello gates protocol
// compatibility, then ping/status/echo prove the IPC carries
// structured payloads, plumbs lifecycle, and supports reconnect.
// PR-2 expands this with the runtime_* family: the wire surface for
// the (conversation, agent) supervised process.
//
// PR-2 step 1 lands the wire schemas here PLUS a daemon-internal fake
// supervisor that satisfies the contract without doing any real
// process spawn. Step 2 wires the supervisor to the real
// ConversationManager execution-plane code carved out of
// `packages/backend/src/conversation-manager.ts`. Both steps share
// THIS file as the single source of truth — drift between client and
// daemon is impossible because both import from one registry.
//
// Naming convention: every PR-2 intent / event prefixes with
// `runtime_` so they are visually distinct from PR-1's
// hello / ping / status / echo lifecycle intents. The split is deliberate:
// runtime_* talk to a (conversation, agent) supervised process;
// PR-1 lifecycle intents talk to the daemon as a whole.

export const PingParamsSchema = z.object({}).strict();
export type PingParams = z.infer<typeof PingParamsSchema>;

export const PingResultSchema = z.object({
  pong: z.literal(true),
  daemon_pid: z.number().int(),
  daemon_version: z.string(),
  /** Epoch ms when the daemon process bound its socket. */
  started_at: z.number().int(),
});
export type PingResult = z.infer<typeof PingResultSchema>;

export const StatusParamsSchema = z.object({}).strict();
export type StatusParams = z.infer<typeof StatusParamsSchema>;

export const StatusResultSchema = z.object({
  daemon_pid: z.number().int(),
  daemon_version: z.string(),
  started_at: z.number().int(),
  uptime_ms: z.number().int(),
  /** Number of intents the daemon has handled this lifetime. Useful
   *  for asserting reconnect tests actually re-issued requests. */
  intent_count: z.number().int(),
  /** Reserved for PR-2: list of currently-tracked (conversation,
   *  agent) runtime intents. Empty in PR-1. */
  runtimes: z.array(z.unknown()),
});
export type StatusResult = z.infer<typeof StatusResultSchema>;

export const EchoParamsSchema = z
  .object({
    message: z.string(),
  })
  .strict();
export type EchoParams = z.infer<typeof EchoParamsSchema>;

export const EchoResultSchema = z
  .object({
    echoed: z.string(),
  })
  .strict();
export type EchoResult = z.infer<typeof EchoResultSchema>;

// ── host_status (outbound command, #task:808) ────────────────────────
//
// Server → daemon RPC powering `GET /api/hosts/:id/status`. Distinct
// from the local inbound `status` intent — that one is HostClient
// (backend/test) → daemon-on-local-socket; this one rides the same
// `outboundServerCommandSchemas` channel as `runtime_start` / etc., so
// backend can dispatch it for ANY host (embedded local, future remote
// sandbox) through `OutboundFrameAdapter.sendCommand(hostId, ...)`.
//
// Payload is host-facts only: OS, architecture, daemon process info,
// uptime, plus the active agent_id set (which agents are currently
// spawned on this host). Backend joins that set against the
// registration table (`agents.host_id == hostId`) to enrich the
// per-row `online` flag in `AgentOnHostDto`. The daemon NEVER
// references agent names, conversation ids, or any other backend-
// owned semantic — those live above the host boundary.
export const HostStatusParamsSchema = z
  .object({
    /** Ask for the machine facts below; see `host_status.machine.v1`. */
    include_machine: z.boolean().optional(),
  })
  .strict();
export type HostStatusParams = z.infer<typeof HostStatusParamsSchema>;

export const HostStatusResultSchema = z
  .object({
    /** Human-readable OS string composed daemon-side
     *  (e.g. "macOS 14.5", "Linux Ubuntu 22.04"). Renderer shows verbatim.
     *  `.min(1)` pins the never-empty contract — the daemon helper always
     *  composes SOMETHING (falls back to `${platform} ${release}`), and an
     *  empty value reaching this schema is a daemon bug we want to fail
     *  loud at the IPC boundary instead of letting a blank cell render
     *  in ComputerPane. */
    os: z.string().min(1),
    /** `os.arch()` token; optional so older daemons stay forward-compatible. */
    os_arch: z.string().optional(),
    daemon_pid: z.number().int(),
    daemon_version: z.string(),
    started_at: z.number().int(),
    uptime_ms: z.number().int(),
    /** Subset of registered agents currently alive on this host. Backend
     *  intersects this with `agents.host_id == hostId` to compute
     *  per-agent `online` in `AgentOnHostDto`. Empty array = host has
     *  no live runtimes (renderer shows everything as offline). */
    active_agent_ids: z.array(z.string()),
    /** Machine facts, present only when requested and readable. */
    hostname: z.string().min(1).optional(),
    ip: z.string().min(1).optional(),
    memory_total_bytes: z.number().int().nonnegative().optional(),
    memory_used_bytes: z.number().int().nonnegative().optional(),
    disk_total_bytes: z.number().int().nonnegative().optional(),
    disk_used_bytes: z.number().int().nonnegative().optional(),
    system_uptime_ms: z.number().int().nonnegative().optional(),
  })
  .strict();
export type HostStatusResult = z.infer<typeof HostStatusResultSchema>;

// ── host_runtime_catalog (outbound command, #task:1375) ─────────────
//
// Server → daemon RPC powering per-computer runtime availability. This
// deliberately runs on the target host and uses the same host-side resolver
// as spawn, so "present" means the daemon can resolve the exact executable
// path it will later spawn. `force` bypasses the short resolver cache for
// manual refresh, create-time hard-block checks, and spawn-miss recovery.
export const HostRuntimeCatalogParamsSchema = z
  .object({
    force: z.boolean().optional(),
  })
  .strict();
export type HostRuntimeCatalogParams = z.infer<typeof HostRuntimeCatalogParamsSchema>;

export const HostRuntimeCatalogResultSchema = RuntimeCatalogResponseSchema;
export type HostRuntimeCatalogResult = z.infer<typeof HostRuntimeCatalogResultSchema>;

// ── conversation_log_debug ─────────────────────────────────────────

export const ConversationLogDebugParamsSchema = z.discriminatedUnion('enabled', [
  z
    .object({
      conversation_id: z.string().min(1),
      enabled: z.literal(true),
      expires_at: z.number().int().positive(),
    })
    .strict(),
  z
    .object({
      conversation_id: z.string().min(1),
      enabled: z.literal(false),
    })
    .strict(),
]);
export type ConversationLogDebugParams = z.infer<typeof ConversationLogDebugParamsSchema>;

export const ConversationLogDebugResultSchema = z
  .object({
    ok: z.literal(true),
    enabled: z.boolean(),
    expires_at: z.number().int().positive().nullable(),
  })
  .strict();
export type ConversationLogDebugResult = z.infer<typeof ConversationLogDebugResultSchema>;

// ── agent_message_send / agent_stale_resolve ───────────────────────
//
// Local agent-runtime → daemon intents for daemon-owned freshness
// authority. The CLI remains a short-lived shim; it submits the visible
// side-effect intent to the daemon, and the daemon stamps
// `seen_up_to_seq` from its runtime coordinator before forwarding to
// the backend. Backend HTTP failures intentionally travel as result
// arms, not host-transport errors, so the CLI can preserve the same
// user-facing error mapping it had on direct HTTP.

export const AgentBackendHttpErrorSchema = z
  .object({
    ok: z.literal(false),
    status: z.number().int().positive(),
    body_text: z.string(),
  })
  .strict();
export type AgentBackendHttpError = z.infer<typeof AgentBackendHttpErrorSchema>;

/** Runtime handle identity for CLI→daemon intents. It names custody scope,
 *  never an HTTP route or message destination. */
export const AgentRuntimeIntentScopeSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('agent') }).strict(),
  z
    .object({
      kind: z.literal('conversation'),
      conversation_id: z.string().min(1),
    })
    .strict(),
]);
export type AgentRuntimeIntentScope = z.infer<typeof AgentRuntimeIntentScopeSchema>;

const AgentRuntimeIntentIdentityFields = {
  runtime_scope: AgentRuntimeIntentScopeSchema,
  agent_id: z.string().min(1),
} as const;

export const AgentMessageSendParamsSchema = z
  .object({
    ...AgentRuntimeIntentIdentityFields,
    content: z.string(),
    /**
     * WHERE the message goes — one absolute address, required, forwarded to the
     * backend verbatim (#task:2021 P4-4).
     *
     * A STRING, not a shape: the grammar has exactly one authority
     * (`delivery-target.ts`), and the daemon parses it only to decide which
     * per-stream freshness boundary this send is anchored to. An address this
     * daemon cannot parse is NOT refused here — it travels on and the backend's
     * resolver gives the one verdict (`target_malformed`), so the two never
     * disagree about what a legal address is.
     *
     * `runtime_scope` above identifies the daemon handle; it is never a
     * destination or a transport route coordinate.
     */
    target: z.string().min(1),
    attachment_ids: z.array(z.string().min(1)).optional(),
    stale_resolution_reason: AgentSendStaleResolutionReasonSchema.optional(),
    freshness_bypass: z.boolean().optional(),
  })
  .strict();
export type AgentMessageSendParams = z.infer<typeof AgentMessageSendParamsSchema>;

export const AgentMessageSendResultSchema = z.union([
  z
    .object({
      ok: z.literal(true),
      message_id: z.string().min(1),
      message_seq: z.number().int().nonnegative().optional(),
      /**
       * The conversation the address RESOLVED to, forwarded from the backend
       * send response (#task:2021 P4-4).
       *
       * Total, not conditional on the address having been readable: "which
       * stream did my words land in" has one answer for every send, and it is
       * the answer a caller can cache — a `dm:@handle` or a title is spelled
       * once and every follow-up in that stream carries the id form. The daemon
       * needs it for the same reason: its freshness cursors are keyed by
       * conversation, and only the backend knows which conversation a NAME
       * denoted at insert time.
       */
      conversation_id: z.string().min(1),
      /** Reply-cluster root the sent message belongs to (§6.4): null on
       *  the main line, else the cluster's root message id. Surfaced from
       *  the backend send response so a caller (CLI) can report which
       *  stream the message landed in. */
      thread_root_id: z.string().min(1).nullable().optional(),
      /** References in the sent body that addressed NOBODY (#task:2021
       *  P4-5-b), forwarded verbatim from the backend send response so the CLI
       *  can tell its agent which of its own tokens landed nowhere. Absent when
       *  every reference was addressed.
       *
       *  Forwarded, never derived: the daemon has no participation authority and
       *  must not acquire one — a second opinion here would be a second
       *  definition of "addressed", reported to the one reader who cannot check
       *  it. Reason-free for the same purpose it is reason-free at the backend
       *  boundary: `unknown handle` and `not a participant` are one outcome. */
      unresolved_references: z.array(MessageReferenceSchema).nonempty().optional(),
    })
    .strict(),
  AgentBackendHttpErrorSchema,
]);
export type AgentMessageSendResult = z.infer<typeof AgentMessageSendResultSchema>;

export const AgentStaleResolveParamsSchema = z
  .object({
    ...AgentRuntimeIntentIdentityFields,
    /** Object identity returned by the `message_stale` conflict. Required
     *  because one AgentSlot can hold independent stale drafts in N streams. */
    stale_event_id: z.string().min(1),
    resolution_reason: z.literal('obsolete'),
  })
  .strict();
export type AgentStaleResolveParams = z.infer<typeof AgentStaleResolveParamsSchema>;

export const AgentStaleResolveResultSchema = z.union([
  z
    .object({
      ok: z.literal(true),
      resolved: z.literal(true),
    })
    .strict(),
  z
    .object({
      ok: z.literal(true),
      resolved: z.literal(false),
      reason: z.literal('no_pending_stale_event'),
    })
    .strict(),
  z
    .object({
      ok: z.literal(true),
      resolved: z.literal(false),
      // #task:2095 F18: a later 409 REPLACED the event the caller is
      // resolving before this call ran. Only ONE stale event is held per
      // runtime, so the id the caller named is gone; `current_stale_event_id`
      // is the LIVE one to re-issue against (actionable on the CLI). Distinct
      // from `no_pending_stale_event` (nothing pending at all).
      reason: z.literal('stale_event_superseded'),
      current_stale_event_id: z.string().min(1),
    })
    .strict(),
  AgentBackendHttpErrorSchema,
]);
export type AgentStaleResolveResult = z.infer<typeof AgentStaleResolveResultSchema>;

// ── agent_stale_receive (CLI→daemon, #task:1614 deferred pull) ──────
//
// After a `message_stale` 409 the agent may PULL the daemon's deferred
// delivery batch as a tool result instead of waiting for the stdin
// push. Custody contract lives on `DeferredEntry` / `flushDeferred`
// (#task:1606): the pull is a SECOND CONSUMER of the same daemon-owned
// deferred queue — same lifecycle lock, same bounded FIFO prefix, one
// release winner — never a second release surface over backend
// pending / runtime-written content.
//
// Outcome arms state only provable facts (#task:1606 P2b):
//   - `batch_queue_drained`  — a prefix was taken AND it emptied the
//                              deferred queue (nothing more is pending).
//   - `batch_more_pending`   — a prefix was taken but the deferred queue
//                              still holds a remainder for the next pull.
//                              (#task:2095 F12: the split is a property of
//                              the QUEUE after the splice, not of any
//                              per-stream seq coverage — freshness is now the
//                              RuntimeScope-wide undelivered-inbox guard.)
//   - `no_pending_stale`     — typed no-op; nothing at all is pending.
//   - `stale_event_superseded` — a later 409 REPLACED the named event before
//                              this pull parked; `current_stale_event_id` is
//                              the LIVE one to re-issue against. Symmetric with
//                              stale-resolve's `stale_event_superseded`, and
//                              distinct from the mid-wait `stale_event_replaced`
//                              arbitration error (which fires only after a wait
//                              was already registered — see below).
//   - `normal_delivery_in_progress` — a release is already underway for this
//                              runtime (`startupDelivery !== null`); the tool
//                              will not take that content back. (#task:2095
//                              F12①: no per-stream seq comparison — a live
//                              release is itself the proof.)
//   - `timeout`              — no matching daemon batch was obtained in
//                              this wait window. States nothing else:
//                              custody and the stale event are both
//                              unchanged, and no cause is inferred.
export const AgentStaleReceiveParamsSchema = z
  .object({
    ...AgentRuntimeIntentIdentityFields,
    /** Object identity returned by the `message_stale` conflict. It selects
     *  one held draft without a route anchor or "most recent" fallback. */
    stale_event_id: z.string().min(1),
    /** Bounded wait for a deferred batch to arrive. Waiting never holds
     *  the lifecycle lock and never takes custody. Clamped to
     *  [0, 120_000]; default 20_000. */
    wait_ms: z.number().int().min(0).max(120_000).optional(),
  })
  .strict();
export type AgentStaleReceiveParams = z.infer<typeof AgentStaleReceiveParamsSchema>;

const staleReceiveBatchFields = {
  ok: z.literal(true),
  stale_event_id: z.string().min(1),
  /** Verbatim combined batch text — the exact bytes the turn_end flush
   *  would have written to stdin (ids/owner/text/stream metadata are
   *  preserved; the batch is opaque and never split). */
  text: z.string().min(1),
  delivery_ids: z.array(z.number().int().positive()).min(1),
} as const;

export const AgentStaleReceiveResultSchema = z.discriminatedUnion('outcome', [
  z.object({ outcome: z.literal('batch_queue_drained'), ...staleReceiveBatchFields }).strict(),
  z.object({ outcome: z.literal('batch_more_pending'), ...staleReceiveBatchFields }).strict(),
  z.object({ ok: z.literal(true), outcome: z.literal('no_pending_stale') }).strict(),
  z
    .object({
      ok: z.literal(true),
      outcome: z.literal('stale_event_superseded'),
      current_stale_event_id: z.string().min(1),
    })
    .strict(),
  z
    .object({
      ok: z.literal(true),
      outcome: z.literal('normal_delivery_in_progress'),
      stale_event_id: z.string().min(1),
    })
    .strict(),
  z
    .object({
      ok: z.literal(true),
      outcome: z.literal('timeout'),
      stale_event_id: z.string().min(1),
      waited_ms: z.number().int().nonnegative(),
    })
    .strict(),
]);
export type AgentStaleReceiveResult = z.infer<typeof AgentStaleReceiveResultSchema>;

/** Non-custody arbitration failures. Each states "this call cannot
 *  arbitrate", never a custody fact — custody facts travel only in the
 *  result outcomes above.
 *   - `pull_already_active`: one waiter/reservation per handle; a second
 *     concurrent pull is refused.
 *   - `stale_event_replaced`: the pending stale event changed identity
 *     after this call bound to it AND a wait was already registered; the old
 *     waiter is invalid by contract. This is the MID-WAIT sibling of the
 *     `stale_event_superseded` result outcome (which is the entry/pre-wait
 *     check): both mean "a later 409 replaced the id you named", but they are
 *     not merged because they fire at different points — `superseded` before
 *     any wait exists (a benign re-anchor, `ok:true`), `replaced` after
 *     (a parked waiter must be torn down, so it is a typed error).
 *   - `runtime_replaced`: the runtime handle this call bound to exited or
 *     was respawned while waiting.
 *   - `handoff_unsupported`: the ingress cannot prove a response
 *     transport handoff (only the control socket can), so the pull is
 *     refused before any custody decision (fail closed).
 *   - `agent_deleting`: the terminal deletion fence was installed before
 *     this call could reserve custody. */
export const AgentStaleReceiveErrorCodeSchema = z.enum([
  'agent_deleting',
  'pull_already_active',
  'stale_event_replaced',
  'runtime_replaced',
  'handoff_unsupported',
]);
export type AgentStaleReceiveErrorCode = z.infer<typeof AgentStaleReceiveErrorCodeSchema>;
// `AgentStaleReceiveIpcErrorSchema` lives next to the other per-intent
// widened error schemas below (after `ResponseErrorCodeSchema`).

// ── PR-2 runtime_* surface ──────────────────────────────────────────
//
// Wire surface for the (conversation, agent) supervisor that the
// daemon owns. Anything that
// looks "runtime_*" goes through one (conversation_id, agent_id) pair
// — the daemon serializes operations on that key with the same
// per-pair lifecycle lock CM uses today.

/**
 * Live-delivery text — the unencoded message text the daemon will
 * hand to its in-process driver's `encodeStdinMessage(text, ...)`
 * just before writing to stdin. Used by `runtime_deliver.text`.
 *
 * Why text and not pre-encoded stream-json bytes (Option A v2 — Cindy
 * `UMGu36zoTDVuKWXi`): stateful drivers (codex) carry `threadId` /
 * `activeTurnId` inside the driver instance that owns the child's
 * stdio. If backend pre-encodes by spinning up its own driver
 * instance, that instance has no `threadId` and `encodeStdinMessage`
 * returns `null`, so the wake silently dies (this was the codex
 * daemon-mode delivery blocker; see commit 2b-iii log). Moving the
 * encode into the daemon restores the legacy CM invariant — one
 * driver instance per (conv, agent), and whoever owns the child
 * process owns the driver state.
 *
 * `.min(1)` is intentional (Cindy `wYjG20JdGjLpwq8K`): every backend
 * call site (formatInboxBatch / combineGated) yields
 * non-empty text. An empty string reaching the daemon is a caller
 * bug — fail loudly at schema validation, not at stdin encode.
 */
export const RuntimeMessageTextSchema = z.string().min(1);
export type RuntimeMessageText = z.infer<typeof RuntimeMessageTextSchema>;

/**
 * Stable runtime identity. This value is carried end-to-end instead of being
 * reconstructed from a nullable conversation coordinate: it is the one input
 * that selects the supervisor key, cwd, pidfile namespace and prompt shape.
 */
export const RuntimeScopeSchema = z.discriminatedUnion('kind', [
  z
    .object({
      kind: z.literal('agent'),
      agent_id: z.string().min(1),
    })
    .strict(),
  z
    .object({
      kind: z.literal('conversation'),
      conversation_id: z.string().min(1),
      agent_id: z.string().min(1),
    })
    .strict(),
]);
export type RuntimeScope = z.infer<typeof RuntimeScopeSchema>;

const SilverhandCliLauncherSchema = z
  .object({
    command: z.string().min(1),
    args: z.array(z.string()),
    /**
     * Explicit launcher metadata for the per-spawn `silverhand` shim.
     * Named `env_vars` so the protocol can keep `env` itself reserved
     * for the forbidden pre-composed child environment map.
     */
    env_vars: z.record(z.string()).optional(),
  })
  .strict();

export const RuntimeStartWorkspaceRefSchema = z
  .object({
    id: z.string().min(1),
    name: z.string().min(1),
    // #task:2081: NO stored container `kind` on this wire. The daemon used to
    // select a filesystem-resolution strategy from a stored word; it now
    // recognizes an agent-home ref from the id convention itself
    // (`AGENT_HOME_WORKSPACE_ID_PREFIX` for the alphabet, exact match against
    // `agentHomeWorkspaceId(agentId)` for ownership — `runtime-start-resolve.ts`),
    // so the decision is keyed on the id, never on a copy of a container word
    // that a product rename could contradict.
    // D4.e-6: SEMANTIC ref only — no `path`. The carve-out that let
    // `workspace.path` cross the boundary as "server DB metadata" is
    // REVOKED (design §3.5). The daemon is the sole authority for
    // host-local workspace paths: relocation-v1 §7 task 4 resolves
    // `regular` workspaces to their deterministic agent-centric bundle
    // coordinate (`resolveAgentWorkspaceBundleDir(agentId, id)`) — no
    // registry establishment gate — and `agent_home` via
    // `resolveAgentDir(agentId)`. No path field travels on this wire.
  })
  .strict();
export type RuntimeStartWorkspaceRef = z.infer<typeof RuntimeStartWorkspaceRefSchema>;

/** Semantic origin coordinate for one delivery action. It deliberately carries
 * no host path: the daemon resolves the absolute workdir for the target using
 * the same path authority as runtime start. */
export const RuntimeActionTargetSchema = z
  .object({
    conversation_id: z.string().min(1),
    workspace: RuntimeStartWorkspaceRefSchema.nullable(),
  })
  .strict();
export type RuntimeActionTarget = z.infer<typeof RuntimeActionTargetSchema>;

const RuntimeStartAgentRefSchema = z
  .object({
    id: z.string().min(1),
    name: z.string().min(1),
    // D4.e-6: SEMANTIC ref only — no `home_dir`. Revokes the prior
    // carve-out (design §3.5). The daemon resolves the agent's L1
    // profile base locally as `resolveAgentDir(agentId)` — identical
    // to how `agent_materialize`/`agent_workspace_tree` already resolve
    // it — and reads `system_prompt.md` under it.
  })
  .strict();

/**
 * Reasoning-effort as it travels on the `runtime_start` wire — the shared OPEN,
 * length/charset-bounded token (`ReasoningEffortTokenSchema`), NOT the closed
 * product enum. See its definition in `@silverhand/shared` for the full
 * two-layer rationale; in short: a daemon must parse a token it has never heard
 * of rather than strict-reject the whole `runtime_start`, so adding a new effort
 * value never requires a protocol bump. The driver forwards this token verbatim,
 * and the backend gates non-baseline tokens on `runtime.codex.reasoning.v1`
 * before they ride here.
 */
const RuntimeStartDriverOptionsSchema = z
  .object({
    kind: RuntimeKindSchema,
    cli_session_id: z.string().min(1).nullable(),
    model: z.string().nullable(),
    reasoning_effort: ReasoningEffortTokenSchema.nullable(),
    fast_mode: z.boolean().nullable(),
  })
  .strict();

const RuntimeStartSessionSchema = z
  .object({
    conversation_id: z.string().min(1),
    placement_kind: ConversationPlacementKindSchema,
    roster_policy: ConversationRosterPolicySchema,
    title: z.string(),
    spawned_by_agent_id: z.string().min(1).nullable(),
    members: z.array(MemberDtoSchema),
  })
  .strict();

const RuntimeStartEnvPolicySchema = z
  .object({
    silverhand: z
      .object({
        auth_token: z.string().min(1),
      })
      .strict(),
    // Host-local truth in standalone mode: the control plane provides
    // server URL/token, while the daemon resolves how to launch its own
    // bundled CLI. Embedded backends may still send an explicit launcher.
    silverhand_cli: SilverhandCliLauncherSchema.optional(),
  })
  .strict();

const FORBIDDEN_RUNTIME_START_OPTION_KEYS = new Set([
  'cwd',
  'workspace_path',
  // D4.e-6: `home_dir` joins the forbidden set — the agent L1 base is
  // daemon-resolved (`resolveAgentDir`), never server-supplied. The
  // strict() ref schemas already reject it at the top level; listing it
  // here also bans it from appearing nested anywhere in options.
  'home_dir',
  'system_prompt_path',
  'spawn_dir',
  'system_prompt',
  'system_prompt_text',
  'memory_text',
  'consensus_text',
  'env',
]);

function rejectForbiddenRuntimeStartOptionKeys(
  value: unknown,
  ctx: z.RefinementCtx,
  path: Array<string | number> = [],
): void {
  if (value === null || typeof value !== 'object') return;
  if (Array.isArray(value)) {
    value.forEach((item, index) =>
      rejectForbiddenRuntimeStartOptionKeys(item, ctx, [...path, index]),
    );
    return;
  }
  for (const [key, nested] of Object.entries(value)) {
    const nextPath = [...path, key];
    if (FORBIDDEN_RUNTIME_START_OPTION_KEYS.has(key)) {
      ctx.addIssue({
        code: 'custom',
        path: nextPath,
        message:
          `runtime_start.options.${nextPath.join('.')} is forbidden by the D2.0 path/byte boundary; ` +
          'daemon-host must derive cwd/spawn paths/prompt bytes/env locally.',
      });
    }
    rejectForbiddenRuntimeStartOptionKeys(nested, ctx, nextPath);
  }
}

/**
 * Runtime start metadata. This schema is deliberately strict and, as of
 * D4.e-6, carries NO host-local paths: `agent`/`workspace`/
 * `workspace_memory_index` are semantic refs (`id`/`name`) and the
 * daemon resolves every absolute path locally (the deterministic bundle
 * coordinate for a regular workspace, `resolveAgentDir` for an agent home
 * recognized by the `wsah_` id convention — #task:2081). Server-derived execution artifacts
 * and prompt bytes were never allowed and remain forbidden below.
 */
const RuntimeStartCommonShape = {
  agent: RuntimeStartAgentRefSchema,
  driver: RuntimeStartDriverOptionsSchema,
  env_policy: RuntimeStartEnvPolicySchema,
} as const;

const ConversationStartOptionsSchema = z
  .object({
    ...RuntimeStartCommonShape,
    runtime_scope: z
      .object({
        kind: z.literal('conversation'),
        conversation_id: z.string().min(1),
        agent_id: z.string().min(1),
      })
      .strict(),
    // Workspace-less placements keep their own conversation-scoped runtime
    // coordinate under the agent home. `null` is a first-class placement arm,
    // never a sentinel for AgentSlot (the discriminated runtime_scope above is
    // the sole identity authority).
    workspace: RuntimeStartWorkspaceRefSchema.nullable(),
    workspace_memory_index: z.array(RuntimeStartWorkspaceRefSchema),
    session: RuntimeStartSessionSchema,
  })
  .strict();

const AgentStartOptionsSchema = z
  .object({
    ...RuntimeStartCommonShape,
    runtime_scope: z
      .object({
        kind: z.literal('agent'),
        agent_id: z.string().min(1),
      })
      .strict(),
  })
  .strict();

export const StartOptionsSchema = z
  .union([ConversationStartOptionsSchema, AgentStartOptionsSchema])
  .superRefine((val, ctx) => rejectForbiddenRuntimeStartOptionKeys(val, ctx));
export type StartOptions = z.infer<typeof StartOptionsSchema>;
export type ConversationStartOptions = Extract<
  StartOptions,
  { runtime_scope: { kind: 'conversation' } }
>;
export type AgentStartOptions = Extract<StartOptions, { runtime_scope: { kind: 'agent' } }>;

/**
 * Where a supervised runtime is in its lifecycle (§3.2.1 startup/live
 * exclusivity state machine). Single source for the `runtime_status` DTO's
 * phase field. (The daemon's own `RuntimeHandle.phase` in
 * `supervisor/handle.ts` is the narrower live subset — it excludes `idle`,
 * since a real handle is always at least `spawning`.) The Stage 1
 * `runtime_start` reply phase was retired in Stage 2c — `runtime_start` is a
 * pure lifecycle spawn now, and delivery lifecycle is the daemon's call.
 */
export const RuntimePhaseSchema = z.enum([
  'idle',
  'spawning',
  'startup_pending',
  'ready',
  'draining',
  'exited',
]);
export type RuntimePhase = z.infer<typeof RuntimePhaseSchema>;

/**
 * #task:1588 — single source for "does this runtime phase count as a
 * live runtime for HOST-level agent liveness". Consumed by BOTH the
 * daemon's `host_status.active_agent_ids` derivation (dispatcher) and
 * the backend's host-status read model, so the two ends cannot drift.
 *
 * Expressed as a whitelist so a NEW phase added to
 * {@link RuntimePhaseSchema} fails CLOSED (not online) until its
 * liveness semantics are decided explicitly (Cindy slice-1 review
 * P1 #2 contract; re-affirmed in #task:1588 review).
 */
export const HOST_ACTIVE_RUNTIME_PHASES: ReadonlySet<RuntimePhase> = new Set([
  'spawning',
  'startup_pending',
  'ready',
  'draining',
] satisfies RuntimePhase[]);

export function isHostActiveRuntimePhase(phase: string): boolean {
  return HOST_ACTIVE_RUNTIME_PHASES.has(phase as RuntimePhase);
}

/** A single (conversation, agent) supervised runtime as the daemon
 *  reports it via `runtime_status`. Mirrors the run-state DTO that
 *  backend already publishes to renderers. */
export const RuntimeStateDtoSchema = z.object({
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  /** Exact supervised generation represented by this snapshot row. */
  runtime_generation: z.number().int().positive(),
  pid: z.number().int().nullable(),
  /** Daemon's view of where this runtime is in its lifecycle. The
   *  enum mirrors the §3.2.1 startup/live exclusivity state machine
   *  so the operator can read intent vs implementation off one
   *  field. */
  phase: RuntimePhaseSchema,
  /** Last known AgentRunState; null until the driver emits its first
   *  run-state transition. */
  run_state: AgentRunStateSchema.nullable(),
  started_at: z.number().int().nullable(),
});
export type RuntimeStateDto = z.infer<typeof RuntimeStateDtoSchema>;

// ── runtime_start ───────────────────────────────────────────────────

/**
 * Per-delivery wire metadata attached to a `runtime_deliver` batch. One entry
 * per delivered inbox row; the entry set MUST match the batch's `delivery_ids`
 * exactly (daemon hard-rejects a mismatch — text and metadata must not drift).
 *
 * The alphabet is exactly the daemon's three readers, and #task:2097 made the
 * schema say so: `delivery_id` (dedup key when it merges deliveries into a
 * parked/pending batch), `seq` (the `delivered_up_to_seq` sidecar it writes to
 * disk is `max(seq)` over this array — #task:2106 removed the wire scalar, so
 * this array is now the SOLE source of that value), and `presentation` (novel-remainder reconstruction). There is no
 * per-stream observed boundary: freshness is now a whole-runtime backend
 * property (RuntimeScope EXISTS-guard), so the daemon maintains no per-stream
 * cursor and this array is not an "observed authority".
 *
 * DELETED here by #task:2097, not deprecated: `message_id`, `thread_root_id`
 * (null ⇒ main line) and `delivery_kind` (`turn`/`observe`). All three were
 * readers of the deleted per-stream freshness subsystem (§6.4) and had no
 * daemon reader left — the live `--reply-to` resolution parses the incoming
 * request JSON, not this array; the turn/observe decision is made backend-side
 * off `conversation_inboxes.delivery_kind` before a batch is ever assembled.
 * Kept-as-optional was rejected: an optional field on a strict internal wire
 * reads as "a consumer may appear", which is the claim being retired.
 */
export const RuntimeDeliverySchema = z
  .object({
    delivery_id: z.number().int(),
    seq: z.number().int().nonnegative(),
    /** Presentation fragment for canonical novel-remainder reconstruction. */
    presentation: z
      .object({
        body: RuntimeMessageTextSchema,
        instruction: z.enum(RUNTIME_DELIVERY_INSTRUCTION_SIGNALS),
      })
      .strict(),
  })
  .strict();
export type RuntimeDelivery = z.infer<typeof RuntimeDeliverySchema>;

export const RuntimeStartParamsSchema = z
  .object({
    conversation_id: z.string().min(1),
    agent_id: z.string().min(1),
    runtime_generation: z.number().int().positive(),
    options: StartOptionsSchema,
  })
  .strict()
  .superRefine((value, ctx) => {
    const scope = value.options.runtime_scope;
    if (scope.agent_id !== value.agent_id) {
      ctx.addIssue({
        code: 'custom',
        path: ['options', 'runtime_scope', 'agent_id'],
        message: 'runtime scope agent must match runtime_start.agent_id',
      });
    }
    if (scope.kind === 'conversation' && scope.conversation_id !== value.conversation_id) {
      ctx.addIssue({
        code: 'custom',
        path: ['options', 'runtime_scope', 'conversation_id'],
        message: 'conversation runtime scope must match runtime_start target',
      });
    }
  });
export type RuntimeStartParams = z.infer<typeof RuntimeStartParamsSchema>;

export const RuntimeStartResultSchema = z.discriminatedUnion('started', [
  z
    .object({
      started: z.literal(true),
      pid: z.number().int(),
      started_at: z.number().int(),
    })
    .strict(),
  /**
   * Cooldown-blocked arm (Cindy `-pdNvRYOWip5e9yA` + plan v2
   * `EOmOrJxeJUnbcmVR`): daemon is in init-failure cooldown for this
   * (conv, agent) and refuses to spawn. This is a BUSINESS state —
   * backend projects it to `agent_status reason:'init_cooldown'` SSE
   * — not a transport error, so it ships in the result union rather
   * than throwing an `internal_error` reply.
   *
   * Only the daemon's user-facing `runtime_start` gate produces
   * this arm. Daemon-internal retry respawns (attempt > 0) take a
   * separate code path that is NEVER blocked by this gate; the
   * watchdog failure decides retry eligibility at the moment of
   * failure. The gate ONLY fires when:
   *   - no live handle exists for (conv, agent), AND
   *   - this is a fresh user-initiated start (initAttempt === 0).
   * Idempotent return for an already-running handle is the success
   * arm even if a stale cooldown would otherwise apply.
   *
   * Programmer / deployment errors (missing `options.runtime`,
   * missing binary, unknown runtime kind) STILL throw and surface
   * as `internal_error` — they are not modeled as business state.
   */
  z
    .object({
      started: z.literal(false),
      reason: z.literal('init_cooldown'),
      /** Wall-clock epoch ms at which cooldown expires. Backend
       *  projects to `agent_status.cooldown_until`. Must be strictly
       *  in the future at emit time. */
      cooldown_until: z.number().int().positive(),
    })
    .strict(),
]);
export type RuntimeStartResult = z.infer<typeof RuntimeStartResultSchema>;

// ── runtime_stop ────────────────────────────────────────────────────

/**
 * Stop a (conv, agent) runtime: SIGTERM 5s → SIGKILL cascade. Result
 * reports the process termination outcome via `{ stopped, exit_code }`.
 *
 * `mode` stays on the wire as a literal `'hard'` so the field name is
 * stable if a graceful mode ever returns. The former `mode:'soft'`
 * stdin-directive arm was removed (#task:1558, shiki decision
 * `mUnNmEyo10T-vnaU`): it had no product entry point and no external
 * contract.
 */
export const RuntimeStopParamsSchema = z
  .object({
    runtime_scope: RuntimeScopeSchema,
    runtime_generation: z.number().int().positive(),
    mode: z.literal('hard'),
  })
  .strict();
export type RuntimeStopParams = z.infer<typeof RuntimeStopParamsSchema>;

export const RuntimeStopResultSchema = z
  .object({
    mode: z.literal('hard'),
    /** Whether the runtime was actually live and got the signal.
     *  False = nothing to stop (already exited / never started). */
    stopped: z.boolean(),
    /** Final exit code if known at response time. SIGTERM cascade
     *  may not have completed; daemon may report `null` and let the
     *  `runtime_exit` event carry the real value. */
    exit_code: z.number().int().nullable(),
  })
  .strict();
export type RuntimeStopResult = z.infer<typeof RuntimeStopResultSchema>;

// ── runtime_deliver (Stage 2, #task:1417) ───────────────────────────
//
// The single delivery command that subsumes the backend start-vs-wake
// fork. Backend claims the pending batch in-flight (`owner=connectionId`),
// then submits `runtime_deliver`; the daemon inspects the REAL handle and
// picks the action — spawn / write / refuse. Backend prescribes none of
// that, which kills the #task:1416 bug class (backend guessing runtime
// lifecycle truth from its desyncable `RunStateCache`).
//
// Canonical: docs/daemon-authoritative-delivery-v1.md §10–§11 (the
// authoritative custody / async-ack contract for runtime_deliver).
//
// Fully wired (Stage 2a/2b): the command is in the command catalogues, its
// consumed/failed events are in `EventFrameSchema` (forcing the
// event-handler exhaustiveness cases), and it is the SOLE message-delivery
// command. Stage 2c retired the legacy `runtime_wake` command and the
// `runtime_startup_delivery_*` event pair; `HOST_PROTOCOL_VERSION` /
// `OUTBOUND_PROTOCOL_VERSION` were bumped with each wire-behavior change.

export const RuntimeDeliverParamsSchema = z
  .object({
    conversation_id: z.string().min(1),
    agent_id: z.string().min(1),
    runtime_generation: z.number().int().positive(),
    target: RuntimeActionTargetSchema,
    delivery_ids: z.array(z.number().int()).min(1),
    deliveries: z.array(RuntimeDeliverySchema).min(1),
    /** Distinguishes a normal message from a schedule fire, exactly as
     *  the retired `runtime_wake` did — the daemon's driver needs it to
     *  frame the payload. Spawn-time startup delivery had no `kind`
     *  because it was always message-shaped; unifying the command makes
     *  it explicit. */
    kind: z.enum(['message', 'schedule_fire']),
    /** Unencoded message text (Option A v2). The daemon encodes via its
     *  in-process driver instance whose state is authoritative; backend
     *  MUST NOT pre-encode. */
    text: RuntimeMessageTextSchema,
    /** Consulted ONLY when the daemon must spawn (no live handle). Same
     *  StartOptions the retired `runtime_start` carried. */
    spawn_options: StartOptionsSchema,
  })
  .strict()
  .superRefine((value, ctx) => {
    if (value.target.conversation_id !== value.conversation_id) {
      ctx.addIssue({
        code: 'custom',
        path: ['target', 'conversation_id'],
        message: 'runtime action target must match runtime_deliver conversation',
      });
    }
    if (value.spawn_options.runtime_scope.agent_id !== value.agent_id) {
      ctx.addIssue({
        code: 'custom',
        path: ['spawn_options', 'runtime_scope', 'agent_id'],
        message: 'runtime scope agent must match runtime_deliver.agent_id',
      });
    }
    const scope = value.spawn_options.runtime_scope;
    if (scope.kind === 'conversation' && scope.conversation_id !== value.conversation_id) {
      ctx.addIssue({
        code: 'custom',
        path: ['spawn_options', 'runtime_scope', 'conversation_id'],
        message: 'conversation runtime scope must match runtime_deliver target',
      });
    }
  });
export type RuntimeDeliverParams = z.infer<typeof RuntimeDeliverParamsSchema>;

/**
 * Refused-custody result. Split into three STRICT arms discriminated on
 * `reason` so the WIRE SCHEMA itself enforces the cross-constraint between
 * `reason` and `cooldown_until` (Cindy P1) — not left to a handler to
 * interpret. `cooldown_until` is present iff `reason === 'cooldown'`, and
 * can never leak onto another reason.
 *
 * Every reason means "the daemon wrote nothing; backend rolls the rows
 * back to pending" — all retriable. The fatal programming-error case (a
 * driver refusing AFTER a write attempt) is a terminal async
 * `runtime_delivery_failed{reason:'driver_refused'}`, NOT a sync refusal.
 *
 * `deferred` is a fourth disposition (Stage 3): a ready handle that cannot
 * take the write *right now* — mid-turn (`gated` driver) or a prior write
 * still in flight — is enqueued in the daemon's per-handle mid-turn queue
 * and flushed at the next safe boundary. It transfers custody to the daemon
 * queue (rows stay in-flight, owner=conn), it is NOT a refusal and NOT an
 * ack. Startup-phase handles (`spawning`/`startup_pending`/not-ready-to-
 * encode/live-oneShot) are still `refused{startup_pending}` — they already
 * have a lossless DB-backed re-drive via `ready_for_drain`, so the daemon
 * never takes volatile custody there. Canonical §11.
 */
export const RuntimeDeliverRefusedResultSchema = z.discriminatedUnion('reason', [
  z
    .object({
      disposition: z.literal('refused'),
      reason: z.literal('startup_pending'),
    })
    .strict(),
  z
    .object({
      disposition: z.literal('refused'),
      reason: z.literal('draining'),
    })
    .strict(),
  z
    .object({
      disposition: z.literal('refused'),
      reason: z.literal('cooldown'),
      /** Wall-clock epoch ms at which the init cooldown expires. Must be
       *  strictly in the future at emit time. Required on this arm ONLY. */
      cooldown_until: z.number().int().positive(),
    })
    .strict(),
]);
export type RuntimeDeliverRefusedResult = z.infer<typeof RuntimeDeliverRefusedResultSchema>;
/** Why the daemon refused custody. Derived from the refused arms
 *  (single source), so consumers can `switch` exhaustively without a
 *  second enum drifting from the schema. */
export type RuntimeDeliverRefusedReason = RuntimeDeliverRefusedResult['reason'];

/**
 * Synchronous custody disposition. `spawning`/`admitted` are single strict
 * arms; `refused` fans out over `reason` (RuntimeDeliverRefusedResultSchema).
 *
 * Outer combinator is `z.union`, NOT `discriminatedUnion('disposition')`:
 * the refused arm needs its own `reason` discriminator to express the
 * `cooldown_until` cross-constraint, and zod cannot nest a discriminated
 * union inside a discriminatedUnion arm. With only three top-level shapes
 * the union's parse cost is trivial, and it expresses the cross-constraint
 * that a flat optional field could not.
 *
 * LOAD-BEARING INVARIANT (§10.3): this carries NO ack / `delivered_at`
 * semantics. It transfers or refuses OWNERSHIP, it does not confirm
 * delivery. "Sent to daemon" ≠ ack.
 *   - `spawning` — daemon spawned; this batch is the startup delivery
 *     (the EXISTING startup-delivery mechanism, not a new queue). Rows
 *     stay in-flight (owner=conn) until an async `consumed` lands.
 *   - `admitted` — handle was ready; daemon encoded + wrote stdin. Rows
 *     stay in-flight until an async `consumed` lands.
 *   - `deferred` — handle was ready but could not take the write now
 *     (mid-turn `gated`, or a prior write still in flight); daemon
 *     enqueued the batch in its per-handle mid-turn queue and will flush
 *     at the next safe boundary. Rows stay in-flight (owner=conn) until an
 *     async `consumed` lands — custody transfer, NOT an ack (§11).
 *   - `refused` — daemon took nothing. Backend rolls the batch back to
 *     pending immediately (retriable).
 */
export const RuntimeDeliverResultSchema = z.union([
  z
    .object({
      disposition: z.literal('spawning'),
      pid: z.number().int(),
      started_at: z.number().int(),
    })
    .strict(),
  z
    .object({
      disposition: z.literal('admitted'),
      pid: z.number().int(),
      started_at: z.number().int(),
    })
    .strict(),
  z
    .object({
      disposition: z.literal('deferred'),
      pid: z.number().int(),
      started_at: z.number().int(),
    })
    .strict(),
  RuntimeDeliverRefusedResultSchema,
]);
export type RuntimeDeliverResult = z.infer<typeof RuntimeDeliverResultSchema>;

// ── unified delivery ack events (Stage 2, #task:1417) ───────────────
//
// The SINGLE async ack/rollback family, keyed by `delivery_ids`. It
// subsumed the retired `runtime_startup_delivery_consumed`/`_failed` pair
// AND the old `runtime_wake {delivered:true}` synchronous ack into one
// family (both deleted in Stage 2c), regardless of whether delivery went
// via spawn or ready-write.
//
//   consumed → backend `markDelivered` (flips `delivered_at`).
//   failed   → backend rolls back to pending (retriable) or marks failed
//              (fatal, e.g. `driver_refused` programming error).
//
// These are the ONLY things that move `delivered_at`. Stage 2 consumed
// sources: startup init-proof (`session_init` for `requires_first_message`
// drivers, preserving the B3 gate; write success for self-init drivers) and
// ready-write success on the `admitted` path. See §10.4.
//
// Wired (Stage 2a): these are in `EventFrameSchema` below and the daemon
// emits them via `deliver()`'s consume path; the backend event-handler has
// its exhaustiveness cases. Stage 2c retired the legacy
// `runtime_startup_delivery_*` pair — this unified family is now the ONLY
// delivery-ack surface.

export const RuntimeDeliveryConsumedEventSchema = z
  .object({
    event: z.literal('runtime_delivery_consumed'),
    runtime_scope: RuntimeScopeSchema,
    conversation_id: z.string().min(1),
    agent_id: z.string().min(1),
    runtime_generation: z.number().int().positive(),
    delivery_ids: z.array(z.number().int()).min(1),
    ts: z.number().int(),
  })
  .strict();
export type RuntimeDeliveryConsumedEvent = z.infer<typeof RuntimeDeliveryConsumedEventSchema>;

export const RuntimeDeliveryFailedReasonSchema = z.enum([
  'child_exit',
  'init_watchdog',
  'write_failed',
  'driver_refused',
]);
export type RuntimeDeliveryFailedReason = z.infer<typeof RuntimeDeliveryFailedReasonSchema>;

export const RuntimeDeliveryFailedEventSchema = z
  .object({
    event: z.literal('runtime_delivery_failed'),
    runtime_scope: RuntimeScopeSchema,
    conversation_id: z.string().min(1),
    agent_id: z.string().min(1),
    runtime_generation: z.number().int().positive(),
    delivery_ids: z.array(z.number().int()).min(1),
    reason: RuntimeDeliveryFailedReasonSchema,
    ts: z.number().int(),
  })
  .strict();
export type RuntimeDeliveryFailedEvent = z.infer<typeof RuntimeDeliveryFailedEventSchema>;

// ── runtime_status ──────────────────────────────────────────────────

export const RuntimeStatusParamsSchema = z
  .object({
    conversation_id: z.string().min(1).optional(),
    agent_id: z.string().min(1).optional(),
  })
  .strict();
export type RuntimeStatusParams = z.infer<typeof RuntimeStatusParamsSchema>;

export const RuntimeStatusResultSchema = z
  .object({
    runtimes: z.array(RuntimeStateDtoSchema),
  })
  .strict();
export type RuntimeStatusResult = z.infer<typeof RuntimeStatusResultSchema>;

// ── runtime_clear_session ───────────────────────────────────────────

export const RuntimeClearSessionParamsSchema = z
  .object({
    runtime_scope: RuntimeScopeSchema,
    runtime_generation: z.number().int().positive(),
  })
  .strict();
export type RuntimeClearSessionParams = z.infer<typeof RuntimeClearSessionParamsSchema>;

export const RuntimeClearSessionResultSchema = z.object({ cleared: z.boolean() }).strict();
export type RuntimeClearSessionResult = z.infer<typeof RuntimeClearSessionResultSchema>;

// ── runtime_stop_all / runtime_close_activity_log ───────────────────

export const RuntimeStopAllParamsSchema = z.object({}).strict();
export type RuntimeStopAllParams = z.infer<typeof RuntimeStopAllParamsSchema>;

export const RuntimeStopAllResultSchema = z
  .object({ stopped_count: z.number().int().nonnegative() })
  .strict();
export type RuntimeStopAllResult = z.infer<typeof RuntimeStopAllResultSchema>;

export const RuntimeCloseActivityLogParamsSchema = z.object({}).strict();
export type RuntimeCloseActivityLogParams = z.infer<typeof RuntimeCloseActivityLogParamsSchema>;

export const RuntimeCloseActivityLogResultSchema = z.object({ flushed: z.boolean() }).strict();
export type RuntimeCloseActivityLogResult = z.infer<typeof RuntimeCloseActivityLogResultSchema>;

// ── output flow control (intents bodies present, v0 unused) ─────────

export const RuntimeOutputPauseParamsSchema = z
  .object({
    runtime_scope: RuntimeScopeSchema,
  })
  .strict();
export type RuntimeOutputPauseParams = z.infer<typeof RuntimeOutputPauseParamsSchema>;

export const RuntimeOutputPauseResultSchema = z.object({ paused: z.boolean() }).strict();
export type RuntimeOutputPauseResult = z.infer<typeof RuntimeOutputPauseResultSchema>;

export const RuntimeOutputResumeParamsSchema = RuntimeOutputPauseParamsSchema;
export type RuntimeOutputResumeParams = RuntimeOutputPauseParams;

export const RuntimeOutputResumeResultSchema = z.object({ resumed: z.boolean() }).strict();
export type RuntimeOutputResumeResult = z.infer<typeof RuntimeOutputResumeResultSchema>;

// ── host_outbound_attach (Stage D3.a Commit 3) ─────────────────────
//
// Late-attach control intent the desktop main process uses to tell a
// running daemon "you now have credentials; load this host config and
// open the outbound websocket". Decouples daemon spawn from credential
// availability so the desktop launcher does not have to either (a)
// hold the spawn until after login, or (b) spawn the daemon with a
// stale `daemon-config.json` left over from a previous user.
//
// Contract:
//   - `config_path` MUST be an absolute path to a regular file (a
//     symlink-to-file is fine; a directory is rejected).
//   - The daemon validates the path, reads + parses the config, and
//     **only on success** swaps the active outbound client. If
//     validation or parsing fails, the existing outbound client (if
//     any) keeps running untouched — a bad Settings change must not
//     take down a working daemon.
//   - Idempotency:
//       - same path, client already attached → `{ attached: true,
//         action: 'noop' }`.
//       - same path, client not yet started → `{ attached: true,
//         action: 'started' }`.
//       - different valid path → `{ attached: true, action:
//         'switched' }` (graceful stop of the previous client, then
//         start with the new config).
//   - Errors carry a typed `reason` so callers don't have to string-
//     match. None of the error reasons cause side effects on the
//     existing client.

export const HostOutboundAttachParamsSchema = z
  .object({
    config_path: z.string().min(1),
  })
  .strict();
export type HostOutboundAttachParams = z.infer<typeof HostOutboundAttachParamsSchema>;

export const HostOutboundAttachFailureReasonSchema = z.enum([
  'path_not_absolute',
  'path_not_found',
  'path_not_a_file',
  'parse_failed',
  'invalid_server_url',
]);
export type HostOutboundAttachFailureReason = z.infer<typeof HostOutboundAttachFailureReasonSchema>;

export const HostOutboundAttachResultSchema = z.discriminatedUnion('attached', [
  z
    .object({
      attached: z.literal(true),
      config_path: z.string().min(1),
      /**
       * What the daemon did with the request. Mirrors the idempotency
       * matrix above so callers can log it and tests can assert
       * branch coverage.
       */
      action: z.enum(['started', 'switched', 'noop']),
    })
    .strict(),
  z
    .object({
      attached: z.literal(false),
      reason: HostOutboundAttachFailureReasonSchema,
      message: z.string().optional(),
    })
    .strict(),
]);
export type HostOutboundAttachResult = z.infer<typeof HostOutboundAttachResultSchema>;

// ── host_outbound_detach (Stage D3.b Commit 3) ─────────────────────
//
// Counterpart to `host_outbound_attach`: tell a running daemon to
// gracefully stop its currently-attached outbound websocket so the
// backend can drop the attachment without waiting for heartbeat /
// socket-close timeout. The desktop main process sends this from its
// `before-quit` cleanup path as a **latency optimization hint** — it
// is NOT a backend correctness dependency. The server-side cleanup
// still falls back to WS-close detection and heartbeat timeout if
// the hint never lands.
//
// Contract:
//   - No params. Detach is unconditional: there is nothing to
//     validate, no file to read, no URL to parse.
//   - Idempotent. With an active client: stop it cleanly, return
//     `action: 'detached'`. With no client: noop, return
//     `action: 'noop'`. Both arms have `detached: true` because the
//     post-condition ("no active outbound client") is satisfied
//     either way.
//   - **Domain result has no failure arm** because there are no
//     input or I/O failure modes. Transport / dispatcher errors
//     (`shutting_down`, `internal_error`, `closed`, network etc.)
//     still surface as `ResponseError` frames per the standard
//     response envelope — callers must treat those as failures, not
//     as a missing result.
//   - Detach does NOT make the daemon's `OutboundSupervisor`
//     terminal. The supervisor can be re-attached later via
//     `host_outbound_attach` in the same daemon process (e.g.
//     log-out → log-in-as-different-user). Daemon shutdown is a
//     separate path — `OutboundSupervisor.stop()` (called from
//     `runDaemon` shutdown) is the one-way close.

export const HostOutboundDetachParamsSchema = z.object({}).strict();
export type HostOutboundDetachParams = z.infer<typeof HostOutboundDetachParamsSchema>;

export const HostOutboundDetachResultSchema = z
  .object({
    detached: z.literal(true),
    /**
     * `detached` = there was an active outbound client and it was
     *              stopped as part of this call.
     * `noop`     = no active client (pre-attach state, or already
     *              detached). The hint is harmless either way.
     */
    action: z.enum(['detached', 'noop']),
  })
  .strict();
export type HostOutboundDetachResult = z.infer<typeof HostOutboundDetachResultSchema>;

// ── host_outbound_reload (Stage D3.c Commit 2) ─────────────────────
//
// Tell a running daemon "re-read the SAME daemon-config.json you're
// already attached to, validate it, and swap to the new bytes if and
// only if everything checks out". The desktop main process sends this
// from its Settings save path AFTER it has atomically rewritten
// `daemon-config.json` (write-temp + fsync + rename → reload IPC).
//
// Why a separate intent rather than re-using `host_outbound_attach`:
//   - **Hard-rejects `not_attached`**: if no client is currently
//     attached, reload must FAIL — it MUST NOT silently fall through
//     to "attach". The caller (desktop main) needs to decide
//     explicitly whether the right verb here is `attach`
//     (post-login bootstrap) or `reload` (Settings save / token
//     rotation). Silent fallback hides logout-races and stale-state
//     bugs.
//   - **Validate-first-then-swap**: any I/O / parse / URL validation
//     failure leaves the existing client running untouched. A bad
//     Settings save must not take down a working outbound — the user
//     gets a typed error in Settings, and the daemon keeps streaming.
//   - **Result success ≠ connected**: `action: 'reloaded'` means the
//     daemon started a NEW client with the new config. WS open /
//     attachment-fresh is a separate signal the desktop verifies via
//     backend `HostWsServerController.getAttachment(hostId)` after
//     the reload returns (Commit 3 concern; this intent's job is to
//     hand the new bytes to the daemon, not to wait for socket
//     handshake).
//
// Contract:
//   - No params: reload re-reads the daemon's CURRENT config_path.
//     Path changes belong to `host_outbound_attach` — by design,
//     reload cannot swap to a different config file.
//   - Success arms:
//       - `action: 'reloaded'` — config bytes differ from the
//         in-memory fingerprint; new client started after old client
//         was stopped (validate-first-then-swap order strict).
//       - `action: 'unchanged'` — config bytes are byte-identical
//         to the in-memory fingerprint (rotate / no-op). Old client
//         keeps running uninterrupted; no WS bounce.
//   - Failure arms (typed business state, NOT ResponseError):
//       - `not_attached`         — no current client; caller must use
//                                  `host_outbound_attach` instead.
//       - `config_unreadable`    — disk read failed (ENOENT after
//                                  attach, EACCES, FIFO substitution,
//                                  etc.). Old client kept.
//       - `parse_failed`         — config JSON / shape invalid. Old
//                                  client kept.
//       - `invalid_server_url`   — `hostConnectWebSocketUrl` threw on
//                                  the NEW server_url (e.g. user edited
//                                  to insecure remote HTTP). Old client
//                                  kept.
//
// Result shape mirrors `host_outbound_attach`: discriminated on a
// boolean flag so callers branch on `result.reloaded` without
// inspecting nested fields. Failure arm includes `message` for log /
// diagnostic continuity; success arm includes `host_id` so the
// caller (desktop main, Commit 3) can hand it straight to the
// backend attachment-freshness check without re-reading the config.

export const HostOutboundReloadParamsSchema = z.object({}).strict();
export type HostOutboundReloadParams = z.infer<typeof HostOutboundReloadParamsSchema>;

export const HostOutboundReloadFailureReasonSchema = z.enum([
  'not_attached',
  'config_unreadable',
  'parse_failed',
  'invalid_server_url',
]);
export type HostOutboundReloadFailureReason = z.infer<typeof HostOutboundReloadFailureReasonSchema>;

export const HostOutboundReloadResultSchema = z.discriminatedUnion('reloaded', [
  z
    .object({
      reloaded: z.literal(true),
      /**
       * `reloaded`  — config bytes differed; old client stopped, new
       *               client started. WS handshake not yet complete.
       * `unchanged` — config bytes identical to in-memory fingerprint;
       *               old client kept untouched. Caller can skip the
       *               attachment-freshness poll Commit 3 runs for the
       *               `reloaded` case (nothing reconnected).
       */
      action: z.enum(['reloaded', 'unchanged']),
      /**
       * The config_path the reload was performed against — echoed back
       * for log correlation. Always the daemon's currently-attached
       * path; reload cannot change paths.
       */
      config_path: z.string().min(1),
      /**
       * `host_id` from the (re)loaded config. The caller uses it as
       * the key for backend `HostWsServerController.getAttachment`
       * polling after a `reloaded` response. For `unchanged` it's the
       * same host_id that was already attached — caller can use it to
       * sanity-check no concurrent attach swapped under it.
       */
      host_id: z.string().min(1),
    })
    .strict(),
  z
    .object({
      reloaded: z.literal(false),
      reason: HostOutboundReloadFailureReasonSchema,
      /** Free-text diagnostic. Renderer should branch on `reason`
       *  for UI copy; `message` is for logs / dev console only. */
      message: z.string().optional(),
    })
    .strict(),
]);
export type HostOutboundReloadResult = z.infer<typeof HostOutboundReloadResultSchema>;

// ── host_proxy_config_* (daemon-host local truth) ──────────────────
//
// Proxy configuration is daemon-host local truth: it lives in a file on
// the physical computer that spawns runtimes. The server owns authz and
// dispatch only; it does not persist proxy bytes. The URL is an opaque
// host-local value and is intentionally not parsed for credentials.

export const HostProxyConfigSchema = SharedHostProxyConfigSchema;
export type HostProxyConfig = z.infer<typeof HostProxyConfigSchema>;

export const HostProxyConfigGetParamsSchema = z.object({}).strict();
export type HostProxyConfigGetParams = z.infer<typeof HostProxyConfigGetParamsSchema>;

export const HostProxyConfigGetResultSchema = z.object({ config: HostProxyConfigSchema }).strict();
export type HostProxyConfigGetResult = z.infer<typeof HostProxyConfigGetResultSchema>;

export const HostProxyConfigSetParamsSchema = HostProxyConfigSchema;
export type HostProxyConfigSetParams = z.infer<typeof HostProxyConfigSetParamsSchema>;

export const HostProxyConfigSetResultSchema = HostProxyConfigGetResultSchema;
export type HostProxyConfigSetResult = HostProxyConfigGetResult;

// ── D4.d: agent_materialize (server↔daemon formal semantics) ──────
//
// Unlike the runtime_* intents (local supervisor / control —
// scheduling, lifecycle, output paging) and the `host_outbound_*`
// intents (transport plumbing), `agent_materialize` IS formal
// server↔daemon semantics by design: it is the canonical way the
// server asks the daemon-owning host to create an agent's L1 home
// (= the DM workspace path; CLAUDE.md §3 / docs/agent-system-prompt-
// memory.md). The schemas live here so the IPC dispatcher can
// validate them through the same `intentSchemas` catalogue used by
// every other intent; the outbound WS frame layer re-exports them
// from `outbound-protocol.ts` so the wire surface (additive,
// per-command error scoping — see Cindy D1 round-1 P1) stays a
// thin envelope around the same Zod definitions.
//
// See `docs/daemon-outbound-protocol.md` (§ "agent_materialize command").

export const AgentMaterializeParamsSchema = z
  .object({
    /**
     * Server-allocated; base64url 16-char string from
     * randomBytes(12).toString('base64url')
     * (packages/backend/src/repos/agent-repo.ts).
     * Schema-level validation is intentionally loose (`min(1)`);
     * the route layer is responsible for tighter format checks
     * (Cindy round-2 P1 #5).
     */
    agent_id: z.string().min(1),
    /**
     * Server-allocated; follows the existing helper
     * `agentHomeWorkspaceId(agentId)` → `wsah_<agentId>`.
     */
    agent_home_workspace_id: z.string().min(1),
    name: z.string().min(1).max(64),
    runtime: RuntimeKindSchema,
    model: z.string().nullable().optional(),
    description: z.string().nullable().optional(),
    /**
     * Template choice for the agent-home bootstrap (system_prompt,
     * MEMORY.md, notes/ skeleton). D2 implements 'default' only;
     * 'onboarding' and 'minimal' return `unsupported_template`
     * until they get a template.
     */
    template_choice: z.enum(['default', 'onboarding', 'minimal']).default('default'),
  })
  .strict();
export type AgentMaterializeParams = z.infer<typeof AgentMaterializeParamsSchema>;

export const AgentMaterializeResultSchema = z
  .object({
    ok: z.literal(true),
    /**
     * Absolute path to the materialized agent home / L1 dir,
     * daemon-side. The server never inspects what the daemon
     * wrote here — `agent_dir` is opaque from the server's
     * perspective.
     */
    agent_dir: z.string().min(1),
    /**
     * MUST equal `agent_dir`. The agent-home (DM) workspace's local
     * realization IS the agent's L1 dir — an agent-home id resolves to
     * that dir itself rather than to a nested `workspaces/<id>/` bundle
     * (`docs/agent-system-prompt-memory.md`,
     * `workspace-layout-relocation-v1.md` §3.2) — so the daemon returns
     * the same string for both fields to make the coincidence explicit
     * on the wire.
     *
     * The server does NOT store either string: relocation-v1 §3.4
     * retired the `workspaces.workspace_path` column, so there is no row
     * for a path to land in. The one backend use is the §6.5 equality
     * cross-check in `create-agent.ts` — on mismatch it flips the agent
     * to `failed` (`daemon_internal_error`) and logs both strings as
     * diagnostics; neither is persisted, parsed, or joined onto.
     *
     * Cindy D2 P1-2: the equality is enforced by the schema's
     * `superRefine` below, NOT just by convention. With no column left
     * to corrupt, what the constraint still catches is a handler (or a
     * future stub) that has silently split the two coordinates apart —
     * the wire is the only place that split is observable at all.
     */
    agent_home_workspace_path: z.string().min(1),
    /**
     * Opaque-to-server timestamp used only for audit. The row's
     * `created_at` remains the server's authoritative time.
     */
    materialized_at: z.number().int().positive(),
  })
  .strict()
  .superRefine((value, ctx) => {
    if (value.agent_home_workspace_path !== value.agent_dir) {
      ctx.addIssue({
        code: 'custom',
        path: ['agent_home_workspace_path'],
        message:
          'agent_home_workspace_path must equal agent_dir (§6.5 invariant: the DM workspace path IS the L1 dir)',
      });
    }
  });
export type AgentMaterializeResult = z.infer<typeof AgentMaterializeResultSchema>;

/**
 * Typed error codes returned by the daemon's `agent_materialize`
 * handler. The HTTP-layer wrapper in `POST /api/agents` adds
 * server-side codes (materialize_timeout, host_attach_pending,
 * dependent_row_insert_failed, backend_crash_during_materialize)
 * that never appear on the wire — they describe failures the
 * server observes locally. See design §3.4 for the
 * wire→HTTP mapping table.
 */
export const AgentMaterializeErrorCodeSchema = z.enum([
  'agent_deleting',
  'fs_quota_exceeded',
  'fs_unavailable',
  /**
   * `agent_dir_collision` (not `already_exists`): the dir is
   * unexpectedly populated by foreign content without the
   * `.materialized` sentinel, or the sentinel exists but does
   * not match the requested (agent_id, template_choice) identity
   * (sentinel "template_match" per §4.6 / §14 Q3). Runtime is
   * materialization-time provenance and may change later.
   * Idempotent re-call of an already-materialized `agent_id`
   * (both identity fields match) returns `ok: true` and is
   * NOT an error.
   */
  'agent_dir_collision',
  'unsupported_template',
  'unsupported_runtime',
  'internal_error',
]);
export type AgentMaterializeErrorCode = z.infer<typeof AgentMaterializeErrorCodeSchema>;

/**
 * Canonical generic-error code taxonomy. The 5 transport-level codes
 * the daemon may emit independent of any intent. Kept small and
 * stable so the client can branch on them without scanning
 * `message` strings:
 *
 *  - `unknown_intent`     — `intent` name not in the registry
 *  - `invalid_params`     — params failed zod validation
 *  - `internal_error`     — handler threw / returned an unexpected error
 *  - `shutting_down`      — request arrived after SIGTERM/SIGINT
 *  - `request_too_large`  — incoming frame exceeded the per-frame cap
 *
 * Single source of truth — referenced by `ResponseErrorSchema`
 * (generic frame) AND by per-intent typed-error schemas (e.g.
 * `AgentMaterializeIpcErrorSchema`) so a new generic code added here
 * automatically widens both surfaces. Cindy D2 round-2 P2 caught
 * the previous hand-copy in `AgentMaterializeIpcErrorSchema`; the
 * extract removes that drift axis.
 */
export const ResponseErrorCodeSchema = z.enum([
  'unknown_intent',
  'invalid_params',
  'internal_error',
  'shutting_down',
  'request_too_large',
]);
export type ResponseErrorCode = z.infer<typeof ResponseErrorCodeSchema>;

/**
 * Per-intent typed-error schemas.
 *
 * Default = `ResponseErrorSchema` (the 5 transport-level codes).
 * Intents that may legitimately surface daemon-business typed
 * codes — `agent_materialize` and the workspace data-plane
 * intents (`agent_workspace_index`,
 * `agent_l2_seed`, `agent_files_purge`) — override with a wider schema that unions
 * the 5 generic codes with the intent's own taxonomy (e.g.
 * `AgentMaterializeErrorCodeSchema`).
 *
 * Why per-intent and NOT a single global widened union (Cindy
 * D2 P1-1 + D1 round-1 P1): a global union would let any intent
 * carry any other intent's typed code on the wire — exactly the
 * scoping failure Cindy caught at D1 round-1. Per-intent scoping
 * keeps each intent's error surface narrow at validation time.
 *
 * This registry is consumed by:
 *   - `IntentDispatcher.dispatch` (daemon side) — to validate the
 *     error code it's about to emit on the wire.
 *   - `HostClient.handleFrame` (client side) — to validate
 *     incoming `error.code` against the intent the caller
 *     dispatched, before resolving as a typed `HostClientError`.
 *   - `SocketTransport.deliverFrame` does NOT consult this map;
 *     it uses `LooseAnyFrameSchema` so a typed code doesn't get
 *     silently dropped at the transport layer. Validation happens
 *     at the client layer above, where the intent name is known.
 */
const AgentMaterializeIpcErrorSchema = z
  .object({
    code: z.union([ResponseErrorCodeSchema, AgentMaterializeErrorCodeSchema]),
    message: z.string(),
  })
  .strict();

// ── D4.e-6: runtime spawn typed-error taxonomy ───────────────────────
//
// Before D4.e-6 runtime spawn shipped paths and could not fail on path
// resolution, so `runtime_start` used only the 5 generic codes. Now the daemon
// resolves `workspace.path`/`home_dir` itself and can fail on path resolution
// or cwd admission — design §3.5 / Cindy criterion 2. The codes below form the
// resolver taxonomy. Both `runtime_start` and `runtime_deliver` may spawn, so
// both declare it; a typed code still cannot leak onto a non-spawning intent's
// wire error.
//
// relocation-v1 §7 task 4 + shiki/Cindy 2026-08-04 pivot: there is no registry
// establishment gate any more, and L1/L2 CONTENT is NEVER a spawn gate. The
// session cwd is the ONLY hard-ensured runtime base — the supervisor `mkdir -p`s
// the deterministic bundle coordinate and requires a usable/enterable directory.
// A missing/half-seeded MEMORY.md still spawns (the prompt just omits the L2
// section).
//   - `workspace_bundle_missing`: the daemon resolved a semantic workspace ref
//     but could NOT hard-ensure its session cwd — the OS cannot spawn a child
//     there (the path is occupied by a non-directory file, or the dir cannot be
//     created/entered). This is process admission, NOT an L2/memory check. The
//     concrete host path is a host-only diagnostic (logged daemon-side) and
//     never rides the wire `message` — the typed error names only the semantic
//     id + kind (relocation-v1 §7 task 3).
//   - `registry_unavailable`: an unsafe workspace id segment that can never form
//     a bundle coordinate (path-safety fail-closed), or the daemon is not
//     attached to a control plane so the authenticated computer id cannot be
//     resolved. Hard fail-closed.
//   - `workspace_not_provisioned`: an `agent_home` ref that does not belong to
//     this agent (a foreign/corrupt ref refused rather than resolved onto this
//     agent's L1). NOTE: no longer emitted for a "regular workspace not bound"
//     case — that gate was removed with the registry demotion; it survives only
//     as the agent-home ownership boundary. (Kept in the enum; a broader
//     epoch-wide narrowing of the shared code is tracked with #1807/#1808.)
//   - `runtime_binary_missing`: the runtime's CLI is not installed / not on PATH.
//   - `runtime_version_unsupported`: the installed CLI parses as older than the
//     driver's `minimumVersion`. An unparseable/unknown version is NOT refused.
export const RuntimeStartErrorCodeSchema = z.enum([
  'agent_deleting',
  'workspace_not_provisioned',
  'workspace_bundle_missing',
  'registry_unavailable',
  'runtime_binary_missing',
  'runtime_version_unsupported',
]);
export type RuntimeStartErrorCode = z.infer<typeof RuntimeStartErrorCodeSchema>;

const RuntimeSpawnIpcErrorSchema = z
  .object({
    code: z.union([ResponseErrorCodeSchema, RuntimeStartErrorCodeSchema]),
    message: z.string(),
  })
  .strict();

/** Per-intent widened wire error for `agent_stale_receive` (#task:1614);
 *  taxonomy documented at `AgentStaleReceiveErrorCodeSchema`. */
const AgentStaleReceiveIpcErrorSchema = z
  .object({
    code: z.union([ResponseErrorCodeSchema, AgentStaleReceiveErrorCodeSchema]),
    message: z.string(),
  })
  .strict();

// ── agent_workspace_index ─────────────────────────────────────────────
//
// Data-plane READ intent that makes the daemon the sole authority for
// host-local path resolution of regular workspaces (relocation-v1 §3.4).
// Server↔daemon formal semantics like `agent_materialize` (NOT runtime_*
// control / host_outbound_* plumbing): the backend speaks only semantic ids;
// the daemon resolves the absolute path via its own layout. Per-command typed
// error scoping mirrors agent_materialize.
//
// Workspace creation is a pure DB write. The daemon resolves regular
// workspaces by their deterministic agent-centric bundle coordinate; there is
// no registry establishment or reconciliation step.

export const AgentWorkspaceIndexParamsSchema = z
  .object({
    agent_id: z.string().min(1),
    /** Workspace ids the backend's ACL/DB list already authorized. */
    workspace_ids: z.array(z.string().min(1)),
  })
  .strict();
export type AgentWorkspaceIndexParams = z.infer<typeof AgentWorkspaceIndexParamsSchema>;

export const AgentWorkspaceIndexEntrySchema = z
  .object({
    workspace_id: z.string().min(1),
    /**
     * Daemon-resolved absolute `agent_dir` for this (agent, workspace):
     * agent-home → the agent's L1 dir; regular → the deterministic
     * agent-centric bundle coordinate `<agent home>/workspaces/
     * <workspace_id>` (relocation-v1 §3.2/§7). Stamped daemon-side; the
     * backend has no path to assemble one from and must not invent one (I1).
     */
    agent_dir: z.string().min(1),
  })
  .strict();
export type AgentWorkspaceIndexEntry = z.infer<typeof AgentWorkspaceIndexEntrySchema>;

export const AgentWorkspaceIndexResultSchema = z
  .object({
    ok: z.literal(true),
    entries: z.array(AgentWorkspaceIndexEntrySchema),
    /**
     * Workspaces whose `agent_dir` this daemon does not report. Since
     * relocation-v1 §7-4 this is NOT a provisioning/adoption verdict — the
     * coordinate is always computable — it is a CWD AVAILABILITY fact: the
     * bundle is not a usable working directory on this computer right now
     * (the normal lazy window before the first spawn there), or its id
     * segment could never form a safe coordinate. The backend surfaces these
     * as `agent_dir: null`; it MUST NOT synthesize a path of its own (I1).
     */
    unresolved: z.array(z.string().min(1)),
  })
  .strict();
export type AgentWorkspaceIndexResult = z.infer<typeof AgentWorkspaceIndexResultSchema>;

export const AgentWorkspaceIndexErrorCodeSchema = z.enum([
  /**
   * The daemon has no attached computer yet, so it has no agents root to
   * resolve ANY entry under (`agentsRootFor`) — the whole call fails closed.
   * Despite the legacy name this is not a registry consult (relocation-v1 §7
   * task 4 demoted the registry): a per-entry unresolvable/unsafe id degrades
   * to the `unresolved` list instead, never a whole-call failure.
   */
  'registry_unavailable',
  'internal_error',
]);
export type AgentWorkspaceIndexErrorCode = z.infer<typeof AgentWorkspaceIndexErrorCodeSchema>;

const AgentWorkspaceIndexIpcErrorSchema = z
  .object({
    code: z.union([ResponseErrorCodeSchema, AgentWorkspaceIndexErrorCodeSchema]),
    message: z.string(),
  })
  .strict();

// ── #task:811 — agent_workspace_tree + agent_workspace_file_read ─────
// Two-protocol read pair backing the two-column AgentWorkspacePane: one
// intent returns the flat tree of every visible file under the agent
// home dir; the other reads any of those files by virtual path. This is
// intentionally NOT segmented by `system_prompt`/`MEMORY`/`notes` — the
// protocol speaks one virtual path namespace, the UI categorizes (or
// doesn't).
//
// Forward-compat: a future expansion that exposes a different file
// root (e.g. an L2 workspace dir) MUST be additive selectors (`scope`,
// `workspace_id`) — never reinterpret an existing path. Today
// `path: "MEMORY.md"` unambiguously means the agent-home `MEMORY.md`,
// and that meaning is frozen.
//
// Security: every input path is validated by
// `parseAndResolveVirtualPath` daemon-side — segment-by-segment shape
// rules (no empty/`.`/`..`/dotfile/NUL/backslash), per-segment lstat
// to refuse symlinks intermediate or leaf, and a final
// `path.relative` boundary check against the realpath of the agent
// home base. Fail-closed: any escape is `forbidden_path`, never a
// silent fallthrough.

export const AgentWorkspaceTreeParamsSchema = z
  .object({
    /** The agent whose home dir is walked. The daemon owns path
     *  resolution (`resolveAgentDir`) — no server-derived path
     *  crosses the wire. */
    agent_id: z.string().min(1),
    /** Optional L2 scope. Omitted = the agent home/L1 dir. Present =
     *  resolve this workspace id daemon-side to the agent's per-workspace
     *  memory dir. Agent-home/DM workspace ids naturally resolve back to the
     *  same home dir; regular workspace ids resolve to their DETERMINISTIC
     *  agent-centric bundle coordinate (`resolveAgentWorkspaceBundleDir`) — no
     *  registry establishment gate. No filesystem path crosses the wire. */
    workspace_id: z.string().min(1).optional(),
  })
  .strict();
export type AgentWorkspaceTreeParams = z.infer<typeof AgentWorkspaceTreeParamsSchema>;

export const AgentWorkspaceTreeResultSchema = z
  .object({
    ok: z.literal(true),
    /** Flat list of every visible entry under the agent home dir,
     *  ordered (parent-dir, kind=directory-first, name). Hidden
     *  entries (anything starting with '.') and symlinks (intermediate
     *  or leaf) are filtered server-side. */
    tree: AgentWorkspaceTreeSchema,
  })
  .strict();
export type AgentWorkspaceTreeResult = z.infer<typeof AgentWorkspaceTreeResultSchema>;

export const AgentWorkspaceFileReadParamsSchema = z
  .object({
    /** The agent whose home dir is the read base. */
    agent_id: z.string().min(1),
    /** Optional L2 scope. Omitted = read from the agent home/L1 dir. Present =
     *  read from the daemon-resolved per-workspace memory dir. */
    workspace_id: z.string().min(1).optional(),
    /** Virtual path under the agent home (e.g. `system_prompt.md`,
     *  `notes/work-log.md`). The daemon parses + resolves with the
     *  per-segment rules described above. */
    path: z.string().min(1),
  })
  .strict();
export type AgentWorkspaceFileReadParams = z.infer<typeof AgentWorkspaceFileReadParamsSchema>;

export const AgentWorkspaceFileReadResultSchema = z
  .object({
    ok: z.literal(true),
    /** Single-file read: bounded preview (daemon-side cap) with a `truncated` flag the
     *  renderer surfaces as a banner. */
    file: AgentWorkspaceFileReadSchema,
  })
  .strict();
export type AgentWorkspaceFileReadResult = z.infer<typeof AgentWorkspaceFileReadResultSchema>;

/**
 * Shared error taxonomy for the two new intents. The daemon never
 * distinguishes "your path was malformed" from "you tried to escape"
 * on the wire — both look like `forbidden_path` so a probe can't use
 * the error class to enumerate filesystem layout.
 */
export const AgentWorkspaceFsErrorCodeSchema = z.enum([
  /** Caller-supplied path is shape-rejected OR resolution would escape
   *  the agent home base (symlink in chain, realpath crossed the
   *  boundary). Route projects 400/403. */
  'forbidden_path',
  /** Graceful missing along the walk (ENOENT/ENOTDIR on a non-leaf,
   *  the leaf is a directory when the read endpoint asked for a file,
   *  etc). Route projects 404. */
  'not_found',
  /** The daemon has no attached computer yet (the machine has not bound to a
   *  server), so its agents root — and therefore any agent memory tree — is
   *  unavailable. relocation-v1 §7 task 4: this is NO LONGER a workspace-registry
   *  consult; a regular workspace resolves purely by its deterministic bundle
   *  coordinate, and a not-yet-materialized bundle degrades to `not_found`, never
   *  a provisioning refusal. Route projects 503 (retriable once attached). */
  'registry_unavailable',
  /** Hard host-local fs failure (perm/IO/EROFS/…). Route projects 503. */
  'fs_unavailable',
  'internal_error',
]);
export type AgentWorkspaceFsErrorCode = z.infer<typeof AgentWorkspaceFsErrorCodeSchema>;

const AgentWorkspaceFsIpcErrorSchema = z
  .object({
    code: z.union([ResponseErrorCodeSchema, AgentWorkspaceFsErrorCodeSchema]),
    message: z.string(),
  })
  .strict();

// ── D4.e-3: agent_l2_seed ────────────────────────────────────────────
// Seeds a member agent's L2 (per-workspace) memory bundle
// `<agent home>/workspaces/<workspace_id>/` (MEMORY.md + notes/) on THAT
// agent's own daemon. Replaces backend `seedAgentL2` (G1). The backend forwards
// only semantic ids; the daemon resolves the deterministic agent-centric bundle
// coordinate itself (regular-only — agent-home L2 == L1 and is already
// materialized by `agent_materialize`, so it is never seeded here) and writes
// the L2 template, which is the L1 template MINUS `system_prompt.md` (L1-only
// invariant). Idempotent: existing files are preserved, never overwritten.
export const AgentL2SeedParamsSchema = z
  .object({
    /** The member agent whose L2 dir is seeded under the workspace. */
    agent_id: z.string().min(1),
    /**
     * The REGULAR workspace to seed into. The daemon resolves its host-local
     * path to the DETERMINISTIC agent-centric bundle coordinate
     * (`<agent home>/workspaces/<wid>`) — seeding IS what materializes it, so
     * there is no establishment gate and no provisioning refusal (an
     * un-materialized bundle is created, not rejected). No path crosses the
     * wire (I1). This intent is regular-only; the backend never sends `kind`.
     */
    workspace_id: z.string().min(1),
    /** Template identifier (matches `agent_materialize`'s wire enum). */
    template_choice: z.enum(['default', 'onboarding', 'minimal']).default('default'),
  })
  .strict();
export type AgentL2SeedParams = z.infer<typeof AgentL2SeedParamsSchema>;

export const AgentL2SeedResultSchema = z
  .object({
    ok: z.literal(true),
    /** True when this call wrote at least one new file (false on an
     *  idempotent re-seed where everything already existed). */
    seeded: z.boolean(),
    /** True when the L2 `MEMORY.md` is present after the call (always true
     *  on success — a sanity flag the route/tests assert). */
    l2_present: z.boolean(),
  })
  .strict();
export type AgentL2SeedResult = z.infer<typeof AgentL2SeedResultSchema>;

export const AgentL2SeedErrorCodeSchema = z.enum([
  'agent_deleting',
  /**
   * The daemon cannot form a bundle coordinate at all: no attached computer (no
   * agents root) or an unsafe workspace id segment (path-safety fail-closed,
   * before any fs write). Legacy name — not a registry consult.
   */
  'registry_unavailable',
  /** `template_choice` is structurally valid but has no host-side entry. */
  'unsupported_template',
  /** A host-local fs write failed HARD (perm/IO/EROFS/ENOSPC). */
  'fs_unavailable',
  'internal_error',
]);
export type AgentL2SeedErrorCode = z.infer<typeof AgentL2SeedErrorCodeSchema>;

const AgentL2SeedIpcErrorSchema = z
  .object({
    code: z.union([ResponseErrorCodeSchema, AgentL2SeedErrorCodeSchema]),
    message: z.string(),
  })
  .strict();

// ── D4.e-4: agent_files_purge ────────────────────────────────────────
// Removes an agent's host-local files on the OWNING daemon. Replaces the
// backend `DELETE /api/agents/:id` `fs.rm(l1_path / l2_paths)` block (G3):
// the backend forwards only `{ agent_id }`; the daemon resolves the agent
// home tree (`<agents root>/<agent_id>` = L1 + every nested
// `workspaces/<id>` L2 bundle) itself — no fs path crosses the wire (I1).
// relocation-v1 §7 task 4 (registry demotion): the whole managed footprint is
// one tree, so the purge removes it in a single rm and consults NO registry.
// Idempotent: a target already gone (ENOENT) counts as success, not failure.
// Fail-closed (§3.3): a HARD fs error → typed `partial_purge` so the route
// keeps the DB row for retry.
export const AgentFilesPurgeParamsSchema = z
  .object({
    /** The agent whose host-local files (the whole agent home tree) are removed. */
    agent_id: z.string().min(1),
  })
  .strict();
export type AgentFilesPurgeParams = z.infer<typeof AgentFilesPurgeParamsSchema>;

export const AgentFilesPurgeResultSchema = z
  .object({
    ok: z.literal(true),
    /** Number of target dirs that existed and were removed (ENOENT targets
     *  are NOT counted — there was nothing to purge). */
    purged_count: z.number().int().nonnegative(),
    /** Runtime handles that reached a real child exit before rm began. */
    quiesced_count: z.number().int().nonnegative(),
  })
  .strict();
export type AgentFilesPurgeResult = z.infer<typeof AgentFilesPurgeResultSchema>;

export const AgentFilesPurgeErrorCodeSchema = z.enum([
  /** At least one runtime child remained alive after SIGKILL timeout. */
  'quiesce_failed',
  /**
   * The agent home tree could not be removed because of a HARD fs error
   * (perm/IO/EROFS/ENOSPC — NOT ENOENT). Fail-closed (§3.3): the route keeps
   * the DB row so a later retry has its routing anchor.
   */
  'partial_purge',
  'internal_error',
]);
export type AgentFilesPurgeErrorCode = z.infer<typeof AgentFilesPurgeErrorCodeSchema>;

const AgentFilesPurgeIpcErrorSchema = z
  .object({
    code: z.union([ResponseErrorCodeSchema, AgentFilesPurgeErrorCodeSchema]),
    message: z.string(),
  })
  .strict();

// ── #task:1135: profile MCP read / write ─────────────────────────────
// Backs `silverhand profile mcp get|set`. The subject's raw MCP config is
// host-local L1 truth (`<agents root>/<agent_id>/mcp.json`); the backend
// never stores raw bytes. Read returns the raw text (or null when absent — the
// normal "no MCP" case, NOT an error). Write validates the canonical schema
// (fail-loud `invalid_config`) and atomically replaces the file. The daemon
// resolves the L1 dir itself (`resolveAgentDir`) — no fs path crosses the wire.
export const AgentProfileMcpReadParamsSchema = z.object({ agent_id: z.string().min(1) }).strict();
export type AgentProfileMcpReadParams = z.infer<typeof AgentProfileMcpReadParamsSchema>;

export const AgentProfileMcpReadResultSchema = z
  .object({
    ok: z.literal(true),
    /** Raw file text, or null when no `mcp.json` exists yet. */
    raw: z.string().nullable(),
  })
  .strict();
export type AgentProfileMcpReadResult = z.infer<typeof AgentProfileMcpReadResultSchema>;

export const AgentProfileMcpWriteParamsSchema = z
  .object({
    agent_id: z.string().min(1),
    /** Raw JSON text to persist. The daemon re-validates (belt) and writes
     *  atomically; the backend route is the primary validation authority. */
    raw: z.string(),
  })
  .strict();
export type AgentProfileMcpWriteParams = z.infer<typeof AgentProfileMcpWriteParamsSchema>;

export const AgentProfileMcpWriteResultSchema = z
  .object({
    ok: z.literal(true),
    bytes_written: z.number().int().nonnegative(),
  })
  .strict();
export type AgentProfileMcpWriteResult = z.infer<typeof AgentProfileMcpWriteResultSchema>;

export const AgentProfileMcpErrorCodeSchema = z.enum([
  /** Raw failed canonical validation (bad JSON / schema). Write-only;
   *  fail-loud, nothing is persisted. Route projects 400. */
  'invalid_config',
  /** Hard host-local fs failure (perm/IO/EROFS). Route projects 503. */
  'fs_unavailable',
  'internal_error',
]);
export type AgentProfileMcpErrorCode = z.infer<typeof AgentProfileMcpErrorCodeSchema>;

export const AgentProfileMcpWriteErrorCodeSchema = z.enum([
  'invalid_config',
  'fs_unavailable',
  'internal_error',
  'agent_deleting',
]);
export type AgentProfileMcpWriteErrorCode = z.infer<typeof AgentProfileMcpWriteErrorCodeSchema>;

const AgentProfileMcpIpcErrorSchema = z
  .object({
    code: z.union([ResponseErrorCodeSchema, AgentProfileMcpErrorCodeSchema]),
    message: z.string(),
  })
  .strict();

const AgentProfileMcpWriteIpcErrorSchema = z
  .object({
    code: z.union([ResponseErrorCodeSchema, AgentProfileMcpWriteErrorCodeSchema]),
    message: z.string(),
  })
  .strict();

// ── #task:1137: live MCP probe ───────────────────────────────────────
// Backs `silverhand profile mcp probe` and the agent-pane "Test connection" /
// "List tools" buttons. The daemon opens ONE short-lived MCP connection to the
// named server, runs `initialize` (+ optional `tools/list`), and closes it
// immediately. The probe SOURCE is scope-discriminated so a future workspace
// layer reuses the command: v1 ships the `profile` arm only (the agent's L1
// `mcp.json`); a workspace arm slots in alongside without re-signing the
// command. The server is named by its RAW name — a probe connects to the real
// endpoint, never the `silverhand_<scope>__` runtime name.
export const McpProbeSourceSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('profile'), agent_id: z.string().min(1) }).strict(),
  // future: z.object({ kind: z.literal('workspace'), workspace_id: ... })
]);
export type McpProbeSource = z.infer<typeof McpProbeSourceSchema>;

export const McpProbeParamsSchema = z
  .object({
    source: McpProbeSourceSchema,
    /** Raw (user-visible) server name to probe. */
    server_name: z.string().min(1),
    /** When true, also run `tools/list` after `initialize`. */
    include_tools: z.boolean().optional(),
  })
  .strict();
export type McpProbeParams = z.infer<typeof McpProbeParamsSchema>;

// The probe RESULT shape is canonical in `@silverhand/shared` (shared with the
// backend route, CLI, and renderer). Re-export it here so the intent registry
// and dispatcher source all three of an intent's types from one module.
export type { McpProbeResult } from '@silverhand/shared';

// Outcome of the connect/initialize/tools walk (canonical shape in
// `@silverhand/shared`): connection/handshake/tools failures live INSIDE the
// result (`ok:false` + `phase`), they are not IPC errors.

export const McpProbeErrorCodeSchema = z.enum([
  'agent_deleting',
  /** Host-local fs fault that prevents reading the source config at all
   *  (perm/IO; ENOENT is handled as a normal `not_found` result). 503. */
  'fs_unavailable',
  'internal_error',
]);
export type McpProbeErrorCode = z.infer<typeof McpProbeErrorCodeSchema>;

const McpProbeIpcErrorSchema = z
  .object({
    code: z.union([ResponseErrorCodeSchema, McpProbeErrorCodeSchema]),
    message: z.string(),
  })
  .strict();

/**
 * Single source of truth for the intent ↔ params / result / error
 * type triple. The client uses it to enforce that
 * `host.call('echo', {...})` only accepts echo-shaped params and
 * returns echo-shaped results; the daemon dispatcher uses it to
 * parse incoming params before handing to a handler, AND to
 * validate the typed error code it is about to emit on the wire.
 *
 * `error` is OPTIONAL on the entry shape. Intents without an
 * explicit `error` entry use `ResponseErrorSchema` (the 5
 * transport-level codes) at IPC validation time. Only intents
 * that may legitimately surface daemon-business typed codes —
 * `agent_materialize` (D4.d) plus the workspace data-plane
 * intents `agent_workspace_index` /
 * `agent_l2_seed` / `agent_files_purge` — declare their own widened error schema. The wire surface stays
 * per-intent-scoped (Cindy D2 P1-1 + D1 round-1 P1): a typed code
 * in one intent's error arm cannot leak into another intent's
 * error arm at validation time.
 */
export const intentSchemas = {
  hello: {
    params: HelloParamsSchema,
    result: HelloResultSchema,
  },
  ping: {
    params: PingParamsSchema,
    result: PingResultSchema,
  },
  status: {
    params: StatusParamsSchema,
    result: StatusResultSchema,
  },
  echo: {
    params: EchoParamsSchema,
    result: EchoResultSchema,
  },
  // #task:808 — host snapshot for the ComputerPane. Dispatched here so the
  // shared `IntentDispatcher` validates params/result via the same registry
  // every other intent uses; the outbound WS surface re-references the same
  // schemas in `outboundServerCommandSchemas` so embedded local-socket and
  // standalone WSS reach the same handler path. Generic error taxonomy is
  // sufficient — failures are transport-level (timeout / disconnected /
  // shutting_down) and surface through the adapter wrapper.
  host_status: {
    params: HostStatusParamsSchema,
    result: HostStatusResultSchema,
  },
  host_runtime_catalog: {
    params: HostRuntimeCatalogParamsSchema,
    result: HostRuntimeCatalogResultSchema,
  },
  conversation_log_debug: {
    params: ConversationLogDebugParamsSchema,
    result: ConversationLogDebugResultSchema,
  },
  agent_message_send: {
    params: AgentMessageSendParamsSchema,
    result: AgentMessageSendResultSchema,
  },
  agent_stale_resolve: {
    params: AgentStaleResolveParamsSchema,
    result: AgentStaleResolveResultSchema,
  },
  agent_stale_receive: {
    // #task:1614 — deferred-pull second consumer. Result arms carry the
    // custody facts; the widened error schema carries the non-custody
    // arbitration failures (pull_already_active / stale_event_replaced /
    // runtime_replaced / handoff_unsupported).
    params: AgentStaleReceiveParamsSchema,
    result: AgentStaleReceiveResultSchema,
    error: AgentStaleReceiveIpcErrorSchema,
  },
  runtime_start: {
    params: RuntimeStartParamsSchema,
    result: RuntimeStartResultSchema,
    // D4.e-6 — daemon now resolves all paths and fails closed on
    // unresolvable workspaces; the typed taxonomy lets the backend
    // tell `workspace_not_provisioned` (an `agent_home` ref that does
    // not belong to this agent), `workspace_bundle_missing` (session
    // cwd could not be created/entered) and `registry_unavailable`
    // (no attached computer / unsafe id) apart from a generic
    // `internal_error`. Per-command scoped like agent_materialize.
    error: RuntimeSpawnIpcErrorSchema,
  },
  runtime_stop: {
    params: RuntimeStopParamsSchema,
    result: RuntimeStopResultSchema,
  },
  runtime_deliver: {
    // #task:1417 Stage 2 — daemon-authoritative delivery command. Result
    // transfers/refuses custody only; `delivered_at` moves solely on the
    // async `runtime_delivery_consumed`/`_failed` events below.
    params: RuntimeDeliverParamsSchema,
    result: RuntimeDeliverResultSchema,
    // `runtime_deliver` is the sole delivery command and its SPAWN arm runs
    // the same path resolver as `runtime_start`. Preserve those typed errors
    // so the backend reads the same taxonomy instead of taxonomy drift.
    error: RuntimeSpawnIpcErrorSchema,
  },
  runtime_status: {
    params: RuntimeStatusParamsSchema,
    result: RuntimeStatusResultSchema,
  },
  runtime_clear_session: {
    params: RuntimeClearSessionParamsSchema,
    result: RuntimeClearSessionResultSchema,
  },
  runtime_stop_all: {
    params: RuntimeStopAllParamsSchema,
    result: RuntimeStopAllResultSchema,
  },
  runtime_close_activity_log: {
    params: RuntimeCloseActivityLogParamsSchema,
    result: RuntimeCloseActivityLogResultSchema,
  },
  runtime_output_pause: {
    params: RuntimeOutputPauseParamsSchema,
    result: RuntimeOutputPauseResultSchema,
  },
  runtime_output_resume: {
    params: RuntimeOutputResumeParamsSchema,
    result: RuntimeOutputResumeResultSchema,
  },
  host_outbound_attach: {
    params: HostOutboundAttachParamsSchema,
    result: HostOutboundAttachResultSchema,
  },
  host_outbound_detach: {
    params: HostOutboundDetachParamsSchema,
    result: HostOutboundDetachResultSchema,
  },
  host_outbound_reload: {
    params: HostOutboundReloadParamsSchema,
    result: HostOutboundReloadResultSchema,
  },
  host_proxy_config_get: {
    params: HostProxyConfigGetParamsSchema,
    result: HostProxyConfigGetResultSchema,
  },
  host_proxy_config_set: {
    params: HostProxyConfigSetParamsSchema,
    result: HostProxyConfigSetResultSchema,
  },
  // D4.d D2: agent_materialize is server↔daemon formal semantics
  // (see comment block above `AgentMaterializeParamsSchema`). The
  // handler is wired through DispatcherOptions.agentMaterializeHandler
  // in `daemon/dispatcher.ts`; when the slot is empty (e.g., legacy
  // test wiring), the dispatcher returns `internal_error` like every
  // other handler slot. `error` widens to the 5 generic codes plus
  // the materialize-specific taxonomy (Cindy D2 P1-1: this is what
  // lets `HostClient.handleFrame` reject typed codes via the wire
  // for the local IPC adapter — without the per-intent error entry
  // here, the transport would silently drop typed-error frames
  // because the strict `ResponseErrorSchema` wouldn't accept them).
  agent_materialize: {
    params: AgentMaterializeParamsSchema,
    result: AgentMaterializeResultSchema,
    error: AgentMaterializeIpcErrorSchema,
  },
  // D4.e-1: data-plane path-authority read intent. Same per-command typed
  // error scoping as agent_materialize — it widens the 5 generic codes with
  // its own taxonomy and nothing else, so a typed code cannot leak onto
  // another intent's wire error. relocation-v1 §7-4 (#task:1808): the registry
  // write/reconcile intents (workspace_provision / _path_index / _unbind /
  // _registry_ids) were retired with the registry's demotion.
  agent_workspace_index: {
    params: AgentWorkspaceIndexParamsSchema,
    result: AgentWorkspaceIndexResultSchema,
    error: AgentWorkspaceIndexIpcErrorSchema,
  },
  agent_workspace_tree: {
    params: AgentWorkspaceTreeParamsSchema,
    result: AgentWorkspaceTreeResultSchema,
    error: AgentWorkspaceFsIpcErrorSchema,
  },
  agent_workspace_file_read: {
    params: AgentWorkspaceFileReadParamsSchema,
    result: AgentWorkspaceFileReadResultSchema,
    error: AgentWorkspaceFsIpcErrorSchema,
  },
  agent_l2_seed: {
    params: AgentL2SeedParamsSchema,
    result: AgentL2SeedResultSchema,
    error: AgentL2SeedIpcErrorSchema,
  },
  agent_files_purge: {
    params: AgentFilesPurgeParamsSchema,
    result: AgentFilesPurgeResultSchema,
    error: AgentFilesPurgeIpcErrorSchema,
  },
  agent_profile_mcp_read: {
    params: AgentProfileMcpReadParamsSchema,
    result: AgentProfileMcpReadResultSchema,
    error: AgentProfileMcpIpcErrorSchema,
  },
  agent_profile_mcp_write: {
    params: AgentProfileMcpWriteParamsSchema,
    result: AgentProfileMcpWriteResultSchema,
    error: AgentProfileMcpWriteIpcErrorSchema,
  },
  mcp_probe: {
    params: McpProbeParamsSchema,
    result: McpProbeResultSchema,
    error: McpProbeIpcErrorSchema,
  },
} as const;

export type IntentName = keyof typeof intentSchemas;
export type IntentParams<N extends IntentName> = z.infer<(typeof intentSchemas)[N]['params']>;
export type IntentResult<N extends IntentName> = z.infer<(typeof intentSchemas)[N]['result']>;
export type IntentError<N extends IntentName> = (typeof intentSchemas)[N] extends {
  error: infer E extends z.ZodType;
}
  ? z.infer<E>
  : ResponseError;

/**
 * Resolve the per-intent typed-error schema, falling back to
 * `ResponseErrorSchema` for intents whose entry has no `error`
 * field. Consumed by `IntentDispatcher.dispatch` to validate the
 * typed code it emits, and by `HostClient.handleFrame` to validate
 * the typed code it receives. The fallback is the load-bearing
 * piece — without it the 17 intents that don't override would each
 * need a no-op `error: ResponseErrorSchema` line just to keep the
 * lookup total.
 *
 * Also tolerates `intent` strings NOT in the registry (e.g. an
 * older client dispatching a removed-since-v0 name like
 * `runtime_inject_env`): the daemon answers `unknown_intent`,
 * which IS in the generic taxonomy, so `ResponseErrorSchema` is
 * the right default. Without this tolerance the lookup would crash
 * with `Cannot read properties of undefined (reading 'error')` and
 * the client would translate a clean rejection into a hang.
 */
export function intentErrorSchema(intent: string): z.ZodType<{ code: string; message: string }> {
  const entry = (intentSchemas as Record<string, { error?: unknown } | undefined>)[intent];
  const error = entry?.error;
  if (error) return error as z.ZodType<{ code: string; message: string }>;
  return ResponseErrorSchema;
}

// ── Frame schemas ───────────────────────────────────────────────────

export const RequestFrameSchema = z.object({
  id: z.string().min(1),
  intent: z.string().min(1),
  params: z.unknown(),
});
export type RequestFrame = z.infer<typeof RequestFrameSchema>;

/**
 * Generic-error frame. The `code` enum lives at
 * `ResponseErrorCodeSchema` above so per-intent typed-error
 * schemas (e.g. `AgentMaterializeIpcErrorSchema`) can reference
 * the same enum without drift.
 */
export const ResponseErrorSchema = z.object({
  code: ResponseErrorCodeSchema,
  message: z.string(),
});
export type ResponseError = z.infer<typeof ResponseErrorSchema>;

export const ResponseFrameSchema = z.union([
  z
    .object({
      id: z.string().min(1),
      result: z.unknown(),
    })
    .strict(),
  z
    .object({
      id: z.string().min(1),
      error: ResponseErrorSchema,
    })
    .strict(),
]);
export type ResponseFrame = z.infer<typeof ResponseFrameSchema>;

/**
 * Transport-layer loose variant of `ResponseFrame` (Cindy D2 P1-1).
 *
 * The strict `ResponseFrameSchema` above only admits the 5 generic
 * `ResponseErrorSchema` codes in the `error.code` field. The
 * transport layer (`SocketTransport.deliverFrame`) sits BELOW the
 * intent dispatcher, so at frame-arrival time it does not yet know
 * which intent the response correlates to — and therefore which
 * typed-error code set is legal. Using the strict frame schema at
 * that layer would cause it to silently drop response frames
 * carrying per-intent typed codes (e.g. `agent_dir_collision` from
 * `agent_materialize`), leaving the pending IPC request to time
 * out. That's the bug Cindy identified.
 *
 * The loose schema accepts `error.code: string` so the transport
 * can deliver the frame upward; `HostClient.handleFrame` then
 * looks up the pending request's intent name and validates the
 * received code against `intentErrorSchema(intent)` before
 * rejecting the typed `HostClientError`. Drift (unknown code for
 * that intent) is itself surfaced as a typed `internal_error` so
 * the caller still gets a clean reject path.
 */
const LooseResponseErrorSchema = z
  .object({
    code: z.string().min(1),
    message: z.string(),
  })
  .strict();

export const LooseResponseFrameSchema = z.union([
  z
    .object({
      id: z.string().min(1),
      result: z.unknown(),
    })
    .strict(),
  z
    .object({
      id: z.string().min(1),
      error: LooseResponseErrorSchema,
    })
    .strict(),
]);
export type LooseResponseFrame = z.infer<typeof LooseResponseFrameSchema>;

/**
 * Unsolicited events. Two categories:
 *
 *  - **droppable** = legacy `runtime_output` only. Production
 *    supervisor wiring no longer emits raw child stdout/stderr by
 *    default: tool output is unbounded and would duplicate the
 *    structured parsed/state stream. The frame remains in the schema
 *    for compatibility and targeted diagnostics; when emitted it is
 *    still subject to the per-(conv,agent) 4 MiB un-flushed buffer cap
 *    (§2.2.2 of design lock). Daemon may emit
 *    `runtime_output_dropped` when bytes are discarded — that ack
 *    itself is non-droppable.
 *  - **non-droppable** = everything else. Daemon must never sacrifice
 *    one of these to relieve backpressure.
 *
 *  PR-1 surface kept verbatim:
 *
 *  - `ready`         — daemon socket is bound; handshake signal mirrored
 *                      to stdout for the spawn helper.
 *  - `shutting_down` — daemon received SIGTERM/SIGINT and is draining.
 *
 *  PR-2 step 1 surface (matches design doc §3.2):
 *
 *  - `runtime_state` / `runtime_output` / `runtime_exit` / `runtime_ready_for_drain`
 *  - `runtime_delivery_consumed` / `runtime_delivery_failed` (the unified
 *    ack family; the legacy `runtime_startup_delivery_*` pair was retired
 *    in Stage 2c)
 *  - `runtime_output_dropped` / `runtime_output_paused` / `runtime_output_resumed`
 *
 *  PR-2 step 2 surface (Cindy `vqDWkLFF6pAeIE5t`):
 *
 *  - `runtime_init_timeout` / `runtime_init_retrying` / `runtime_init_cooldown`
 *    — daemon-internal init-watchdog / retry / cooldown loop exposed
 *    to backend so backend can drive UI (`agent_status` SSE) + DB
 *    projection without owning the retry state machine.
 *
 *  PR-2 step 2 commit 3b (Cindy `OQCA05pivtB8nSTC` + `C2BZGDbQWfnq-bKJ`):
 *
 *  - `runtime_parsed_event` — non-droppable structured runtime
 *    projection. Each line the driver's `parseLine` recognizes
 *    becomes one of these frames. This is the production activity /
 *    coordination surface; raw `runtime_output` is legacy/diagnostic.
 *  - `runtime_parsed_event_dropped` — non-droppable metadata-only
 *    frame the daemon emits when a parsed event's encoded form
 *    exceeds `MAX_FRAME_BYTES`. Carries `parsed_kind` + `encoded_bytes`
 *    + `reason:'frame_too_large'` ONLY; explicitly NO payload (no
 *    raw line, no tool input, no text) so we never half-leak a
 *    truncated event onto the wire.
 *
 *  #task:2099 (folded into this same unreleased generation — see the
 *  `HOST_PROTOCOL_VERSION` note above; NO separate number):
 *
 *  - `runtime_message_stale` — the ONE authoritative carrier for "this
 *    runtime's `message send` was refused as stale". The fact is the
 *    backend's freshness verdict, relayed by the daemon because the daemon
 *    is the one holding both halves of it: it made the call, and it owns
 *    the focus conversation the row belongs to. It is NOT a
 *    `runtime_parsed_event`: `parsed` means "the driver read this out of
 *    the runtime's own output", and a synthetic parsed event would both
 *    forge that provenance and push a non-driver value into
 *    `ParsedEventKind`, which the run-state projection consumes.
 */
export const EventFrameSchema = z
  .discriminatedUnion('event', [
    HelloResultSchema.extend({
      event: z.literal('ready'),
      socket_path: z.string(),
      daemon_pid: z.number().int(),
      started_at: z.number().int(),
    }),
    z.object({
      event: z.literal('shutting_down'),
      reason: z.string(),
    }),
    // ── runtime_* events ────────────────────────────────────────────
    z.object({
      event: z.literal('runtime_state'),
      runtime_scope: RuntimeScopeSchema,
      conversation_id: z.string().min(1),
      agent_id: z.string().min(1),
      runtime_generation: z.number().int().positive(),
      state: AgentRunStateSchema,
      ts: z.number().int(),
    }),
    z.object({
      event: z.literal('runtime_output'),
      runtime_scope: RuntimeScopeSchema,
      conversation_id: z.string().min(1),
      agent_id: z.string().min(1),
      stream: z.enum(['stdout', 'stderr']),
      /** Raw bytes from the child process, base64-free utf-8 string.
       *  This is the ONLY droppable field on the wire — see the per-
       *  stream cap in design doc §2.2.2. */
      chunk: z.string(),
      ts: z.number().int(),
    }),
    z.object({
      event: z.literal('runtime_exit'),
      runtime_scope: RuntimeScopeSchema,
      conversation_id: z.string().min(1),
      agent_id: z.string().min(1),
      runtime_generation: z.number().int().positive(),
      code: z.number().int().nullable(),
      intentional: z.boolean(),
      ts: z.number().int(),
    }),
    z.object({
      event: z.literal('runtime_ready_for_drain'),
      runtime_scope: RuntimeScopeSchema,
      conversation_id: z.string().min(1),
      agent_id: z.string().min(1),
      runtime_generation: z.number().int().positive(),
      ts: z.number().int(),
    }),
    // ── unified delivery ack events (Stage 2, #task:1417) ───────────
    //
    // The single async ack/rollback family for `runtime_deliver`, keyed by
    // `delivery_ids`. Adding them here is what forces the backend
    // event-handler exhaustiveness case (`_exhaustive`), so the ingestion
    // wiring lands with this. Stage 2c retired the legacy
    // `runtime_startup_delivery_consumed`/`_failed` pair these subsumed —
    // this is now the ONLY delivery-ack family. Shapes are single-sourced
    // from the standalone schemas defined above.
    RuntimeDeliveryConsumedEventSchema,
    RuntimeDeliveryFailedEventSchema,
    // ── runtime init lifecycle (locked by Cindy `vqDWkLFF6pAeIE5t`) ──
    //
    // These three events surface the daemon-internal init watchdog +
    // retry loop to backend. Init lifecycle is ORTHOGONAL to
    // delivery acks (`runtime_delivery_*`): empty-inbox and self-init paths
    // have no delivery batch to attach to but still get watchdog
    // timeouts / retries / cooldown. Backend doesn't own retry — daemon
    // does — but backend translates each event into UI / DB projection
    // (`agent_status` SSE, `updateAgentRuntime` columns, crash
    // notifications) since it owns those surfaces.
    //
    // Sequence over a typical failure path (retry_attempt is 0-indexed,
    // matching CM's `handle.initAttempt`):
    //   runtime_init_timeout (retry_attempt=0, retrying=true) →
    //   runtime_init_retrying (retry_attempt=1 of N) →
    //   runtime_init_timeout (retry_attempt=1, retrying=true) →
    //   ... →
    //   runtime_init_timeout (retry_attempt=N-1, retrying=false, cooldown_until=…) →
    //   runtime_init_cooldown (attempts_exhausted = N).
    // The cooldown path may also fire with retry_attempt=0 if max_retries=0.
    // The terminal `runtime_exit` follows separately when the daemon
    // gives up and the child process is gone.
    z
      .object({
        event: z.literal('runtime_init_timeout'),
        runtime_scope: RuntimeScopeSchema,
        conversation_id: z.string().min(1),
        agent_id: z.string().min(1),
        runtime_generation: z.number().int().positive(),
        /** Which watchdog tier tripped (mirrors CM's three-tier
         *  PR-A watchdog: silence between bytes / pre-protocol cap /
         *  post-protocol stall). Names match CM's `InitTimeoutReason` so
         *  backend can pass the value straight through to `agent_init_timeout`
         *  SSE without translation (Cindy `4CJATqU0DdbsGVXW`). */
        timeout_reason: z.enum(['silence', 'pre_protocol_cap', 'post_protocol']),
        /** Wall-clock ms between spawn and watchdog firing — only the
         *  daemon has the precise spawn timestamp, so we ship it on the
         *  same event rather than asking backend to reconstruct it. */
        elapsed_ms: z.number().int().nonnegative(),
        /** The configured timeout value (ms) for the tier that fired —
         *  varies per driver and may be overridden by daemon config. */
        timeout_ms: z.number().int().nonnegative(),
        /** True iff the daemon has already decided to re-spawn after this
         *  timeout. False means cooldown / give-up — a `runtime_exit`
         *  follows and no `runtime_init_retrying` will. Lets backend mark
         *  the projection terminal in a single hop without buffering. */
        retrying: z.boolean(),
        /** 0-indexed attempt number that just timed out (first attempt
         *  is 0). Matches CM's `handle.initAttempt` semantics. */
        retry_attempt: z.number().int().nonnegative(),
        /** Total retry budget the daemon will honor before cooldown. */
        max_retries: z.number().int().nonnegative(),
        /** Optional: present iff this timeout transition also engaged
         *  cooldown — wall-clock epoch ms when backend / user may
         *  attempt a fresh start. Carrying it on the same event keeps the
         *  cooldown-on-timeout path self-contained; backend never has to
         *  wait for a follow-up `runtime_init_cooldown` to know whether
         *  to publish `agent_status:crashed reason=init_cooldown` vs
         *  `init_timeout`. */
        cooldown_until: z.number().int().positive().optional(),
        ts: z.number().int(),
      })
      .strict(),
    z
      .object({
        event: z.literal('runtime_init_retrying'),
        runtime_scope: RuntimeScopeSchema,
        conversation_id: z.string().min(1),
        agent_id: z.string().min(1),
        runtime_generation: z.number().int().positive(),
        /** The attempt number about to start (= the previous timeout's
         *  retry_attempt + 1). */
        retry_attempt: z.number().int().positive(),
        max_retries: z.number().int().nonnegative(),
        /** How long the daemon will wait before re-spawning, ms. */
        delay_ms: z.number().int().nonnegative(),
        ts: z.number().int(),
      })
      .strict(),
    z
      .object({
        event: z.literal('runtime_init_cooldown'),
        runtime_scope: RuntimeScopeSchema,
        conversation_id: z.string().min(1),
        agent_id: z.string().min(1),
        runtime_generation: z.number().int().positive(),
        /** Wall-clock epoch ms after which backend / user may attempt a
         *  fresh start. Daemon will NOT auto-retry past this point. */
        cooldown_until: z.number().int().positive(),
        /** Total attempts that failed before cooldown engaged (== max_retries). */
        attempts_exhausted: z.number().int().nonnegative(),
        ts: z.number().int(),
      })
      .strict(),
    // ── runtime_compaction_stale (Cindy `-pdNvRYOWip5e9yA` 3c plan
    //     refinement) ───────────────────────────────────────────────
    //
    // Per-handle compaction watchdog (5 min, matches CM
    // COMPACTION_STALE_MS) fired without observing `compaction_finished`.
    // CRITICAL: this is observational only — daemon DOES NOT kill the
    // child or change phase. CM's `recordCompactionStale` preserves
    // `compacting: true` and only stacks `compactionStale: true` on the
    // run-state, so the UI can show "still compacting, but stale"
    // instead of "no longer compacting". Backend maps this 1:1 onto
    // the existing `compaction_stale` SSE event.
    //
    // Non-droppable: backend depends on this to refresh `compactionStale`
    // in its RunStateCache; dropping would leave the UI showing fresh
    // compacting state indefinitely. Single implicit reason
    // (watchdog timeout) — additional reasons would each get their own
    // strict() variant.
    z
      .object({
        event: z.literal('runtime_compaction_stale'),
        runtime_scope: RuntimeScopeSchema,
        conversation_id: z.string().min(1),
        agent_id: z.string().min(1),
        /** Ms elapsed since `compaction_started` was observed, at the
         *  moment the watchdog fired. */
        elapsed_ms: z.number().int().nonnegative(),
        ts: z.number().int(),
      })
      .strict(),
    z.object({
      event: z.literal('runtime_output_dropped'),
      runtime_scope: RuntimeScopeSchema,
      conversation_id: z.string().min(1),
      agent_id: z.string().min(1),
      dropped_count: z.number().int().nonnegative(),
      bytes_dropped: z.number().int().nonnegative(),
      since_ts: z.number().int(),
    }),
    z.object({
      event: z.literal('runtime_output_paused'),
      runtime_scope: RuntimeScopeSchema,
      conversation_id: z.string().min(1),
      agent_id: z.string().min(1),
    }),
    z.object({
      event: z.literal('runtime_output_resumed'),
      runtime_scope: RuntimeScopeSchema,
      conversation_id: z.string().min(1),
      agent_id: z.string().min(1),
    }),
    // ── runtime parsed event fanout (locked by Cindy `OQCA05pivtB8nSTC` +
    //     `C2BZGDbQWfnq-bKJ`) ─────────────────────────────────────────
    //
    // `runtime_parsed_event` is the structured runtime projection
    // surface: each line the driver's `parseLine` recognizes becomes
    // one of these frames. Production no longer forwards raw child
    // stdout/stderr by default (`runtime_output` is a legacy/diagnostic
    // compatibility arm) because tool output is unbounded and can fill
    // the outbound socket buffer. Backend observability / activity UI /
    // delivery coordination must use parsed/state events instead.
    //
    // Non-droppable: backend's `handleParsedEvent` drives the
    // AgentRunState lifecycle (thinking / tool_running / compacting /
    // turn_end) and the `session_events` DB projection. Dropping one
    // would leave run-state stuck or DB out of sync, neither of which
    // is recoverable from `runtime_output` alone (only the driver
    // knows the schema).
    //
    // No `raw_line` field: carrying raw bytes here would reintroduce
    // the same unbounded tool-output channel this split is meant to
    // avoid. The `parsed` payload is the bounded structured contract.
    z
      .object({
        event: z.literal('runtime_parsed_event'),
        runtime_scope: RuntimeScopeSchema,
        conversation_id: z.string().min(1),
        agent_id: z.string().min(1),
        runtime_generation: z.number().int().positive(),
        parsed: ParsedEventSchema,
        ts: z.number().int(),
      })
      .strict(),
    // `runtime_parsed_event_dropped` is the safety valve when a parsed
    // event's encoded payload exceeds MAX_FRAME_BYTES. We CANNOT silently
    // truncate the `parsed` payload (Cindy: "frame 保护不要静默改写任意
    // ParsedEvent") — that would let backend believe it received a
    // complete `tool_call` / `text` / `unknown.raw` when it didn't.
    //
    // The dropped frame is metadata-only by hard rule (Cindy
    // `C2BZGDbQWfnq-bKJ`): `parsed_kind` lets backend project a
    // best-effort run-state hint (e.g. "we know a tool_call happened
    // even though we lost the input"), `encoded_bytes` lets backend
    // logs surface the size budget, and `reason` is a single-arm enum.
    // NO payload — explicitly NO `unknown.raw`, NO tool input, NO text.
    //
    // Note: this dropped frame is on the WIRE only. Supervisor
    // still updates its in-memory AgentRunState from the original
    // ParsedEvent before encoding — frame-too-large is a broadcast
    // concern, not a run-state concern (Cindy `C2BZGDbQWfnq-bKJ`).
    z
      .object({
        event: z.literal('runtime_parsed_event_dropped'),
        runtime_scope: RuntimeScopeSchema,
        conversation_id: z.string().min(1),
        agent_id: z.string().min(1),
        parsed_kind: ParsedEventKindSchema,
        encoded_bytes: z.number().int().nonnegative(),
        reason: z.literal('frame_too_large'),
        ts: z.number().int(),
      })
      .strict(),
    // `runtime_message_stale` (#task:2099) is the authoritative fact axis for a
    // refused send. Before it, the desktop inferred staleness from the shape of
    // the agent's command line (`--stale-resolution` / `--anyway` present), which
    // is a client-side syntax proxy for a server-side verdict: it missed every
    // retry that omitted the optional reason, reported a pre-emptive `--anyway`
    // with nothing pending as a conflict, and went blind entirely on runtimes
    // whose tool-call capture carries no command at all (OpenCode persists only
    // `{cwd}`).
    //
    // The daemon does NOT classify: `extractStaleConflict` reads the backend's
    // typed `details.subkind === 'message_stale'` and fails closed on a body it
    // cannot parse, so this frame carries a verdict rather than a guess.
    //
    // `conversation_id` is the handle's CURRENT FOCUS target, i.e. the same key
    // every other activity row of this turn (including the `tool_call` row for the
    // very `silverhand message send` invocation) is written under. The refused
    // send's TARGET conversation may differ under an agent-scoped session and is
    // deliberately NOT carried: the row states that this runtime's send was
    // refused, and per-conflict triage detail (`stale_event_id`, target, queue
    // depth) stays in the backend's WARN log — the authority named by
    // `docs/freshness-boundary-authority-v1.md` §3.2 (whose §6 records this row's
    // own contract). Adding a field later is
    // purely additive; retracting one that reached a persisted payload is not.
    z
      .object({
        event: z.literal('runtime_message_stale'),
        runtime_scope: RuntimeScopeSchema,
        conversation_id: z.string().min(1),
        agent_id: z.string().min(1),
        runtime_generation: z.number().int().positive(),
        ts: z.number().int(),
      })
      .strict(),
  ])
  .superRefine((frame, ctx) => {
    if (frame.event === 'runtime_state' && frame.state.conversationId !== frame.conversation_id) {
      ctx.addIssue({
        code: 'custom',
        path: ['state', 'conversationId'],
        message: 'runtime_state outer conversation must match state.conversationId',
      });
    }
  });
export type EventFrame = z.infer<typeof EventFrameSchema>;

/**
 * Any frame either side may put on the wire. Daemon sends Response
 * (always) or Event (occasionally); client sends Request only. Treat
 * `event` field's presence as the discriminator over `id`.
 */
export const AnyFrameSchema = z.union([RequestFrameSchema, ResponseFrameSchema, EventFrameSchema]);
export type AnyFrame = z.infer<typeof AnyFrameSchema>;

/**
 * Transport-layer loose variant of `AnyFrameSchema` (Cindy D2 P1-1).
 * Substitutes `LooseResponseFrameSchema` for `ResponseFrameSchema`
 * so per-intent typed error codes survive the transport-layer
 * frame validation. The intent-aware validation happens one layer
 * up, in `HostClient.handleFrame`. See `LooseResponseFrameSchema`
 * for the full rationale.
 */
export const LooseAnyFrameSchema = z.union([
  RequestFrameSchema,
  LooseResponseFrameSchema,
  EventFrameSchema,
]);
export type LooseAnyFrame = z.infer<typeof LooseAnyFrameSchema>;

// ── Wire encoding ───────────────────────────────────────────────────

/** Ordinary complete JSON frame budget, independent of product file limits. */
export const MAX_FRAME_BYTES = 1024 * 1024;
/** 512KiB preview × six-byte JSON escaping + 1MiB envelope allowance. */
export const MAX_WORKSPACE_FILE_FRAME_BYTES = 4 * 1024 * 1024;

export function responseFrameBudget(intent: string | undefined, success: boolean): number {
  return success && intent === 'agent_workspace_file_read'
    ? MAX_WORKSPACE_FILE_FRAME_BYTES
    : MAX_FRAME_BYTES;
}

/** Encode an outgoing frame to its NDJSON wire form (single line +
 *  trailing newline). Throws if the encoded payload would exceed the
 *  per-frame cap so we fail loud at the caller instead of silently
 *  emitting an oversized line the peer will reject. */
export function encodeFrame(frame: AnyFrame, responseIntent?: string): string {
  const line = JSON.stringify(frame);
  const budget = responseFrameBudget(
    responseIntent,
    'result' in frame && !('error' in frame) && !('intent' in frame),
  );
  if (Buffer.byteLength(line, 'utf8') > budget) {
    throw new Error(`host frame exceeds MAX_FRAME_BYTES budget (${budget})`);
  }
  return `${line}\n`;
}

/**
 * Incremental NDJSON splitter — pure function over (buffer, chunk) →
 * (frames[], leftover). Same logic both sides need; lifted out so we
 * can unit-test it on its own.
 *
 * The buffer is held by the caller. We return UTF-8 string frames; the
 * caller JSON.parses + schema-validates each one. Lines longer than
 * the supplied physical budget (ordinary `MAX_FRAME_BYTES` by default) are reported via `overflow` so the caller can
 * close the socket with a typed reason rather than letting the buffer
 * grow without bound.
 */
export interface SplitResult {
  /** Newly complete frame strings (without trailing `\n`). */
  frames: string[];
  /** Leftover unterminated bytes — pass back as `buffer` next call. */
  leftover: string;
  /** True if a single line exceeded MAX_FRAME_BYTES while accumulating.
   *  When true, caller must close the socket; further frames cannot be
   *  trusted because we had to drop the overflowing prefix. */
  overflow: boolean;
}

export function splitNdjson(
  buffer: string,
  chunk: string,
  maxBytes = MAX_FRAME_BYTES,
): SplitResult {
  const combined = buffer + chunk;
  const frames: string[] = [];
  let start = 0;
  let overflow = false;
  while (true) {
    const nl = combined.indexOf('\n', start);
    if (nl === -1) break;
    const line = combined.slice(start, nl);
    start = nl + 1;
    if (line.length === 0) continue;
    if (Buffer.byteLength(line, 'utf8') > maxBytes) {
      overflow = true;
      // Stop accumulating further frames once we detect overflow; the
      // caller is contractually required to tear down the socket.
      return { frames, leftover: '', overflow };
    }
    frames.push(line);
  }
  const leftover = combined.slice(start);
  // Defensive: if the still-unterminated tail already exceeds the cap,
  // we will never close it (no `\n` will arrive in time). Signal
  // overflow now so the caller drops the connection.
  if (Buffer.byteLength(leftover, 'utf8') > maxBytes) {
    return { frames, leftover: '', overflow: true };
  }
  return { frames, leftover, overflow };
}
