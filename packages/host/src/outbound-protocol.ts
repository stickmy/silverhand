/**
 * D2 daemon-outbound WebSocket protocol.
 *
 * This is the wire contract for the long-lived connection opened by
 * the desktop-local daemon to the server at `/api/hosts/connect`.
 * It deliberately reuses the existing runtime intent schemas from
 * `protocol.ts`; D2 adds a transport envelope and host-token rotation,
 * not a second runtime vocabulary.
 *
 * Envelope rule:
 *   - `direction` is the outer discriminator.
 *   - `type` separates command / command_response / event / heartbeat frames.
 *   - `kind` is scoped by `type`; server commands and daemon events
 *     are separate catalogues.
 */
import { z } from 'zod';
import {
  AgentRunStateSchema,
  DAEMON_ADMISSION,
  McpProbeResultSchema,
  ParsedEventKindSchema,
  ParsedEventSchema,
  PROTOCOL_VERSIONS,
} from '@silverhand/shared';
import {
  AgentMaterializeErrorCodeSchema,
  AgentMaterializeParamsSchema,
  AgentMaterializeResultSchema,
  AgentWorkspaceIndexErrorCodeSchema,
  AgentWorkspaceIndexParamsSchema,
  AgentWorkspaceIndexResultSchema,
  AgentWorkspaceFsErrorCodeSchema,
  AgentWorkspaceTreeParamsSchema,
  AgentWorkspaceTreeResultSchema,
  AgentWorkspaceFileReadParamsSchema,
  AgentWorkspaceFileReadResultSchema,
  AgentL2SeedErrorCodeSchema,
  AgentL2SeedParamsSchema,
  AgentL2SeedResultSchema,
  AgentFilesPurgeErrorCodeSchema,
  AgentFilesPurgeParamsSchema,
  AgentFilesPurgeResultSchema,
  AgentProfileMcpErrorCodeSchema,
  AgentProfileMcpWriteErrorCodeSchema,
  AgentProfileMcpReadParamsSchema,
  AgentProfileMcpReadResultSchema,
  AgentProfileMcpWriteParamsSchema,
  AgentProfileMcpWriteResultSchema,
  McpProbeErrorCodeSchema,
  McpProbeParamsSchema,
  HelloResultSchema,
  HostProxyConfigGetParamsSchema,
  HostProxyConfigGetResultSchema,
  HostProxyConfigSetParamsSchema,
  HostProxyConfigSetResultSchema,
  HostRuntimeCatalogParamsSchema,
  HostRuntimeCatalogResultSchema,
  HostStatusParamsSchema,
  HostStatusResultSchema,
  responseFrameBudget,
  ResponseErrorSchema,
  RuntimeClearSessionParamsSchema,
  RuntimeClearSessionResultSchema,
  RuntimeCloseActivityLogParamsSchema,
  RuntimeCloseActivityLogResultSchema,
  RuntimeDeliverParamsSchema,
  RuntimeDeliverResultSchema,
  RuntimeOutputPauseParamsSchema,
  RuntimeOutputPauseResultSchema,
  RuntimeOutputResumeParamsSchema,
  RuntimeOutputResumeResultSchema,
  RuntimeStartParamsSchema,
  RuntimeStartResultSchema,
  RuntimeStartErrorCodeSchema,
  RuntimeScopeSchema,
  RuntimeStatusParamsSchema,
  RuntimeStatusResultSchema,
  RuntimeStopAllParamsSchema,
  RuntimeStopAllResultSchema,
  RuntimeStopParamsSchema,
  RuntimeStopResultSchema,
} from './protocol.js';

// D4.d D2: agent_materialize schemas live in `protocol.ts` so the
// daemon's IPC `intentSchemas` catalogue can validate them through
// the same path as every other intent. We re-export from this file
// to keep the wire surface coherent (callers who only deal with
// outbound WS frames still see them here).
export {
  AgentMaterializeErrorCodeSchema,
  AgentMaterializeParamsSchema,
  AgentMaterializeResultSchema,
  type AgentMaterializeErrorCode,
  type AgentMaterializeParams,
  type AgentMaterializeResult,
} from './protocol.js';

