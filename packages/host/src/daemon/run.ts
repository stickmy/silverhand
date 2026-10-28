/**
 * `runDaemon` — programmatic daemon launcher.
 *
 * Composes IntentDispatcher + DaemonServer + pid file + signal handler
 * into one start/stop primitive. Two consumer shapes:
 *
 *  - Process entry (`./entry.ts`): parses argv into `RunDaemonOptions`,
 *    calls `runDaemon(opts)`, blocks until shutdown. Stdout receives
 *    the `ready` handshake line so a parent spawn helper can detect
 *    "daemon is now listening".
 *
 *  - Test harness: calls `runDaemon({ ..., installSignals: false,
 *    writeStdoutHandshake: false })` to spin a daemon up in the test
 *    process, then drives it via a HostClient. The `stop()` method on
 *    the returned handle is the clean teardown path.
 *
 * Why split from `entry.ts`: the entry is the *thinnest possible*
 * argv-parsing shim. All testable logic lives here so the test path
 * never has to fork a subprocess just to assert "daemon refuses to
 * start when another pid owns the lock".
 */
import {
  defaultAppRoot,
  resolveComputerSlotFromComputersRoot,
} from '@silverhand/shared/app-data-layout';
import { claimPidFile, installShutdownHandlers, releasePidFile } from './lifecycle.js';
import { readFileSync } from 'node:fs';
import type { DaemonHostConfig } from '@silverhand/shared';
import { IntentDispatcher } from './dispatcher.js';
import { AgentHomeMutationCoordinator } from './agent-home-mutation-coordinator.js';
import { DaemonServer } from './server.js';
import { createDaemonLogger, type DaemonLogger } from './logger.js';
import { encodeFrame, type EventFrame } from '../protocol.js';
import { DAEMON_VERSION, createDaemonHelloResult } from './version.js';
import { logDaemon } from './daemon-logging.js';
import {
  defaultDaemonIdentityPath,
  type DaemonIdentity,
  readOrCreateDaemonIdentity,
} from './identity.js';
import { OutboundSupervisor, type OutboundBinding } from './outbound-supervisor.js';
import { OutboundDaemonClient, type OutboundCommandDispatcher } from './outbound-client.js';
import type { OwnerCleanup } from './outbound-frame-mapping.js';
import { LocalOutboundFrameServer } from './local-outbound-frame-server.js';
import { AgentMaterializeError, materializeAgent } from './agent-materialize/index.js';
import {
  AgentWorkspaceIndexError,
  indexAgentWorkspaces,
  AgentL2SeedError,
  seedAgentL2,
  AgentFilesPurgeError,
  purgeAgentFiles,
} from './workspace-registry/index.js';
import {
  AgentWorkspaceFsError,
  listAgentWorkspaceTree,
  readAgentWorkspaceFile,
} from './agent-workspace-fs/index.js';
import {
  AgentProfileMcpError,
  readAgentProfileMcp,
  writeAgentProfileMcp,
} from './agent-profile-mcp/index.js';
import { probeMcpServer } from './mcp-probe/index.js';
import {
  resolveAuthenticatedComputerId,
  RuntimeStartResolveError,
  type RuntimeStartResolveDeps,
} from './runtime-start-resolve.js';
import {
  buildDaemonInstanceId,
  defaultRuntimePidRootDir,
  reconcileRuntimePidfiles,
} from './runtime-pidfiles.js';
import type { SupervisorContract } from './supervisor-contract.js';
import { readDaemonProxyConfig, writeDaemonProxyConfig } from './proxy-config.js';
import { computerProxyConfigPath } from './computer-profile.js';
import {
  resolveLocalSilverhandCliLauncher,
  type SilverhandCliLauncher,
} from './silverhand-cli-launcher.js';

