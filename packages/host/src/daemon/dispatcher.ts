/**
 * In-memory intent dispatcher.
 *
 * Split from socket transport so:
 *
 *  - Unit tests can drive `dispatch(intent, params)` with raw JSON
 *    and assert the typed response without spinning up `node:net`.
 *  - PR-2 can slot in `runtime_*` handlers here without touching the
 *    socket loop.
 *
 * The dispatcher owns:
 *
 *  - The daemon's `started_at` (set on construction, surfaced via
 *    `ping` / `status`)
 *  - `intent_count` — number of intents handled this lifetime, used
 *    by reconnect tests to assert "second batch actually re-issued".
 *  - `shuttingDown` flag — once set, every further intent returns
 *    `{ code: 'shutting_down' }` instead of executing the handler.
 *
 * It does NOT own:
 *
 *  - The socket. The transport layer (server.ts) calls `dispatch` per
 *    decoded frame and writes the returned `ResponseFrame` back.
 *  - The pid file or signal handlers (lifecycle.ts).
 *  - Logging policy. The transport hands a logger in via the ctor; the
 *    dispatcher only logs errors that handlers throw.
 */
import {
  type HelloResult,
  intentErrorSchema,
  intentSchemas,
  HOST_ACTIVE_RUNTIME_PHASES,
  type AgentMessageSendParams,
  type AgentStaleReceiveParams,
  type AgentStaleResolveParams,
  type AgentMaterializeParams,
  type AgentMaterializeResult,
  type AgentWorkspaceIndexParams,
  type AgentWorkspaceIndexResult,
  type AgentWorkspaceTreeParams,
  type AgentWorkspaceTreeResult,
  type AgentWorkspaceFileReadParams,
  type AgentWorkspaceFileReadResult,
  type AgentL2SeedParams,
  type AgentL2SeedResult,
  type AgentFilesPurgeParams,
  type AgentFilesPurgeResult,
  type AgentProfileMcpReadParams,
  type AgentProfileMcpReadResult,
  type AgentProfileMcpWriteParams,
  type AgentProfileMcpWriteResult,
  type McpProbeParams,
  type McpProbeResult,
  type ConversationLogDebugParams,
  type HostOutboundAttachParams,
  type HostOutboundAttachResult,
  type HostOutboundDetachResult,
  type HostOutboundReloadResult,
  type HostProxyConfigGetResult,
  type HostProxyConfigSetParams,
  type HostProxyConfigSetResult,
  type HostRuntimeCatalogParams,
  type HostStatusParams,
  type IntentName,
  type ResponseError,
  type ResponseFrame,
  type RuntimeClearSessionParams,
  type RuntimeCloseActivityLogParams,
  type RuntimeDeliverParams,
  type RuntimeOutputPauseParams,
  type RuntimeOutputResumeParams,
  type RuntimeStartParams,
  type RuntimeStatusParams,
  type RuntimeStopAllParams,
  type RuntimeStopParams,
} from '../protocol.js';
import { detectRuntimeCatalog } from '../runtimes.js';
import { DAEMON_VERSION, createDaemonHelloResult } from './version.js';
import { composeHostOs, hostOsArch } from './host-os.js';
import { readHostMachineFacts } from './host-machine.js';
import type { DaemonLogger } from './logger.js';
import type { DeliveryOwnerContext, SupervisorContract } from './supervisor-contract.js';
import { logIntent } from './daemon-logging.js';
import { DaemonTypedError } from './typed-error.js';
import {
  agentHomeCommandEffect,
  commandAgentId,
  isAgentHomeMutatorEffect,
} from '../agent-home-command-effect.js';
import { AgentHomeMutationCoordinator } from './agent-home-mutation-coordinator.js';