// D4.e-1: agent_workspace_index schema also lives in `protocol.ts` (same
// IPC-catalogue reason as agent_materialize); re-exported here so outbound-WS
// callers see it. D4.e-3 adds agent_l2_seed (same reason). D4.e-4 adds
// agent_files_purge (same reason). #task:811 adds agent_workspace_tree +
// agent_workspace_file_read. relocation-v1 §7-4 retired the
// workspace_provision / workspace_path_index / workspace_unbind /
// workspace_registry_ids intent family (registry demotion).
export {
  AgentWorkspaceIndexParamsSchema,
  AgentWorkspaceIndexResultSchema,
  AgentWorkspaceIndexErrorCodeSchema,
  AgentL2SeedParamsSchema,
  AgentL2SeedResultSchema,
  AgentL2SeedErrorCodeSchema,
  AgentFilesPurgeParamsSchema,
  AgentFilesPurgeResultSchema,
  AgentFilesPurgeErrorCodeSchema,
  AgentProfileMcpReadParamsSchema,
  AgentProfileMcpReadResultSchema,
  AgentProfileMcpWriteParamsSchema,
  AgentProfileMcpWriteResultSchema,
  AgentProfileMcpErrorCodeSchema,
  HostProxyConfigGetParamsSchema,
  HostProxyConfigGetResultSchema,
  HostProxyConfigSetParamsSchema,
  HostProxyConfigSetResultSchema,
  HostRuntimeCatalogParamsSchema,
  HostRuntimeCatalogResultSchema,
  RuntimeStartErrorCodeSchema,
  type AgentWorkspaceIndexParams,
  type AgentWorkspaceIndexResult,
  type AgentWorkspaceIndexErrorCode,
  type AgentL2SeedParams,
  type AgentL2SeedResult,
  type AgentL2SeedErrorCode,
  type AgentFilesPurgeParams,
  type AgentFilesPurgeResult,
  type AgentFilesPurgeErrorCode,
  type AgentProfileMcpReadParams,
  type AgentProfileMcpReadResult,
  type AgentProfileMcpWriteParams,
  type AgentProfileMcpWriteResult,
  type AgentProfileMcpErrorCode,
  type HostProxyConfigGetParams,
  type HostProxyConfigGetResult,
  type HostProxyConfigSetParams,
  type HostProxyConfigSetResult,
  type HostRuntimeCatalogParams,
  type HostRuntimeCatalogResult,
  type RuntimeStartErrorCode,
} from './protocol.js';

const FrameIdSchema = z.string().min(1);

// `OUTBOUND_PROTOCOL_VERSION` is the EQUALITY SENTINEL on the remote daemon ↔
// standalone server boundary, enforced at the WS attach handshake (exact match
// of the daemon's advertised `ready` version). It moves in lock-step with
// HOST_PROTOCOL_VERSION whenever a cutover crosses this wire: the two share the
// same `runtime_start` params object and the same daemon-authoritative delivery
// surface, so a break on one is usually a break on both. They remain distinct
// axes — a change touching only the CLI↔daemon socket moves HOST alone — and the
// NUMBER for both lives in the single authority (`protocol-versions.ts`).
//
// Same exact-equality rationale as HOST_PROTOCOL_VERSION: a mixed pair's failure
// is not a parse error but a reply delivered into the WRONG stream, and no shim
// is possible, so the pair is refused at attach before any agent is spawned.
// Unreleased breaking changes fold into one generation; per-generation history
// lives in the owning tasks/docs, not as a number ledger here.
export const OUTBOUND_PROTOCOL_VERSION = PROTOCOL_VERSIONS.outbound;

// ── server → daemon command catalogue ──────────────────────────────

export const RotateTokenParamsSchema = z
  .object({
    host_id: z.string().min(1),
    host_token: z.string().min(1),
    /** Epoch ms when the server issued this replacement token. */
    issued_at: z.number().int().nonnegative(),
    /** Epoch ms until which the previous token remains valid. */
    grace_until: z.number().int().positive(),
  })
  .superRefine((value, ctx) => {
    if (value.grace_until <= value.issued_at) {
      ctx.addIssue({
        code: 'custom',
        path: ['grace_until'],
        message: 'grace_until must be after issued_at',
      });
    }
  });
