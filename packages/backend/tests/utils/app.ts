import { Hono } from 'hono';
import { mkdirSync } from 'node:fs';
import type {
  AgentRunState,
  AgentRunTerminalReason,
  ServerConfig,
} from '@silverhand/shared';
import type { BackendContext, BackendLogger } from '../../src/context.js';
import { silverhandCors } from '../../src/cors.js';
import type { AppEnv } from '../../src/auth/principal.js';
import { ScopeEventBus } from '../../src/ws/scope-event-bus.js';
import { HostRepo } from '../../src/repos/host-repo.js';
import { AgentRepo } from '../../src/repos/agent-repo.js';
import { AttachmentRepo } from '../../src/repos/attachment-repo.js';
import { ConversationRepo } from '../../src/repos/conversation-repo.js';
import { MessageRepo } from '../../src/repos/message-repo.js';
import { NotificationRepo } from '../../src/repos/notification-repo.js';
import { ScheduleRepo } from '../../src/repos/schedule-repo.js';
import { SettingsRepo } from '../../src/repos/settings-repo.js';
import { WorkspaceRepo } from '../../src/repos/workspace-repo.js';
import { createApiRouter, createInternalRouter } from '../../src/routes.js';
import type { AgentRuntimeHost } from '../../src/runtime/agent-runtime-host.js';
import { AgentTokenStore } from '../../src/auth/agent-token-store.js';
import { AttachmentService } from '../../src/services/attachment-service.js';
import { AgentService } from '../../src/services/agent-service.js';
import { ChannelService } from '../../src/services/channel-service.js';
import { ConversationMessageService } from '../../src/services/conversation-message-service.js';
import { ScheduleService } from '../../src/services/schedule-service.js';
import { TaskService } from '../../src/services/task-service.js';
import { ThreadService } from '../../src/services/thread-service.js';
import { UserService } from '../../src/services/user-service.js';
import { UserRepo } from '../../src/repos/user-repo.js';
import { createLocalBlobFileStorage } from '../../src/storage/blob-file-storage.js';
import { makeTempSilverhandDir, openTestDb } from './db.js';

/**
 * Records every call routes / provision make into the runtime host.
 * Routes use `.start`, `.stop`, `.deliverMessage`, `.stopAll`,
 * `.reconcileOnBoot`, `.isRunning` — none of which can be allowed to
 * spawn real CLIs in tests. (Class name kept for diff readability;
 * the type cast is to `AgentRuntimeHost` post-Commit B.)
 */
export class FakeConversationManager {
  starts: Array<{ conversationId: string; agentId: string }> = [];
  stops: Array<{
    conversationId: string;
    agentId: string;
    mode: 'soft' | 'hard';
    directive?: string;
  }> = [];
  clearedSessions: Array<{
    conversationId: string;
    agentId: string;
    actorUserId: string | null;
  }> = [];
  delivered: Array<{ conversationId: string; agentId: string; messageId: string }> = [];
  agentRunStates: AgentRunState[] = [];
  resolvedRuns: Array<{
    conversationId: string;
    agentId: string;
    reason: AgentRunTerminalReason;
    runId?: string;
  }> = [];
  resolvedConversations: Array<{ conversationId: string; reason: AgentRunTerminalReason }> = [];
  stoppedAll = 0;
  reconciled = 0;

  /** Override to make `.start` reject. */
  startError: Error | null = null;
  clearSessionResult = { prevCliSessionId: 'prev-session-1', wasRunning: false };

  reconcileOnBoot(): void {
    this.reconciled++;
  }
  isRunning(): boolean {
    return false;
  }
  getAgentRunStates(conversationId: string): AgentRunState[] {
    return this.agentRunStates.filter(s => s.conversationId === conversationId);
  }
  resolveRun(
    conversationId: string,
    agentId: string,
    reason: AgentRunTerminalReason,
    runId?: string,
  ): void {
    this.resolvedRuns.push({ conversationId, agentId, reason, runId });
  }
  resolveConversationRuns(
    conversationId: string,
    reason: AgentRunTerminalReason = 'conversation_closed',
  ): void {
    this.resolvedConversations.push({ conversationId, reason });
  }
  async start(conversationId: string, agentId: string): Promise<void> {
    if (this.startError) throw this.startError;
    this.starts.push({ conversationId, agentId });
  }
  async stop(
    conversationId: string,
    agentId: string,
    mode: 'soft' | 'hard' = 'hard',
    directive?: string,
  ): Promise<void> {
    this.stops.push({ conversationId, agentId, mode, directive });
  }
  async clearSession(
    conversationId: string,
    agentId: string,
    actorUserId: string | null = null,
  ): Promise<{ prevCliSessionId: string | null; wasRunning: boolean }> {
    this.clearedSessions.push({ conversationId, agentId, actorUserId });
    return this.clearSessionResult;
  }
  async stopAll(): Promise<void> {
    this.stoppedAll++;
  }
  async deliverMessage(
    conversationId: string,
    agentId: string,
    messageId: string,
  ): Promise<void> {
    this.delivered.push({ conversationId, agentId, messageId });
  }
}