export interface DispatcherOptions {
  logger: DaemonLogger;
  /** Epoch ms; injected so tests can pin time. Default: Date.now(). */
  startedAt?: number;
  /** Epoch ms supplier; injected so tests can pin time. Default: Date.now. */
  now?: () => number;
  /** Daemon hello payload bound to this daemon process generation. */
  helloResult?: HelloResult;
  /**
   * SupervisorContract backing the runtime_* intents. `null` keeps the
   * protocol-only PR-1 surface for tests; production wires the
   * concrete `Supervisor`.
   */
  supervisor?: SupervisorContract;
  /** Single per-agent fence owner shared with Supervisor in production. */
  agentHomeMutationCoordinator?: AgentHomeMutationCoordinator;
  /**
   * Stage D3.a Commit 3 — handler for the `host_outbound_attach`
   * intent. The daemon's `OutboundSupervisor.attach` is the
   * production wiring; tests pass a stub. `undefined` means the
   * intent is not wired and the dispatcher throws (translated to
   * `internal_error` by the catch arm). This shape mirrors the
   * `supervisor` field rather than passing the supervisor object
   * directly so the dispatcher stays free of `OutboundSupervisor`'s
   * concrete dependencies (filesystem, ws, etc.).
   */
  hostOutboundAttachHandler?: (
    params: HostOutboundAttachParams,
  ) => HostOutboundAttachResult | Promise<HostOutboundAttachResult>;
  /**
   * Stage D3.b Commit 3 — handler for the `host_outbound_detach`
   * intent. Production wiring: `OutboundSupervisor.detach`. Mirrors
   * the `hostOutboundAttachHandler` slot pattern so the dispatcher
   * stays free of supervisor concrete dependencies. `undefined`
   * means the intent is not wired and the dispatcher throws
   * (translated to `internal_error` by the catch arm).
   */
  hostOutboundDetachHandler?: () => HostOutboundDetachResult | Promise<HostOutboundDetachResult>;
  /**
   * Stage D3.c Commit 2 — handler for the `host_outbound_reload`
   * intent. Production wiring: `OutboundSupervisor.reload`. Same
   * slot pattern as attach/detach so the dispatcher stays free of
   * supervisor concrete dependencies. Reload is a daemon-control
   * intent (not a runtime_* intent), so it has its own slot.
   * `undefined` means the intent is not wired and the dispatcher
   * throws (translated to `internal_error` by the catch arm).
   */
  hostOutboundReloadHandler?: () => HostOutboundReloadResult | Promise<HostOutboundReloadResult>;
  hostProxyConfigGetHandler?: () => HostProxyConfigGetResult | Promise<HostProxyConfigGetResult>;
  hostProxyConfigSetHandler?: (
    params: HostProxyConfigSetParams,
  ) => HostProxyConfigSetResult | Promise<HostProxyConfigSetResult>;
  /**
   * D4.d D2 — handler for the `agent_materialize` intent. Unlike
   * the runtime_* family (local supervisor / control) and the
   * host_outbound_* family (transport plumbing), agent_materialize
   * is formal server↔daemon semantics: the server asks the
   * daemon-owning host to seed an agent's L1 home dir. Production
   * wiring: `materializeAgent` from
   * `./agent-materialize/handler.ts`. Tests pass a stub.
   *
   * `undefined` means the slot is not wired and the dispatcher
   * throws (translated to `internal_error` by the catch arm), same
   * pattern as the host_outbound_* slots.
   *
   * Typed error preservation (Cindy D2 pre-gate #3): the handler
   * may throw `AgentMaterializeError` carrying a typed code
   * (`fs_quota_exceeded` / `fs_unavailable` / `agent_dir_collision`
   * / `unsupported_template` / `unsupported_runtime` /
   * `internal_error`). The dispatcher's `DaemonTypedError` catch arm
   * recognises it and emits the typed code on the wire instead of
   * collapsing to `internal_error` — but only after re-validating
   * the code against `intentErrorSchema(intent)`, so the typed
   * surface stays decided by protocol schema, not the class. The
   * outbound layer's per-command error schema
   * (`AgentMaterializeWireErrorSchema`) already permits this widened
   * code set; the D4.e workspace intent `agent_workspace_index` is
   * likewise typed, while runtime/default commands keep the narrow
   * `ResponseError` enum.
   */
  agentMaterializeHandler?: (
    params: AgentMaterializeParams,
  ) => AgentMaterializeResult | Promise<AgentMaterializeResult>;
  /**
   * D4.e-1 — handler for `agent_workspace_index` (route-time `agent_dir`
   * resolver). Production wiring: `indexAgentWorkspaces`. Typed errors
   * (`AgentWorkspaceIndexError`) projected as above. `undefined` → the
   * dispatcher throws (→ `internal_error`).
   */
  agentWorkspaceIndexHandler?: (
    params: AgentWorkspaceIndexParams,
  ) => AgentWorkspaceIndexResult | Promise<AgentWorkspaceIndexResult>;
  /**
   * #task:811 — handler for `agent_workspace_tree` (flat tree of every
   * visible entry under the agent home dir). Production wiring:
   * `listAgentWorkspaceTree`. Typed errors (`AgentWorkspaceFsError`)
   * are projected by the catch arm after per-intent schema validation.
   * `undefined` → the dispatcher throws (→ `internal_error`).
   */
  agentWorkspaceTreeHandler?: (
    params: AgentWorkspaceTreeParams,
  ) => AgentWorkspaceTreeResult | Promise<AgentWorkspaceTreeResult>;
  /**
   * #task:811 — handler for `agent_workspace_file_read` (read one file
   * from the agent home dir by virtual path). Production wiring:
   * `readAgentWorkspaceFile`. Same typed-error handling as the tree
   * handler (the two share `AgentWorkspaceFsError` taxonomy:
   * `forbidden_path` / `not_found` / `fs_unavailable` /
   * `internal_error`).
   */
  agentWorkspaceFileReadHandler?: (
    params: AgentWorkspaceFileReadParams,
  ) => AgentWorkspaceFileReadResult | Promise<AgentWorkspaceFileReadResult>;
  /**
   * D4.e-3 — handler for `agent_l2_seed` (member agent L2 seed on the
   * owning host). Production wiring: `seedAgentL2`. Typed errors
   * (`AgentL2SeedError`) are projected with their code by the catch arm
   * after per-intent schema validation. `undefined` → the dispatcher
   * throws (→ `internal_error`).
   */
  agentL2SeedHandler?: (
    params: AgentL2SeedParams,
  ) => AgentL2SeedResult | Promise<AgentL2SeedResult>;
  /**
   * D4.e-4 — handler for `agent_files_purge` (remove an agent's host-local
   * L1/L2 files on the owning host). Production wiring: `purgeAgentFiles`.
   * Typed errors (`AgentFilesPurgeError`) are projected with their code by
   * the catch arm after per-intent schema validation. `undefined` → the
   * dispatcher throws (→ `internal_error`).
   */
  agentFilesPurgeHandler?: (
    params: AgentFilesPurgeParams,
  ) =>
    | Omit<AgentFilesPurgeResult, 'quiesced_count'>
    | Promise<Omit<AgentFilesPurgeResult, 'quiesced_count'>>;
  /**
   * #task:1135 — handlers for `agent_profile_mcp_read` /
   * `agent_profile_mcp_write` (the host-local truth behind
   * `silverhand profile mcp get|set`). Production wiring:
   * `readAgentProfileMcp` / `writeAgentProfileMcp`. Typed errors
   * (`AgentProfileMcpError`) are projected with their code by the
   * DaemonTypedError catch arm.
   */
  agentProfileMcpReadHandler?: (
    params: AgentProfileMcpReadParams,
  ) => AgentProfileMcpReadResult | Promise<AgentProfileMcpReadResult>;
  agentProfileMcpWriteHandler?: (
    params: AgentProfileMcpWriteParams,
  ) => AgentProfileMcpWriteResult | Promise<AgentProfileMcpWriteResult>;
  /**
   * #task:1137 — handler for `mcp_probe` (the live MCP diagnostic behind
   * `silverhand profile mcp probe` and the agent-pane Test connection / List
   * tools buttons). Production wiring: `probeMcpServer`. Connection/handshake/
   * tools failures come back INSIDE the result (`ok:false` + phase); only a
   * host-local fault throws a typed `McpProbeError`.
   */
  mcpProbeHandler?: (params: McpProbeParams) => McpProbeResult | Promise<McpProbeResult>;
}