export type RotateTokenParams = z.infer<typeof RotateTokenParamsSchema>;

export const RotateTokenResultSchema = z.discriminatedUnion('rotated', [
  z.object({
    rotated: z.literal(true),
    host_id: z.string().min(1),
    /** Epoch ms after daemon atomically refreshed daemon-config.json. */
    config_refreshed_at: z.number().int().nonnegative(),
  }),
  z.object({
    rotated: z.literal(false),
    reason: z.enum(['host_mismatch', 'invalid_config', 'write_failed']),
    message: z.string().optional(),
  }),
]);
export type RotateTokenResult = z.infer<typeof RotateTokenResultSchema>;

/**
 * Per-command error schema for `agent_materialize` (D4.d D1).
 *
 * Daemon-side typed errors travel in the response frame's `error`
 * slot, just like the five generic transport-level codes from
 * `ResponseErrorSchema`. The wire union accepts EITHER set so the
 * daemon handler can return e.g. `internal_error` (generic) OR
 * `agent_dir_collision` (command-specific). Per-route mapping in
 * §3.4 translates wire codes to HTTP.
 *
 * Scoped per-command (not global) so other commands (runtime_* /
 * rotate_token) do NOT accidentally accept `agent_dir_collision`
 * in their error arm — that was the Cindy D1 round-1 P1 failure
 * mode: a global union widened every command's error surface.
 */
export const AgentMaterializeWireErrorSchema = z.object({
  code: z.union([ResponseErrorSchema.shape.code, AgentMaterializeErrorCodeSchema]),
  message: z.string(),
});
export type AgentMaterializeWireError = z.infer<typeof AgentMaterializeWireErrorSchema>;

/**
 * Per-command wire error schemas for the D4.e data-plane intents.
 * Each widens the 5 generic transport codes with ONLY its own taxonomy
 * (per-command scoping — Cindy D1 round-1 P1): a typed code from one
 * command cannot leak onto another's error arm.
 */
export const AgentWorkspaceIndexWireErrorSchema = z.object({
  code: z.union([ResponseErrorSchema.shape.code, AgentWorkspaceIndexErrorCodeSchema]),
  message: z.string(),
});
export type AgentWorkspaceIndexWireError = z.infer<typeof AgentWorkspaceIndexWireErrorSchema>;

/**
 * #task:811 — shared wire error envelope for the new agent home
 * workspace tree+read pair. Both intents project the same
 * `AgentWorkspaceFsError` taxonomy on the daemon side
 * (`forbidden_path` / `not_found` / `fs_unavailable` /
 * `internal_error`), so the renderer can write one translation table
 * for both endpoints.
 */
export const AgentWorkspaceFsWireErrorSchema = z.object({
  code: z.union([ResponseErrorSchema.shape.code, AgentWorkspaceFsErrorCodeSchema]),
  message: z.string(),
});
export type AgentWorkspaceFsWireError = z.infer<typeof AgentWorkspaceFsWireErrorSchema>;

export const AgentL2SeedWireErrorSchema = z.object({
  code: z.union([ResponseErrorSchema.shape.code, AgentL2SeedErrorCodeSchema]),
  message: z.string(),
});
export type AgentL2SeedWireError = z.infer<typeof AgentL2SeedWireErrorSchema>;

export const AgentFilesPurgeWireErrorSchema = z.object({
  code: z.union([ResponseErrorSchema.shape.code, AgentFilesPurgeErrorCodeSchema]),
  message: z.string(),
});
export type AgentFilesPurgeWireError = z.infer<typeof AgentFilesPurgeWireErrorSchema>;

// #task:1135: profile-mcp write widens its wire error with the same
// per-command scoping — `invalid_config` (fail-loud re-validation),
// `fs_unavailable` (E* host-fs error), `internal_error`.
export const AgentProfileMcpWireErrorSchema = z.object({
  code: z.union([ResponseErrorSchema.shape.code, AgentProfileMcpErrorCodeSchema]),
  message: z.string(),
});
export type AgentProfileMcpWireError = z.infer<typeof AgentProfileMcpWireErrorSchema>;