const silentLogger: BackendLogger = {
  info() {},
  warn() {},
  error() {},
};

export interface TestApp {
  app: Hono<AppEnv>;
  ctx: BackendContext;
  fake: FakeConversationManager;
  authToken: string;
  /** The resolved viewer id for this test fixture. Always `'local'` —
   *  see `openTestDb` for the test fixture registered users. */
  viewerUserId: string;
  cleanup: () => void;
}

/**
 * Build a Hono app + BackendContext suitable for in-process testing
 * (`app.fetch(req)`). No port is opened. The runtime host is a fake —
 * the real one would try to spawn agent CLI subprocesses on every
 * channel provision, which we don't want in unit tests.
 */
export function buildTestApp(): TestApp {
  const { db, viewerUserId } = openTestDb();
  const tmp = makeTempSilverhandDir();
  // The real `startBackend` mkdirs workspacesRoot before mounting routes.
  mkdirSync(tmp.workspacesRoot, { recursive: true });

  const fake = new FakeConversationManager();
  const authToken = 'a'.repeat(32);
  const serverConfig: ServerConfig = {
    mode: 'embedded',
    url: 'http://127.0.0.1:0',
    auth_token: authToken,
    refresh_token: null,
    child_sessions: {
      max_concurrent_per_parent: null,
      spawn_cooldown_seconds: null,
      max_depth: 1,
    },
  };

  const blobStorage = createLocalBlobFileStorage({ silverhandDir: tmp.silverhandDir });
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
    serverConfig,
    logger: silentLogger,
    loggers: { system: silentLogger, agent: silentLogger, cli: silentLogger },
    silverhandDir: tmp.silverhandDir,
    workspacesRoot: tmp.workspacesRoot,
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
    silverhandCliLauncher: { command: 'noop', args: [] },
    agentTokens: new AgentTokenStore(),
    runtimeHost: fake as unknown as AgentRuntimeHost,
  };
  ctx.channelService = new ChannelService({
    db,
    conversations: new ConversationRepo(db),
    workspaces: new WorkspaceRepo(db, { workspacesRoot: tmp.workspacesRoot }),
    agents: new AgentRepo(db),
    messages: new MessageRepo(db),
    events: ctx.events,
    runtimeHost: ctx.runtimeHost,
    logger: silentLogger,
  });
  ctx.conversationMessageService = new ConversationMessageService({
    db,
    conversations: new ConversationRepo(db),
    messages: new MessageRepo(db),
    agents: new AgentRepo(db),
    notifications: new NotificationRepo(db),
    workspaces: new WorkspaceRepo(db, { workspacesRoot: tmp.workspacesRoot }),
    attachmentService,
    runtimeHost: ctx.runtimeHost,
    events: ctx.events,
    logger: silentLogger,
  });
  ctx.threadService = new ThreadService({
    db,
    conversations: new ConversationRepo(db),
    messages: new MessageRepo(db),
    agents: new AgentRepo(db),
    workspaces: new WorkspaceRepo(db, { workspacesRoot: tmp.workspacesRoot }),
    runtimeHost: ctx.runtimeHost,
    events: ctx.events,
    logger: silentLogger,
    serverConfig: () => ctx.serverConfig,
    lastSpawnAtByAgent: ctx.lastSpawnAtByAgent,
    taskService: () => ctx.taskService,
  });
  ctx.taskService = new TaskService({
    db,
    conversations: new ConversationRepo(db),
    messages: new MessageRepo(db),
    agents: new AgentRepo(db),
    runtimeHost: ctx.runtimeHost,
    events: ctx.events,
    logger: silentLogger,
  });
  ctx.scheduleService = new ScheduleService({
    schedules: new ScheduleRepo(db),
    workspaces: new WorkspaceRepo(db, { workspacesRoot: tmp.workspacesRoot }),
    conversations: new ConversationRepo(db),
    messages: new MessageRepo(db),
    agents: new AgentRepo(db),
    settings: new SettingsRepo(db),
    runtimeHost: ctx.runtimeHost,
    events: ctx.events,
    logger: silentLogger,
    disableAutoTick: true,
  });

  // routes.ts calls hosts.ensureLocal() at construction; openTestDb() does
  // the same — this is a safety net if anyone changes the helper later.
  new HostRepo(db).ensureLocal();

  const app = new Hono<AppEnv>();
  app.use('*', silverhandCors());
  app.route('/', createApiRouter(ctx));
  app.route('/', createInternalRouter(ctx));

  return {
    app,
    ctx,
    fake,
    authToken,
    viewerUserId,
    cleanup: () => {
      ctx.scheduleService.stop();
      try {
        db.close();
      } catch {}
      tmp.cleanup();
    },
  };
}