export class IntentDispatcher {
  private readonly logger: DaemonLogger;
  private readonly startedAt: number;
  private readonly now: () => number;
  private readonly helloResult: HelloResult;
  private readonly supervisor: SupervisorContract | undefined;
  private readonly agentHomeMutationCoordinator: AgentHomeMutationCoordinator;
  private readonly hostOutboundAttachHandler:
    | ((
        params: HostOutboundAttachParams,
      ) => HostOutboundAttachResult | Promise<HostOutboundAttachResult>)
    | undefined;
  private readonly hostOutboundDetachHandler:
    | (() => HostOutboundDetachResult | Promise<HostOutboundDetachResult>)
    | undefined;
  private readonly hostOutboundReloadHandler:
    | (() => HostOutboundReloadResult | Promise<HostOutboundReloadResult>)
    | undefined;
  private readonly hostProxyConfigGetHandler:
    | (() => HostProxyConfigGetResult | Promise<HostProxyConfigGetResult>)
    | undefined;
  private readonly hostProxyConfigSetHandler:
    | ((
        params: HostProxyConfigSetParams,
      ) => HostProxyConfigSetResult | Promise<HostProxyConfigSetResult>)
    | undefined;
  private readonly agentMaterializeHandler:
    | ((params: AgentMaterializeParams) => AgentMaterializeResult | Promise<AgentMaterializeResult>)
    | undefined;
  private readonly agentWorkspaceIndexHandler:
    | ((
        params: AgentWorkspaceIndexParams,
      ) => AgentWorkspaceIndexResult | Promise<AgentWorkspaceIndexResult>)
    | undefined;
  private readonly agentWorkspaceTreeHandler:
    | ((
        params: AgentWorkspaceTreeParams,
      ) => AgentWorkspaceTreeResult | Promise<AgentWorkspaceTreeResult>)
    | undefined;
  private readonly agentWorkspaceFileReadHandler:
    | ((
        params: AgentWorkspaceFileReadParams,
      ) => AgentWorkspaceFileReadResult | Promise<AgentWorkspaceFileReadResult>)
    | undefined;
  private readonly agentL2SeedHandler:
    | ((params: AgentL2SeedParams) => AgentL2SeedResult | Promise<AgentL2SeedResult>)
    | undefined;
  private readonly agentFilesPurgeHandler:
    | ((
        params: AgentFilesPurgeParams,
      ) =>
        | Omit<AgentFilesPurgeResult, 'quiesced_count'>
        | Promise<Omit<AgentFilesPurgeResult, 'quiesced_count'>>)
    | undefined;
  private readonly agentProfileMcpReadHandler:
    | ((
        params: AgentProfileMcpReadParams,
      ) => AgentProfileMcpReadResult | Promise<AgentProfileMcpReadResult>)
    | undefined;
  private readonly agentProfileMcpWriteHandler:
    | ((
        params: AgentProfileMcpWriteParams,
      ) => AgentProfileMcpWriteResult | Promise<AgentProfileMcpWriteResult>)
    | undefined;
  private readonly mcpProbeHandler:
    | ((params: McpProbeParams) => McpProbeResult | Promise<McpProbeResult>)
    | undefined;
  private intentCount = 0;
  private shuttingDown = false;

  constructor(opts: DispatcherOptions) {
    this.logger = opts.logger;
    this.now = opts.now ?? Date.now;
    this.startedAt = opts.startedAt ?? this.now();
    // The fallback `helloResult` exists for the protocol-only test
    // matrix where the dispatcher is exercised in isolation. Prod
    // wiring goes through `runDaemon`, which always passes a fully
    // materialised `helloResult` (real `instance_id`, real identity).
    // `'__test_instance__'` is the grep-friendly sentinel — if it
    // ever shows up on the wire in prod logs, something forgot to
    // pass `helloResult`.
    this.helloResult = opts.helloResult ?? createDaemonHelloResult('__test_instance__');
    this.supervisor = opts.supervisor;
    const supervisorCoordinator = opts.supervisor?.agentHomeMutationCoordinator?.() ?? null;
    if (
      supervisorCoordinator &&
      opts.agentHomeMutationCoordinator &&
      supervisorCoordinator !== opts.agentHomeMutationCoordinator
    ) {
      throw new Error('IntentDispatcher refused split agent-home fence authorities');
    }
    if (opts.supervisor?.fenceAndQuiesceAgent && !supervisorCoordinator) {
      throw new Error('purge-capable Supervisor must expose its agent-home mutation coordinator');
    }
    this.agentHomeMutationCoordinator =
      supervisorCoordinator ??
      opts.agentHomeMutationCoordinator ??
      new AgentHomeMutationCoordinator();
    this.hostOutboundAttachHandler = opts.hostOutboundAttachHandler;
    this.hostOutboundDetachHandler = opts.hostOutboundDetachHandler;
    this.hostOutboundReloadHandler = opts.hostOutboundReloadHandler;
    this.hostProxyConfigGetHandler = opts.hostProxyConfigGetHandler;
    this.hostProxyConfigSetHandler = opts.hostProxyConfigSetHandler;
    this.agentMaterializeHandler = opts.agentMaterializeHandler;
    this.agentWorkspaceIndexHandler = opts.agentWorkspaceIndexHandler;
    this.agentWorkspaceTreeHandler = opts.agentWorkspaceTreeHandler;
    this.agentWorkspaceFileReadHandler = opts.agentWorkspaceFileReadHandler;
    this.agentL2SeedHandler = opts.agentL2SeedHandler;
    this.agentFilesPurgeHandler = opts.agentFilesPurgeHandler;
    this.agentProfileMcpReadHandler = opts.agentProfileMcpReadHandler;
    this.agentProfileMcpWriteHandler = opts.agentProfileMcpWriteHandler;
    this.mcpProbeHandler = opts.mcpProbeHandler;
  }