export const AgentProfileMcpWriteWireErrorSchema = z.object({
  code: z.union([ResponseErrorSchema.shape.code, AgentProfileMcpWriteErrorCodeSchema]),
  message: z.string(),
});

// #task:1137: mcp_probe widens its wire error with the same per-command
// scoping — `fs_unavailable` (E* host-fs error reading the source config),
// `internal_error`. Connection/handshake/tools failures are NOT errors; they
// ride in the result (`ok:false` + phase).
export const McpProbeWireErrorSchema = z.object({
  code: z.union([ResponseErrorSchema.shape.code, McpProbeErrorCodeSchema]),
  message: z.string(),
});
export type McpProbeWireError = z.infer<typeof McpProbeWireErrorSchema>;

// D4.e-6: commands whose execution may spawn a runtime widen their wire error
// with the same resolver taxonomy. The daemon resolves all host paths and
// fails closed; the backend reads `workspace_not_provisioned` (an `agent_home`
// ref that does not belong to this agent), `workspace_bundle_missing` (the
// session cwd could not be created/entered on this computer), or
// `registry_unavailable` (no attached computer, or an unsafe workspace id
// segment) off this arm. `runtime_deliver` shares it because its SPAWN arm
// executes the same resolver as `runtime_start`.
export const RuntimeStartWireErrorSchema = z.object({
  code: z.union([ResponseErrorSchema.shape.code, RuntimeStartErrorCodeSchema]),
  message: z.string(),
});
export type RuntimeStartWireError = z.infer<typeof RuntimeStartWireErrorSchema>;

/**
 * Source of truth for D2 server-issued commands. The runtime_* entries
 * are the D2.0 request surface; rotate_token is D2.f's transport-local
 * credential refresh command. Each entry pins (a) the params shape,
 * (b) the success-result shape, and (c) the WIRE ERROR shape — the
 * error arm in the response frame is built from `error` rather than
 * a single global error schema, so command-specific typed codes
 * cannot leak across kinds (Cindy D1 round-1 P1).
 *
 * Default `error` is `ResponseErrorSchema` (the five generic
 * transport-level codes). Commands that may legitimately surface
 * typed daemon-business errors (`agent_materialize`; the D4.e
 * workspace data-plane commands `agent_workspace_index` / `agent_l2_seed` /
 * `agent_files_purge`;
 * #task:811 commands `agent_workspace_tree` / `agent_workspace_file_read`;
 * and runtime-spawning commands (`runtime_start` plus `runtime_deliver`'s
 * SPAWN arm — D4.e-6, codes `workspace_not_provisioned` /
 * `workspace_bundle_missing` / `registry_unavailable`) override with their own
 * wider schema.
 */