/** Convenience: GET/POST/DELETE wrappers around `app.fetch`.
 *  All bodies returned via `.json()` are typed `any` to keep tests terse;
 *  Response itself preserves status/headers. */
export interface HttpHelper {
  get(path: string, init?: RequestInit): Promise<Response>;
  post(path: string, body?: unknown, init?: RequestInit): Promise<Response>;
  patch(path: string, body?: unknown, init?: RequestInit): Promise<Response>;
  del(path: string, init?: RequestInit): Promise<Response>;
  json<T = any>(res: Promise<Response> | Response): Promise<T>;
}

export function http(
  target: Hono<AppEnv> | TestApp,
  defaultHeaders: Record<string, string> = {},
): HttpHelper {
  const isTestApp = (value: unknown): value is TestApp =>
    typeof value === 'object' && value !== null && 'authToken' in value && 'app' in value;
  const app = isTestApp(target) ? target.app : target;
  const publicAuthHeaders: Record<string, string> = isTestApp(target)
    ? { authorization: `Bearer ${target.authToken}` }
    : {};
  const headers: Record<string, string> = { ...publicAuthHeaders, ...defaultHeaders };
  const base = 'http://localhost';
  const merge = (path: string, init?: RequestInit): RequestInit => {
    const mergedHeaders = {
      ...headers,
      ...(init?.headers as Record<string, string> | undefined),
    };
    if (isTestApp(target)) {
      patchInternalAuthHeader(target, path, mergedHeaders);
    }
    return {
      ...init,
      headers: mergedHeaders,
    };
  };
  const jsonReq = (
    path: string,
    method: 'POST' | 'PATCH',
    body: unknown,
    init?: RequestInit,
  ) =>
    merge(path, {
      method,
      headers: { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
      ...init,
    });
  return {
    get: async (p, init) =>
      app.fetch(new Request(base + p, merge(p, { method: 'GET', ...init }))),
    post: async (p, body, init) =>
      app.fetch(new Request(base + p, jsonReq(p, 'POST', body, init))),
    patch: async (p, body, init) =>
      app.fetch(new Request(base + p, jsonReq(p, 'PATCH', body, init))),
    del: async (p, init) =>
      app.fetch(new Request(base + p, merge(p, { method: 'DELETE', ...init }))),
    json: async <T = any>(res: Promise<Response> | Response): Promise<T> => {
      const r = await res;
      return r.json() as Promise<T>;
    },
  };
}

function patchInternalAuthHeader(
  target: TestApp,
  path: string,
  headers: Record<string, string>,
): void {
  const match = path.match(
    /^\/internal\/conversations\/([^/?#]+)\/agents\/([^/?#]+)(?:[/?#]|$)/,
  );
  if (!match) return;
  const current = headers.authorization ?? headers.Authorization;
  const publicBearer = `Bearer ${target.authToken}`;
  // Most route tests were written before internal tokens became scoped and
  // used the public bearer by default. Preserve their intent by translating
  // the default/public bearer to a valid per-agent token; explicit wrong
  // tokens remain wrong so auth tests still exercise rejection.
  if (current && current !== publicBearer) return;
  const [, conversationId, agentId] = match as [string, string, string];
  headers.authorization = `Bearer ${target.ctx.agentTokens.issue(
    decodeURIComponent(conversationId),
    decodeURIComponent(agentId),
  )}`;
}