  /** Signal that subsequent intents should be rejected with
   *  `shutting_down`. Idempotent. */
  markShuttingDown(): void {
    this.shuttingDown = true;
  }

  isShuttingDown(): boolean {
    return this.shuttingDown;
  }

  getStartedAt(): number {
    return this.startedAt;
  }

  getIntentCount(): number {
    return this.intentCount;
  }

  getHelloResult(): HelloResult {
    return {
      ...this.helloResult,
      host_descriptor: { ...this.helloResult.host_descriptor },
      capabilities: [...this.helloResult.capabilities],
    };
  }

  /**
   * Dispatch a single intent. Always returns a well-formed
   * `ResponseFrame` — never throws. Handlers that throw are caught
   * and projected as `internal_error`; the caller writes the frame
   * to the socket as-is.
   */
  async dispatch(
    requestId: string,
    intent: string,
    rawParams: unknown,
    ctx: DeliveryOwnerContext,
  ): Promise<ResponseFrame> {
    const startedAtMs = this.now();
    // Extract conversationId / agent_id from params (when present) for the log
    // line — strictly read-only, never modifies params. Other fields
    // (notably `payload`) MUST NOT be logged per §3.4.
    const convId =
      typeof rawParams === 'object' && rawParams !== null && 'conversation_id' in rawParams
        ? String((rawParams as { conversation_id: unknown }).conversation_id)
        : undefined;
    const agentId =
      typeof rawParams === 'object' && rawParams !== null && 'agent_id' in rawParams
        ? String((rawParams as { agent_id: unknown }).agent_id)
        : undefined;
    logIntent(this.logger, {
      intent,
      request_id: requestId,
      status: 'received',
      conversationId: convId,
      agent_id: agentId,
    });
    const finish = (frame: ResponseFrame): ResponseFrame => {
      const isError = 'error' in frame;
      logIntent(this.logger, {
        intent,
        request_id: requestId,
        status: isError ? 'error' : 'ok',
        conversationId: convId,
        agent_id: agentId,
        error_code: isError ? frame.error.code : undefined,
        duration_ms: this.now() - startedAtMs,
      });
      return frame;
    };
    if (this.shuttingDown) {
      return finish(
        this.errorFrame(requestId, {
          code: 'shutting_down',
          message: 'daemon is shutting down',
        }),
      );
    }
    const schema = intentSchemas[intent as IntentName];
    if (!schema) {
      return finish(
        this.errorFrame(requestId, {
          code: 'unknown_intent',
          message: `unknown intent: ${intent}`,
        }),
      );
    }
    const parsed = schema.params.safeParse(rawParams);
    if (!parsed.success) {
      return finish(
        this.errorFrame(requestId, {
          code: 'invalid_params',
          message: parsed.error.errors.map((e) => `${e.path.join('.')}: ${e.message}`).join('; '),
        }),
      );
    }
    this.intentCount += 1;
    try {
      const intentName = intent as IntentName;
      const effect = agentHomeCommandEffect(intentName);
      const execute = (): Promise<unknown> => this.runHandler(intentName, parsed.data, ctx);
      let result: unknown;
      if (isAgentHomeMutatorEffect(effect)) {
        const scopedAgentId = commandAgentId(parsed.data);
        if (!scopedAgentId) {
          throw new Error(`${intent} is classified as an agent mutator but has no agent_id`);
        }
        result = await this.agentHomeMutationCoordinator.runBounded(scopedAgentId, execute);
      } else if (effect === 'purge') {
        const scopedAgentId = commandAgentId(parsed.data);
        if (!scopedAgentId) {
          throw new Error(`${intent} is classified as purge but has no agent_id`);
        }
        result = await this.agentHomeMutationCoordinator.runPurge(scopedAgentId, async () => {
          const quiesced = await this.requireAgentQuiescenceSupervisor()(scopedAgentId);
          const purged = await this.requireAgentFilesPurgeHandler()(
            parsed.data as AgentFilesPurgeParams,
          );
          return { ...purged, quiesced_count: quiesced.quiesced_count };
        });
      } else {
        result = await execute();
      }
      // Validate the handler's own result against the declared schema
      // BEFORE sending it on the wire. If a handler ever drifts from
      // the contract (e.g. a refactor accidentally drops a field, or
      // the schema gets tightened without updating the handler), we
      // want to catch it at the IPC boundary on the *daemon* side
      // rather than ship malformed bytes the client has to puzzle out.
      //
      // The mirror check on the client (HostClient.handleFrame) is a
      // second line of defense against version skew between a newer
      // daemon and older client (or vice versa).
      const resultValidated = schema.result.safeParse(result);
      if (!resultValidated.success) {
        this.logger.error(
          {
            intent,
            issues: resultValidated.error.errors
              .map((e) => `${e.path.join('.')}: ${e.message}`)
              .join('; '),
          },
          'daemon emitted result that does not match schema',
        );
        return finish(
          this.errorFrame(requestId, {
            code: 'internal_error',
            message: `daemon result drift for intent ${intent}`,
          }),
        );
      }
      return finish({ id: requestId, result: resultValidated.data });
    } catch (err) {
      // D4.d D2 (Cindy pre-gate #3 + P1-1): preserve typed daemon-
      // business errors from `agent_materialize` (fs_quota_exceeded
      // / fs_unavailable / agent_dir_collision / unsupported_template
      // / unsupported_runtime / internal_error). Without this branch
      // the catch arm below would squash all six codes into
      // `internal_error`, which would lose the recoverable /
      // unrecoverable / collision distinction the route layer needs
      // to decide retry / 503 / 507 / 409.
      //
      // P1-1 closes the loop on the EMITTING side too: the typed
      // code is validated against `intentErrorSchema(intent)` before
      // being put on the wire, so a handler that drifts (throws a
      // code the intent's error taxonomy doesn't cover) still gets
      // surfaced as `internal_error` — preventing daemon-side drift
      // from reaching the wire as it did during the bare-cast era.
      // D4.d D2 / D4.e-1: preserve typed daemon-business errors thrown by
      // a handler (any `DaemonTypedError` subclass: AgentMaterializeError,
      // AgentWorkspaceIndexError). Without this
      // branch the generic catch below would squash the typed code into
      // `internal_error`, losing the recoverable / unrecoverable /
      // collision / unresolved distinctions the route layer needs.
      //
      // Boundary (Cindy): the base class does NOT decide the typed
      // surface — the code is validated against `intentErrorSchema(intent)`
      // before going on the wire, so a code that is not in the dispatching
      // intent's error taxonomy still collapses to `internal_error`. This
      // keeps the wire-error scope per-intent: only intents whose schema
      // widens (agent_materialize /
      // agent_workspace_index) can carry their typed codes; a runtime_*
      // non-spawning runtime intent (defaulting to the 5 generic codes)
      // rejects any foreign typed code here. `runtime_start` and
      // `runtime_deliver` both widen because either may spawn.
      if (err instanceof DaemonTypedError) {
        const errorSchema = intentErrorSchema(intent as IntentName);
        const validated = errorSchema.safeParse({
          code: err.code,
          message: err.message,
        });
        if (validated.success) {
          this.logger.warn(
            {
              intent,
              err: err.message,
              code: err.code,
            },
            'intent handler returned typed error',
          );
          return finish(this.typedErrorFrame(requestId, err.code, err.message));
        }
        this.logger.error(
          {
            intent,
            err: err.message,
            code: err.code,
            issues: validated.error.errors
              .map((e) => `${e.path.join('.')}: ${e.message}`)
              .join('; '),
          },
          'intent handler returned a code not in its IPC error taxonomy; emitting internal_error',
        );
        return finish(
          this.errorFrame(requestId, {
            code: 'internal_error',
            message: `drift: code "${err.code}" not in ${intent} error taxonomy`,
          }),
        );
      }
      this.logger.error(
        {
          intent,
          err: err instanceof Error ? err.message : String(err),
        },
        'intent handler threw',
      );
      return finish(
        this.errorFrame(requestId, {
          code: 'internal_error',
          message: err instanceof Error ? err.message : String(err),
        }),
      );
    }
  }

