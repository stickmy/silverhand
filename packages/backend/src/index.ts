import { Hono } from 'hono';
import { serve } from '@hono/node-server';
import type { AddressInfo } from 'node:net';
import { isAbsolute, join } from 'node:path';
import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { performance } from 'node:perf_hooks';
import { silverhandCors } from './cors.js';
import { openDatabase } from './db.js';
import { createApiRouter, createInternalRouter } from './routes.js';
import type { AppEnv } from './auth/principal.js';
import {
  consoleLogger,
  type BackendContext,
  type BackendLogger,
  type BackendLoggers,
  type SilverhandCliLauncher,
} from './context.js';
import { AgentTokenStore } from './auth/agent-token-store.js';
import { HostRepo } from './repos/host-repo.js';
import { ensureWorkspacesRoot } from './provision.js';
import { ScopeEventBus } from './ws/scope-event-bus.js';
import {
  installEventWebSocketServer,
  type EventWsServerController,
} from './ws/ws-server.js';
import { DaemonAgentRuntimeHost } from './runtime/daemon-agent-runtime-host.js';
import { RuntimeDeliveryCoordinator } from './runtime/delivery-coordinator.js';
import { combineGatedBatch } from './runtime/stdin-format.js';
import type { AgentRuntimeHost } from './runtime/agent-runtime-host.js';
import { JsonlActivityLogWriter } from './runtime/activity-log-writer.js';
import { RuntimeBridge, type ProxySettingsSource } from './runtime/bridge.js';
import { RunStateCache } from './runtime/run-state-cache.js';
import {
  RuntimeEventHandler,
  type InboxAckSink,
} from './runtime/event-handler.js';
import { DbRuntimeStatusProjector } from './runtime/status-projector.js';
import { AgentRepo } from './repos/agent-repo.js';
import { AttachmentRepo } from './repos/attachment-repo.js';
import { ConversationRepo } from './repos/conversation-repo.js';
import { ConversationInboxRepo } from './repos/conversation-inbox-repo.js';
import { MessageRepo } from './repos/message-repo.js';
import { NotificationRepo } from './repos/notification-repo.js';
import { ScheduleRepo } from './repos/schedule-repo.js';
import { SettingsRepo } from './repos/settings-repo.js';
import { WorkspaceRepo } from './repos/workspace-repo.js';
import { MembershipService } from './services/membership-service.js';
import { AttachmentService } from './services/attachment-service.js';
import { AgentService } from './services/agent-service.js';
import { ChannelService } from './services/channel-service.js';
import { ConversationMessageService } from './services/conversation-message-service.js';
import { ScheduleService } from './services/schedule-service.js';
import { ThreadService } from './services/thread-service.js';
import { TaskService } from './services/task-service.js';
import { UserService } from './services/user-service.js';
import { UserRepo } from './repos/user-repo.js';
import { createLocalBlobFileStorage } from './storage/blob-file-storage.js';
import type { ServerConfig } from '@silverhand/shared';
import type { HostClient } from '@silverhand/host';
import type { Server as HttpServer } from 'node:http';

export interface CreateBackendOptions {
  dbPath: string;
  silverhandDir: string;
  serverConfig: ServerConfig;
  silverhandCliLauncher: SilverhandCliLauncher;
  logger?: BackendLogger;
  /** Categorized loggers (system + agent + cli sinks). When provided
   *  this overrides whatever `logger` was passed; missing categories
   *  fall back to `logger` so callers can opt in incrementally. */
  loggers?: Partial<BackendLoggers>;
  /** Where the file logger writes. Optional. */
  logDir?: string;
  /**
   * Connected `silverhand-daemon` client. **Required.** Backend
   * routes every spawn / wake / clearSession / inbox delivery /
   * run-state read through this client; without it the boot throws.
   * Legacy in-process spawn was removed in #task:512 cleanup
   * Commit B and the env switch in Commit C — there is no runtime
   * fallback. Type stays optional so existing call sites compile,
   * but startBackend throws if it is undefined.
   */
  hostClient?: HostClient;
}

export interface RunningBackend {
  url: string;
  port: number;
  context: BackendContext;
  close(): Promise<void>;
}