export interface RunDaemonOptions {
  /** Absolute path where the daemon should bind its Unix socket. */
  socketPath: string;
  /** Absolute path of the pid file the daemon should claim. */
  pidFilePath: string;
  /** Directory for the daemon's own JSONL log file. */
  logDir: string;
  /**
   * Optional Stage D2 daemon-outbound host config. When present, the daemon
   * polls this atomic JSON file and opens `/api/hosts/connect` with its
   * long-lived host token. Omit/null keeps protocol tests on the Unix socket
   * only.
   */
  hostConfigPath?: string | null;
  /**
   * In-memory outbound host config for the public `connect` command. Unlike
   * `hostConfigPath`, this does not write the connect token into daemon-owned
   * config; the token lives in the user's copied command / service unit.
   */
  hostConfig?: DaemonHostConfig | null;
  /** Install SIGTERM/SIGINT handlers. Defaults to true. Tests pass
   *  false so a daemon running in-process doesn't intercept the test
   *  runner's own signal handling. */
  installSignals?: boolean;
  /** Print the `ready` handshake event to stdout on successful bind.
   *  Defaults to true (entry-script consumers need it). Tests pass
   *  false to keep their stdout clean. */
  writeStdoutHandshake?: boolean;
  /** Mirror log lines to stderr. Defaults to false; the entry script
   *  passes true so a parent that forwards daemon stderr gets a live
   *  view of what the daemon is doing. */
  alsoStderr?: boolean;
  /** Injected logger; tests use it to inspect log lines. When omitted,
   *  one is created at `logDir/daemon.YYYY-MM-DD.log` (UTC daily rotation). */
  logger?: DaemonLogger;
  /** Root for per-runtime pidfiles. Production defaults to
   *  `~/.silverhand/daemon/runtime-pids`; tests pass a temp dir.
   *  `null` disables the phase-1 pidfile feature for protocol-only
   *  fixtures. */
  runtimePidRootDir?: string | null;
  /**
   * #task:1608 — the per-computer durable slots root
   * (`<app-root>/computers`). When set (desktop), the ACCEPTED binding's
   * slot `computers/<computer_id>/agents` is frozen into the
   * generation-scoped computer path context at attach; hot paths only
   * consume that context. An explicit `agentHomeRootDir` override
   * (connect --data-root, tests) wins over the slot.
   */
  computersRootDir?: string | null;
  /**
   * Root for daemon-owned per-agent L1 dirs. #task:1813 removed the legacy
   * home-relative default: omit it and the daemon has a durable agents root
   * ONLY from the accepted binding's `computers/<id>/agents` slot; a bare
   * daemon with neither refuses every agent-file intent instead of writing
   * into the user's real profile. Connect passes a data-root-scoped path so
   * custom roots are fully isolated from that profile.
   */
  agentHomeRootDir?: string | null;
  /**
   * #task:1598 — the EXPLICIT global silverhand root of this computer. Desktop
   * passes its app root; connect passes the global home root. Never derived
   * from identity paths. relocation-v1 §7-4: it no longer roots any
   * requested-path guard (that guard retired with the registry's write path);
   * its live consumer is the per-computer proxy-config coordinate.
   */
  silverhandStateRootDir?: string | null;
  /**
   * Explicit daemon-local proxy config file. Standalone `connect --data-root`
   * uses this to keep every runtime artifact under its chosen data root.
   * Embedded desktop leaves it null so the daemon resolves the file from the
   * current computer binding under `<silverhand>/computers/<profile>/`.
   */
  proxyConfigPath?: string | null;
  /**
   * Host-local control-plane URL file for embedded desktop boots. Desktop
   * writes the finalized loopback URL after the embedded backend starts;
   * the daemon reads it at runtime spawn. Standalone remote boots use
   * `daemon-config.json` / outbound binding instead.
   */
  controlPlaneServerUrlPath?: string | null;
  /**
   * Persistent daemon identity file. Resolution (#task:1598 round 4 —
   * the historical `dirname(runtimePidRootDir)/identity.json` sibling
   * derivation is gone; embedders pass the coordinate explicitly):
   *
   *   - `=== null` — skip reading/creating any identity file; the
   *     daemon advertises `DEFAULT_DAEMON_IDENTITY` (bootstrap
   *     `daemon_id='local'` + `host_descriptor={name:'local',
   *     kind:'local'}`). Protocol-only tests and `connect` (which
   *     supplies `daemonIdentity` directly) use this.
   *   - explicit string — use the given path verbatim. Desktop passes
   *     the resolver's `localComputer.run.identity` through
   *     `--daemon-identity-path`.
   *   - omitted — bare-daemon fallback `defaultDaemonIdentityPath()`
   *     (`~/.silverhand/daemon/identity.json` via the resolver).
   *
   * On first read for any non-null path, the file is created with the
   * bootstrap identity (atomic tmp+rename, mode 0o600). Subsequent
   * boots reuse it; multi-daemon launchers may pre-populate a
   * different file before boot.
   */
  daemonIdentityPath?: string | null;
  /** Explicit process identity for public `connect` launches. This lets the
   *  ready event report the target machine hostname as diagnostic metadata
   *  without creating a desktop-style identity file. */
  daemonIdentity?: DaemonIdentity | null;
  /**
   * D5.a-2 — optional local outbound frame socket path. When set
   * (non-null), the daemon starts a {@link LocalOutboundFrameServer} on
   * this Unix socket alongside the control `DaemonServer`. It speaks the
   * SAME outbound frame envelope the standalone WSS client speaks and
   * routes commands through the same `IntentDispatcher`, so the embedded
   * backend reaches the daemon over the unified formal path (closing R4)
   * instead of the legacy `HostClient` IPC protocol.
   *
   * Omitted/`null` keeps the daemon control-socket-only — every existing
   * protocol test and the standalone deployment leave this unset. The
   * embedded backend wiring (D5.a-3) sets it to
   * `deriveLocalOutboundFrameSocketPath()`.
   */
  localOutboundSocketPath?: string | null;
  /**
   * SupervisorContract factory — REQUIRED, but explicitly nullable (Cindy
   * `XuKGmdna3GWZcY2G` + shiki `Go9ZiB4OnwTbELZU`). The daemon
   * constructs the supervisor AFTER the server is bound, passing an
   * `emit` callback that broadcasts events to every live socket
   * connection (with runtime_output backpressure applied per §2.2.2).
   *
   * Two legitimate shapes:
   *   - `(deps) => new Supervisor(deps)` — production wire
   *     (`entry.ts`). Anything that owns spawn/deliver/stop lifecycle.
   *   - `(deps) => new FakeSupervisor({...})` — protocol/e2e tests
   *     validating the full intent → supervisor → broadcast →
   *     HostClient.subscribe path without a real CLI child.
   *   - `null` — explicit opt-out for tests that only exercise the
   *     PR-1 surface (ping/status/echo). `runtime_*` intents in that
   *     mode return `internal_error: no supervisor wired`.
   *
   * Why required-but-nullable instead of optional:
   *   - Optional (`supervisorFactory?:`) was the original PR-1 shape;
   *     after PR-2 step 2 every production caller MUST wire a real
   *     supervisor, and a missing factory silently degrades to PR-1
   *     mode at runtime. This bit us at `43d5295`'s pre-fix state:
   *     `entry.ts` was missing the wire and TypeScript was happy,
   *     daemon booted, all `runtime_*` intents failed.
   *   - Forcing the field to be present makes "I deliberately want
   *     no supervisor here" a typed decision (`null`) instead of
   *     accidental absence. The `entry-supervisor-wiring.test.ts`
   *     suite + the `// @ts-expect-error` contract test in
   *     `run-daemon-type-contract.test.ts` lock both halves of the invariant.
   */
  supervisorFactory:
    | ((deps: {
        emit: (event: EventFrame) => void;
        logger: DaemonLogger;
        agentHomeMutationCoordinator: AgentHomeMutationCoordinator;
        daemonInstanceId: string;
        runtimePidRootDir: string | null;
        daemonSocketPath: string;
        /**
         * D4.e-6 — host path resolution deps for `runtime_start`. The
         * daemon resolves semantic workspace/agent refs to absolute host
         * paths via these (relocation-v1 §7-4: the deterministic
         * agent-centric coordinate for `regular`, `resolveAgentDir` for
         * `agent_home` — no registry lookup on either), fail-closed.
         * `runDaemon` builds them from the frozen computer path context; the
         * factory threads them into `Supervisor`.
         */
        runtimeStartResolveDeps: RuntimeStartResolveDeps;
        proxyConfigPath: () => string | null;
        localSilverhandCliLauncher: SilverhandCliLauncher | null;
        controlPlaneServerUrl: () => string | null;
      }) => SupervisorContract)
    | null;
}