export const outboundServerCommandSchemas = {
  runtime_start: {
    params: RuntimeStartParamsSchema,
    result: RuntimeStartResultSchema,
    error: RuntimeStartWireErrorSchema,
  },
  runtime_stop: {
    params: RuntimeStopParamsSchema,
    result: RuntimeStopResultSchema,
    error: ResponseErrorSchema,
  },
  runtime_deliver: {
    // #task:1417 Stage 2 — daemon-authoritative delivery over the outbound
    // transport. Params/result single-sourced from `protocol.js`.
    params: RuntimeDeliverParamsSchema,
    result: RuntimeDeliverResultSchema,
    error: RuntimeStartWireErrorSchema,
  },
  runtime_status: {
    params: RuntimeStatusParamsSchema,
    result: RuntimeStatusResultSchema,
    error: ResponseErrorSchema,
  },
  runtime_clear_session: {
    params: RuntimeClearSessionParamsSchema,
    result: RuntimeClearSessionResultSchema,
    error: ResponseErrorSchema,
  },
  runtime_stop_all: {
    params: RuntimeStopAllParamsSchema,
    result: RuntimeStopAllResultSchema,
    error: ResponseErrorSchema,
  },
  runtime_close_activity_log: {
    params: RuntimeCloseActivityLogParamsSchema,
    result: RuntimeCloseActivityLogResultSchema,
    error: ResponseErrorSchema,
  },
  runtime_output_pause: {
    params: RuntimeOutputPauseParamsSchema,
    result: RuntimeOutputPauseResultSchema,
    error: ResponseErrorSchema,
  },
  runtime_output_resume: {
    params: RuntimeOutputResumeParamsSchema,
    result: RuntimeOutputResumeResultSchema,
    error: ResponseErrorSchema,
  },
  rotate_token: {
    params: RotateTokenParamsSchema,
    result: RotateTokenResultSchema,
    error: ResponseErrorSchema,
  },
  agent_materialize: {
    params: AgentMaterializeParamsSchema,
    result: AgentMaterializeResultSchema,
    error: AgentMaterializeWireErrorSchema,
  },
  agent_workspace_index: {
    params: AgentWorkspaceIndexParamsSchema,
    result: AgentWorkspaceIndexResultSchema,
    error: AgentWorkspaceIndexWireErrorSchema,
  },
  agent_workspace_tree: {
    params: AgentWorkspaceTreeParamsSchema,
    result: AgentWorkspaceTreeResultSchema,
    error: AgentWorkspaceFsWireErrorSchema,
  },
  agent_workspace_file_read: {
    params: AgentWorkspaceFileReadParamsSchema,
    result: AgentWorkspaceFileReadResultSchema,
    error: AgentWorkspaceFsWireErrorSchema,
  },
  agent_l2_seed: {
    params: AgentL2SeedParamsSchema,
    result: AgentL2SeedResultSchema,
    error: AgentL2SeedWireErrorSchema,
  },
  agent_files_purge: {
    params: AgentFilesPurgeParamsSchema,
    result: AgentFilesPurgeResultSchema,
    error: AgentFilesPurgeWireErrorSchema,
  },
  /** #task:1135 — profile-mcp read/write: the host-local truth behind
   *  `silverhand profile mcp get|set`. Read returns raw|null (absent is
   *  normal); write re-validates fail-loud then atomically replaces the
   *  L1 `mcp.json`. */
  agent_profile_mcp_read: {
    params: AgentProfileMcpReadParamsSchema,
    result: AgentProfileMcpReadResultSchema,
    error: AgentProfileMcpWireErrorSchema,
  },
  agent_profile_mcp_write: {
    params: AgentProfileMcpWriteParamsSchema,
    result: AgentProfileMcpWriteResultSchema,
    error: AgentProfileMcpWriteWireErrorSchema,
  },
  /** #task:1137 — mcp_probe: one-shot live MCP connection (initialize +
   *  optional tools/list) to a named server, closed immediately. Backs
   *  `silverhand profile mcp probe` + the agent-pane Test connection / List
   *  tools buttons. Failures of the connection itself ride in the result. */
  mcp_probe: {
    params: McpProbeParamsSchema,
    result: McpProbeResultSchema,
    error: McpProbeWireErrorSchema,
  },
  /** #task:808 — dynamic host snapshot for the ComputerPane / host
   *  registry view. Payload is host-facts only (OS, daemon process,
   *  active agent_ids); backend joins the active set against
   *  `agents.host_id` to render per-agent `online` state. Generic
   *  `ResponseErrorSchema` is enough — failures are transport-level
   *  (timeout / disconnected) and surface through the adapter wrapper. */
  host_status: {
    params: HostStatusParamsSchema,
    result: HostStatusResultSchema,
    error: ResponseErrorSchema,
  },
  /** #task:1375 — per-computer runtime availability catalog. The daemon
   *  probes the same host-side resolver spawn uses; backend only dispatches. */
  host_runtime_catalog: {
    params: HostRuntimeCatalogParamsSchema,
    result: HostRuntimeCatalogResultSchema,
    error: ResponseErrorSchema,
  },
  host_proxy_config_get: {
    params: HostProxyConfigGetParamsSchema,
    result: HostProxyConfigGetResultSchema,
    error: ResponseErrorSchema,
  },
  host_proxy_config_set: {
    params: HostProxyConfigSetParamsSchema,
    result: HostProxyConfigSetResultSchema,
    error: ResponseErrorSchema,
  },
} as const;

export type OutboundServerCommandKind = keyof typeof outboundServerCommandSchemas;
export type OutboundServerCommandParams<K extends OutboundServerCommandKind> = z.infer<
  (typeof outboundServerCommandSchemas)[K]['params']