export async function startBackend(opts: CreateBackendOptions): Promise<RunningBackend> {
  const logger = opts.logger ?? consoleLogger;
  const loggers: BackendLoggers = {
    system: opts.loggers?.system ?? logger,
    agent: opts.loggers?.agent ?? logger,
    cli: opts.loggers?.cli ?? logger,
  };
  const db = openDatabase(opts.dbPath);
  const workspacesRoot = join(opts.silverhandDir, 'workspaces');
  const blobStorage = createLocalBlobFileStorage({ silverhandDir: opts.silverhandDir });
  await ensureWorkspacesRoot(workspacesRoot);
  await blobStorage.ensureReady();

  assertLauncherResolvable(opts.silverhandCliLauncher.command, 'silverhand CLI');

  // Bootstrap: ensure 'local' host exists.
  new HostRepo(db).ensureLocal();

  const attachmentService = new AttachmentService({
    storage: blobStorage,
    attachments: new AttachmentRepo(db),
    conversations: new ConversationRepo(db),
  });

  const userService = new UserService({
    db,
    users: new UserRepo(db),
    blobStorage,
  });

  const agentService = new AgentService({
    agents: new AgentRepo(db),
    blobStorage,
  });

  const ctx: BackendContext = {
    db,
    serverConfig: opts.serverConfig,
    logger,
    loggers,
    silverhandDir: opts.silverhandDir,
    workspacesRoot,
    logDir: opts.logDir,
    blobStorage,
    attachmentService,
    agentService,
    userService,
    channelService: undefined as unknown as ChannelService,
    conversationMessageService: undefined as unknown as ConversationMessageService,
    threadService: undefined as unknown as ThreadService,
    taskService: undefined as unknown as TaskService,
    scheduleService: undefined as unknown as ScheduleService,
    lastSpawnAtByAgent: new Map(),
    events: new ScopeEventBus(),
    silverhandCliLauncher: opts.silverhandCliLauncher,
    agentTokens: new AgentTokenStore(),
    // Filled immediately below when the daemon runtime host is constructed.
    runtimeHost: undefined as unknown as AgentRuntimeHost,
  };

  // ── Runtime host wiring ─────────────────────────────────────────
  // Daemon is the only supported runtime path. The host (Electron
  // main or dev-headless) MUST spawn silverhand-daemon and pass a
  // connected `HostClient`; without it the boot throws — there is no
  // runtime fallback (the legacy in-process ConversationManager
  // runtime was removed in #task:512 cleanup Commit B; the
  // `SILVERHAND_RUNTIME_HOST` env switch was removed in Commit C).
  if (!opts.hostClient) {
    throw new Error(
      'startBackend: a connected HostClient is required but received hostClient=undefined. ' +
        'The host (Electron main or dev-headless) must spawn silverhand-daemon and pass a ' +
        'connected HostClient. There is no runtime fallback — the legacy in-process ' +
        'ConversationManager runtime was removed in #task:512 cleanup Commit B; the ' +
        'SILVERHAND_RUNTIME_HOST env switch was removed in Commit C. Recovery: ensure ' +
        'silverhand-daemon is spawnable / its socket is reachable.',
    );
  }
  const proxySettings: ProxySettingsSource = {
    get: (key: 'proxy_url' | 'proxy_runtimes') =>
      new SettingsRepo(db).get(key),
  };
  const bridge = new RuntimeBridge(opts.hostClient, proxySettings);
  const cache = new RunStateCache();
  const activityLog = new JsonlActivityLogWriter({
    logDir: ctx.logDir,
    logger,
  });
  const inboxRepo = new ConversationInboxRepo(db);
  const inboxAck: InboxAckSink = {
    markDelivered: (deliveryIds: readonly number[]) =>
      inboxRepo.markDelivered([...deliveryIds]),
    markUndelivered: () => {},
  };
  // Construct the coordinator BEFORE EventHandler so the handler's
  // forward-only callbacks can reference it. Coordinator's
  // `respawn` callback closes over `ctx.runtimeHost` via a thunk —
  // the field isn't populated yet, so we defer the lookup to
  // call-time (start() is invoked from `onRuntimeExit` paths only,
  // long after construction finishes).
  const coordinator = new RuntimeDeliveryCoordinator({
    bridge: {
      runtimeWake: params => opts.hostClient!.runtimeWake(params),
    },
    cache,
    inboxAck,
    callbacks: {
      respawn: (cid, aid) => ctx.runtimeHost.start(cid, aid),
      hasPendingInbox: (cid, aid) =>
        inboxRepo.listPending(cid, aid).length > 0,
      // `combineGated` is invoked with UN-ENCODED message texts BEFORE
      // the per-driver encode step (Cindy `Mqf1h-FKudtEbH1O`: feeding
      // driver-encoded JSON lines through `combineGatedBatch` would
      // produce malformed claude stdin — combine before encode). The
      // lifted `combineGatedBatch` keeps daemon mode byte-identical
      // to the original CM stdin formatter. Tool-name is hardcoded to
      // match `sendMessageToolName(driver)` (currently driver-agnostic).
      combineGated: texts =>
        combineGatedBatch(texts, 'silverhand message send'),
      publishActivity: state => {
        ctx.events.publish(state.conversationId, {
          kind: 'agent_activity',
          payload: state,
        });
        activityLog.write(state);
      },
    },
    logger: {
      info: (meta, msg) => logger.info(meta, msg),
      warn: (meta, msg) => logger.warn(meta, msg),
      error: (meta, msg) => logger.error(meta, msg),
    },
  });
  const eventHandler = new RuntimeEventHandler({
    client: opts.hostClient,
    bridge,
    cache,
    events: ctx.events,
    inboxAck,
    // B1 (Cindy `usjxj8AxX4AvJ5E-`): the projector owns both the DB
    // write to `conversation_agents` AND the matching `agent_status`
    // SSE for the four status-mutating events (session_init,
    // runtime_exit, init_retrying, init_cooldown). EventHandler stays
    // event-fan-out — see `runtime/status-projector.ts`.
    statusProjector: new DbRuntimeStatusProjector(
      new ConversationRepo(db),
      ctx.events,
    ),
    // Forward parsed events to the coordinator's busy tracker
    // (Cindy `3l4bZ5vaMvlC9DVS`: forward-only — the handler MUST
    // NOT keep its own queue or call bridge.runtimeWake).
    onParsedEvent: (cid, aid, ev) =>
      coordinator.onParsedEvent(cid, aid, ev),
    // Forward runtime_exit to the coordinator so it can clear its
    // per-agent queue + decide oneShot auto-respawn (gated by the
    // intentional / initTimedOut / fatalError / hasPendingInbox
    // trio in `delivery-coordinator.ts:onRuntimeExit`).
    onRuntimeExit: (cid, aid, meta) =>
      coordinator.onRuntimeExit(cid, aid, meta),
    onInitTimeout: (cid, aid) =>
      coordinator.clearWakeWatchdog(cid, aid, 'timeout'),
    activityLog,
    // B2 (Cindy `usjxj8AxX4AvJ5E-`): wire `runtime_ready_for_drain`
    // to DaemonHost.drainPending so rows that arrived during the
    // startup_pending window are flushed when the runtime crosses
    // into `ready` without ever being woken by submitLiveDelivery.
    // Late-bound via the daemonHostRef closure because DaemonHost
    // construction needs the eventHandler above. The callback only
    // fires after `eventHandler.start()`, which DaemonHost calls
    // from `reconcileOnBoot` — strictly after `daemonHostRef` is
    // populated below.
    onReadyForDrain: (cid, aid) => {
      const host = daemonHostRef;
      if (!host) {
        logger.warn(
          { kind: 'runtime', conv_id: cid, agent_id: aid },
          'drain: ready_for_drain fired before DaemonHost wired; skipping',
        );
        return;
      }
      return host.drainPending(cid, aid);
    },
    logger,
  });
  let daemonHostRef: DaemonAgentRuntimeHost | null = null;
  ctx.runtimeHost = new DaemonAgentRuntimeHost({
    client: opts.hostClient,
    bridge,
    cache,
    eventHandler,
    events: ctx.events,
    conversations: new ConversationRepo(db),
    agents: new AgentRepo(db),
    messages: new MessageRepo(db),
    inboxes: inboxRepo,
    users: new UserRepo(db),
    settings: new SettingsRepo(db),
    coordinator,
    workspaces: new WorkspaceRepo(db, { workspacesRoot }),
    membership: new MembershipService({
      agents: new AgentRepo(db),
      users: new UserRepo(db),
      conversations: new ConversationRepo(db),
    }),
    silverhandDir: opts.silverhandDir,
    silverhandCli: opts.silverhandCliLauncher,
    agentTokens: ctx.agentTokens,
    // serverConfig is read fresh on every start() so it picks up the
    // bound port after the Hono server listens (ctx.serverConfig is
    // mutated mid-boot from `:0` placeholder to the real URL).
    serverConfig: () => ctx.serverConfig,
    activityLog,
    db,
    logger,
  });
  daemonHostRef = ctx.runtimeHost as DaemonAgentRuntimeHost;
  logger.info(
    { runtime: 'daemon' },
    'agent runtime: silverhand-daemon (the only supported runtime)',
  );

  ctx.channelService = new ChannelService({
    db,
    conversations: new ConversationRepo(db),
    workspaces: new WorkspaceRepo(db, { workspacesRoot }),
    agents: new AgentRepo(db),
    messages: new MessageRepo(db),
    events: ctx.events,
    runtimeHost: ctx.runtimeHost,
    logger,
  });
  ctx.conversationMessageService = new ConversationMessageService({
    db,
    conversations: new ConversationRepo(db),
    messages: new MessageRepo(db),
    agents: new AgentRepo(db),
    notifications: new NotificationRepo(db),
    workspaces: new WorkspaceRepo(db, { workspacesRoot }),
    attachmentService,
    runtimeHost: ctx.runtimeHost,
    events: ctx.events,
    logger,
  });
  ctx.threadService = new ThreadService({
    db,
    conversations: new ConversationRepo(db),
    messages: new MessageRepo(db),
    agents: new AgentRepo(db),
    workspaces: new WorkspaceRepo(db, { workspacesRoot }),
    runtimeHost: ctx.runtimeHost,
    events: ctx.events,
    logger,
    serverConfig: () => ctx.serverConfig,
    lastSpawnAtByAgent: ctx.lastSpawnAtByAgent,
    // Late-bound: TaskService is constructed below.
    taskService: () => ctx.taskService,
  });
  ctx.taskService = new TaskService({
    db,
    conversations: new ConversationRepo(db),
    messages: new MessageRepo(db),
    agents: new AgentRepo(db),
    runtimeHost: ctx.runtimeHost,
    events: ctx.events,
    logger,
  });
  ctx.scheduleService = new ScheduleService({
    schedules: new ScheduleRepo(db),
    workspaces: new WorkspaceRepo(db, { workspacesRoot }),
    conversations: new ConversationRepo(db),
    messages: new MessageRepo(db),
    agents: new AgentRepo(db),
    settings: new SettingsRepo(db),
    runtimeHost: ctx.runtimeHost,
    events: ctx.events,
    logger,
  });
  // Boot recovery (daemon hydrate from `runtime_status` then diff
  // stale DB rows). Scheduling waits until reconcile lands so any
  // handles brought back are visible to the first wake delivery.
  await ctx.runtimeHost.reconcileOnBoot();
  ctx.scheduleService.start();
  const stopEventLoopLagProbe = startEventLoopLagProbe(logger);

  // Daemon liveness ping. The daemon shim's first real call will
  // surface a disconnect more concretely than the ping; this is a
  // best-effort smoke confirming the IPC channel is alive at boot.
  try {
    const ping = await opts.hostClient.ping();
    logger.info(
      {
        daemon_pid: ping.daemon_pid,
        daemon_version: ping.daemon_version,
        started_at: ping.started_at,
      },
      'silverhand-daemon ping ok',
    );
  } catch (err) {
    logger.warn(
      { err: err instanceof Error ? err.message : String(err) },
      'silverhand-daemon ping failed at boot',
    );
  }

  const app = new Hono<AppEnv>();
  app.use('*', silverhandCors());
  app.route('/', createApiRouter(ctx));
  app.route('/', createInternalRouter(ctx));

  return new Promise<RunningBackend>((resolve, reject) => {
    let eventWs: EventWsServerController | null = null;
    const server = serve(
      { fetch: app.fetch, hostname: '127.0.0.1', port: 0 },
      info => {
        const address = info as AddressInfo;
        const port = address.port;
        const url = `http://127.0.0.1:${port}`;
        ctx.serverConfig = { ...opts.serverConfig, url };
        logger.info({ url }, 'backend listening');
        resolve({
          url,
          port,
          context: ctx,
          close: async () => {
            stopEventLoopLagProbe();
            // Stop the scheduler before any agent shutdown so we don't
            // race a fire mid-teardown (would write to a dying CM).
            ctx.scheduleService.stop();
            // Phase 1: stop all agents (must finish before activity-log flush
            // so terminal events are written before the stream closes).
            await ctx.runtimeHost.stopAll().catch(() => undefined);

            // Phase 2: close WS lifecycle before the HTTP drain removes sockets.
            await eventWs?.close();

            // Phase 3: flush activity log AND drain the HTTP server
            // concurrently — they are independent of each other.
            //
            // Order matters for the server drain:
            //   1. `server.close()` first — stops accepting new connections
            //      immediately, so no new HTTP work can sneak in.
            //   2. `closeAllConnections()` second — terminates all existing
            //      sockets so `server.close()` resolves right away.
            // The `in` guard handles the Http2Server union member which lacks
            // this method (we always use HTTP/1.1, but the type is a union).
            const serverDrained = new Promise<void>(res => server.close(() => res()));
            if ('closeAllConnections' in server) server.closeAllConnections();
            await Promise.allSettled([
              ctx.runtimeHost.closeActivityLog(),
              serverDrained,
            ]);

            // Phase 4: close the DB. At this point all connections —
            // including in-flight HTTP requests — have been forcibly
            // terminated by closeAllConnections(). Strict in-flight write
            // safety is sacrificed for a clean quit; if that becomes a
            // concern, use route-specific request tracking before this point.
            db.close();
          },
        });
      },
    );
    eventWs = installEventWebSocketServer(server as HttpServer, ctx);
    server.on('error', reject);
  });
}