export interface RunningDaemon {
  socketPath: string;
  pidFilePath: string;
  logger: DaemonLogger;
  /**
   * Broadcast an unsolicited event to every live connection. Used by
   * a SupervisorContract's `emit` callback to surface `runtime_*` events to
   * the backend. `runtime_output` frames participate in the
   * per-(conv, agent) 4 MiB drop cap (§2.2.2); all other events write
   * straight through.
   */
  broadcast(event: EventFrame): void;
  /** Stop the daemon: drain in-flight intents, close socket, release
   *  pid file. Idempotent. Resolves once everything is flushed. */
  stop(reason: string): Promise<void>;
}

function readControlPlaneServerUrl(
  filepath: string | null | undefined,
  logger: DaemonLogger,
): string | null {
  if (!filepath) return null;
  try {
    const raw = readFileSync(filepath, 'utf8').trim();
    if (!raw) return null;
    return new URL(raw).origin;
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    if (code !== 'ENOENT') {
      logger.warn(
        { filepath, err: (err as Error).message },
        'failed to read daemon-local control-plane URL; runtime_start will fail if no outbound binding is attached',
      );
    }
    return null;
  }
}

function resolveControlPlaneServerUrl(input: {
  controlPlaneBinding: OutboundBinding | null;
  controlPlaneServerUrlPath: string | null | undefined;
  logger: DaemonLogger;
}): string | null {
  const daemonLocalUrl = readControlPlaneServerUrl(input.controlPlaneServerUrlPath, input.logger);
  if (daemonLocalUrl) return daemonLocalUrl;

  const origin = input.controlPlaneBinding?.server_origin ?? null;
  if (!origin) return null;
  try {
    const url = new URL(origin);
    if (url.protocol === 'http:' || url.protocol === 'https:') {
      return url.origin;
    }
  } catch {
    // `local-embedded` is a workspace-registry namespace, not an HTTP
    // control-plane origin. It must never reach SILVERHAND_SERVER_URL or
    // daemon-side backend fetch URLs.
  }
  return null;
}