>;
export type OutboundServerCommandResult<K extends OutboundServerCommandKind> = z.infer<
  (typeof outboundServerCommandSchemas)[K]['result']
>;
export type OutboundServerCommandError<K extends OutboundServerCommandKind> = z.infer<
  (typeof outboundServerCommandSchemas)[K]['error']
>;

export const OUTBOUND_SERVER_COMMAND_KINDS = Object.keys(
  outboundServerCommandSchemas,
) as OutboundServerCommandKind[];

const serverCommandFrameOptions = OUTBOUND_SERVER_COMMAND_KINDS.map((kind) =>
  z.object({
    direction: z.literal('server_to_daemon'),
    type: z.literal('command'),
    id: FrameIdSchema,
    kind: z.literal(kind),
    params: outboundServerCommandSchemas[kind].params,
  }),
) as unknown as [z.ZodDiscriminatedUnionOption<'kind'>, ...z.ZodDiscriminatedUnionOption<'kind'>[]];

export const OutboundServerToDaemonCommandFrameSchema = z.discriminatedUnion(
  'kind',
  serverCommandFrameOptions,
);

export type OutboundServerToDaemonCommandFrame = z.infer<
  typeof OutboundServerToDaemonCommandFrameSchema
>;

// ── daemon → server command responses ──────────────────────────────

/**
 * Aggregate wire error type for any outbound command response.
 *
 * Kept for callers that need a type covering "an error in any
 * response frame, regardless of kind" (e.g., the supervisor's
 * fallback / drift path). It is the union of every per-command
 * error schema — NOT a single schema used for all command arms.
 * The actual response frames use `outboundServerCommandSchemas[kind].error`
 * so command-specific codes are scoped per kind.
 */
export type OutboundCommandError = {
  [K in OutboundServerCommandKind]: OutboundServerCommandError<K>;
}[OutboundServerCommandKind];

// Error arm first: a result schema of optional keys also matches an error frame.
const commandResponseFrameOptions = OUTBOUND_SERVER_COMMAND_KINDS.flatMap((kind) => [
  z.object({
    direction: z.literal('daemon_to_server'),
    type: z.literal('command_response'),
    id: FrameIdSchema,
    kind: z.literal(kind),
    error: outboundServerCommandSchemas[kind].error,
  }),
  z.object({
    direction: z.literal('daemon_to_server'),
    type: z.literal('command_response'),
    id: FrameIdSchema,
    kind: z.literal(kind),
    result: outboundServerCommandSchemas[kind].result,
  }),
]) as unknown as [z.ZodTypeAny, z.ZodTypeAny, ...z.ZodTypeAny[]];

export const OutboundDaemonToServerCommandResponseFrameSchema = z.union(
  commandResponseFrameOptions,
);
export type OutboundDaemonToServerCommandResponseFrame = z.infer<
  typeof OutboundDaemonToServerCommandResponseFrameSchema
>;

// ── heartbeat frames ───────────────────────────────────────────────

const HeartbeatTimestampSchema = z.number().int().nonnegative();

export const OutboundHeartbeatPingFrameSchema = z.object({
  direction: z.enum(['server_to_daemon', 'daemon_to_server']),
  type: z.literal('heartbeat'),
  kind: z.literal('ping'),
  ts: HeartbeatTimestampSchema,
});
export type OutboundHeartbeatPingFrame = z.infer<typeof OutboundHeartbeatPingFrameSchema>;

export const OutboundHeartbeatPongFrameSchema = z.object({
  direction: z.enum(['server_to_daemon', 'daemon_to_server']),
  type: z.literal('heartbeat'),
  kind: z.literal('pong'),
  ts: HeartbeatTimestampSchema,
  echo_ts: HeartbeatTimestampSchema,
});
export type OutboundHeartbeatPongFrame = z.infer<typeof OutboundHeartbeatPongFrameSchema>;

export const OutboundHeartbeatFrameSchema = z.discriminatedUnion('kind', [
  OutboundHeartbeatPingFrameSchema,
  OutboundHeartbeatPongFrameSchema,
]);
export type OutboundHeartbeatFrame = z.infer<typeof OutboundHeartbeatFrameSchema>;