export type { BackendContext, BackendLogger, BackendLoggers } from './context.js';
export {
  createCategorizedLoggers,
  createFileLogger,
  type CategorizedLoggers,
  type FileLogger,
  type FileLoggerOptions,
} from './logger.js';

function assertLauncherResolvable(command: string, label = 'launcher'): void {
  if (isAbsolute(command)) {
    if (!existsSync(command)) {
      throw new Error(
        `${label} not found: ${command}. ` +
          `Without it agents may be unable to use Silverhand runtime actions.`,
      );
    }
    return;
  }
  // Bare name — probe PATH. `command -v` is POSIX shell syntax, so use
  // `which` on Unix and `where` on Windows. We avoid spawning a shell
  // (no PATH-injection vector); execFileSync throws on non-zero exit.
  const probe = process.platform === 'win32' ? 'where' : 'which';
  try {
    execFileSync(probe, [command], { stdio: 'ignore' });
  } catch {
    throw new Error(
      `${label} \`${command}\` is not on PATH. ` +
        `Without it agents may be unable to use Silverhand runtime actions.`,
    );
  }
}

const EVENT_LOOP_LAG_INTERVAL_MS = 500;
const EVENT_LOOP_LAG_WARN_MS = 250;

function startEventLoopLagProbe(logger: BackendLogger): () => void {
  let expectedAt = performance.now() + EVENT_LOOP_LAG_INTERVAL_MS;
  const timer = setInterval(() => {
    const now = performance.now();
    const lagMs = now - expectedAt;
    expectedAt = now + EVENT_LOOP_LAG_INTERVAL_MS;
    if (lagMs >= EVENT_LOOP_LAG_WARN_MS) {
      logger.warn(
        {
          lagMs: roundMs(lagMs),
          intervalMs: EVENT_LOOP_LAG_INTERVAL_MS,
          thresholdMs: EVENT_LOOP_LAG_WARN_MS,
        },
        'backend: event loop lag detected',
      );
    }
  }, EVENT_LOOP_LAG_INTERVAL_MS);
  timer.unref?.();
  return () => clearInterval(timer);
}

function roundMs(ms: number): number {
  return Math.round(ms * 10) / 10;
}