  private errorFrame(id: string, error: ResponseError): ResponseFrame {
    return { id, error };
  }

  /**
   * Build an error frame carrying a typed code that is NOT in the
   * standard `ResponseError` enum. The IPC `ResponseFrame` type
   * narrows `error.code` to the 5 generic codes; this helper
   * bypasses that narrowing so daemon-business typed codes
   * (the `AgentMaterializeErrorCode` values plus the D4.e
   * `AgentWorkspaceIndexErrorCode` values) can travel through to:
   *   - The outbound WS layer, which validates them against the
   *     wider per-command wire error schema for that intent.
   *   - The local IPC transport, which uses `LooseAnyFrameSchema`
   *     so the typed code is not dropped at frame-decode time.
   *     `HostClient.handleFrame` then validates the code against
   *     `intentErrorSchema(intent)` and surfaces a typed
   *     `HostClientError` (Cindy D2 P1-1).
   *
   * The cast is honest: this helper IS the place the typed
   * widening happens. The caller side has already validated the
   * code against the per-intent error schema, so we know it's
   * legitimate within the per-intent contract.
   */
  private typedErrorFrame(id: string, code: string, message: string): ResponseFrame {
    return {
      id,
      error: { code, message } as unknown as ResponseError,
    };
  }

  /**
   * Per-intent business logic. PR-1's lifecycle intents
   * (ping / status / echo) are handled inline; PR-2's runtime_*
   * intents are routed to the injected `SupervisorContract` when one is
   * provided, and reported as `unknown_intent` (via the schema
   * registry already rejecting unknown names) when no supervisor
   * is wired — that keeps backward-compatibility with PR-1 tests
   * that construct a dispatcher without a supervisor.
   *
   */
  private async runHandler(
    intent: IntentName,
    params: unknown,
    ctx: DeliveryOwnerContext,
  ): Promise<unknown> {
    switch (intent) {
      case 'hello':
        return this.getHelloResult();
      case 'ping':
        return {
          pong: true as const,
          daemon_pid: process.pid,
          daemon_version: DAEMON_VERSION,
          started_at: this.startedAt,
        };
      case 'status':
        return {
          daemon_pid: process.pid,
          daemon_version: DAEMON_VERSION,
          started_at: this.startedAt,
          uptime_ms: this.now() - this.startedAt,
          intent_count: this.intentCount,
          runtimes: this.supervisor ? (await this.supervisor.status({})).runtimes : [],
        };
      case 'echo': {
        const p = params as { message: string };
        return { echoed: p.message };
      }
      case 'host_status': {
        const p = params as HostStatusParams;
        // #task:808 — host snapshot powering `GET /api/hosts/:id/status`.
        // Daemon-owned facts: OS string composed locally (the server NEVER
        // synthesizes OS from cached DB rows — that's the whole point of
        // routing this through the daemon), `process.pid`/version/uptime,
        // plus the active agent_id subset so backend can intersect against
        // the registration table for `online` enrichment. The full
        // `RuntimeStateDto` shape is intentionally NOT echoed back — backend
        // only needs the id set; sending pid/phase/run_state here would
        // duplicate the `runtime_status` surface and leak runtime detail
        // that ComputerPane doesn't render.
        //
        // Phase whitelist (Cindy `oJ-mzKP7kbXtHUTe` slice-1 review P1 #2):
        // backend's outbound-event-fanout already treats `idle || exited`
        // as non-live (delivery / start gates). Express the contract as a
        // whitelist of active phases so a NEW phase added to the protocol
        // enum fails CLOSED (not online) until the ComputerPane semantics
        // for it are explicit. #task:1588 single-sourced the whitelist to
        // protocol.ts (`HOST_ACTIVE_RUNTIME_PHASES`); the backend host-
        // status read model derives liveness from the same set, so the
        // two ends cannot drift.
        return {
          os: composeHostOs(),
          os_arch: hostOsArch(),
          daemon_pid: process.pid,
          daemon_version: DAEMON_VERSION,
          started_at: this.startedAt,
          uptime_ms: this.now() - this.startedAt,
          active_agent_ids: this.supervisor
            ? Array.from(
                new Set(
                  (await this.supervisor.status({})).runtimes
                    .filter((r) => HOST_ACTIVE_RUNTIME_PHASES.has(r.phase))
                    .map((r) => r.agent_id),
                ),
              )
            : [],
          ...(p.include_machine === true ? readHostMachineFacts() : {}),
        };
      }
      case 'host_runtime_catalog': {
        const p = params as HostRuntimeCatalogParams;
        return detectRuntimeCatalog({
          force: p.force === true,
        });
      }
      case 'conversation_log_debug': {
        const p = params as ConversationLogDebugParams;
        if (p.enabled) {
          this.logger.logControl.enableConversationDebug(p.conversation_id, p.expires_at);
          return { ok: true, enabled: true, expires_at: p.expires_at };
        }
        this.logger.logControl.disableConversationDebug(p.conversation_id);
        return { ok: true, enabled: false, expires_at: null };
      }
      case 'agent_message_send':
        return this.requireSupervisor().agentMessageSend(params as AgentMessageSendParams);
      case 'agent_stale_resolve':
        return this.requireSupervisor().agentStaleResolve(params as AgentStaleResolveParams);
      case 'agent_stale_receive':
        // #task:1614 — deferred-pull second consumer. `ctx` carries the
        // ingress's response-handoff capability; the supervisor defers
        // custody settlement to `ctx.responseHandoff` (invoked by the
        // control-socket ingress after the response frame's write
        // settles) and fails closed on ingresses that cannot prove it.
        return this.requireSupervisor().agentStaleReceive(params as AgentStaleReceiveParams, ctx);
      // ── runtime_* family ────────────────────────────────────────
      // Each case below requires `this.supervisor` to be set; when
      // it isn't, we throw — `dispatch()` catches and projects as
      // `internal_error`. Production daemons always wire a
      // supervisor, so this is only reachable via misconfigured
      // test setups.
      case 'runtime_start':
        return this.requireSupervisor().start(params as RuntimeStartParams);
      case 'runtime_stop':
        return this.requireSupervisor().stop(params as RuntimeStopParams);
      case 'runtime_deliver':
        // #task:1417 — daemon-authoritative delivery. Result transfers/
        // refuses/defers custody only; the DB ack rides the async
        // `runtime_delivery_consumed`/`_failed` events. `ctx.owner` (Stage 3,
        // §11.4.1) is the connection identity injected by the ingress — the
        // supervisor stamps it onto any `deferred` queue entry.
        return this.requireSupervisor().deliver(params as RuntimeDeliverParams, ctx);
      case 'runtime_status':
        return this.requireSupervisor().status(params as RuntimeStatusParams);
      case 'runtime_clear_session':
        return this.requireSupervisor().clearSession(params as RuntimeClearSessionParams);
      case 'runtime_stop_all':
        return this.requireSupervisor().stopAll(params as RuntimeStopAllParams);
      case 'runtime_close_activity_log':
        return this.requireSupervisor().closeActivityLog(params as RuntimeCloseActivityLogParams);
      // ── output flow control ─────────────────────────────────────
      // Cindy `_YD0KJF-YAQXlcki`: route through the SupervisorContract
      // interface instead of emitting from the dispatcher. The
      // concrete supervisor owns both flow-control state and the
      // `runtime_output_paused` / `_resumed` ack event.
      case 'runtime_output_pause':
        return this.requireSupervisor().pauseOutput(params as RuntimeOutputPauseParams);
      case 'runtime_output_resume':
        return this.requireSupervisor().resumeOutput(params as RuntimeOutputResumeParams);
      case 'host_outbound_attach':
        // Late-attach is a daemon-control intent, not a runtime_*
        // intent, so it has its own handler slot. When a daemon is
        // launched in the protocol-only test surface no handler is
        // wired; the require helper throws, the catch arm in
        // `dispatch()` projects that as `internal_error`.
        return this.requireHostOutboundAttachHandler()(params as HostOutboundAttachParams);
      case 'host_outbound_detach':
        // Detach is the symmetric daemon-control intent. No params
        // (validated to `{}` by the schema before reaching here).
        // Same protocol-only-surface caveat as attach.
        return this.requireHostOutboundDetachHandler()();
      case 'host_outbound_reload':
        // Reload is the third daemon-control intent (D3.c Commit 2).
        // No params — daemon re-reads its currently-attached
        // configPath. Hard-rejects `not_attached` via a typed result
        // arm, not via `ResponseError` (business state vs transport).
        return this.requireHostOutboundReloadHandler()();
      case 'host_proxy_config_get':
        return this.requireHostProxyConfigGetHandler()();
      case 'host_proxy_config_set':
        return this.requireHostProxyConfigSetHandler()(params as HostProxyConfigSetParams);
      case 'agent_materialize':
        // D4.d D2 — server↔daemon formal semantics. The slot is
        // wired by `runDaemon` to `materializeAgent` from
        // `./agent-materialize/handler.ts`. Typed errors thrown by
        // the handler (AgentMaterializeError) are caught in
        // `dispatch()` and projected with their typed code; other
        // throws still flow through the generic `internal_error`
        // path.
        return this.requireAgentMaterializeHandler()(params as AgentMaterializeParams);
      case 'agent_workspace_index':
        // D4.e-1 — route-time agent_dir resolver. Slot wired to
        // `indexAgentWorkspaces`.
        return this.requireAgentWorkspaceIndexHandler()(params as AgentWorkspaceIndexParams);
      case 'agent_workspace_tree':
        // #task:811 — flat tree of every visible file/directory under
        // the agent home dir. Slot wired to `listAgentWorkspaceTree`.
        // Typed errors (AgentWorkspaceFsError) are projected by the
        // DaemonTypedError catch arm.
        return this.requireAgentWorkspaceTreeHandler()(params as AgentWorkspaceTreeParams);
      case 'agent_workspace_file_read':
        // #task:811 — read one file from the agent home dir by virtual
        // path. Slot wired to `readAgentWorkspaceFile`. Same typed-
        // error taxonomy as the tree handler (forbidden_path /
        // not_found / fs_unavailable / internal_error).
        return this.requireAgentWorkspaceFileReadHandler()(params as AgentWorkspaceFileReadParams);
      case 'agent_l2_seed':
        // D4.e-3 — member agent L2 seed on the owning host. Slot wired to
        // `seedAgentL2`. Typed errors (AgentL2SeedError) are projected by
        // the DaemonTypedError catch arm.
        return this.requireAgentL2SeedHandler()(params as AgentL2SeedParams);
      case 'agent_files_purge':
        // D4.e-4 — remove an agent's host-local L1/L2 files on the owning
        // host. Slot wired to `purgeAgentFiles`. Typed errors
        // (AgentFilesPurgeError) are projected by the DaemonTypedError
        // catch arm.
        return this.requireAgentFilesPurgeHandler()(params as AgentFilesPurgeParams);
      case 'agent_profile_mcp_read':
        // #task:1135 — read the subject's L1 mcp.json. Slot wired to
        // `readAgentProfileMcp`. Typed errors (AgentProfileMcpError)
        // are projected by the DaemonTypedError catch arm.
        return this.requireAgentProfileMcpReadHandler()(params as AgentProfileMcpReadParams);
      case 'agent_profile_mcp_write':
        // #task:1135 — validate + atomically write the subject's L1
        // mcp.json. Slot wired to `writeAgentProfileMcp`.
        return this.requireAgentProfileMcpWriteHandler()(params as AgentProfileMcpWriteParams);
      case 'mcp_probe':
        // #task:1137 — one-shot live MCP probe. Slot wired to
        // `probeMcpServer`. Connection/handshake/tools failures come back in
        // the result; only host faults throw (McpProbeError).
        return this.requireMcpProbeHandler()(params as McpProbeParams);
    }
  }