// ── daemon → server event catalogue ────────────────────────────────

export const OutboundReadyEventFrameSchema = HelloResultSchema.extend({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.ready),
  protocol_version: z.literal(OUTBOUND_PROTOCOL_VERSION),
  daemon_pid: z.number().int(),
  started_at: z.number().int(),
});

export const OutboundShuttingDownEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.shuttingDown),
  reason: z.string(),
});

export const OutboundRuntimeStateEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeState),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  runtime_generation: z.number().int().positive(),
  state: AgentRunStateSchema,
  ts: z.number().int(),
});

export const OutboundRuntimeOutputEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeOutput),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  stream: z.enum(['stdout', 'stderr']),
  chunk: z.string(),
  ts: z.number().int(),
});

export const OutboundRuntimeExitEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeExit),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  runtime_generation: z.number().int().positive(),
  code: z.number().int().nullable(),
  intentional: z.boolean(),
  ts: z.number().int(),
});

export const OutboundRuntimeReadyForDrainEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeReadyForDrain),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  runtime_generation: z.number().int().positive(),
  ts: z.number().int(),
});

export const OutboundRuntimeDeliveryConsumedEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeDeliveryConsumed),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  runtime_generation: z.number().int().positive(),
  delivery_ids: z.array(z.number().int()).min(1),
  ts: z.number().int(),
});

export const OutboundRuntimeDeliveryFailedEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeDeliveryFailed),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  runtime_generation: z.number().int().positive(),
  delivery_ids: z.array(z.number().int()).min(1),
  reason: z.enum(['child_exit', 'init_watchdog', 'write_failed', 'driver_refused']),
  ts: z.number().int(),
});

export const OutboundRuntimeInitTimeoutEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeInitTimeout),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  runtime_generation: z.number().int().positive(),
  timeout_reason: z.enum(['silence', 'pre_protocol_cap', 'post_protocol']),
  elapsed_ms: z.number().int().nonnegative(),
  timeout_ms: z.number().int().nonnegative(),
  retrying: z.boolean(),
  retry_attempt: z.number().int().nonnegative(),
  max_retries: z.number().int().nonnegative(),
  cooldown_until: z.number().int().positive().optional(),
  ts: z.number().int(),
});

export const OutboundRuntimeInitRetryingEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeInitRetrying),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  runtime_generation: z.number().int().positive(),
  retry_attempt: z.number().int().positive(),
  max_retries: z.number().int().nonnegative(),
  delay_ms: z.number().int().nonnegative(),
  ts: z.number().int(),
});

export const OutboundRuntimeInitCooldownEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeInitCooldown),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  runtime_generation: z.number().int().positive(),
  cooldown_until: z.number().int().positive(),
  attempts_exhausted: z.number().int().nonnegative(),
  ts: z.number().int(),
});

export const OutboundRuntimeCompactionStaleEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeCompactionStale),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  elapsed_ms: z.number().int().nonnegative(),
  ts: z.number().int(),
});

export const OutboundRuntimeOutputDroppedEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeOutputDropped),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  dropped_count: z.number().int().nonnegative(),
  bytes_dropped: z.number().int().nonnegative(),
  since_ts: z.number().int(),
});

export const OutboundRuntimeOutputPausedEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeOutputPaused),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
});

export const OutboundRuntimeOutputResumedEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeOutputResumed),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
});

export const OutboundRuntimeParsedEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeParsedEvent),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  runtime_generation: z.number().int().positive(),
  parsed: ParsedEventSchema,
  ts: z.number().int(),
});

export const OutboundRuntimeParsedEventDroppedFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeParsedEventDropped),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  parsed_kind: ParsedEventKindSchema,
  encoded_bytes: z.number().int().nonnegative(),
  reason: z.literal('frame_too_large'),
  ts: z.number().int(),
});

/**
 * `runtime_message_stale` (#task:2099) — the backend's own freshness verdict,
 * relayed back by the daemon so the activity axis has ONE author and the row
 * lands on the handle's focus conversation. See the local `EventFrameSchema`
 * arm in `protocol.ts` for why this is not a `runtime_parsed_event` and why it
 * carries no per-conflict ids.
 */