export async function runDaemon(opts: RunDaemonOptions): Promise<RunningDaemon> {
  const startedAt = Date.now();
  const daemonInstanceId = buildDaemonInstanceId(startedAt);
  const runtimePidRootDir =
    opts.runtimePidRootDir === undefined ? defaultRuntimePidRootDir() : opts.runtimePidRootDir;
  const daemonIdentityPath =
    opts.daemonIdentityPath === undefined ? defaultDaemonIdentityPath() : opts.daemonIdentityPath;
  const daemonIdentity =
    opts.daemonIdentity !== undefined
      ? (opts.daemonIdentity ?? undefined)
      : daemonIdentityPath === null
        ? undefined
        : readOrCreateDaemonIdentity(daemonIdentityPath);
  const helloResult = createDaemonHelloResult(daemonInstanceId, daemonIdentity);
  const readyEvent = (): Extract<EventFrame, { event: 'ready' }> => ({
    event: 'ready',
    ...helloResult,
    socket_path: opts.socketPath,
    daemon_pid: process.pid,
    started_at: startedAt,
  });
  const logger =
    opts.logger ??
    createDaemonLogger({
      dir: opts.logDir,
      alsoStderr: opts.alsoStderr ?? false,
      base: { component: 'silverhand-daemon', daemon_version: DAEMON_VERSION },
    });
  const localSilverhandCliLauncher = resolveLocalSilverhandCliLauncher();
  if (localSilverhandCliLauncher) {
    logDaemon(logger, {
      action: 'cli_launcher_resolved',
      pid: process.pid,
    });
  } else if (opts.supervisorFactory) {
    logDaemon(logger, {
      action: 'cli_launcher_unavailable',
      pid: process.pid,
    });
  }

  const claim = claimPidFile(opts.pidFilePath);
  if (claim.status === 'conflict') {
    const err = new Error(
      `another silverhand-daemon is already running (pid=${claim.conflictingPid})`,
    );
    (err as NodeJS.ErrnoException).code = 'EALREADY';
    logger.error(
      { pidFilePath: opts.pidFilePath, conflictingPid: claim.conflictingPid },
      'refusing to start: pid file already owned by live daemon',
    );
    await logger.close();
    throw err;
  }
  logDaemon(logger, {
    action: 'pid_claim',
    pid: process.pid,
    pid_file_path: opts.pidFilePath,
  });

  if (runtimePidRootDir !== null) {
    try {
      const reconcile = reconcileRuntimePidfiles({
        rootDir: runtimePidRootDir,
        selfDaemonInstanceId: daemonInstanceId,
        logger,
      });
      logger.info(
        {
          runtimePidRootDir,
          daemonInstanceId,
          ...reconcile,
        },
        'runtime pidfile orphan reconcile completed',
      );
    } catch (err) {
      logger.warn(
        {
          runtimePidRootDir,
          daemonInstanceId,
          err: (err as Error).message,
        },
        'runtime pidfile orphan reconcile failed; continuing daemon boot',
      );
    }
  }

  // Resolve the circular dep between supervisor (needs `emit`),
  // dispatcher (needs the supervisor), and server (needs the
  // dispatcher; provides `broadcast`) by giving the supervisor a
  // closure-bound `emit` that we wire to `server.broadcast` once the
  // server exists. Until that point, `emit` is a no-op — the
  // supervisor is constructed but won't fire events before
  // `server.start()` returns anyway.
  let broadcaster: (event: EventFrame) => void = () => {};
  // D3.a Commit 3: the outbound client is no longer constructed
  // unconditionally at boot. The OutboundSupervisor encapsulates the
  // `attach(configPath)` decision tree (validate, idempotency,
  // graceful swap) and is consumed by both the legacy CLI `--host-config`
  // auto-start path (below) and the new `host_outbound_attach` IPC
  // intent (wired into the dispatcher).
  //
  // Note the deliberate forward-declaration dance: OutboundSupervisor
  // needs a `dispatchCommand` callback that routes outbound-WS
  // commands into the dispatcher, but the dispatcher itself needs a
  // `hostOutboundAttachHandler` that calls into the supervisor. We
  // close the loop with a mutable `commandForwarder` placeholder that
  // is assigned to the real `dispatcher.dispatch` once the dispatcher
  // exists. Calling the forwarder before that point throws — but no
  // outbound socket can be open before this function returns, so the
  // throw is unreachable in practice.
  let commandForwarder: OutboundCommandDispatcher = () => {
    throw new Error('outbound dispatchCommand invoked before dispatcher was wired');
  };
  // Late-bound like `commandForwarder` (the dispatcher is created below): the
  // owner-scoped deferred-queue cleanup an outbound ingress fires on its
  // connection drop (#task:1417 Stage 3, §11.4.1).
  let ownerCleanup: OwnerCleanup = () => {};
  const outboundSupervisor = new OutboundSupervisor({
    logger,
    readyEvent,
    dispatchCommand: (command, ctx) => commandForwarder(command, ctx),
    cleanupDeferredForOwner: (owner) => ownerCleanup(owner),
  });
  const directOutboundBinding = opts.hostConfig
    ? {
        server_origin: new URL(opts.hostConfig.server_url).origin,
        host_id: opts.hostConfig.host_id,
      }
    : null;
  // Runtime, seed, purge and file-read paths resolve the deterministic
  // agent-centric coordinate. Nothing below builds, reads, or roots a
  // workspace inventory.
  //
  // #task:1596 durability rule still holds for what this function DOES root:
  // durable state (agent L1/L2 under the agents root) must never live under
  // the identity file's directory (desktop's wipeable `~/.silverhand/run`) nor
  // the historical `~/.silverhand/daemon`. The embedder passes the durable
  // coordinate explicitly — `agentHomeRootDir`, or `computersRootDir` for the
  // binding-keyed slot below.
  // Non-slot root selection (explicit override / bare default). With
  // `computersRootDir` set this is superseded by the accepted binding's slot
  // inside the frozen computer path context below.
  const explicitAgentsRoot = opts.agentHomeRootDir ?? undefined;
  // The app/state root of THIS computer: an EXPLICIT resolved coordinate from
  // the composition root (desktop passes its app root; connect passes the
  // global home root), never a parent inversion of the identity paths — a v2
  // layout that moves those files must not silently move this root
  // (#task:1598 round 2, Cindy P1-2). Bare daemons fall back to the resolver's
  // home root. relocation-v1 §7-4: its former job — rooting the registry's
  // requested-path guard — went away with `validateRequestedPath`; what still
  // reads it is the per-computer proxy-config path below.
  const silverhandStateRoot = opts.silverhandStateRootDir ?? defaultAppRoot();
  const fallbackEmbeddedHostId = daemonIdentity?.daemon_id ?? 'local';
  const liveOutboundBinding = (): OutboundBinding | null =>
    directOutboundBinding ?? outboundSupervisor.currentBinding();
  // The binding the control-plane server URL is derived from — its ONLY
  // consumer (`resolveControlPlaneServerUrl` below). Distinct from
  // `liveOutboundBinding()` because the embedded arm must still name a
  // `local-embedded` origin before desktop has claimed the real host, and
  // distinct from the frozen `computerContext` because a URL lookup does not
  // need generation-scoped path safety. Carries no path authority.
  const resolveControlPlaneUrlBinding = (): OutboundBinding | null => {
    if (localOutboundServer != null) {
      return {
        server_origin: 'local-embedded',
        host_id: liveOutboundBinding()?.host_id ?? fallbackEmbeddedHostId,
      };
    }
    return liveOutboundBinding();
  };
  // ── #task:1608 — generation-scoped computer path context ─────────────
  // The accepted binding's computer id is frozen into ONE typed path
  // context per binding generation, at the attach/detach transitions.
  // Hot paths (agent-bundle coordinate resolution, L1 materialize,
  // seed/purge, runtime
  // spawn resolution) only consume this context — they never re-derive
  // roots from a mutable `currentBinding()` lookup, so an in-flight
  // operation can never straddle two computer directories. The embedded
  // pre-claim placeholder host id (`fallbackEmbeddedHostId`) NEVER enters
  // the context: before the machine registers there is no computer slot,
  // and data-plane intents fail closed with their existing typed
  // `registry_unavailable`.
  // Only what the runtime actually reads per generation: the binding (proxy
  // config coordinate) and the agents root (every agent-file + spawn path).
  interface ComputerPathContext {
    binding: OutboundBinding;
    agentsRoot: string | undefined;
  }
  let computerContext: ComputerPathContext | null = null;
  const freezeComputerContext = (): void => {
    const live = liveOutboundBinding();
    if (live == null) {
      computerContext = null;
      return;
    }
    const binding: OutboundBinding =
      localOutboundServer != null
        ? { server_origin: 'local-embedded', host_id: live.host_id }
        : live;
    const slot =
      opts.computersRootDir != null
        ? resolveComputerSlotFromComputersRoot(opts.computersRootDir, binding.host_id)
        : null;
    computerContext = {
      binding,
      agentsRoot: explicitAgentsRoot ?? slot?.agents,
    };
  };
  const fenceComputerContext = (): void => {
    computerContext = null;
  };
  // NOTE (#task:1608 P0-1): the startup freeze for a direct `--host-config`
  // binding happens AFTER `localOutboundServer` is initialized (below) —
  // calling it here would read the const inside its temporal dead zone and
  // crash every connect boot.
  const currentAgentsRoot = (): string | undefined =>
    computerContext?.agentsRoot ?? explicitAgentsRoot;
  // #task:1608 P0-2 / #task:1813 — an agent-file operation with no agents
  // root resolved fails closed with the handler's own typed error. In slot
  // mode (desktop passes `computersRootDir`) that is the pre-registration
  // window; with no slot and no explicit root (a bare daemon) there is no
  // durable coordinate at all. Neither may fall through to a home-relative
  // default: that is how the retired top-level `agents/` authority gets
  // silently rebuilt — in the real user home, since the old default read
  // `SILVERHAND_DIR`/`homedir()` rather than the embedder's root. Explicit
  // roots (connect, desktop post-attach, tests) are the ONLY way in.
  // The message states the missing ROOT, not a missing registration: a bare
  // daemon can hold an accepted binding and still have no agents root (no
  // `computersRootDir` to resolve the slot with, no explicit root), so
  // "until the machine binds" would misreport that half of the refusal set.
  const agentsRootFor = <E extends Error>(wrap: (message: string) => E): string => {
    const root = currentAgentsRoot();
    if (root === undefined) {
      throw wrap(
        'no agents root is available on this daemon; agent files need a root from a registered computer slot or from explicit configuration',
      );
    }
    return root;
  };

  const resolveProxyConfigPath = (): string | null => {
    if (opts.proxyConfigPath) return opts.proxyConfigPath;
    const binding = computerContext?.binding ?? null;
    if (binding == null) return null;
    return computerProxyConfigPath(binding, silverhandStateRoot);
  };
  // D4.e-6 — runtime_start path resolution deps. The wire options are
  // semantic-only; `resolveRuntimeStartPaths` resolves the L2/cwd PATH to the
  // deterministic agent-centric bundle root (agent-home via
  // `resolveAgentDir`, one join onto the agents root below).
  // relocation-v1 §7 task 4 + the shiki+Cindy pivot (2026-08-04): there is no
  // registry establishment gate, and L2 presence is NOT a spawn gate. A
  // workspace id maps to a unique bundle coordinate; the ACTIVE workspace
  // resolves to it unconditionally, and the sole HARD runtime-start gate is the
  // supervisor's `ensureRuntimeStartWorkspaceCwd` — a session-cwd
  // mkdir+usability ensure (`bundleCwdUsable`: isDirectory + enterable) that
  // fails closed as `workspace_bundle_missing` ONLY when the OS genuinely
  // cannot spawn a child there, NEVER because MEMORY.md is absent. L2 readable
  // (`bundleL2Present`) governs only whether the prompt's memory index offers a
  // memory path. `agentHomeRootDir` is generation-scoped (read via getter at
  // spawn time), so the standalone binding (known only post-attach) is current.
  const runtimeStartResolveDeps = {
    // Generation-scoped (#task:1608): reads the FROZEN context at spawn
    // time via a getter — never a mutable binding lookup.
    get agentHomeRootDir(): string {
      return agentsRootFor((m) => new RuntimeStartResolveError('registry_unavailable', m));
    },
    // The prompt's This Context → Computer line. Authority is the LIVE
    // authenticated outbound binding only — never
    // resolveControlPlaneUrlBinding(),
    // whose embedded arm substitutes the pre-login placeholder
    // (`daemon_id` / 'local') when desktop has not claimed yet. Pre-attach
    // this fails closed (typed, recoverable) instead of stamping a
    // placeholder into prompt provenance.
    resolveComputerId: (): string => resolveAuthenticatedComputerId(liveOutboundBinding()),
  };
  // One authority is shared by both dispatcher ingresses and Supervisor.
  // Installing a purge fence is therefore immediately visible to internal
  // respawn; there is no second flag or await-sized handoff gap.
  const agentHomeMutationCoordinator = new AgentHomeMutationCoordinator();
  const supervisor = opts.supervisorFactory
    ? opts.supervisorFactory({
        agentHomeMutationCoordinator,
        emit: (event) => broadcaster(event),
        logger,
        daemonInstanceId,
        runtimePidRootDir,
        daemonSocketPath: opts.socketPath,
        runtimeStartResolveDeps,
        proxyConfigPath: resolveProxyConfigPath,
        localSilverhandCliLauncher,
        controlPlaneServerUrl: () =>
          resolveControlPlaneServerUrl({
            controlPlaneBinding: resolveControlPlaneUrlBinding(),
            controlPlaneServerUrlPath: opts.controlPlaneServerUrlPath,
            logger,
          }),
      })
    : undefined;
  const dispatcher = new IntentDispatcher({
    logger,
    startedAt,
    helloResult,
    supervisor,
    agentHomeMutationCoordinator,
    // #task:1608 — binding transitions are the ONLY places the computer
    // path context changes: freeze after an accepted attach/reload, fence
    // BEFORE detach so no in-flight operation can land in a stale (or,
    // after re-registration, a new) computer directory.
    hostOutboundAttachHandler: async (params) => {
      const result = await outboundSupervisor.attach(params);
      freezeComputerContext();
      // #task:1719 dogfood finding — the connection-scoped `ready` is
      // emitted at socket accept, which on an embedded boot precedes
      // this attach by milliseconds, so every attach-dependent consumer
      // the backend triggers off `ready` raced the binding and ran for
      // the whole generation against "no computer bound yet". (The
      // consumer that first exposed it, the backend's workspace-registry
      // reconcile, is gone — relocation-v1 §7-4 retired it; the ordering
      // hazard is not.) Re-announce `ready` once the control-plane
      // binding is REAL so those triggers are causal, not a timing bet.
      // Consumers are idempotent for an unchanged daemon generation: same
      // pid/started_at skips the re-hydrate, and the eager status seed
      // no-ops on an existing seed.
      // Scope (Cindy P1-2): ONLY the embedded local outbound surface —
      // the race exists solely there (accept-time ready precedes attach).
      // The standalone outbound WS declares its own ready on ws.open
      // AFTER its binding exists; pushing a second ready through the
      // wide broadcaster would hit a CONNECTING ws (guaranteed
      // event_disconnected warn) or send a redundant ready to a remote
      // backend.
      if (result.attached) {
        localOutboundServer?.broadcast(readyEvent());
      }
      return result;
    },
    hostOutboundDetachHandler: async () => {
      fenceComputerContext();
      return outboundSupervisor.detach();
    },
    hostOutboundReloadHandler: async () => {
      const result = await outboundSupervisor.reload();
      freezeComputerContext();
      return result;
    },
    hostProxyConfigGetHandler: () => ({
      config: readDaemonProxyConfig(resolveProxyConfigPath(), logger),
    }),
    hostProxyConfigSetHandler: (params) => {
      const proxyConfigPath = resolveProxyConfigPath();
      if (!proxyConfigPath) {
        throw new Error('host proxy config path is not configured');
      }
      return {
        config: writeDaemonProxyConfig(proxyConfigPath, params),
      };
    },
    // D4.d D2 — server↔daemon formal semantics. `materializeAgent`
    // owns the §4.6 idempotency decision tree + atomic seed +
    // sentinel write. The L1 root is the agents root resolved above
    // — always explicit, never derived from the daemon process's
    // `homedir()` (#task:1813). The dispatcher always invokes the real
    // handler; filesystem errors are classified at their owner.
    agentMaterializeHandler: async (params) => {
      return materializeAgent(params, {
        agentHomeRootDir: agentsRootFor((m) => new AgentMaterializeError('fs_unavailable', m)),
      });
    },
    // D4.e-1 — agent_workspace_index: route-time `agent_dir` resolver.
    // agent-home dirs resolve under the agents root resolved above;
    // regular workspaces resolve to their
    // deterministic agent-centric bundle coordinate and report `agent_dir`
    // only when that bundle is materialized here (relocation-v1 §7 task 4 —
    // no registry establishment gate; availability is an fs fact).
    agentWorkspaceIndexHandler: async (params) => {
      return indexAgentWorkspaces(params, {
        agentHomeRootDir: agentsRootFor(
          (m) => new AgentWorkspaceIndexError('registry_unavailable', m),
        ),
      });
    },
    // #task:811/#1190 — agent_workspace_tree: flat tree of every visible
    // file/directory under the selected agent memory root. No `workspace_id`
    // means agent-home/L1 (the agents root above); `workspace_id` resolves the
    // regular workspace by its deterministic agent-centric coordinate
    // (relocation-v1 §7 task 4 — no registry establishment gate; a not-yet-
    // materialized bundle degrades to an empty tree, not a refusal), or
    // agent_home/DM back to the same L1 dir. No server-derived path crosses
    // the wire.
    agentWorkspaceTreeHandler: async (params) =>
      listAgentWorkspaceTree(params, {
        agentHomeRootDir: agentsRootFor(
          (m) => new AgentWorkspaceFsError('registry_unavailable', m),
        ),
      }),
    // #task:811/#1190 — agent_workspace_file_read: read one file from the
    // selected agent memory root by virtual path. The helper enforces the
    // segment-shape rules + per-segment lstat-walk + realpath boundary check
    // so the daemon never follows a symlink or reads outside the resolved base.
    // Regular workspaces resolve by deterministic coordinate (§7 task 4); no
    // registry lookup.
    agentWorkspaceFileReadHandler: async (params) =>
      readAgentWorkspaceFile(params, {
        agentHomeRootDir: agentsRootFor(
          (m) => new AgentWorkspaceFsError('registry_unavailable', m),
        ),
      }),
    // D4.e-3 — agent_l2_seed: materialize a member agent's L2 (per-workspace)
    // memory bundle on this agent's own computer — the deterministic
    // agent-centric bundle root, writing the L2 template (L1 minus
    // system_prompt.md), idempotent. relocation-v1 §7 task 4: no registry
    // establishment gate — seeding IS what makes the coordinate available, and
    // fails closed only on the fs.
    agentL2SeedHandler: async (params) => {
      return seedAgentL2(params, {
        agentHomeRootDir: agentsRootFor((m) => new AgentL2SeedError('registry_unavailable', m)),
      });
    },
    // D4.e-4 — agent_files_purge: remove an agent's host-local files on the
    // owning host. relocation-v1 §7 task 4 (registry demotion): the agent's
    // whole managed footprint is a single tree under its agent home
    // (`<agents root>/<id>` = L1 + every nested `workspaces/<id>` L2 bundle),
    // so the purge removes that tree in one rm and consults NO registry. A
    // hard fs failure → typed `partial_purge` so the route keeps the DB row
    // for retry.
    agentFilesPurgeHandler: async (params) => {
      return purgeAgentFiles(params, {
        agentHomeRootDir: agentsRootFor((m) => new AgentFilesPurgeError('partial_purge', m)),
      });
    },
    // #task:1135 — agent_profile_mcp_read / _write: the host-local truth
    // behind `silverhand profile mcp get|set`. L1 resolves under the agents
    // root above; the write handler
    // re-validates the canonical schema and replaces `mcp.json` atomically.
    agentProfileMcpReadHandler: async (params) =>
      readAgentProfileMcp(params, {
        agentHomeRootDir: agentsRootFor((m) => new AgentProfileMcpError('fs_unavailable', m)),
      }),
    agentProfileMcpWriteHandler: async (params) =>
      writeAgentProfileMcp(params, {
        agentHomeRootDir: agentsRootFor((m) => new AgentProfileMcpError('fs_unavailable', m)),
      }),
    // #task:1137 — mcp_probe: open a one-shot live MCP connection to a named
    // server (initialize + optional tools/list) and close it immediately. L1
    // resolves under the same agents root as read/write.
    mcpProbeHandler: async (params) =>
      probeMcpServer(params, {
        agentHomeRootDir: agentsRootFor((m) => new AgentProfileMcpError('fs_unavailable', m)),
      }),
  });
  commandForwarder = (command, ctx) =>
    dispatcher.dispatch(command.id, String(command.kind), command.params, ctx);
  ownerCleanup = (owner) => dispatcher.cleanupDeferredForOwner(owner);
  const server = new DaemonServer({
    socketPath: opts.socketPath,
    dispatcher,
    logger,
  });
  // D5.a-2 — embedded formal path. The local outbound frame server shares
  // the exact `commandForwarder` (→ `dispatcher.dispatch`) and the same
  // outbound envelope mapping as the standalone WSS client, so embedded
  // and standalone are one dispatch path, not two protocols. Constructed
  // only when a socket path is supplied (embedded mode); standalone and
  // protocol-only tests leave it null.
  const directOutboundClient = opts.hostConfig
    ? new OutboundDaemonClient({
        config: opts.hostConfig,
        logger,
        readyEvent,
        dispatchCommand: (command, ctx) => commandForwarder(command, ctx),
        cleanupDeferredForOwner: (owner) => ownerCleanup(owner),
      })
    : null;
  const localOutboundServer =
    opts.localOutboundSocketPath != null
      ? new LocalOutboundFrameServer({
          socketPath: opts.localOutboundSocketPath,
          dispatchCommand: (command, ctx) => commandForwarder(command, ctx),
          cleanupDeferredForOwner: (owner) => ownerCleanup(owner),
          logger,
          // Same handshake the standalone WSS client sends on connect
          // (`outbound-client.ts` → `sendEvent(readyEvent())`). The
          // embedded adapter relies on it to latch the daemon key and
          // hydrate, so embedded and standalone share the `ready` path.
          readyEvent,
        })
      : null;
  broadcaster = (event) => {
    server.broadcast(event);
    outboundSupervisor.sendEvent(event);
    directOutboundClient?.sendEvent(event);
    localOutboundServer?.broadcast(event);
  };
  // #task:1608 P0-1 — the startup freeze for a direct `--host-config`
  // binding runs HERE, after every const it reads is initialized (an
  // earlier call sat in `localOutboundServer`'s temporal dead zone and
  // crashed every connect boot). No-binding boots leave the context null.
  freezeComputerContext();

  try {
    await server.start();
  } catch (err) {
    logger.error(
      { err: (err as Error).message, socketPath: opts.socketPath },
      'failed to bind socket',
    );
    releasePidFile(opts.pidFilePath);
    await logger.close();
    throw err;
  }

  if (localOutboundServer) {
    try {
      await localOutboundServer.start();
    } catch (err) {
      logger.error(
        {
          err: (err as Error).message,
          socketPath: opts.localOutboundSocketPath,
        },
        'failed to bind local outbound frame socket',
      );
      await server.stop('local_outbound_bind_failed');
      releasePidFile(opts.pidFilePath);
      await logger.close();
      throw err;
    }
  }

  // The handshake is the contract the spawn helper relies on to know
  // the daemon is *ready to accept connections*. Print it AFTER
  // server.start() so a parent process that reads "ready" can connect
  // immediately without retry.
  if (opts.writeStdoutHandshake ?? true) {
    process.stdout.write(encodeFrame(readyEvent()));
  }
  logger.info(
    {
      socketPath: opts.socketPath,
      pidFilePath: opts.pidFilePath,
      logFile: logger.filePath,
    },
    'daemon ready',
  );
  logDaemon(logger, {
    action: 'boot',
    pid: process.pid,
    socket_path: opts.socketPath,
    pid_file_path: opts.pidFilePath,
  });
  if (opts.hostConfigPath) {
    // Legacy `--host-config` CLI auto-start path, preserved for
    // standalone / manual daemon launches that have a credential file
    // ready at spawn time (e.g. headless server hosts, protocol
    // fixtures). Desktop main does NOT take this path under D3.a —
    // it spawns without `--host-config` and triggers attach via the
    // `host_outbound_attach` IPC after login. Both paths funnel
    // through the same `OutboundSupervisor.attach`, which means the
    // validation, idempotency, and swap semantics are identical.
    const cliAttach = outboundSupervisor.attach({
      config_path: opts.hostConfigPath,
    });
    if (cliAttach.attached) freezeComputerContext();
    if (!cliAttach.attached) {
      // Don't fail-loud the whole daemon boot — the supervisor's
      // failure path leaves state untouched and a later
      // host_outbound_attach IPC can recover. Just log so the
      // operator can see why outbound never connected.
      logger.warn(
        {
          kind: 'outbound_supervisor',
          action: 'cli_auto_attach_failed',
          reason: cliAttach.reason,
          message: cliAttach.message,
        },
        'daemon outbound CLI auto-attach failed; outbound is inactive until host_outbound_attach IPC arrives',
      );
    }
  }
  directOutboundClient?.start();

  let stopPromise: Promise<void> | null = null;
  const teardown = async (reason: string): Promise<void> => {
    dispatcher.markShuttingDown();
    logDaemon(logger, { action: 'shutting_down', pid: process.pid });
    outboundSupervisor.sendEvent({ event: 'shutting_down', reason });
    // The daemon owns these children in both Desktop modes. Drain before
    // closing transports/logs, and fence internal as well as RPC spawns.
    try {
      await supervisor?.shutdown();
    } catch (err) {
      logger.error({ err: (err as Error).message }, 'runtime shutdown failed; continuing teardown');
    }
    outboundSupervisor.stop(reason);
    directOutboundClient?.stop(reason);
    if (localOutboundServer) {
      try {
        await localOutboundServer.stop();
      } catch (err) {
        logger.error(
          { err: (err as Error).message },
          'local outbound server stop threw; continuing teardown',
        );
      }
    }
    try {
      await server.stop(reason);
    } catch (err) {
      logger.error({ err: (err as Error).message }, 'server.stop threw; continuing teardown');
    }
    releasePidFile(opts.pidFilePath);
    logger.info({ reason }, 'daemon stopped');
    await logger.close();
  };
  const stop = (reason: string): Promise<void> => {
    stopPromise ??= teardown(reason);
    return stopPromise;
  };

  if (opts.installSignals ?? true) {
    installShutdownHandlers(logger, async (signal) => {
      await stop(signal);
    });
  }

  return {
    socketPath: opts.socketPath,
    pidFilePath: opts.pidFilePath,
    logger,
    broadcast: (event) => broadcaster(event),
    stop,
  };
}