  /** Internal helper for runtime_* handlers. Throws a typed error
   *  the dispatcher's catch arm will project as `internal_error`. */
  private requireSupervisor(): SupervisorContract {
    if (!this.supervisor) {
      throw new Error('runtime_* intent reached dispatcher with no supervisor wired');
    }
    return this.supervisor;
  }

  private requireAgentQuiescenceSupervisor(): NonNullable<
    SupervisorContract['fenceAndQuiesceAgent']
  > {
    const supervisor = this.requireSupervisor();
    if (!supervisor.fenceAndQuiesceAgent) {
      throw new Error(
        'agent_files_purge intent reached dispatcher with no agent quiescence supervisor wired',
      );
    }
    return supervisor.fenceAndQuiesceAgent.bind(supervisor);
  }

  /**
   * Owner-scoped deferred-queue cleanup (#task:1417 Stage 3, §11.4.1).
   * An ingress calls this when its connection drops so the daemon discards
   * only that connection's queued (unwritten) deliveries. No-op when no
   * supervisor is wired. Never throws — a cleanup on a torn-down connection
   * must not surface an error.
   */
  cleanupDeferredForOwner(owner: string): void {
    this.supervisor?.cleanupDeferredForOwner(owner);
  }

  private requireHostOutboundAttachHandler(): (
    params: HostOutboundAttachParams,
  ) => HostOutboundAttachResult | Promise<HostOutboundAttachResult> {
    if (!this.hostOutboundAttachHandler) {
      throw new Error('host_outbound_attach intent reached dispatcher with no handler wired');
    }
    return this.hostOutboundAttachHandler;
  }