export const OutboundRuntimeMessageStaleEventFrameSchema = z.object({
  direction: z.literal('daemon_to_server'),
  type: z.literal('event'),
  kind: z.literal(DAEMON_ADMISSION.daemonEventKinds.runtimeMessageStale),
  runtime_scope: RuntimeScopeSchema,
  conversation_id: z.string().min(1),
  agent_id: z.string().min(1),
  runtime_generation: z.number().int().positive(),
  ts: z.number().int(),
});

const OutboundDaemonToServerEventFrameUnion = z.discriminatedUnion('kind', [
  OutboundReadyEventFrameSchema,
  OutboundShuttingDownEventFrameSchema,
  OutboundRuntimeStateEventFrameSchema,
  OutboundRuntimeOutputEventFrameSchema,
  OutboundRuntimeExitEventFrameSchema,
  OutboundRuntimeReadyForDrainEventFrameSchema,
  OutboundRuntimeDeliveryConsumedEventFrameSchema,
  OutboundRuntimeDeliveryFailedEventFrameSchema,
  OutboundRuntimeInitTimeoutEventFrameSchema,
  OutboundRuntimeInitRetryingEventFrameSchema,
  OutboundRuntimeInitCooldownEventFrameSchema,
  OutboundRuntimeCompactionStaleEventFrameSchema,
  OutboundRuntimeOutputDroppedEventFrameSchema,
  OutboundRuntimeOutputPausedEventFrameSchema,
  OutboundRuntimeOutputResumedEventFrameSchema,
  OutboundRuntimeParsedEventFrameSchema,
  OutboundRuntimeParsedEventDroppedFrameSchema,
  OutboundRuntimeMessageStaleEventFrameSchema,
]);

/**
 * The kinds implemented by the real Zod union. This is intentionally derived
 * from the union rather than from daemon-admission.json, so the fixture gate
 * detects drift in either direction.
 */
export const OUTBOUND_DAEMON_EVENT_SCHEMA_KINDS = OutboundDaemonToServerEventFrameUnion.options.map(
  (option) => option.shape.kind.value,
);

export const OutboundDaemonToServerEventFrameSchema =
  OutboundDaemonToServerEventFrameUnion.superRefine((frame, ctx) => {
    if (
      frame.kind === DAEMON_ADMISSION.daemonEventKinds.runtimeState &&
      (frame.state.agentId !== frame.agent_id ||
        frame.state.conversationId !== frame.conversation_id)
    ) {
      ctx.addIssue({
        code: 'custom',
        path: ['state'],
        message: 'runtime state coordinate must match its outer event',
      });
    }
  });

export type OutboundDaemonToServerEventFrame = z.infer<
  typeof OutboundDaemonToServerEventFrameSchema
>;

export type OutboundDaemonEventKind = OutboundDaemonToServerEventFrame['kind'];

export const OUTBOUND_DAEMON_EVENT_KINDS = Object.values(
  DAEMON_ADMISSION.daemonEventKinds,
) as OutboundDaemonEventKind[];

export const OutboundFrameSchema = z.union([
  OutboundServerToDaemonCommandFrameSchema,
  OutboundDaemonToServerCommandResponseFrameSchema,
  OutboundHeartbeatFrameSchema,
  OutboundDaemonToServerEventFrameSchema,
]);
export type OutboundFrame = z.infer<typeof OutboundFrameSchema>;

export function encodeOutboundFrame(
  frame: OutboundFrame,
  responseCommand?: OutboundServerToDaemonCommandFrame,
): string {
  const line = JSON.stringify(frame);
  const budget = responseFrameBudget(
    typeof responseCommand?.kind === 'string' ? responseCommand.kind : undefined,
    frame.type === 'command_response' &&
      'result' in frame &&
      !('error' in frame) &&
      frame.id === responseCommand?.id &&
      frame.kind === responseCommand?.kind,
  );
  if (Buffer.byteLength(line, 'utf8') > budget) {
    throw new Error(`encoded outbound frame exceeds MAX_FRAME_BYTES budget=${budget}`);
  }
  return line;
}