  private requireHostOutboundDetachHandler(): () =>
    | HostOutboundDetachResult
    | Promise<HostOutboundDetachResult> {
    if (!this.hostOutboundDetachHandler) {
      throw new Error('host_outbound_detach intent reached dispatcher with no handler wired');
    }
    return this.hostOutboundDetachHandler;
  }

  private requireHostOutboundReloadHandler(): () =>
    | HostOutboundReloadResult
    | Promise<HostOutboundReloadResult> {
    if (!this.hostOutboundReloadHandler) {
      throw new Error('host_outbound_reload intent reached dispatcher with no handler wired');
    }
    return this.hostOutboundReloadHandler;
  }

  private requireHostProxyConfigGetHandler(): () =>
    | HostProxyConfigGetResult
    | Promise<HostProxyConfigGetResult> {
    if (!this.hostProxyConfigGetHandler) {
      throw new Error('host_proxy_config_get intent reached dispatcher with no handler wired');
    }
    return this.hostProxyConfigGetHandler;
  }

  private requireHostProxyConfigSetHandler(): (
    params: HostProxyConfigSetParams,
  ) => HostProxyConfigSetResult | Promise<HostProxyConfigSetResult> {
    if (!this.hostProxyConfigSetHandler) {
      throw new Error('host_proxy_config_set intent reached dispatcher with no handler wired');
    }
    return this.hostProxyConfigSetHandler;
  }

  private requireAgentMaterializeHandler(): (
    params: AgentMaterializeParams,
  ) => AgentMaterializeResult | Promise<AgentMaterializeResult> {
    if (!this.agentMaterializeHandler) {
      throw new Error('agent_materialize intent reached dispatcher with no handler wired');
    }
    return this.agentMaterializeHandler;
  }

  private requireAgentWorkspaceIndexHandler(): (
    params: AgentWorkspaceIndexParams,
  ) => AgentWorkspaceIndexResult | Promise<AgentWorkspaceIndexResult> {
    if (!this.agentWorkspaceIndexHandler) {
      throw new Error('agent_workspace_index intent reached dispatcher with no handler wired');
    }
    return this.agentWorkspaceIndexHandler;
  }

  private requireAgentWorkspaceTreeHandler(): (
    params: AgentWorkspaceTreeParams,
  ) => AgentWorkspaceTreeResult | Promise<AgentWorkspaceTreeResult> {
    if (!this.agentWorkspaceTreeHandler) {
      throw new Error('agent_workspace_tree intent reached dispatcher with no handler wired');
    }
    return this.agentWorkspaceTreeHandler;
  }

  private requireAgentWorkspaceFileReadHandler(): (
    params: AgentWorkspaceFileReadParams,
  ) => AgentWorkspaceFileReadResult | Promise<AgentWorkspaceFileReadResult> {
    if (!this.agentWorkspaceFileReadHandler) {
      throw new Error('agent_workspace_file_read intent reached dispatcher with no handler wired');
    }
    return this.agentWorkspaceFileReadHandler;
  }

  private requireAgentL2SeedHandler(): (
    params: AgentL2SeedParams,
  ) => AgentL2SeedResult | Promise<AgentL2SeedResult> {
    if (!this.agentL2SeedHandler) {
      throw new Error('agent_l2_seed intent reached dispatcher with no handler wired');
    }
    return this.agentL2SeedHandler;
  }

  private requireAgentFilesPurgeHandler(): (
    params: AgentFilesPurgeParams,
  ) =>
    | Omit<AgentFilesPurgeResult, 'quiesced_count'>
    | Promise<Omit<AgentFilesPurgeResult, 'quiesced_count'>> {
    if (!this.agentFilesPurgeHandler) {
      throw new Error('agent_files_purge intent reached dispatcher with no handler wired');
    }
    return this.agentFilesPurgeHandler;
  }

  private requireAgentProfileMcpReadHandler(): (
    params: AgentProfileMcpReadParams,
  ) => AgentProfileMcpReadResult | Promise<AgentProfileMcpReadResult> {
    if (!this.agentProfileMcpReadHandler) {
      throw new Error('agent_profile_mcp_read intent reached dispatcher with no handler wired');
    }
    return this.agentProfileMcpReadHandler;
  }

  private requireAgentProfileMcpWriteHandler(): (
    params: AgentProfileMcpWriteParams,
  ) => AgentProfileMcpWriteResult | Promise<AgentProfileMcpWriteResult> {
    if (!this.agentProfileMcpWriteHandler) {
      throw new Error('agent_profile_mcp_write intent reached dispatcher with no handler wired');
    }
    return this.agentProfileMcpWriteHandler;
  }

  private requireMcpProbeHandler(): (
    params: McpProbeParams,
  ) => McpProbeResult | Promise<McpProbeResult> {
    if (!this.mcpProbeHandler) {
      throw new Error('mcp_probe intent reached dispatcher with no handler wired');
    }
    return this.mcpProbeHandler;
  }
}
