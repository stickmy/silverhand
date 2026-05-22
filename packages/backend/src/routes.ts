import { Hono, type Context } from 'hono';
import { promises as fs } from 'node:fs';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';
import {
  apiPaths,
  internalPaths,
  AddWorkspaceMemberInputSchema,
  AgentCloseThreadInputSchema,
  AgentCreateChannelInputSchema,
  AgentListChannelsQuerySchema,
  AgentReadHistoryInputSchema,
  AgentSearchMessagesInputSchema,
  AgentSendMessageInputSchema,
  AgentSpawnChildInputSchema,
  ClaimTasksInputSchema,
  CreateAgentInputSchema,
  CreateConversationChannelInputSchema,
  CreateWorkspaceInputSchema,
  CreateTasksInputSchema,
  DeleteAgentInputSchema,
  HumanClaimTaskInputSchema,
  HumanUpdateTaskStatusInputSchema,
  ListTasksQuerySchema,
  MarkConversationReadInputSchema,
  MarkNotificationsReadInputSchema,
  ScheduleCancelInputSchema,
  ScheduleCreateInputSchema,
  CliTraceInputSchema,
  ScheduleListInputSchema,
  SearchQueryInputSchema,
  SendMessageInputSchema,
  UnclaimTaskInputSchema,
  UpdateAgentInputSchema,
  UpdateUserProfileInputSchema,
  RegisterUserInputSchema,
  UpdateWorkspaceInputSchema,
  ArchiveWorkspaceInputSchema,
  ArchiveChannelInputSchema,
  ArchivedChannelsQueryInputSchema,
  UpdateTaskStatusInputSchema,
  type AgentDto,
  type AgentDeleteImpactDto,
  type AgentListedChannel,
  type AgentListedWorkspace,
  type AgentListChannelsDto,
  type AgentListWorkspacesDto,
  type CreateAgentInput,
  type AgentReadHistoryDto,
  type AgentSearchMessageHit,
  type AgentSearchMessagesDto,
  type MessageRef,
  type MessageRow,
  type MessageSenderRef,
  type ReadHistoryMessage,
  type ReadHistoryTarget,
  type ReadHistoryTargetRef,
  type SendMessageTarget,
  systemSubtype,
} from '@silverhand/shared';
import type { BackendContext } from './context.js';
import { createCliInvokeMiddleware } from './observability/cli-invoke-middleware.js';
import { AgentRepo } from './repos/agent-repo.js';
import { AttachmentRepo } from './repos/attachment-repo.js';
import { HostRepo } from './repos/host-repo.js';
import { MessageRepo, messageRowToDto } from './repos/message-repo.js';
import { NotificationRepo } from './repos/notification-repo.js';
import { SavedMessageRepo } from './repos/saved-message-repo.js';
import { SearchRepo } from './repos/search-repo.js';
import { ConversationReadRepo } from './repos/conversation-read-repo.js';
import { ConversationInboxRepo } from './repos/conversation-inbox-repo.js';
import { SettingsRepo, SETTINGS_KEYS } from './repos/settings-repo.js';
import { WorkspaceRepo } from './repos/workspace-repo.js';
import { ConversationRepo, type RepoError } from './repos/conversation-repo.js';
import { validateWorkspacePath } from './provision.js';
import { ensureAgentL1Dir, initWorkspaceDir, seedAgentL2 } from './workspace-files.js';
import { detectRuntimes } from '@silverhand/host/runtimes';
import { buildAgentWorkspaceDto } from './agent-workspace.js';
import { AttachmentRejection } from './services/attachment-service.js';
import {
  ActorError as ChannelActorError,
  MemberError,
  SeedError,
  WorkspaceUnsuitableError,
} from './services/channel-service.js';
import {
  ActorError as MessageActorError,
  AttachmentError as MessageAttachmentError,
  ConversationError,
  PolicyError,
  TargetError,
} from './services/conversation-message-service.js';
import {
  ActorError as ThreadActorError,
  MembershipError as ThreadMembershipError,
  MessageError as ThreadMessageError,
  ParentError as ThreadParentError,
  SeedAgentError,
  ThreadError,
  ThreadOpenTasksError,
} from './services/thread-service.js';
import {
  ScheduleError,
  type ScheduleErrorCode,
} from './services/schedule-service.js';
import { UserServiceError } from './services/user-service.js';
import { AgentServiceError } from './services/agent-service.js';
import { MembershipService } from './services/membership-service.js';
import { UserRepo } from './repos/user-repo.js';
import {
  type AppEnv,
  getAgentPrincipal,
  getUserPrincipal,
  installInternalPrincipalMiddleware,
  installPublicPrincipalMiddleware,
  installRequireRegisteredViewerMiddleware,
} from './auth/principal.js';
import { installPublicBearerMiddleware } from './auth/bearer.js';
import {
  canAgentAccessWorkspace,
  canDeleteAttachment,
  canEditAgent,
  canManageMembers,
  canMutateWorkspace,
  canPostToConversation,
  canViewAttachment,
  canViewConversation,
  canViewOwnedState,
  canViewWorkspace,
} from './permissions.js';

function isRepoError(err: unknown): err is RepoError {
  return err instanceof Error && typeof (err as RepoError).code === 'string';
}

const repoErrorStatusMap: Record<string, 400 | 404 | 409 | 429> = {
  parent_not_found: 404,
  parent_closed: 409,
  cross_channel_parent: 400,
  max_depth_exceeded: 400,
  concurrent_limit: 429,
  spawn_cooldown: 429,
  not_found: 404,
  thread: 400,
  not_archivable: 400,
};

function channelServiceErrorResponse(c: Context, err: unknown): Response | null {
  if (err instanceof WorkspaceUnsuitableError) {
    return c.json(
      { error: err.message, code: err.code },
      err.code === 'workspace_not_found' ? 404 : 400,
    );
  }
  if (err instanceof MemberError || err instanceof SeedError) {
    return c.json({ error: err.message, code: err.code }, 400);
  }
  if (err instanceof ChannelActorError) {
    const status =
      err.code === 'source_conversation_not_found' || err.code === 'agent_not_found'
        ? 404
        : 403;
    return c.json({ error: err.message, code: err.code }, status);
  }
  if (isRepoError(err)) {
    return c.json(
      { error: err.message, code: err.code },
      repoErrorStatusMap[err.code] ?? 400,
    );
  }
  return null;
}

function conversationMessageServiceErrorResponse(
  c: Context,
  err: unknown,
): Response | null {
  if (err instanceof ConversationError) {
    const status = err.code === 'conversation_not_found' ? 404 : 409;
    return c.json({ error: err.message, code: err.code }, status);
  }
  if (err instanceof MessageActorError) {
    const status = err.code === 'agent_not_found' ? 404 : 403;
    return c.json({ error: err.message, code: err.code }, status);
  }
  if (err instanceof TargetError) {
    const status = err.code === 'thread_not_found' ? 404 : err.code === 'target_closed' || err.code === 'target_archived' ? 409 : 400;
    return c.json({ error: err.message, code: err.code }, status);
  }
  if (err instanceof PolicyError) {
    return c.json({ error: err.message, code: err.code }, 400);
  }
  if (err instanceof MessageAttachmentError) {
    return c.json(
      {
        error: err.message,
        code: err.code,
        ...(err.attachmentId ? { attachment_id: err.attachmentId } : {}),
      },
      400,
    );
  }
  return null;
}

function threadServiceErrorResponse(c: Context, err: unknown): Response | null {
  if (err instanceof ThreadMessageError) {
    return c.json({ error: err.message, code: err.code }, 404);
  }
  if (err instanceof ThreadParentError) {
    const status = err.code === 'parent_not_found' ? 404 : 409;
    return c.json({ error: err.message, code: err.code }, status);
  }
  if (err instanceof ThreadMembershipError) {
    return c.json(
      {
        error: err.message,
        code: err.code,
        ...(err.agentId ? { agent_id: err.agentId } : {}),
      },
      400,
    );
  }
  if (err instanceof ThreadActorError) {
    const status = err.code === 'agent_not_found' ? 404 : 403;
    return c.json({ error: err.message, code: err.code }, status);
  }
  if (err instanceof SeedAgentError) {
    return c.json({ error: err.message, code: err.code }, 404);
  }
  if (err instanceof ThreadError) {
    const status = err.code === 'thread_not_found' ? 404 : 409;
    return c.json({ error: err.message, code: err.code }, status);
  }
  if (err instanceof ThreadOpenTasksError) {
    // Nested error shape on purpose: the CLI's stable ERROR_CODES enum
    // doesn't include silverhand-specific reasons like
    // `has_open_tasks`, so we surface the top-level `code: 'conflict'`
    // (which IS in the enum, drives `exitCodeFor` and `retriable`) and
    // park the silverhand reason in `details.reason`. The structured
    // `details.blocking_tasks` payload is what the CLI uses to render
    // the task list in human mode.
    return c.json(
      {
        error: {
          code: 'conflict',
          message: err.message,
          details: {
            reason: 'has_open_tasks',
            blocking_tasks: err.blockingTasks,
          },
        },
      },
      409,
    );
  }
  if (isRepoError(err)) {
    return c.json(
      { error: err.message, code: err.code },
      repoErrorStatusMap[err.code] ?? 400,
    );
  }
  return null;
}

function resolveViewerUserId(c: Context): string {
  return getUserPrincipal(c).userId;
}

const scheduleErrorStatusMap: Record<ScheduleErrorCode, 400 | 403 | 404 | 409 | 429> = {
  invalid_input: 400,
  invalid_cron_expression: 400,
  invalid_timezone: 400,
  fire_at_too_soon: 400,
  fire_at_too_far: 400,
  unbounded_repeat_requires_explicit_limit: 400,
  too_many_active_schedules: 429,
  agent_not_found: 404,
  agent_home_not_found: 404,
  target_conversation_not_found: 404,
  target_conversation_closed: 409,
  agent_not_in_target_conversation: 403,
  schedule_not_found: 404,
  not_owner: 403,
  not_cancellable: 409,
};

function scheduleServiceErrorResponse(c: Context, err: unknown): Response | null {
  if (err instanceof ScheduleError) {
    const status = scheduleErrorStatusMap[err.code] ?? 400;
    return c.json({ error: err.message, code: err.code }, status);
  }
  return null;
}

function requireViewerUserId(c: Context): string {
  return resolveViewerUserId(c);
}

const userServiceErrorStatusMap: Partial<
  Record<UserServiceError['code'], 400 | 404 | 409 | 413 | 415>
> = {
  user_not_found: 404,
  name_taken: 409,
  already_registered: 409,
  invalid_input: 400,
  avatar_missing: 400,
  avatar_too_large: 413,
  avatar_unsupported_type: 415,
  avatar_storage_failed: 400,
};

function userServiceErrorStatus(err: unknown): 400 | 404 | 409 | 413 | 415 | null {
  if (err instanceof UserServiceError) {
    return userServiceErrorStatusMap[err.code] ?? 400;
  }
  return null;
}

function userServiceErrorBody(err: unknown): { error: string; code: string } {
  if (err instanceof UserServiceError) {
    return { error: err.message, code: err.code };
  }
  return { error: String(err), code: 'unknown_error' };
}

const agentServiceErrorStatusMap: Partial<
  Record<AgentServiceError['code'], 400 | 404 | 413 | 415>
> = {
  agent_not_found: 404,
  avatar_missing: 400,
  avatar_too_large: 413,
  avatar_unsupported_type: 415,
  avatar_storage_failed: 400,
};

function agentServiceErrorStatus(err: unknown): 400 | 404 | 413 | 415 | null {
  if (err instanceof AgentServiceError) {
    return agentServiceErrorStatusMap[err.code] ?? 400;
  }
  return null;
}

function agentServiceErrorBody(err: unknown): { error: string; code: string } {
  if (err instanceof AgentServiceError) {
    return { error: err.message, code: err.code };
  }
  return { error: String(err), code: 'unknown_error' };
}

function onboardingBody(
  workspaceName: string,
  recipient: AgentDto,
  others: AgentDto[],
): string {
  return [
    `You (@${recipient.name}) just joined workspace #${workspaceName}.`,
    others.length === 0
      ? `You're the only agent here right now.`
      : `Other agents in this workspace:\n${others.map(m => `- @${m.name} (${m.runtime})`).join('\n')}`,
    ``,
    `Your role contract is auto-loaded above (sourced from L1 system_prompt.md, shared across every workspace), and this workspace's MEMORY.md is auto-loaded as Active Workspace Memory.`,
    `Send one short greeting introducing yourself by your global role. As you work, write by scope: global identity / role / working-style guidance → update L1 system_prompt.md; this workspace's collaboration, tasks, project facts, or work logs → update L2 MEMORY.md / notes/ / CONSENSUS.md. Do not mention internal file paths to the user unless they ask.`,
  ].join('\n');
}

const SLOW_REQUEST_MS = 500;
const DIAGNOSTIC_REQUEST_MS = 100;
const SLOW_ROUTE_SPAN_MS = 100;
let apiRequestSequence = 0;

function installApiTimingMiddleware(app: Hono<AppEnv>, ctx: BackendContext): void {
  app.use('*', async (c, next) => {
    const url = new URL(c.req.url);
    const path = url.pathname;
    const isDiagnosticPath = isChannelSwitchDiagnosticPath(path);
    const requestId = nextApiRequestId();
    const startedAt = performance.now();
    ctx.logger.info(
      {
        requestId,
        method: c.req.method,
        path,
        query: url.search || null,
      },
      'api: request start',
    );
    try {
      await next();
    } finally {
      const elapsedMs = elapsedSince(startedAt);
      ctx.logger.info(
        {
          requestId,
          method: c.req.method,
          path,
          query: url.search || null,
          status: c.res.status,
          elapsedMs,
        },
        'api: request complete',
      );
      if (
        elapsedMs >= SLOW_REQUEST_MS ||
        (isDiagnosticPath && elapsedMs >= DIAGNOSTIC_REQUEST_MS)
      ) {
        ctx.logger.warn(
          {
            requestId,
            method: c.req.method,
            path,
            status: c.res.status,
            elapsedMs,
            thresholdMs: isDiagnosticPath ? DIAGNOSTIC_REQUEST_MS : SLOW_REQUEST_MS,
          },
          'api: slow request',
        );
      }
    }
  });
}

function nextApiRequestId(): string {
  apiRequestSequence = (apiRequestSequence + 1) % Number.MAX_SAFE_INTEGER;
  return `api-${apiRequestSequence}`;
}

function isChannelSwitchDiagnosticPath(path: string): boolean {
  if (path === apiPaths.messageUnreadSummary) {
    return true;
  }
  if (/^\/api\/workspaces\/[^/]+\/channels(\/archived)?$/.test(path)) return true;
  if (
    /^\/api\/channels\/[^/]+\/(messages|agents|agent-run-states|threads|mark-read|message-unread)$/.test(
      path,
    )
  ) {
    return true;
  }
  return /^\/api\/threads\/[^/]+\/(messages|agents|agent-run-states|mark-read)$/.test(
    path,
  );
}

function timedRouteSpan<T>(
  ctx: BackendContext,
  span: string,
  fields: Record<string, unknown>,
  fn: () => T,
  resultFields?: (result: T) => Record<string, unknown>,
): T {
  const startedAt = performance.now();
  try {
    const result = fn();
    const elapsedMs = elapsedSince(startedAt);
    if (elapsedMs >= SLOW_ROUTE_SPAN_MS) {
      ctx.logger.warn(
        {
          span,
          elapsedMs,
          thresholdMs: SLOW_ROUTE_SPAN_MS,
          ...fields,
          ...(resultFields?.(result) ?? {}),
        },
        'api: slow route span',
      );
    }
    return result;
  } catch (err) {
    ctx.logger.warn(
      {
        span,
        elapsedMs: elapsedSince(startedAt),
        ...fields,
        err,
      },
      'api: route span failed',
    );
    throw err;
  }
}

function elapsedSince(startedAt: number): number {
  return Math.round((performance.now() - startedAt) * 10) / 10;
}

export function createApiRouter(ctx: BackendContext): Hono<AppEnv> {
  const app = new Hono<AppEnv>();
  installApiTimingMiddleware(app, ctx);
  installPublicBearerMiddleware(app, ctx, {
    // Liveness probe stays open so a process supervisor can health-check
    // without holding the auth token.
    allowlist: [apiPaths.health],
  });
  installPublicPrincipalMiddleware(app, ctx);
  // Registration gate: when no viewer is registered yet (no
  // `app_settings.viewer_user_id`), the principal middleware sets no
  // principal and this gate refuses every public route except
  // `GET /api/me` (so the renderer can branch on the
  // registration-required shape) and `POST /api/register` (the sole
  // creator of `users` rows). `/api/health` and `/internal/*` are
  // anonymous / agent-principal and skip both layers. After
  // registration, the principal is set and the gate is a no-op for the
  // viewer; under DB corruption (principal set but row missing) it 500s.
  // Renderer-side `App.tsx` mirrors this by mounting `<RegistrationGate />`
  // instead of the AppShell while in the registration-required state.
  installRequireRegisteredViewerMiddleware(app, ctx, {
    whitelistPaths: [apiPaths.me, apiPaths.register],
  });
  const hosts = new HostRepo(ctx.db);
  const agents = new AgentRepo(ctx.db);
  const workspaces = new WorkspaceRepo(ctx.db, { workspacesRoot: ctx.workspacesRoot });
  const conversations = new ConversationRepo(ctx.db);
  const messages = new MessageRepo(ctx.db);
  const attachments = new AttachmentRepo(ctx.db);
  const saved = new SavedMessageRepo(ctx.db);
  const search = new SearchRepo(ctx.db);
  const settings = new SettingsRepo(ctx.db);
  const conversationInboxes = new ConversationInboxRepo(ctx.db);
  const notifications = new NotificationRepo(ctx.db);
  const conversationReads = new ConversationReadRepo(ctx.db);
  const users = new UserRepo(ctx.db);
  const membership = new MembershipService({ agents, users, conversations });

  const localHost = hosts.ensureLocal();

  // ── Health / runtimes / hosts ────────────────────────────────────
  app.get(apiPaths.health, c =>
    c.json({ ok: true, mode: ctx.serverConfig.mode, ts: Date.now() }),
  );

  app.get(apiPaths.runtimes, async c => {
    const runtimes = await detectRuntimes();
    return c.json({ runtimes });
  });

  app.get(apiPaths.hosts, c => c.json({ hosts: hosts.list() }));

  // ── Workspaces / conversations ──────────────────────────────────
  //
  // Canonical workspace/channel/thread API backed by workspaces and
  // conversations. Route kind guards prevent a thread id from silently
  // passing as a channel id.

  app.get(apiPaths.workspaces, c =>
    c.json({
      workspaces: workspaces
        .list()
        .filter(workspace => canViewWorkspace(ctx, getUserPrincipal(c), workspace.id)),
    }),
  );

  app.post(apiPaths.workspaces, async c => {
    const body = await c.req.json().catch(() => ({}));
    const parsed = CreateWorkspaceInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    if (workspaces.getByName(parsed.data.name)) {
      return c.json({ error: 'workspace name already exists' }, 409);
    }

    const workspacePath =
      parsed.data.workspace_path?.trim() || workspaces.workspacePathFor(parsed.data.name);
    const validation = validateWorkspacePath(
      workspacePath,
      ctx.silverhandDir,
      ctx.workspacesRoot,
    );
    if (!validation.ok) {
      return c.json(
        { error: `invalid workspace_path: ${validation.reason}` },
        400,
      );
    }
    await initWorkspaceDir(validation.resolved);

    const viewerUserId = requireViewerUserId(c);

    const workspace = workspaces.insert({
      name: parsed.data.name,
      description: parsed.data.description ?? null,
      workspacePath: validation.resolved,
      workspaceHostId: parsed.data.workspace_host_id ?? localHost.id,
    });

    // v25: human membership. The actor (registered viewer) becomes a
    // member of every workspace they create. In v1 single-human this is
    // always the same one viewer; in v2 multi-human it's still
    // correct — the creator is a member by default.
    workspaces.addUserMember(workspace.id, viewerUserId);

    const memberAgents = parsed.data.member_agent_ids
      .map(id => agents.get(id))
      .filter((agent): agent is AgentDto => agent !== null);
    for (const agent of memberAgents) {
      workspaces.addMember(workspace.id, agent.id);
      const agentL1Dir = join(ctx.silverhandDir, 'agents', agent.id);
      await ensureAgentL1Dir(agentL1Dir);
      await seedAgentL2({
        workspacePath: workspace.workspace_path,
        agentId: agent.id,
      });
    }

    const onboarding = conversations.insertChannel({
      workspaceId: workspace.id,
      workspacePath: workspace.workspace_path,
      title: 'Onboarding',
    });
    // Snapshot workspace_users into the onboarding channel.
    // v1: single viewer; v2 will fan out across all workspace humans.
    for (const userId of workspaces.listUserMemberIds(workspace.id)) {
      conversations.addUser(onboarding.id, userId);
    }
    for (const agent of memberAgents) {
      conversations.addAgent(onboarding.id, agent.id);
    }
    for (const agent of memberAgents) {
      const others = memberAgents.filter(m => m.id !== agent.id);
      const onboardingMsg = messages.insertForConversation({
        conversationId: onboarding.id,
        fromKind: 'system',
        content: onboardingBody(workspace.name, agent, others),
        metadata: { subtype: systemSubtype('onboarding') },
      });
      await ctx.runtimeHost.deliverMessage(
        onboarding.id,
        agent.id,
        onboardingMsg.id,
      );
      ctx.runtimeHost
        .start(onboarding.id, agent.id)
        .catch(err =>
          ctx.logger.error(
            { conversationId: onboarding.id, agentId: agent.id, err },
            'onboarding spawn failed',
          ),
        );
    }

    return c.json(
      { workspace, onboarding_channel_id: onboarding.id, channel: onboarding },
      201,
    );
  });

  app.get(apiPaths.workspace(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'workspace id required' }, 400);
    const workspace = workspaces.get(id);
    if (!workspace) {
      return c.json({ error: 'workspace not found' }, 404);
    }
    if (!canViewWorkspace(ctx, getUserPrincipal(c), workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    return c.json({ workspace });
  });

  app.patch(apiPaths.workspace(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'workspace id required' }, 400);
    const workspace = workspaces.get(id);
    if (!workspace || workspace.workspace_kind !== 'regular') {
      return c.json({ error: 'workspace not found' }, 404);
    }
    if (!canMutateWorkspace(ctx, getUserPrincipal(c), workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = UpdateWorkspaceInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    if (parsed.data.name && parsed.data.name !== workspace.name) {
      const collision = workspaces.getByName(parsed.data.name);
      if (collision && collision.id !== workspace.id) {
        return c.json({ error: 'workspace name already exists' }, 409);
      }
    }
    const updated = workspaces.updateMutableMetadata(workspace.id, parsed.data);
    if (!updated) return c.json({ error: 'workspace update failed' }, 500);

    if (parsed.data.name && parsed.data.name !== workspace.name) {
      for (const channel of conversations.listAllChannelsForWorkspace(workspace.id)) {
        if (channel.closed_at !== null) continue;
        const announce = messages.insertForConversation({
          conversationId: channel.id,
          fromKind: 'system',
          content:
            `This workspace was renamed from #${workspace.name} to #${parsed.data.name}. ` +
            `Update any references in your MEMORY.md when you next edit it.`,
          metadata: {
            subtype: systemSubtype('rename'),
            old_name: workspace.name,
            new_name: parsed.data.name,
          },
        });
        for (const ca of conversations.listAgents(channel.id)) {
          await ctx.runtimeHost.deliverMessage(
            channel.id,
            ca.agent_id,
            announce.id,
          );
        }
        ctx.events.publish(channel.id, {
          kind: 'message',
          payload: { message: announce },
        });
      }
    }

    return c.json({ workspace: updated });
  });

  app.patch('/api/workspaces/:id/archive', async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'workspace id required' }, 400);
    const workspace = workspaces.get(id);
    if (!workspace || workspace.workspace_kind !== 'regular') {
      return c.json({ error: 'workspace not found' }, 404);
    }
    if (!canMutateWorkspace(ctx, getUserPrincipal(c), workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = ArchiveWorkspaceInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const updated = workspaces.setArchived(workspace.id, parsed.data.archived);
    if (!updated) return c.json({ error: 'archive update failed' }, 500);
    return c.json({ workspace: updated });
  });

  app.post(apiPaths.workspacePin(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'workspace id required' }, 400);
    const workspace = workspaces.get(id);
    if (
      !workspace ||
      workspace.workspace_kind !== 'regular' ||
      workspace.archived_at !== null
    ) {
      return c.json({ error: 'workspace not found' }, 404);
    }
    if (!canMutateWorkspace(ctx, getUserPrincipal(c), workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const updated = workspaces.setPinned(workspace.id, true);
    if (!updated) return c.json({ error: 'workspace pin failed' }, 500);
    return c.json({ workspace: updated });
  });

  app.delete(apiPaths.workspacePin(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'workspace id required' }, 400);
    const workspace = workspaces.get(id);
    if (
      !workspace ||
      workspace.workspace_kind !== 'regular' ||
      workspace.archived_at !== null
    ) {
      return c.json({ error: 'workspace not found' }, 404);
    }
    if (!canMutateWorkspace(ctx, getUserPrincipal(c), workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const updated = workspaces.setPinned(workspace.id, false);
    if (!updated) return c.json({ error: 'workspace unpin failed' }, 500);
    return c.json({ workspace: updated });
  });

  app.delete(apiPaths.workspace(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'workspace id required' }, 400);
    const workspace = workspaces.get(id);
    if (!workspace || workspace.workspace_kind !== 'regular') {
      return c.body(null, 204);
    }
    if (!canMutateWorkspace(ctx, getUserPrincipal(c), workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const workspaceConversations = conversations
      .list()
      .filter(conversation => conversation.workspace_id === workspace.id);
    for (const conversation of workspaceConversations) {
      ctx.runtimeHost.resolveConversationRuns(conversation.id, 'conversation_closed');
      for (const ca of conversations.listAgents(conversation.id)) {
        void ctx.runtimeHost.stop(conversation.id, ca.agent_id, 'hard');
      }
    }
    // Sweep physical attachment blobs for every top-level channel before
    // dropping the workspace. attachments.channel_id only points at
    // channel/dm_channel rows (threads upload via parent), so iterate
    // those — DB rows go via CASCADE, blobs need an explicit delete.
    for (const conversation of workspaceConversations) {
      if (conversation.kind === 'channel' || conversation.kind === 'dm_channel') {
        await ctx.attachmentService
          .sweepForChannel(conversation.id)
          .catch(err =>
            ctx.logger.warn(
              { workspaceId: workspace.id, conversationId: conversation.id, err },
              'delete workspace: attachment sweep failed (continuing)',
            ),
          );
      }
    }
    workspaces.delete(workspace.id);
    return c.body(null, 204);
  });

  app.get(apiPaths.workspaceChannels(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'workspace id required' }, 400);
    const workspace = workspaces.get(id);
    if (!workspace) {
      return c.json({ error: 'workspace not found' }, 404);
    }
    const principal = getUserPrincipal(c);
    if (!canViewWorkspace(ctx, principal, workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    // Membership filter: human principals only see channels/threads
    // they are explicit members of (`conversation_users`). Agent /
    // system principals never call these public list endpoints in
    // practice; if they ever do, the unfiltered behavior is the
    // historical contract.
    const forUserId =
      principal.kind === 'user' ? principal.userId : undefined;
    const channels =
      workspace.workspace_kind === 'regular'
        ? timedRouteSpan(
            ctx,
            'workspace.channels.list_active',
            { workspaceId: workspace.id },
            () =>
              conversations.listActiveChannelsForWorkspace(workspace.id, {
                forUserId,
              }),
            result => ({ channelCount: result.length }),
          )
        : timedRouteSpan(
            ctx,
            'workspace.dm_channel.list',
            { workspaceId: workspace.id },
            () =>
              conversations.listDmChannelsForWorkspace(workspace.id, {
                forUserId,
              }),
            result => ({ channelCount: result.length }),
          );
    // Active endpoint only returns threads whose parent is also active —
    // the response is a self-contained tree, no orphan threads.
    const threads =
      workspace.workspace_kind === 'regular'
        ? timedRouteSpan(
            ctx,
            'workspace.threads.list_active',
            { workspaceId: workspace.id },
            () =>
              conversations.listThreadsForWorkspace(workspace.id, {
                parentArchived: false,
                forUserId,
              }),
            result => ({ threadCount: result.length }),
          )
        : timedRouteSpan(
            ctx,
            'workspace.threads.list',
            { workspaceId: workspace.id },
            () =>
              conversations.listThreadsForWorkspace(workspace.id, {
                forUserId,
              }),
            result => ({ threadCount: result.length }),
          );
    return c.json({
      channels,
      threads,
    });
  });

  app.get(apiPaths.workspaceArchivedChannels(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'workspace id required' }, 400);
    const workspace = workspaces.get(id);
    if (!workspace) {
      return c.json({ error: 'workspace not found' }, 404);
    }
    const principal = getUserPrincipal(c);
    if (!canViewWorkspace(ctx, principal, workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    // agent_home workspaces don't support channel archive (their only
    // conversation is the dm_channel). Return an empty tree so the
    // frontend can call this hook unconditionally.
    if (workspace.workspace_kind !== 'regular') {
      return c.json({ channels: [], threads: [] });
    }
    const forUserId =
      principal.kind === 'user' ? principal.userId : undefined;
    const channels = timedRouteSpan(
      ctx,
      'workspace.channels.list_archived',
      { workspaceId: workspace.id },
      () =>
        conversations.listArchivedChannelsForWorkspace(workspace.id, {
          forUserId,
        }),
      result => ({ channelCount: result.length }),
    );
    const threads = timedRouteSpan(
      ctx,
      'workspace.threads.list_archived',
      { workspaceId: workspace.id },
      () =>
        conversations.listThreadsForWorkspace(workspace.id, {
          parentArchived: true,
          forUserId,
        }),
      result => ({ threadCount: result.length }),
    );
    return c.json({ channels, threads });
  });

  app.get(apiPaths.archivedChannels, c => {
    const principal = getUserPrincipal(c);
    const parsed = ArchivedChannelsQueryInputSchema.safeParse({
      workspace_id: c.req.query('workspace_id'),
      since: c.req.query('since'),
    });
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    if (
      parsed.data.workspace_id !== undefined &&
      !canViewWorkspace(ctx, principal, parsed.data.workspace_id)
    ) {
      // Mirror the workspace-scoped endpoint: 404 when the workspace
      // doesn't exist, 403 when the caller can't see it. v1 user can
      // see every workspace, so this is mostly future-proofing.
      const exists = workspaces.get(parsed.data.workspace_id);
      return c.json(
        { error: exists ? 'forbidden' : 'workspace not found' },
        exists ? 403 : 404,
      );
    }
    const rows = timedRouteSpan(
      ctx,
      'archived_channels.list',
      {
        workspaceFilter: parsed.data.workspace_id ?? null,
        since: parsed.data.since ?? null,
      },
      () =>
        conversations.listArchivedChannelsAggregate({
          workspaceId: parsed.data.workspace_id,
          since: parsed.data.since,
          forUserId:
            principal.kind === 'user' ? principal.userId : undefined,
        }),
      result => ({ rowCount: result.length }),
    );
    // Membership filter is applied at the repo level (forUserId), so
    // the result already only includes channels the principal is a
    // `conversation_users` member of. v1 single-human is auto-added
    // to every conversation; v2 multi-human relies on this filter.
    return c.json({ channels: rows });
  });

  app.post(apiPaths.workspaceChannels(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'workspace id required' }, 400);
    const body = await c.req.json().catch(() => ({}));
    const parsed = CreateConversationChannelInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const workspace = workspaces.get(id);
    if (!workspace) {
      return c.json({ error: 'workspace not found' }, 404);
    }
    if (!canMutateWorkspace(ctx, getUserPrincipal(c), workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    if (workspace.workspace_kind === 'agent_home') {
      if (parsed.data.member_agent_ids.length > 0) {
        return c.json(
          { error: 'agent_home channels always use the home owner as member' },
          400,
        );
      }
      if (!workspace.owner_agent_id) {
        return c.json({ error: 'agent_home workspace missing owner' }, 500);
      }
      const owner = agents.get(workspace.owner_agent_id);
      if (!owner) return c.json({ error: 'agent_home owner not found' }, 404);
      const principal = getUserPrincipal(c);
      const channel = ctx.db.transaction(() => {
        const dm = conversations.insertDmChannel({
          workspaceId: workspace.id,
          workspacePath: workspace.workspace_path,
          title: parsed.data.title,
          agentId: owner.id,
        });
        conversations.addAgent(dm.id, owner.id);
        // v25: human membership for the freshly-created DM. The user
        // who triggered the route is a workspace member (canMutate
        // already passed), so they are a legitimate human member of
        // their newly-created DM. Without this row the post-create
        // GET /messages would 403 for the same principal that just
        // created it.
        if (principal.kind === 'user') {
          conversations.addUser(dm.id, principal.userId);
        }
        return dm;
      })();
      ctx.events.publishToScope(`workspace:${workspace.id}`, 'workspace_channels_changed', {
        workspace_id: workspace.id,
        channel_id: channel.id,
      });
      return c.json({ channel }, 201);
    }
    try {
      const result = await ctx.channelService.createChannel({
        workspaceId: id,
        title: parsed.data.title,
        memberAgentIds: parsed.data.member_agent_ids,
        actor: getUserPrincipal(c),
      });
      return c.json({ channel: result.channel }, 201);
    } catch (err) {
      const mapped = channelServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  app.get(apiPaths.workspaceMembers(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'workspace id required' }, 400);
    const workspace = workspaces.get(id);
    if (!workspace) return c.json({ error: 'workspace not found' }, 404);
    if (!canViewWorkspace(ctx, getUserPrincipal(c), workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    if (workspace.workspace_kind === 'agent_home') {
      return c.json({ members: [] });
    }
    return c.json({ members: workspaces.listMembers(workspace.id) });
  });

  app.post(apiPaths.workspaceMembers(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'workspace id required' }, 400);
    const workspace = workspaces.get(id);
    if (!workspace || workspace.workspace_kind !== 'regular') {
      return c.json({ error: 'workspace not found' }, 404);
    }
    if (!canManageMembers(ctx, getUserPrincipal(c), workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = AddWorkspaceMemberInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const agent = agents.get(parsed.data.agent_id);
    if (!agent) return c.json({ error: 'agent not found' }, 404);
    const member = workspaces.addMember(workspace.id, agent.id);
    const agentL1Dir = join(ctx.silverhandDir, 'agents', agent.id);
    await ensureAgentL1Dir(agentL1Dir);
    await seedAgentL2({
      workspacePath: workspace.workspace_path,
      agentId: agent.id,
    });
    return c.json({ member }, 201);
  });

  app.delete(apiPaths.workspaceMember(':id', ':agentId'), async c => {
    const id = c.req.param('id');
    const agentId = c.req.param('agentId');
    if (!id || !agentId) {
      return c.json({ error: 'workspace id and agent id required' }, 400);
    }
    const workspace = workspaces.get(id);
    if (!workspace || workspace.workspace_kind !== 'regular') return c.body(null, 204);
    if (!canManageMembers(ctx, getUserPrincipal(c), workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    for (const conversation of conversations.list()) {
      if (conversation.workspace_id !== workspace.id) continue;
      await ctx.runtimeHost
        .stop(conversation.id, agentId, 'hard')
        .catch(err =>
          ctx.logger.warn(
            { conversationId: conversation.id, agentId, err },
            'workspace member remove: stop failed',
          ),
        );
      conversations.removeAgent(conversation.id, agentId);
    }
    workspaces.removeMember(workspace.id, agentId);
    return c.body(null, 204);
  });

  app.get('/api/workspaces/:id/message-unread', c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'workspace id required' }, 400);
    const workspace = workspaces.get(id);
    if (!workspace) {
      return c.json({ error: 'workspace not found' }, 404);
    }
    if (!canViewWorkspace(ctx, getUserPrincipal(c), workspace.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const viewerUserId = requireViewerUserId(c);
    const unread = timedRouteSpan(
      ctx,
      'workspace.message_unread.counts',
      { workspaceId: workspace.id, viewerUserId },
      () =>
        conversationReads.unreadCountsByConversationForWorkspace(
          workspace.id,
          viewerUserId,
        ),
      result => ({
        conversationCount: Object.keys(result.by_conversation).length,
        total: result.total,
      }),
    );
    return c.json(unread);
  });

  app.get(apiPaths.messageUnreadSummary, c => {
    const viewerUserId = requireViewerUserId(c);
    if (!canViewOwnedState(ctx, getUserPrincipal(c), viewerUserId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const byWorkspace = timedRouteSpan(
      ctx,
      'workspace.message_unread.summary',
      { viewerUserId },
      () => conversationReads.unreadCountsByWorkspaceSidebar(viewerUserId),
      result => ({ scopeCount: Object.keys(result).length }),
    );
    return c.json({
      by_workspace: byWorkspace,
    });
  });

  app.get('/api/channels/:id/message-unread', c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'channel id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), channel.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const viewerUserId = requireViewerUserId(c);
    if (!canViewOwnedState(ctx, getUserPrincipal(c), viewerUserId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const count = timedRouteSpan(
      ctx,
      'conversation.message_unread.count',
      { conversationId: channel.id, viewerUserId },
      () =>
        conversationReads.unreadCountForConversation(
          channel.id,
          viewerUserId,
        ),
    );
    return c.json({ by_conversation: { [channel.id]: count }, total: count });
  });

  app.get(apiPaths.conversationChannelMessages(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'channel id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), channel.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const parsedLimit = Number(c.req.query('limit') ?? 200);
    const parsedBefore = c.req.query('before_seq');
    const limit = Number.isFinite(parsedLimit) ? Math.min(Math.max(parsedLimit, 1), 500) : 200;
    const beforeSeq = parsedBefore ? Number(parsedBefore) : null;
    const window = timedRouteSpan(
      ctx,
      'conversation.messages.window',
      { conversationId: channel.id, kind: channel.kind, limit, beforeSeq },
      () => messages.listWindowForConversation(channel.id, { limit, beforeSeq }),
      result => ({
        messageCount: result.messages.length,
        hasMoreBefore: result.has_more_before,
      }),
    );
    return c.json(window);
  });

  app.get(apiPaths.conversationChannel(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'channel id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), channel.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    return c.json({ channel });
  });

  app.get(apiPaths.thread(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'thread id required' }, 400);
    const thread = conversations.getThread(id);
    if (!thread) return c.json({ error: 'thread not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), thread.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    return c.json({ thread });
  });

  /**
   * GET /api/conversations/:id/ref
   *
   * Identity + routing metadata for any conversation (channel /
   * dm_channel / thread) by id. Designed to back cross-workspace
   * secondary-pane resolution and the `#conv:<id>` sigil renderer (PR-3).
   *
   * Permission: `canViewWorkspace(user, workspace_id)` only — the
   * caller must be allowed to *see* the conversation's workspace.
   * This route is on the user-facing surface only (no `/internal`
   * twin), so agent principals never reach it. Do NOT generalise to
   * agents without a dedicated ACL helper.
   *
   * Response intentionally excludes messages and per-user unread
   * state — those go through the existing window/cursor endpoints.
   * `workspace_kind` is projected `agent_home → 'dm'` so the
   * renderer never sees a third kind.
   */
  app.get(apiPaths.conversationRef(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'conversation id required' }, 400);
    const conversation = conversations.get(id);
    if (!conversation) return c.json({ error: 'conversation not found' }, 404);
    if (!canViewWorkspace(ctx, getUserPrincipal(c), conversation.workspace_id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const workspace = workspaces.get(conversation.workspace_id);
    if (!workspace) return c.json({ error: 'conversation not found' }, 404);

    const parentChannelId =
      conversation.kind === 'thread' ? conversation.parent_conversation_id : null;
    let parentTitle: string | null = null;
    if (parentChannelId) {
      const parent = conversations.get(parentChannelId);
      // Parent missing implies a stale row; we still return ref but
      // leave parent_title null so the renderer falls back to "thread"
      // instead of crashing.
      parentTitle = parent ? parent.title : null;
    }

    const ref = {
      id: conversation.id,
      kind: conversation.kind,
      title: conversation.title,
      workspace_id: conversation.workspace_id,
      // External projection: agents see `agent_home` internally; users
      // see `dm`. The renderer/UI uses regular|dm exclusively.
      workspace_kind:
        workspace.workspace_kind === 'agent_home'
          ? ('dm' as const)
          : ('regular' as const),
      workspace_path: conversation.workspace_path,
      parent_channel_id: parentChannelId,
      parent_title: parentTitle,
      closed_at: conversation.closed_at,
      archived_at: conversation.archived_at,
    };
    return c.json({ ref });
  });

  /**
   * GET /api/conversations/:id/members
   *
   * Resolved member list (agents + humans) for any conversation by id.
   * Backs the chat-header member-chip strip (PR-C). Same `MemberDto`
   * shape that the platform prompt's "Members in this conversation:"
   * block (PR-B) consumes — single source of truth so the agent's
   * stated participants and the chips the human sees can never drift.
   *
   * Permission: `canViewConversation` — strict membership / system
   * gate (workspace member or conversation member). Threads inherit
   * their parent's membership at creation time (v25 PR-A invariant),
   * so this route is correct for channel / dm_channel / thread alike
   * without special-casing.
   *
   * Self filtering is the renderer's job. Agent self has no UI
   * meaning here; the renderer drops the current human viewer
   * (UI-self) by `kind === 'human' && user_id === viewer.id` —
   * unrelated to the prompt's per-spawn `self: true` agent marker.
   */
  app.get(apiPaths.conversationMembers(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'conversation id required' }, 400);
    const conversation = conversations.get(id);
    if (!conversation) return c.json({ error: 'conversation not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), conversation.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const members = membership.listMembers(conversation.id);
    return c.json({ members });
  });

  app.get(apiPaths.threadMessages(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'thread id required' }, 400);
    const thread = conversations.getThread(id);
    if (!thread) return c.json({ error: 'thread not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), thread.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const parsedLimit = Number(c.req.query('limit') ?? 200);
    const parsedBefore = c.req.query('before_seq');
    const limit = Number.isFinite(parsedLimit) ? Math.min(Math.max(parsedLimit, 1), 500) : 200;
    const beforeSeq = parsedBefore ? Number(parsedBefore) : null;
    const window = timedRouteSpan(
      ctx,
      'conversation.messages.window',
      { conversationId: thread.id, kind: thread.kind, limit, beforeSeq },
      () => messages.listWindowForConversation(thread.id, { limit, beforeSeq }),
      result => ({
        messageCount: result.messages.length,
        hasMoreBefore: result.has_more_before,
      }),
    );
    return c.json(window);
  });

  app.get(apiPaths.conversationChannelAgents(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'channel id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), channel.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const channelAgents = timedRouteSpan(
      ctx,
      'conversation.agents.list',
      { conversationId: channel.id, kind: channel.kind },
      () => conversations.listAgents(channel.id),
      result => ({ agentCount: result.length }),
    );
    return c.json({ agents: channelAgents });
  });

  app.get(apiPaths.threadAgents(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'thread id required' }, 400);
    const thread = conversations.getThread(id);
    if (!thread) return c.json({ error: 'thread not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), thread.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const threadAgents = timedRouteSpan(
      ctx,
      'conversation.agents.list',
      { conversationId: thread.id, kind: thread.kind },
      () => conversations.listAgents(thread.id),
      result => ({ agentCount: result.length }),
    );
    return c.json({ agents: threadAgents });
  });

  app.post('/api/channels/:id/pin', async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'channel id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    if (!canMutateWorkspace(ctx, getUserPrincipal(c), channel.workspace_id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    try {
      const result = await ctx.channelService.setPinned({ channelId: id, pinned: true });
      return c.json({ channel: result.channel });
    } catch (err) {
      const mapped = channelServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  app.delete('/api/channels/:id/pin', async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'channel id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    if (!canMutateWorkspace(ctx, getUserPrincipal(c), channel.workspace_id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    try {
      const result = await ctx.channelService.setPinned({ channelId: id, pinned: false });
      return c.json({ channel: result.channel });
    } catch (err) {
      const mapped = channelServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  app.patch(apiPaths.channelArchive(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'channel id required' }, 400);
    const conversation = conversations.get(id);
    if (!conversation) return c.json({ error: 'channel not found' }, 404);
    if (!canMutateWorkspace(ctx, getUserPrincipal(c), conversation.workspace_id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = ArchiveChannelInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    try {
      const result = await ctx.channelService.setArchived({
        channelId: id,
        archived: parsed.data.archived,
      });
      return c.json({ channel: result.channel });
    } catch (err) {
      const mapped = channelServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  app.get(apiPaths.channelAgentRunStates(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'channel id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), channel.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const states = timedRouteSpan(
      ctx,
      'conversation.agent_run_states.snapshot',
      { conversationId: channel.id, kind: channel.kind },
      () => ctx.runtimeHost.getAgentRunStates(channel.id),
      result => ({ stateCount: result.length }),
    );
    return c.json({ states });
  });

  app.get(apiPaths.threadAgentRunStates(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'thread id required' }, 400);
    const thread = conversations.getThread(id);
    if (!thread) return c.json({ error: 'thread not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), thread.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const states = timedRouteSpan(
      ctx,
      'conversation.agent_run_states.snapshot',
      { conversationId: thread.id, kind: thread.kind },
      () => ctx.runtimeHost.getAgentRunStates(thread.id),
      result => ({ stateCount: result.length }),
    );
    return c.json({ states });
  });

  app.post(apiPaths.channelMessages(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'channel id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    if (!canPostToConversation(ctx, getUserPrincipal(c), channel.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = SendMessageInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    try {
      const result = await ctx.conversationMessageService.sendMessage({
        conversationId: channel.id,
        actor: getUserPrincipal(c),
        content: parsed.data.content,
        attachmentIds: parsed.data.attachment_ids ?? [],
      });
      return c.json({ message: result.message }, 201);
    } catch (err) {
      const mapped = conversationMessageServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  app.post(apiPaths.threadMessages(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'thread id required' }, 400);
    const thread = conversations.getThread(id);
    if (!thread) return c.json({ error: 'thread not found' }, 404);
    if (!canPostToConversation(ctx, getUserPrincipal(c), thread.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = SendMessageInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    try {
      const result = await ctx.conversationMessageService.sendMessage({
        conversationId: thread.id,
        actor: getUserPrincipal(c),
        content: parsed.data.content,
        attachmentIds: parsed.data.attachment_ids ?? [],
      });
      return c.json({ message: result.message }, 201);
    } catch (err) {
      const mapped = conversationMessageServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  app.post(apiPaths.channelMarkRead(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'channel id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), channel.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    return markConversationRead(channel.id, c);
  });

  app.post(apiPaths.threadMarkRead(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'thread id required' }, 400);
    const thread = conversations.getThread(id);
    if (!thread) return c.json({ error: 'thread not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), thread.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    return markConversationRead(thread.id, c);
  });

  app.get(apiPaths.channelThreads(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'channel id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    if (!canViewConversation(ctx, getUserPrincipal(c), channel.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const threads = timedRouteSpan(
      ctx,
      'conversation.threads.summary',
      { conversationId: channel.id, kind: channel.kind },
      () => conversations.listThreadSummariesForParent(channel.id),
      result => ({ threadCount: result.length }),
    );
    return c.json({ threads });
  });

  app.get(apiPaths.saved, c => c.json({ saved: saved.list() }));

  // ── Attachments ──────────────────────────────────────────────────
  // Upload via channel/thread context so the server resolves the access
  // boundary; clients never specify channel_id directly. Download is
  // by id — auth is bearer (renderer) or v1-permissive (TODO when DM
  // membership is enforceable per blob).

  app.post(apiPaths.channelAttachments(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'channel id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    const viewerUserId = requireViewerUserId(c);
    if (!canPostToConversation(ctx, getUserPrincipal(c), channel.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    return uploadAttachment(ctx, c, channel.id, 'user', viewerUserId);
  });

  app.post(apiPaths.threadAttachments(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'thread id required' }, 400);
    const thread = conversations.getThread(id);
    if (!thread) return c.json({ error: 'thread not found' }, 404);
    const viewerUserId = requireViewerUserId(c);
    if (!canPostToConversation(ctx, getUserPrincipal(c), thread.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    return uploadAttachment(ctx, c, thread.id, 'user', viewerUserId);
  });

  app.get(apiPaths.attachment(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'attachment id required' }, 400);
    const viewerUserId = requireViewerUserId(c);
    const principal = getUserPrincipal(c);
    const response = await ctx.attachmentService.serveForViewer(id, row =>
      canViewAttachment(ctx, principal, row),
    );
    if (!response) return c.json({ error: 'attachment not found' }, 404);
    return response;
  });

  app.delete(apiPaths.attachment(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'attachment id required' }, 400);
    const viewerUserId = requireViewerUserId(c);
    const attachment = attachments.getById(id);
    if (!attachment) return c.json({ error: 'attachment not found' }, 404);
    if (!canDeleteAttachment(ctx, getUserPrincipal(c), attachment)) {
      return c.json({ error: 'cannot delete attachment owned by another uploader' }, 403);
    }
    const result = await ctx.attachmentService.deleteOrphan(id, {
      kind: 'user',
      userId: viewerUserId,
    });
    if (result.ok) return c.body(null, 204);
    if (result.error === 'not_found') {
      return c.json({ error: 'attachment not found' }, 404);
    }
    if (result.error === 'forbidden') {
      return c.json({ error: 'cannot delete attachment owned by another uploader' }, 403);
    }
    // 'in_use' — at least one persisted message still references this id.
    return c.json(
      { error: 'attachment is referenced by a message and cannot be deleted' },
      409,
    );
  });

  app.post(apiPaths.messageSave(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'message id required' }, 400);
    const row = saved.save(id);
    if (!row) return c.json({ error: 'message not found' }, 404);
    // Notify the resolved viewer's private scope so the saved list
    // invalidates without polling. Pre-PR1 this was 'viewer:local';
    // routes now resolve the current viewer principal.
    const viewerUserId = requireViewerUserId(c);
    ctx.events.publishToScope(
      `viewer:${viewerUserId}`,
      'saved_changed',
      { message_id: id, action: 'save' },
    );
    return c.json({ saved: row }, 201);
  });

  app.delete(apiPaths.messageSave(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'message id required' }, 400);
    saved.remove(id);
    const viewerUserId = requireViewerUserId(c);
    ctx.events.publishToScope(
      `viewer:${viewerUserId}`,
      'saved_changed',
      { message_id: id, action: 'unsave' },
    );
    return c.body(null, 204);
  });

  app.get(apiPaths.search, c => {
    const parsed = SearchQueryInputSchema.safeParse(
      Object.fromEntries(new URL(c.req.url).searchParams.entries()),
    );
    if (!parsed.success) return c.json({ error: parsed.error.flatten() }, 400);
    // Legacy `messages.from_user_id IS NULL` rows predate the principal
    // middleware. The renderer treats them as authored by the current
    // viewer, so when the search filter targets the viewer we widen the
    // SQL to include NULLs. Done here (not in the repo) so SearchRepo
    // stays a pure SQL adapter without an implicit settings lookup.
    const viewerUserId = requireViewerUserId(c);
    const includeNullAuthor =
      parsed.data.user_id !== undefined && parsed.data.user_id === viewerUserId;
    const results = search.search(parsed.data, { includeNullAuthor });
    return c.json({ query: parsed.data, results });
  });

  app.post(apiPaths.messageThread(':id'), async c => {
    const messageId = c.req.param('id');
    if (!messageId) return c.json({ error: 'message id required' }, 400);
    const message = messages.getById(messageId);
    if (!message) return c.json({ error: 'message not found' }, 404);
    if (!canPostToConversation(ctx, getUserPrincipal(c), message.conversation_id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    try {
      const result = await ctx.threadService.createThreadFromMessage({
        messageId,
        actor: getUserPrincipal(c),
      });
      return c.json(
        { thread_id: result.thread.id, thread: result.thread, reused: result.reused },
        result.reused ? 200 : 201,
      );
    } catch (err) {
      const mapped = threadServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  app.post('/api/channels/:id/agents/:aid/start', async c => {
    const id = c.req.param('id');
    const aid = c.req.param('aid');
    if (!id || !aid) return c.json({ error: 'channel id and agent id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    return startConversationAgent(channel.id, aid, c);
  });

  app.post('/api/threads/:id/agents/:aid/start', async c => {
    const id = c.req.param('id');
    const aid = c.req.param('aid');
    if (!id || !aid) return c.json({ error: 'thread id and agent id required' }, 400);
    const thread = conversations.getThread(id);
    if (!thread) return c.json({ error: 'thread not found' }, 404);
    return startConversationAgent(thread.id, aid, c);
  });

  app.post('/api/channels/:id/agents/:aid/stop', async c => {
    const id = c.req.param('id');
    const aid = c.req.param('aid');
    if (!id || !aid) return c.json({ error: 'channel id and agent id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    return stopConversationAgent(channel.id, aid, c);
  });

  app.post('/api/threads/:id/agents/:aid/stop', async c => {
    const id = c.req.param('id');
    const aid = c.req.param('aid');
    if (!id || !aid) return c.json({ error: 'thread id and agent id required' }, 400);
    const thread = conversations.getThread(id);
    if (!thread) return c.json({ error: 'thread not found' }, 404);
    return stopConversationAgent(thread.id, aid, c);
  });

  app.post('/api/channels/:id/agents/:aid/clear-session', async c => {
    const id = c.req.param('id');
    const aid = c.req.param('aid');
    if (!id || !aid) return c.json({ error: 'channel id and agent id required' }, 400);
    const channel = conversations.getChannelLike(id);
    if (!channel) return c.json({ error: 'channel not found' }, 404);
    return clearConversationAgentSession(channel.id, aid, c);
  });

  app.post('/api/threads/:id/agents/:aid/clear-session', async c => {
    const id = c.req.param('id');
    const aid = c.req.param('aid');
    if (!id || !aid) return c.json({ error: 'thread id and agent id required' }, 400);
    const thread = conversations.getThread(id);
    if (!thread) return c.json({ error: 'thread not found' }, 404);
    return clearConversationAgentSession(thread.id, aid, c);
  });

  async function markConversationRead(
    conversationId: string,
    c: Context,
  ): Promise<Response> {
    const viewerUserId = requireViewerUserId(c);
    if (!canViewOwnedState(ctx, getUserPrincipal(c), viewerUserId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = MarkConversationReadInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const unreadBefore = conversationReads.unreadCountForConversation(
      conversationId,
      viewerUserId,
    );
    const row = conversationReads.markRead({
      viewerUserId,
      conversationId,
      seq: parsed.data.seq,
    });
    const unreadAfter =
      unreadBefore > 0
        ? conversationReads.unreadCountForConversation(conversationId, viewerUserId)
        : unreadBefore;
    // Notify viewer scope only when unread counters changed. Repeated
    // mark-read calls happen during focus/channel effects and should be no-ops.
    if (unreadAfter !== unreadBefore) {
      publishUnreadUpdated(ctx, conversationId, viewerUserId);
    }
    return c.json({ last_read_seq: row.last_read_seq });
  }

  async function startConversationAgent(
    conversationId: string,
    agentId: string,
    c: Context,
  ): Promise<Response> {
    if (!agents.get(agentId)) return c.json({ error: 'agent not found' }, 404);
    if (!conversations.getAgent(conversationId, agentId)) {
      return c.json({ error: 'agent is not a member of this conversation' }, 400);
    }
    if (!canPostToConversation(ctx, getUserPrincipal(c), conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    ctx.logger.info(
      { conversationId, agentId },
      'api: start agent requested',
    );
    try {
      await ctx.runtimeHost.start(conversationId, agentId);
      return c.json({ ok: true });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      ctx.logger.error(
        { conversationId, agentId, err },
        'api: start agent failed',
      );
      return c.json({ error: msg }, 400);
    }
  }

  async function stopConversationAgent(
    conversationId: string,
    agentId: string,
    c: Context,
  ): Promise<Response> {
    if (!canPostToConversation(ctx, getUserPrincipal(c), conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const body = await c.req.json().catch(() => ({}));
    const mode: 'soft' | 'hard' = body?.mode === 'soft' ? 'soft' : 'hard';
    const directive =
      typeof body?.directive === 'string' ? body.directive : undefined;
    ctx.logger.info(
      { conversationId, agentId, mode, hasDirective: !!directive },
      'api: stop agent requested',
    );
    await ctx.runtimeHost.stop(conversationId, agentId, mode, directive);
    return c.json({ ok: true });
  }

  async function clearConversationAgentSession(
    conversationId: string,
    agentId: string,
    c: Context,
  ): Promise<Response> {
    if (!agents.get(agentId)) return c.json({ error: 'agent not found' }, 404);
    if (!conversations.getAgent(conversationId, agentId)) {
      return c.json({ error: 'agent is not a member of this conversation' }, 400);
    }
    const principal = getUserPrincipal(c);
    if (!canPostToConversation(ctx, principal, conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    ctx.logger.info(
      { conversationId, agentId },
      'api: clear agent session requested',
    );
    try {
      const result = await ctx.runtimeHost.clearSession(
        conversationId,
        agentId,
        principal.userId,
      );
      return c.json({
        ok: true,
        prev_cli_session_id: result.prevCliSessionId,
        was_running: result.wasRunning,
      });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      ctx.logger.error(
        { conversationId, agentId, err },
        'api: clear agent session failed',
      );
      return c.json({ error: msg }, 400);
    }
  }

  function createAgentWithHome(
    input: CreateAgentInput,
    creatorUserId: string,
  ) {
    return ctx.db.transaction(() => {
      const agent = agents.create(input, localHost.id, creatorUserId);
      const agentL1Dir = join(ctx.silverhandDir, 'agents', agent.id);
      const home = workspaces.insertAgentHome({
        agent,
        workspacePath: agentL1Dir,
      });
      const dm = conversations.insertDmChannel({
        workspaceId: home.id,
        workspacePath: agentL1Dir,
        title: 'Onboarding',
        agentId: agent.id,
      });
      conversations.addAgent(dm.id, agent.id);

      // v25 human membership for agent_home + its DM channel. The
      // creator (the human principal that drove this route) is the
      // human side of this DM and the owner of the agent_home
      // surface, so they must be a member of both for the v25
      // permission gates to allow access. agent_home is explicitly
      // allowed for `workspace_users` (no v17-style trigger blocks
      // it), unlike `workspace_members` which is agent-only.
      //
      // Multi-human note: we use `creatorUserId` (passed in from the
      // route's `getUserPrincipal(c).userId`) rather than the global
      // `viewer_user_id` setting. v1 single-human these are always
      // the same; v2 multi-human a second human creating their own
      // agent must own its agent_home, not the install's primary
      // viewer.
      //
      // v26 made `creatorUserId` non-null (FK on agents table); the
      // user-gate middleware guarantees a user principal here, so
      // there is no defensive null path anymore.
      workspaces.addUserMember(home.id, creatorUserId);
      conversations.addUser(dm.id, creatorUserId);

      const onboardingMsg = messages.insertForConversation({
        conversationId: dm.id,
        fromKind: 'system',
        content: [
          `You (@${agent.name}) are now in your personal DM with the user.`,
          `Your role contract lives at L1 \`system_prompt.md\` and is shared across every workspace; updates to it propagate everywhere on the agent's next spawn. This DM also has its own L2 \`MEMORY.md\` / \`notes/\` for memory that stays here.`,
          `Send one short greeting: say hi by name, state your current global role briefly (or say it is not defined yet), and ask what role the user would like you to play. Write by scope: global identity / role / working-style guidance → update L1 \`system_prompt.md\`; DM-local notes → update this DM's \`MEMORY.md\` / \`notes/\`. Do not mention internal file paths to the user unless they ask.`,
        ].join('\n'),
        metadata: { subtype: systemSubtype('onboarding') },
      });
      return { agent, home, dm, onboardingMsg, agentL1Dir };
    })();
  }

  // ── Agents ───────────────────────────────────────────────────────
  app.get(apiPaths.agents, c => c.json({ agents: agents.list() }));

  app.post(apiPaths.agents, async c => {
    const body = await c.req.json().catch(() => ({}));
    const parsed = CreateAgentInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    if (agents.getByName(parsed.data.name)) {
      return c.json({ error: 'agent name already exists' }, 409);
    }
    // The user-gate middleware guarantees `principal.kind === 'user'`
    // on every public route, so `creatorUserId` is always non-null
    // here. v26 enforces this at the FK layer (`created_by_user_id
    // NOT NULL REFERENCES users(id)`).
    const principal = getUserPrincipal(c);
    const creatorUserId = principal.userId;
    const { agent, dm, onboardingMsg, agentL1Dir } = createAgentWithHome(
      parsed.data,
      creatorUserId,
    );
    await ensureAgentL1Dir(agentL1Dir);
    await ctx.runtimeHost.deliverMessage(dm.id, agent.id, onboardingMsg.id);
    ctx.runtimeHost
      .start(dm.id, agent.id)
      .catch(err =>
        ctx.logger.error(
          { conversationId: dm.id, agentId: agent.id, err },
          'dm onboarding spawn failed',
        ),
      );
    return c.json({ agent, dm_channel_id: dm.id }, 201);
  });

  app.get('/api/agents/:id', c => {
    const agent = agents.get(c.req.param('id'));
    if (!agent) return c.json({ error: 'agent not found' }, 404);
    return c.json({ agent });
  });

  // PATCH — partial update. `name` and `host_id` are intentionally not
  // patchable here: `name` is the immutable handle (renaming would
  // ripple through the agent_home workspace, every @mention in history,
  // every L2 file path), and host swap is out of v1 scope. Runtime change is
  // refused while any conversation_agent for this agent is starting/active
  // — otherwise the next event from the in-flight CLI would be parsed
  // by the wrong driver.
  //
  // Permission: `canEditAgent` (v26). The creator user — recorded on
  // `agents.created_by_user_id` at create time — and the agent acting
  // as itself are the only allowed editors. v1 single-human installs
  // see this as viewer-only (the viewer created every agent); v2
  // multi-human deployments will deny non-creator humans even if they
  // share the workspace.
  app.patch('/api/agents/:id', async c => {
    const id = c.req.param('id');
    const agent = agents.get(id);
    if (!agent) return c.json({ error: 'agent not found' }, 404);
    if (!canEditAgent(ctx, getUserPrincipal(c), id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = UpdateAgentInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    if (parsed.data.runtime && parsed.data.runtime !== agent.runtime) {
      const live = agents
        .listRuntimeConversations(id)
        .some(s => s.status === 'starting' || s.status === 'active');
      if (live) {
        return c.json(
          { error: 'agent_busy_change_runtime', detail: 'stop the agent before changing runtime' },
          409,
        );
      }
    }
    const updated = agents.update(id, parsed.data);
    return c.json({ agent: updated });
  });

  // Public avatar lifecycle. Mirrors `/api/me/avatar` but keyed by
  // agent id and gated by the v26 `canEditAgent` predicate (creator
  // user ∨ system on the public surface; agent-self is reachable only
  // via the internal route below). v1 single-human installs see this
  // as viewer-only; v2 multi-human deployments deny non-creator humans
  // even when they share the workspace.
  //
  // Read gate: any logged-in viewer who reaches the existing
  // `GET /api/agents/:id` can also fetch the avatar — there is no
  // hidden-profile concept in v1 (matches `GET /api/users/:id/avatar`).
  app.post(apiPaths.agentAvatar(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'agent id required' }, 400);
    if (!agents.get(id)) return c.json({ error: 'agent not found' }, 404);
    if (!canEditAgent(ctx, getUserPrincipal(c), id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    let form: FormData;
    try {
      form = await c.req.formData();
    } catch {
      return c.json({ error: 'multipart/form-data required' }, 400);
    }
    const file = form.get('file');
    if (!(file instanceof File)) {
      return c.json({ error: 'file field required' }, 400);
    }
    const buffer = Buffer.from(await file.arrayBuffer());
    try {
      const dto = await ctx.agentService.uploadAvatar(id, {
        buffer,
        contentType: file.type || 'application/octet-stream',
        filename: file.name || 'avatar',
      });
      return c.json({ avatar_url: dto.avatar_url, agent: dto });
    } catch (err) {
      const status = agentServiceErrorStatus(err);
      if (status) return c.json(agentServiceErrorBody(err), status);
      throw err;
    }
  });

  app.delete(apiPaths.agentAvatar(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'agent id required' }, 400);
    if (!agents.get(id)) return c.json({ error: 'agent not found' }, 404);
    if (!canEditAgent(ctx, getUserPrincipal(c), id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    try {
      const dto = await ctx.agentService.clearAvatar(id);
      return c.json({ agent: dto });
    } catch (err) {
      const status = agentServiceErrorStatus(err);
      if (status) return c.json(agentServiceErrorBody(err), status);
      throw err;
    }
  });

  // Public-by-id avatar fetch. Read gate matches GET /api/agents/:id
  // (authed + agent exists). Returns 404 when the agent has no avatar
  // — the renderer's `useAgentAvatarObjectUrls` hook treats 404 as
  // "fall back to the runtime icon".
  app.get(apiPaths.agentAvatar(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'agent id required' }, 400);
    if (!agents.get(id)) return c.json({ error: 'agent not found' }, 404);
    const ref = ctx.agentService.getAvatarStorageRef(id);
    if (!ref) return c.json({ error: 'avatar not found' }, 404);
    return ctx.blobStorage.serve(
      { storage: ctx.blobStorage.kind, storageKey: ref.storageKey },
      { contentType: ref.contentType, filename: ref.filename },
    );
  });

  app.get('/api/agents/:id/workspace', async c => {
    const agent = agents.get(c.req.param('id'));
    if (!agent) return c.json({ error: 'agent not found' }, 404);
    const l1Dir = join(ctx.silverhandDir, 'agents', agent.id);
    const workspace = await buildAgentWorkspaceDto(l1Dir);
    return c.json({ workspace });
  });

  app.get('/api/agents/:id/runtime-state', c => {
    const agent = agents.get(c.req.param('id'));
    if (!agent) return c.json({ error: 'agent not found' }, 404);
    return c.json({ conversation_agents: agents.listRuntimeConversations(agent.id) });
  });

  // Schedules — user-facing read & cancel for the per-agent panel.
  // Reuses ScheduleService (same code path the agent's MCP tools use)
  // so the data is identical to what the agent sees via list_schedules.
  // Permission posture matches the rest of /api/agents/:id/* — host
  // viewer can see/manage any agent on this host. When multi-user
  // permissioning lands these routes inherit it from the same gate.
  app.get(apiPaths.agentSchedules(':id'), c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'agent id required' }, 400);
    const agent = agents.get(id);
    if (!agent) return c.json({ error: 'agent not found' }, 404);
    const parsed = ScheduleListInputSchema.safeParse({
      status: c.req.query('status') || undefined,
      // limit comes through the URL as a string; coerce upfront so the
      // schema's `.int()` doesn't bounce a numeric string.
      limit: c.req.query('limit') ? Number(c.req.query('limit')) : undefined,
    });
    if (!parsed.success) {
      return c.json(
        { error: 'invalid query', issues: parsed.error.issues },
        400,
      );
    }
    const schedules = ctx.scheduleService.list(
      agent.id,
      parsed.data.status,
      parsed.data.limit,
    );
    return c.json({ schedules });
  });

  app.post(apiPaths.agentScheduleCancel(':id', ':scheduleId'), c => {
    const agentId = c.req.param('id');
    const scheduleId = c.req.param('scheduleId');
    if (!agentId || !scheduleId) {
      return c.json({ error: 'agent id and schedule id required' }, 400);
    }
    const agent = agents.get(agentId);
    if (!agent) return c.json({ error: 'agent not found' }, 404);
    try {
      const schedule = ctx.scheduleService.cancel(scheduleId, agent.id);
      // Audit hook (no separate audit infra in v1 — single-host model
      // means logger is the audit trail). Resolves the viewer via the
      // public principal middleware: an explicit principal header wins
      // over the registered `app_settings.viewer_user_id` lookup.
      ctx.logger.info(
        {
          user_id: resolveViewerUserId(c),
          agent_id: agent.id,
          schedule_id: scheduleId,
        },
        'agent schedule cancelled by user',
      );
      return c.json({ schedule });
    } catch (err) {
      const mapped = scheduleServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  // POST /stop — hard-stop every running conversation_agent for this
  // agent. Returns the count we actually attempted to stop. No-op
  // (200 with stopped: 0) when nothing is running.
  app.post('/api/agents/:id/stop', async c => {
    const id = c.req.param('id');
    const agent = agents.get(id);
    if (!agent) return c.json({ error: 'agent not found' }, 404);
    const live = agents
      .listRuntimeConversations(id)
      .filter(s => s.status === 'starting' || s.status === 'active');
    await Promise.all(
      live.map(s =>
        ctx.runtimeHost.stop(s.conversation_id, id, 'hard').catch(err =>
          ctx.logger.warn(
            { conversationId: s.conversation_id, agentId: id, err },
            'agent stop: conversation stop failed',
          ),
        ),
      ),
    );
    return c.json({ stopped: live.length });
  });

  // GET /delete-impact — drives the destructive-confirm dialog.
  app.get('/api/agents/:id/delete-impact', c => {
    const agent = agents.get(c.req.param('id'));
    if (!agent) return c.json({ error: 'agent not found' }, 404);
    return c.json({ impact: computeDeleteImpact(agent.id) });
  });

  // DELETE — full destructive flow. See docs/progress.md (this PR's
  // entry). Caller must pass `{ confirm: true }` so wallet-fingers
  // can't drop a heavy DM channel by accident from devtools.
  app.delete('/api/agents/:id', async c => {
    const id = c.req.param('id');
    const agent = agents.get(id);
    if (!agent) return c.body(null, 204); // idempotent on unknown id
    const body = await c.req.json().catch(() => ({}));
    const parsed = DeleteAgentInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json(
        { error: 'confirm flag required', detail: 'pass {"confirm": true}' },
        400,
      );
    }

    const impact = computeDeleteImpact(id);

    // 1. Hard-stop every running conversation_agent. Must precede any DB
    //    delete — once conversation_agents rows are gone, the runtime host
    //    can't look up handles by (conversation id, agent id).
    for (const live of impact.running_conversation_agents) {
      await ctx.runtimeHost
        .stop(live.conversation_id, id, 'hard')
        .catch(err =>
          ctx.logger.warn(
            { conversationId: live.conversation_id, agentId: id, err },
            'delete: stop failed (continuing)',
          ),
        );
    }

    // 2. Remove from every regular workspace and its conversations.
    for (const ws of impact.regular_memberships) {
      const workspace = workspaces.get(ws.id);
      if (!workspace) continue;
      for (const conversation of conversations.list()) {
        if (conversation.workspace_id === workspace.id) {
          conversations.removeAgent(conversation.id, id);
        }
      }
      workspaces.removeMember(workspace.id, id);
    }

    // 3. Sweep DM blobs before deleting the agent. DB rows cascade through
    //    agent -> agent_home -> conversations, but physical files need an
    //    explicit pass. Thread uploads are channel-scoped to the dm channel.
    const dmChannels = impact.owned_home
      ? conversations.listDmChannelsForWorkspace(impact.owned_home.id)
      : [];
    for (const dm of dmChannels) {
      await ctx.attachmentService.sweepForChannel(dm.id).catch(err =>
        ctx.logger.warn(
          { agentId: id, conversationId: dm.id, err },
          'delete agent: DM attachment sweep failed (continuing)',
        ),
      );
    }

    // 4. Drop the agent. owner_agent_id ON DELETE CASCADE removes its
    //    agent_home subtree; regular conversation history keeps deleted
    //    sender rows via SET NULL on messages.from_agent_id.
    agents.delete(id);

    // 5. Wipe L1 — the agent's global memory/notes/system_prompt.
    await fs
      .rm(impact.l1_path, { recursive: true, force: true })
      .catch(err =>
        ctx.logger.warn({ agentId: id, l1: impact.l1_path, err }, 'delete: L1 rm failed'),
      );
    await Promise.all(
      impact.l2_paths.map(p =>
        fs.rm(p, { recursive: true, force: true }).catch(err =>
          ctx.logger.warn({ agentId: id, path: p, err }, 'delete: L2 rm failed'),
        ),
      ),
    );

    ctx.logger.info(
      {
        agentId: id,
        agentName: agent.name,
        dmChannelId: impact.dm_channel?.id ?? null,
        regularWorkspaces: impact.regular_memberships.length,
        stoppedConversations: impact.running_conversation_agents.length,
      },
      'agent deleted',
    );
    return c.body(null, 204);
  });

  /**
   * Build the AgentDeleteImpactDto from current DB + filesystem
   * state. Pure read — used both by the GET preview endpoint and by
   * the DELETE handler so the two stay in lock-step.
   */
  function computeDeleteImpact(agentId: string): AgentDeleteImpactDto {
    const agent = agents.get(agentId);
    if (!agent) throw new Error('agent not found'); // guarded by callers
    const ownedHome = workspaces.getAgentHomeByOwner(agentId);
    const regulars = workspaces
      .listForAgent(agentId)
      .filter(w => w.workspace_kind === 'regular');
    const dmChannels = ownedHome
      ? conversations.listDmChannelsForWorkspace(ownedHome.id)
      : conversations.listDmChannelsForAgent(agentId);
    const dm = dmChannels[0] ?? null;

    let dmChannel: AgentDeleteImpactDto['dm_channel'] = null;
    let attachmentsCount = 0;
    if (dm) {
      const rootIds = dmChannels.map(c => c.id);
      const threadIds = rootIds.flatMap(channelId =>
        conversations.listThreadsForChannel(channelId).map(t => t.id),
      );
      const ids = [...rootIds, ...threadIds];
      const placeholders = ids.map(() => '?').join(',');
      const messageCount =
        ids.length === 0
          ? 0
          : ctx.db
              .prepare<string[], { c: number }>(
	                `SELECT COUNT(*) AS c
	                   FROM messages
	                  WHERE conversation_id IN (${placeholders})`,
	              )
              .get(...ids)?.c ?? 0;
      attachmentsCount =
        ctx.db
          .prepare<string[], { c: number }>(
            `SELECT COUNT(*) AS c
               FROM attachments
              WHERE channel_id IN (${rootIds.map(() => '?').join(',')})`,
          )
          .get(...rootIds)?.c ?? 0;
      dmChannel = {
        id: dm.id,
        name: dm.title,
        conversation_count: ids.length,
        message_count: messageCount,
      };
    }

    const runtimeConversations = agents.listRuntimeConversations(agentId);
    const running = runtimeConversations
      .filter(s => s.status === 'starting' || s.status === 'active')

    const l1Path = join(ctx.silverhandDir, 'agents', agentId);
    const l2Paths = regulars.map(c =>
      join(c.workspace_path, '.silverhand', 'agents', agentId),
    );

    return {
      agent_id: agent.id,
      agent_name: agent.name,
      agent_display_name: agent.display_name,
      dm_channel: dmChannel,
      regular_workspaces: regulars.map(c => ({ id: c.id, name: c.name })),
      regular_memberships: regulars.map(c => ({ id: c.id, name: c.name })),
      owned_home: ownedHome,
      attachments_count: attachmentsCount,
      running_conversation_agents: running,
      pending_inbox_count: conversationInboxes.countPendingForAgent(agentId),
      l1_path: l1Path,
      l2_paths: l2Paths,
    };
  }

  // Old HTTP route aliases were removed in the hard cutover.

  // ── Host-wide settings ───────────────────────────────────────────
  // Flat KV. Currently exposes `proxy_url`. Empty string deletes the row.
  app.get(apiPaths.settings, c => c.json({ settings: settings.all() }));

  app.put(apiPaths.settings, async c => {
    const body = await c.req.json().catch(() => null);
    if (!body || typeof body !== 'object') {
      return c.json({ error: 'invalid input' }, 400);
    }
    const allowed = new Set<string>(Object.values(SETTINGS_KEYS));
    for (const [k, v] of Object.entries(body as Record<string, unknown>)) {
      if (!allowed.has(k)) {
        return c.json({ error: `unknown settings key: ${k}` }, 400);
      }
      if (typeof v !== 'string') {
        return c.json({ error: `value for ${k} must be a string` }, 400);
      }
      settings.set(k, v);
    }
    return c.json({ settings: settings.all() });
  });

  // ── Current viewer (user profile) ────────────────────────────────
  // GET /api/me is the renderer's first read on boot. It's whitelisted
  // by the registration gate so it works *before* `POST /api/register`
  // — in that state it returns `{ registration_required: true,
  // suggested_name }` so the renderer can mount `<RegistrationGate />`.
  // Once the viewer is registered the principal middleware sets a
  // user principal and this returns the resolved UserDto.
  //
  // Boundary contract:
  //   - POST /api/register is the SOLE creator of `users` rows. Called
  //     once per machine in V1 (per signup in V2); 409 on a second
  //     call. Whitelisted by the gate so the renderer can reach it
  //     pre-registration.
  //   - PATCH /api/me only accepts `display_name`. Renaming is not
  //     exposed at any service / route surface.
  //   - Avatar lives on its own routes (POST/DELETE /api/me/avatar)
  app.get(apiPaths.me, c => {
    // Whitelisted route — principal may be absent in the
    // registration-required state. Branch on its presence rather than
    // calling `requireViewerUserId` (which throws).
    const principal = c.get('principal');
    if (!principal || principal.kind !== 'user') {
      return c.json({
        registration_required: true,
        suggested_name: ctx.userService.suggestRegistrationName(),
      });
    }
    const dto = ctx.userService.getDto(principal.userId);
    if (!dto) {
      // Principal middleware just validated this; corruption only.
      return c.json({ error: 'viewer user is missing post-validation' }, 500);
    }
    return c.json(dto);
  });

  app.post(apiPaths.register, async c => {
    // Whitelisted route — also runs in the registration-required state.
    // Service layer is the source of truth for "already registered"
    // (it checks `app_settings.viewer_user_id`); we don't need a
    // principal here.
    const body = await c.req.json().catch(() => null);
    const parsed = RegisterUserInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json(
        { error: parsed.error.errors[0]?.message ?? 'invalid input' },
        400,
      );
    }
    try {
      const dto = ctx.userService.register(parsed.data);
      return c.json(dto);
    } catch (err) {
      const status = userServiceErrorStatus(err);
      if (status) return c.json(userServiceErrorBody(err), status);
      throw err;
    }
  });
  //     so we have a single binary upload path the renderer can call
  //     after registration succeeds.
  //
  // V2 multi-user keeps the same endpoints — only the principal
  // resolution moves from `app_settings` to session/token claims, and
  // the registration call lands after auth signup instead of on
  // first launch.

  app.patch(apiPaths.me, async c => {
    const viewerUserId = requireViewerUserId(c);
    const body = await c.req.json().catch(() => null);
    const parsed = UpdateUserProfileInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json(
        { error: parsed.error.errors[0]?.message ?? 'invalid input' },
        400,
      );
    }
    try {
      const dto = ctx.userService.updateProfile(viewerUserId, parsed.data);
      return c.json(dto);
    } catch (err) {
      const status = userServiceErrorStatus(err);
      if (status) return c.json(userServiceErrorBody(err), status);
      throw err;
    }
  });

  app.post(apiPaths.meAvatar, async c => {
    const viewerUserId = requireViewerUserId(c);
    let form: FormData;
    try {
      form = await c.req.formData();
    } catch {
      return c.json({ error: 'multipart/form-data required' }, 400);
    }
    const file = form.get('file');
    if (!(file instanceof File)) {
      return c.json({ error: 'file field required' }, 400);
    }
    const buffer = Buffer.from(await file.arrayBuffer());
    try {
      const dto = await ctx.userService.uploadAvatar(viewerUserId, {
        buffer,
        contentType: file.type || 'application/octet-stream',
        filename: file.name || 'avatar',
      });
      return c.json({ avatar_url: dto.avatar_url, user: dto });
    } catch (err) {
      const status = userServiceErrorStatus(err);
      if (status) return c.json(userServiceErrorBody(err), status);
      throw err;
    }
  });

  app.delete(apiPaths.meAvatar, async c => {
    const viewerUserId = requireViewerUserId(c);
    try {
      const dto = await ctx.userService.clearAvatar(viewerUserId);
      return c.json(dto);
    } catch (err) {
      const status = userServiceErrorStatus(err);
      if (status) return c.json(userServiceErrorBody(err), status);
      throw err;
    }
  });

  // Public-by-id avatar fetch. The renderer never assembles storage
  // keys; it just GETs this URL with the standard auth headers and
  // turns the bytes into an object URL (mirrors the attachment
  // download pattern). Any logged-in viewer can fetch any user's
  // avatar — there is no hidden-profile concept in V1.
  app.get(apiPaths.userAvatar(':id'), async c => {
    const id = c.req.param('id');
    if (!id) return c.json({ error: 'user id required' }, 400);
    const ref = ctx.userService.getAvatarStorageRef(id);
    if (!ref) return c.json({ error: 'avatar not found' }, 404);
    return ctx.blobStorage.serve(
      { storage: ctx.blobStorage.kind, storageKey: ref.storageKey },
      { contentType: ref.contentType, filename: ref.filename },
    );
  });

  // ── Notifications ────────────────────────────────────────────────
  // v1 single-user. user_id is always NULL on writes; reads filter on
  // `user_id IS NULL` (see NotificationRepo.list). Multi-user migration
  // will pass through a user id resolved from auth.
  app.get(apiPaths.notifications, c => {
    const unread = c.req.query('unread') === '1';
    const limit = parseLimit(c.req.query('limit'), 100);
    return c.json({
      notifications: notifications.list({ unread, limit }),
    });
  });

  // Lightweight endpoint: total unread + per-sidebar-scope counts + the
  // most recent unread pointer so the renderer can jump straight there.
  app.get(apiPaths.notificationsUnread, c => {
    const total = timedRouteSpan(
      ctx,
      'notifications.unreadCount',
      {},
      () => notifications.unreadCount(),
      result => ({ total: result }),
    );
    const byWorkspace = Object.fromEntries(
      timedRouteSpan(
        ctx,
        'notifications.unreadCountsByWorkspace',
        {},
        () => notifications.unreadCountsByWorkspace(),
        result => ({ workspaceCount: result.size }),
      ).entries(),
    );
    const latestByWorkspace = Object.fromEntries(
      timedRouteSpan(
        ctx,
        'notifications.latestUnreadByWorkspace',
        {},
        () => notifications.latestUnreadByWorkspace(),
        result => ({ workspaceCount: result.size }),
      ).entries(),
    );
    return c.json({
      total,
      by_workspace: byWorkspace,
      latest_by_workspace: latestByWorkspace,
    });
  });

  // Mark-read with three modes: by id, by conversation/workspace, or all. Validation
  // mirrors `MarkNotificationsReadInputSchema` in shared/dto.ts.
  app.post(apiPaths.notificationsRead, async c => {
    const body = await c.req.json().catch(() => ({}));
    const parsed = MarkNotificationsReadInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const { id, workspace_id, conversation_id, all } = parsed.data;
    let marked = 0;
    if (id) {
      const ok = notifications.markRead(id);
      marked = ok ? 1 : 0;
    } else if (workspace_id) {
      marked = notifications.markWorkspaceRead(workspace_id);
    } else if (conversation_id) {
      marked = notifications.markConversationRead(conversation_id);
    } else if (all) {
      marked = notifications.markAllRead();
    } else {
      // Unreachable thanks to the schema refine, but TS doesn't narrow.
      return c.json({ error: 'invalid input' }, 400);
    }
    // Push to the resolved viewer's private scope only when unread
    // notification counters changed.
    if (marked > 0) {
      const viewerUserId = requireViewerUserId(c);
      ctx.events.publishToScope(
        `viewer:${viewerUserId}`,
        'unread_updated',
        { marked },
      );
    }
    return c.json({ marked });
  });

  // ── Tasks (human-facing list + overrides) ────────────────────────
  app.get(apiPaths.tasks, c => {
    const statusValues = (c.req.queries('status') ?? []).filter(s => s !== 'all');
    const taskNumberValues = c.req.queries('task_number') ?? [];
    const parsed = ListTasksQuerySchema.safeParse({
      workspace_id: c.req.query('workspace_id') || undefined,
      conversation_id: c.req.query('conversation_id') || undefined,
      status: statusValues.length > 0 ? statusValues : undefined,
      task_number: taskNumberValues.length > 0 ? taskNumberValues : undefined,
      assignee_id: c.req.query('assignee_id') || undefined,
      limit: c.req.query('limit') || undefined,
    });
    if (!parsed.success) {
      return c.json({ error: 'invalid query', issues: parsed.error.issues }, 400);
    }
    return c.json({
      tasks: ctx.taskService.list({
        actor: getUserPrincipal(c),
        workspaceId: parsed.data.workspace_id,
        conversationId: parsed.data.conversation_id,
        status: parsed.data.status,
        taskNumbers: parsed.data.task_number,
        assigneeAgentId: parsed.data.assignee_id,
        limit: parsed.data.limit,
      }),
    });
  });

  app.post('/api/tasks/:messageId/claim', async c => {
    const body = await c.req.json().catch(() => ({}));
    const parsed = HumanClaimTaskInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const result = await ctx.taskService.claimTask({
      actor: getUserPrincipal(c),
      by: { messageId: c.req.param('messageId') },
      assignee: parsed.data.agent_id ? { agentId: parsed.data.agent_id } : undefined,
    });
    return c.json(result, result.success ? 200 : 409);
  });

  app.post('/api/tasks/:messageId/unclaim', async c => {
    const result = await ctx.taskService.unclaimTask({
      actor: getUserPrincipal(c),
      by: { messageId: c.req.param('messageId') },
    });
    return c.json(result, result.success ? 200 : 409);
  });

  app.post('/api/tasks/:messageId/status', async c => {
    const body = await c.req.json().catch(() => ({}));
    const parsed = HumanUpdateTaskStatusInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const result = await ctx.taskService.updateStatus({
      actor: getUserPrincipal(c),
      by: { messageId: c.req.param('messageId') },
      status: parsed.data.status,
    });
    return c.json(result, result.success ? 200 : 409);
  });

  return app;
}

function parseLimit(raw: string | undefined, fallback: number): number {
  if (!raw) return fallback;
  const n = Number.parseInt(raw, 10);
  if (!Number.isFinite(n) || n <= 0) return fallback;
  return Math.min(n, 500);
}

function parseBeforeSeq(raw: string | undefined): number | null | 'invalid' {
  if (!raw) return null;
  const trimmed = raw.trim();
  if (!/^\d+$/.test(trimmed)) return 'invalid';
  const n = Number.parseInt(trimmed, 10);
  if (!Number.isFinite(n) || n <= 0) return 'invalid';
  return n;
}

const READ_HISTORY_SNIPPET_MAX_CHARS = 160;
const SEARCH_EXCERPT_CONTEXT_CHARS = 80;

type ConversationDto = NonNullable<ReturnType<ConversationRepo['get']>>;

type HistoryMessageRow = MessageRow & {
  task_assignee_name: string | null;
  sender_agent_name: string | null;
};

type AgentSearchMessageRow = HistoryMessageRow & {
  workspace_id: string;
  conversation_kind: ConversationDto['kind'];
  channel_id: string;
  thread_id: string | null;
};

type ReadHistoryTargetResult =
  | { ok: true; conversation: ConversationDto }
  | {
      ok: false;
      status: 400 | 403 | 404;
      code: string;
      message: string;
    };

function resolveAgentReadHistoryTarget(
  conversations: ConversationRepo,
  sourceConversation: ConversationDto,
  agentId: string,
  target: ReadHistoryTarget | undefined,
): ReadHistoryTargetResult {
  const requested = target ?? 'self';
  let resolved: ConversationDto | null;
  if (requested === 'self') {
    resolved = sourceConversation;
  } else if (requested === 'parent') {
    if (!sourceConversation.parent_conversation_id) {
      return {
        ok: false,
        status: 400,
        code: 'no_parent',
        message: 'no parent channel',
      };
    }
    resolved = conversations.get(sourceConversation.parent_conversation_id);
    if (!resolved) {
      return {
        ok: false,
        status: 404,
        code: 'parent_not_found',
        message: 'parent channel not found',
      };
    }
  } else if ('thread_id' in requested) {
    const thread = conversations.getThread(requested.thread_id);
    if (!thread) {
      return {
        ok: false,
        status: 404,
        code: 'thread_not_found',
        message: 'thread not found',
      };
    }
    if (thread.parent_conversation_id !== sourceConversation.id) {
      return {
        ok: false,
        status: 400,
        code: 'thread_not_child',
        message: 'target is not a direct thread of current channel',
      };
    }
    resolved = thread;
  } else {
    // Cross-conversation read by id. The agent must be in
    // `conversation_agents` for the target — workspace-level
    // membership alone is not enough; channel/thread membership is
    // the bottom guard. The check below covers it uniformly.
    const conversation = conversations.get(requested.conversation_id);
    if (!conversation) {
      return {
        ok: false,
        status: 404,
        code: 'conversation_not_found',
        message: 'conversation not found',
      };
    }
    if (
      conversation.kind !== 'channel' &&
      conversation.kind !== 'dm_channel' &&
      conversation.kind !== 'thread'
    ) {
      return {
        ok: false,
        status: 400,
        code: 'unsupported_conversation_kind',
        message: 'target conversation kind is not readable',
      };
    }
    resolved = conversation;
  }

  if (!conversations.getAgent(resolved.id, agentId)) {
    return {
      ok: false,
      status: 403,
      code: 'not_target_conversation_member',
      message: 'agent is not a member of target conversation',
    };
  }
  return { ok: true, conversation: resolved };
}

function targetRef(conversation: ConversationDto): ReadHistoryTargetRef {
  return {
    conversation_id: conversation.id,
    conversation_kind: conversation.kind,
    channel_id:
      conversation.kind === 'thread'
        ? conversation.parent_conversation_id ?? conversation.id
        : conversation.id,
    thread_id: conversation.kind === 'thread' ? conversation.id : null,
    workspace_id: conversation.workspace_id,
    title: conversation.title,
  };
}

function messageRef(row: MessageRow, target: ReadHistoryTargetRef): MessageRef {
  return {
    id: row.id,
    workspace_id: target.workspace_id,
    conversation_id: target.conversation_id,
    conversation_kind: target.conversation_kind,
    channel_id: target.channel_id,
    thread_id: target.thread_id,
    seq: row.seq,
    created_at: row.created_at,
  };
}

function senderRef(row: HistoryMessageRow): MessageSenderRef {
  return {
    kind: row.from_kind,
    agent_id: row.from_agent_id,
    user_id: row.from_user_id,
    display_name:
      row.from_kind === 'agent'
        ? row.sender_agent_name ?? row.from_agent_id
        : row.from_kind === 'user'
          ? row.from_user_id
          : null,
  };
}

function snippetFor(content: string): string {
  const compact = content.replace(/\s+/g, ' ').trim();
  if (compact.length <= READ_HISTORY_SNIPPET_MAX_CHARS) return compact;
  return `${compact.slice(0, READ_HISTORY_SNIPPET_MAX_CHARS - 1)}…`;
}

function escapeLike(value: string): string {
  return value.replace(/[\\%_]/g, ch => `\\${ch}`);
}

function likeNeedle(query: string): string {
  return `%${escapeLike(query)}%`;
}

function historyRowToMessage(
  row: HistoryMessageRow,
  target: ReadHistoryTargetRef,
  includeContent: boolean,
): ReadHistoryMessage {
  const dto = messageRowToDto(row, { assigneeName: row.task_assignee_name });
  return {
    message_ref: messageRef(row, target),
    sender: senderRef(row),
    snippet: snippetFor(dto.content),
    metadata: dto.metadata,
    attachments: dto.attachments,
    ...(includeContent ? { content: dto.content } : {}),
  };
}

function historySystemFilter(includeSystem: boolean): string {
  return includeSystem ? '' : `AND m.from_kind != 'system'`;
}

function historySelectSql(whereSql: string, orderSql: string, limitSql = 'LIMIT ?'): string {
  return `SELECT
            m.*,
            assignee.display_name AS task_assignee_name,
            sender.display_name AS sender_agent_name
          FROM messages m
          LEFT JOIN agents assignee ON assignee.id = m.task_assignee_agent_id
          LEFT JOIN agents sender ON sender.id = m.from_agent_id
         WHERE ${whereSql}
         ${orderSql}
         ${limitSql}`;
}

function listHistoryBefore(
  ctx: BackendContext,
  conversationId: string,
  beforeSeq: number,
  includeSystem: boolean,
  limit: number,
): HistoryMessageRow[] {
  if (limit <= 0) return [];
  const rows = ctx.db
    .prepare<[string, number, number], HistoryMessageRow>(
      `SELECT *
         FROM (
           ${historySelectSql(
             `m.conversation_id = ?
              AND m.seq < ?
              ${historySystemFilter(includeSystem)}`,
             'ORDER BY m.seq DESC',
           )}
         )
        ORDER BY seq ASC`,
    )
    .all(conversationId, beforeSeq, limit);
  return rows;
}

function listHistoryAfter(
  ctx: BackendContext,
  conversationId: string,
  afterSeq: number,
  includeSystem: boolean,
  limit: number,
): HistoryMessageRow[] {
  if (limit <= 0) return [];
  return ctx.db
    .prepare<[string, number, number], HistoryMessageRow>(
      historySelectSql(
        `m.conversation_id = ?
         AND m.seq > ?
         ${historySystemFilter(includeSystem)}`,
        'ORDER BY m.seq ASC',
      ),
    )
    .all(conversationId, afterSeq, limit);
}

function getHistoryAnchorSeq(
  ctx: BackendContext,
  conversationId: string,
  messageId: string,
): number | null {
  const row = ctx.db
    .prepare<[string, string], { seq: number }>(
      `SELECT seq
         FROM messages
        WHERE id = ?
          AND conversation_id = ?`,
    )
    .get(messageId, conversationId);
  return row?.seq ?? null;
}

function getHistoryRowById(
  ctx: BackendContext,
  conversationId: string,
  messageId: string,
  includeSystem: boolean,
): HistoryMessageRow | null {
  const row = ctx.db
    .prepare<[string, string], HistoryMessageRow>(
      historySelectSql(
        `m.id = ?
         AND m.conversation_id = ?
         ${historySystemFilter(includeSystem)}`,
        '',
        '',
      ),
    )
    .get(messageId, conversationId);
  return row ?? null;
}

function hasHistoryBefore(
  ctx: BackendContext,
  conversationId: string,
  seq: number,
  includeSystem: boolean,
): boolean {
  return (
    ctx.db
      .prepare<[string, number], { found: 1 }>(
        `SELECT 1 AS found
           FROM messages m
          WHERE m.conversation_id = ?
            AND m.seq < ?
            ${historySystemFilter(includeSystem)}
          LIMIT 1`,
      )
      .get(conversationId, seq) !== undefined
  );
}

function hasHistoryAfter(
  ctx: BackendContext,
  conversationId: string,
  seq: number,
  includeSystem: boolean,
): boolean {
  return (
    ctx.db
      .prepare<[string, number], { found: 1 }>(
        `SELECT 1 AS found
           FROM messages m
          WHERE m.conversation_id = ?
            AND m.seq > ?
            ${historySystemFilter(includeSystem)}
          LIMIT 1`,
      )
      .get(conversationId, seq) !== undefined
  );
}

function buildReadHistoryDto(
  ctx: BackendContext,
  targetConversation: ConversationDto,
  input: {
    before_seq?: number;
    after_seq?: number;
    around_message_id?: string;
    include_content: boolean;
    include_system: boolean;
    limit: number;
  },
): AgentReadHistoryDto | { error: string; code: string; status: 404 } {
  const target = targetRef(targetConversation);
  let rows: HistoryMessageRow[];
  let anchor: AgentReadHistoryDto['anchor'];

  if (input.before_seq !== undefined) {
    anchor = { type: 'before_seq', seq: input.before_seq };
    rows = listHistoryBefore(
      ctx,
      targetConversation.id,
      input.before_seq,
      input.include_system,
      input.limit,
    );
  } else if (input.after_seq !== undefined) {
    anchor = { type: 'after_seq', seq: input.after_seq };
    rows = listHistoryAfter(
      ctx,
      targetConversation.id,
      input.after_seq,
      input.include_system,
      input.limit,
    );
  } else {
    const messageId = input.around_message_id!;
    const anchorSeq = getHistoryAnchorSeq(ctx, targetConversation.id, messageId);
    if (anchorSeq === null) {
      return {
        error: 'message not found',
        code: 'message_not_found',
        status: 404,
      };
    }
    anchor = { type: 'around_message_id', message_id: messageId };
    const beforeCount = Math.floor((input.limit - 1) / 2);
    const before = listHistoryBefore(
      ctx,
      targetConversation.id,
      anchorSeq,
      input.include_system,
      beforeCount,
    );
    const anchorRow = getHistoryRowById(
      ctx,
      targetConversation.id,
      messageId,
      input.include_system,
    );
    const afterLimit = Math.max(
      0,
      input.limit - before.length - (anchorRow ? 1 : 0),
    );
    const after = listHistoryAfter(
      ctx,
      targetConversation.id,
      anchorSeq,
      input.include_system,
      afterLimit,
    );
    rows = [...before, ...(anchorRow ? [anchorRow] : []), ...after];
  }

  const minSeq = rows[0]?.seq ?? null;
  const maxSeq = rows.at(-1)?.seq ?? null;
  return {
    target,
    anchor,
    limit: input.limit,
    include_content: input.include_content,
    include_system: input.include_system,
    messages: rows.map(row =>
      historyRowToMessage(row, target, input.include_content),
    ),
    has_more_before:
      minSeq === null
        ? false
        : hasHistoryBefore(
            ctx,
            targetConversation.id,
            minSeq,
            input.include_system,
          ),
    has_more_after:
      maxSeq === null
        ? false
        : hasHistoryAfter(
            ctx,
            targetConversation.id,
            maxSeq,
            input.include_system,
          ),
  };
}

function listAgentVisibleChannels(
  ctx: BackendContext,
  workspaceId: string,
  agentId: string,
): AgentListChannelsDto {
  const rows = ctx.db
    .prepare<[string, string], ConversationDto>(
      `SELECT c.*
         FROM conversations c
         JOIN conversation_agents ca
           ON ca.conversation_id = c.id
          AND ca.agent_id = ?
        WHERE c.workspace_id = ?
          AND c.kind IN ('channel', 'dm_channel')
        ORDER BY (c.archived_at IS NOT NULL) ASC,
                 (c.pinned_at IS NOT NULL) DESC,
                 c.pinned_at ASC,
                 c.updated_at DESC`,
    )
    .all(agentId, workspaceId);
  return {
    workspace_id: workspaceId,
    channels: rows.map(
      (row): AgentListedChannel => ({
        id: row.id,
        title: row.title,
        kind: row.kind === 'dm_channel' ? 'dm_channel' : 'channel',
        archived: row.archived_at !== null,
        updated_at: row.updated_at,
      }),
    ),
  };
}

/**
 * Build the cross-workspace `list_workspaces` payload for the calling
 * agent. The workspace list is the canonical
 * `WorkspaceRepo.listForAgent` (regular ∪ owned agent_home); per-row
 * `agent_dir` mirrors `activeAgentProfileDir` — for `agent_home` the
 * workspace root is itself the agent's home, for `regular` we point
 * at the nested L2 profile dir.
 */
function buildAgentListWorkspacesDto(
  ctx: BackendContext,
  agentId: string,
): AgentListWorkspacesDto {
  const workspaces = new WorkspaceRepo(ctx.db, {
    workspacesRoot: ctx.workspacesRoot,
  }).listForAgent(agentId);
  return {
    workspaces: workspaces.map((w): AgentListedWorkspace => {
      const agentDir =
        w.workspace_kind === 'agent_home'
          ? w.workspace_path
          : join(w.workspace_path, '.silverhand', 'agents', agentId);
      return {
        id: w.id,
        name: w.name,
        kind: w.workspace_kind,
        archived: w.archived_at !== null,
        updated_at: w.updated_at,
        agent_dir: agentDir,
      };
    }),
  };
}

function searchMessageRef(row: AgentSearchMessageRow): MessageRef {
  return {
    id: row.id,
    workspace_id: row.workspace_id,
    conversation_id: row.conversation_id,
    conversation_kind: row.conversation_kind,
    channel_id: row.channel_id,
    thread_id: row.thread_id,
    seq: row.seq,
    created_at: row.created_at,
  };
}

function searchExcerptFor(
  content: string,
  query: string,
): { excerpt: string; match_offset: number } {
  const compact = content.replace(/\s+/g, ' ').trim();
  const lower = compact.toLocaleLowerCase();
  const lowerQuery = query.toLocaleLowerCase();
  const matchStart = lower.indexOf(lowerQuery);
  if (matchStart < 0) {
    const excerpt = compact.slice(0, SEARCH_EXCERPT_CONTEXT_CHARS * 2);
    return { excerpt, match_offset: 0 };
  }
  const start = Math.max(0, matchStart - SEARCH_EXCERPT_CONTEXT_CHARS);
  const end = Math.min(
    compact.length,
    matchStart + query.length + SEARCH_EXCERPT_CONTEXT_CHARS,
  );
  const prefix = start > 0 ? '...' : '';
  const suffix = end < compact.length ? '...' : '';
  return {
    excerpt: `${prefix}${compact.slice(start, end)}${suffix}`,
    match_offset: matchStart - start + prefix.length,
  };
}

type SearchMessagesResult =
  | { ok: true; dto: AgentSearchMessagesDto }
  | {
      ok: false;
      status: 400 | 403 | 404;
      code: string;
      message: string;
    };

function buildAgentSearchMessagesDto(
  ctx: BackendContext,
  conversations: ConversationRepo,
  sourceConversation: ConversationDto,
  agentId: string,
  input: {
    q: string;
    scope: 'current' | 'workspace' | 'channel';
    workspace_id?: string;
    channel_id?: string;
    include_system: boolean;
    limit: number;
  },
): SearchMessagesResult {
  const where: string[] = [`m.content LIKE ? ESCAPE '\\'`];
  const params: Array<string | number> = [likeNeedle(input.q)];
  let channelId: string | null = null;
  let workspaceId: string | null = null;

  if (!input.include_system) {
    where.push(`m.from_kind != 'system'`);
  }

  if (input.scope === 'current') {
    where.push(`conv.id = ?`);
    params.push(sourceConversation.id);
  } else if (input.scope === 'workspace') {
    // Cross-workspace handled by the route before we get here:
    // `canAgentAccessWorkspace` already gated `input.workspace_id`
    // when it differs from `sourceConversation.workspace_id`. Here we
    // just pick the SQL filter; the conversation_agents JOIN below
    // remains the bottom guard so unjoined channels in the target
    // workspace stay invisible.
    workspaceId = input.workspace_id ?? sourceConversation.workspace_id;
    where.push(`conv.workspace_id = ?`);
    params.push(workspaceId);
  } else {
    channelId = input.channel_id ?? null;
    if (!channelId) {
      return {
        ok: false,
        status: 400,
        code: 'channel_id_required',
        message: 'channel_id is required for channel scope',
      };
    }
    const channel = conversations.getChannelLike(channelId);
    if (!channel) {
      return {
        ok: false,
        status: 404,
        code: 'channel_not_found',
        message: 'channel not found',
      };
    }
    // channel_id is globally unique. PR-B made list_channels return
    // channels from any workspace the agent has access to, so the
    // bottom guard is conversation-membership, not source-workspace
    // identity. The SQL `JOIN conversation_agents` below still
    // filters child threads by per-conversation membership.
    if (!conversations.getAgent(channel.id, agentId)) {
      return {
        ok: false,
        status: 403,
        code: 'not_channel_member',
        message: 'agent is not a member of this channel',
      };
    }
    where.push(`(conv.id = ? OR conv.parent_conversation_id = ?)`);
    params.push(channel.id, channel.id);
  }

  const sqlParams: Array<string | number> = [agentId, ...params, input.limit];
  const rows = ctx.db
    .prepare<Array<string | number>, AgentSearchMessageRow>(
      `SELECT
          m.*,
          assignee.display_name AS task_assignee_name,
          sender.display_name AS sender_agent_name,
          conv.workspace_id AS workspace_id,
          conv.kind AS conversation_kind,
          CASE WHEN conv.kind = 'thread' THEN conv.parent_conversation_id ELSE conv.id END
            AS channel_id,
          CASE WHEN conv.kind = 'thread' THEN conv.id ELSE NULL END AS thread_id
         FROM messages m
         JOIN conversations conv ON conv.id = m.conversation_id
         JOIN conversation_agents ca
           ON ca.conversation_id = conv.id
          AND ca.agent_id = ?
         LEFT JOIN agents assignee ON assignee.id = m.task_assignee_agent_id
         LEFT JOIN agents sender ON sender.id = m.from_agent_id
        WHERE ${where.join(' AND ')}
        ORDER BY m.created_at DESC, m.seq DESC
        LIMIT ?`,
    )
    .all(...sqlParams);

  return {
    ok: true,
    dto: {
      query: input.q,
      scope: input.scope,
      workspace_id: workspaceId,
      channel_id: channelId,
      include_system: input.include_system,
      limit: input.limit,
      hits: rows.map((row): AgentSearchMessageHit => {
        const excerpt = searchExcerptFor(row.content, input.q);
        return {
          message_ref: searchMessageRef(row),
          sender: senderRef(row),
          excerpt: excerpt.excerpt,
          match_offset: excerpt.match_offset,
        };
      }),
    },
  };
}

function publishUnreadUpdated(
  ctx: BackendContext,
  conversationId: string,
  viewerUserId: string,
): void {
  ctx.events.publishToScope(`viewer:${viewerUserId}`, 'unread_updated', {
    conversation_id: conversationId,
  });
}

export function createInternalRouter(ctx: BackendContext): Hono<AppEnv> {
  const app = new Hono<AppEnv>();
  const agents = new AgentRepo(ctx.db);
  const conversations = new ConversationRepo(ctx.db);
  const messages = new MessageRepo(ctx.db);
  const taskDeps = { ctx, agents, conversations, messages };

  app.use('/internal/conversations/:cid/agents/:aid/*', async (c, next) => {
    const header = c.req.header('authorization') ?? '';
    const token = header.replace(/^Bearer\s+/i, '').trim();
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    const verified = ctx.agentTokens.verify(token, cid, aid);
    if (!verified.ok) {
      ctx.logger.warn(
        {
          path: c.req.path,
          hasHeader: !!header,
          tokenLen: token.length,
          reason: verified.reason,
          conversationId: cid,
          agentId: aid,
          caller: c.req.header('x-agent-hub-caller') ?? 'unknown',
        },
        'internal: unauthorized request',
      );
      return c.json({ error: 'unauthorized' }, 401);
    }
    await next();
  });
  installInternalPrincipalMiddleware(app, { agents, conversations });

  // CLI invocation logging — must run after auth so we only emit lines
  // for verified principals, but before route handlers so handler
  // throws/short-circuits still produce one line per call.
  app.use(
    '/internal/conversations/:cid/agents/:aid/*',
    createCliInvokeMiddleware(ctx),
  );

  // Observability-only: CLI fire-and-forget pre-flight failure trace.
  // Excluded from the cli.invoke middleware (the middleware checks the
  // suffix and skips it); logged here as `cli.preflight_failure`.
  app.post(internalPaths.agentCliTrace(':cid', ':aid'), async c => {
    const body = await c.req.json().catch(() => ({}));
    const parsed = CliTraceInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const principal = getAgentPrincipal(c);
    ctx.loggers.cli.info(
      {
        category: 'cli.preflight_failure',
        cid: principal.conversationId,
        aid: principal.agentId,
        noun: parsed.data.noun ?? null,
        verb: parsed.data.verb ?? null,
        flags_seen: parsed.data.flags_seen ?? [],
        error_code: parsed.data.error_code ?? null,
        error_subkind: parsed.data.error_subkind ?? null,
        error_message: parsed.data.error_message ?? null,
        duration_ms: parsed.data.duration_ms ?? null,
        status: 'failure',
      },
      'cli.preflight_failure',
    );
    return c.json({ ok: true });
  });

  // ── canonical conversation-scoped MCP routes ───────────────────
  app.post(internalPaths.agentSendConversationMessage(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = AgentSendMessageInputSchema.safeParse(body);
    if (!parsed.success) {
      ctx.logger.warn(
        { conversationId: cid, agentId: aid, issues: parsed.error.issues },
        'internal: agent send-message rejected (invalid input)',
      );
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const principal = getAgentPrincipal(c);
    if (!canPostToConversation(ctx, principal, principal.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    try {
      const result = await ctx.conversationMessageService.sendMessage({
        conversationId: principal.conversationId,
        actor: principal,
        content: parsed.data.content,
        attachmentIds: parsed.data.attachment_ids ?? [],
        target: parsed.data.target,
      });
      return c.json({ message_id: result.message.id });
    } catch (err) {
      const mapped = conversationMessageServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  app.post(internalPaths.agentCreateThread(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = AgentSpawnChildInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const principal = getAgentPrincipal(c);
    if (!canPostToConversation(ctx, principal, principal.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    try {
      const result = await ctx.threadService.createThreadFromAgent({
        parentConversationId: principal.conversationId,
        actor: principal,
        title: parsed.data.title,
        contextSummary: parsed.data.context_summary,
        initialPrompt: parsed.data.initial_prompt,
        requestedMembers: parsed.data.members,
      });
      return c.json({ thread_id: result.thread.id });
    } catch (err) {
      const mapped = threadServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  app.post(internalPaths.agentCreateChannel(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = AgentCreateChannelInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }

    const principal = getAgentPrincipal(c);
    const current = conversations.get(principal.conversationId);
    if (!current) return c.json({ error: 'conversation not found' }, 404);
    if (!canPostToConversation(ctx, principal, current.id)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const memberAgentIds = conversations
      .listAgents(current.id)
      .map(member => member.agent_id);

    try {
      const result = await ctx.channelService.createChannel({
        workspaceId: current.workspace_id,
        title: parsed.data.title,
        memberAgentIds,
        actor: principal,
        seed: {
          systemMessageBody:
            `Channel created from '${current.title}'. Reason: ${parsed.data.context_summary}`,
          initialPrompt: parsed.data.initial_prompt,
          autoStartMembers: 'all',
        },
      });
      return c.json({ channel_id: result.channel.id });
    } catch (err) {
      const mapped = channelServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  app.post(internalPaths.agentCloseThread(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = AgentCloseThreadInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const principal = getAgentPrincipal(c);
    if (!canPostToConversation(ctx, principal, principal.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    try {
      const result = await ctx.threadService.closeThread({
        threadId: principal.conversationId,
        actor: principal,
        summary: parsed.data.summary,
        postedToParent: parsed.data.posted_to_parent ?? false,
        allowOpenTasks: parsed.data.allow_open_tasks ?? false,
      });
      return c.json({ thread: result.thread });
    } catch (err) {
      const mapped = threadServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  app.post(internalPaths.agentReadHistory(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = AgentReadHistoryInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const principal = getAgentPrincipal(c);
    if (!canViewConversation(ctx, principal, principal.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const sourceConversation = conversations.get(principal.conversationId);
    if (!sourceConversation) return c.json({ error: 'conversation not found' }, 404);
    const target = resolveAgentReadHistoryTarget(
      conversations,
      sourceConversation,
      principal.agentId,
      parsed.data.target,
    );
    if (!target.ok) {
      return c.json({ error: target.message, code: target.code }, target.status);
    }
    const result = timedRouteSpan(
      ctx,
      'agent.read_history',
      {
        sourceConversationId: principal.conversationId,
        targetConversationId: target.conversation.id,
        agentId: principal.agentId,
        limit: parsed.data.limit,
      },
      () => buildReadHistoryDto(ctx, target.conversation, parsed.data),
      out => ({
        messageCount: 'messages' in out ? out.messages.length : 0,
      }),
    );
    if ('error' in result) {
      return c.json({ error: result.error, code: result.code }, result.status);
    }
    return c.json(result);
  });

  app.get(internalPaths.agentListWorkspaces(':cid', ':aid'), c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const principal = getAgentPrincipal(c);
    if (!canViewConversation(ctx, principal, principal.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const result = timedRouteSpan(
      ctx,
      'agent.list_workspaces',
      {
        sourceConversationId: principal.conversationId,
        agentId: principal.agentId,
      },
      () => buildAgentListWorkspacesDto(ctx, principal.agentId),
      out => ({ workspaceCount: out.workspaces.length }),
    );
    return c.json(result);
  });

  app.get(internalPaths.agentListChannels(':cid', ':aid'), c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const parsedQuery = AgentListChannelsQuerySchema.safeParse({
      workspace_id: c.req.query('workspace_id') || undefined,
    });
    if (!parsedQuery.success) {
      return c.json({ error: 'invalid query', issues: parsedQuery.error.issues }, 400);
    }
    const principal = getAgentPrincipal(c);
    if (!canViewConversation(ctx, principal, principal.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const sourceConversation = conversations.get(principal.conversationId);
    if (!sourceConversation) return c.json({ error: 'conversation not found' }, 404);
    // Default to source workspace; cross-workspace access is gated by
    // `canAgentAccessWorkspace` (workspace_members ∪ owned agent_home).
    const targetWorkspaceId =
      parsedQuery.data.workspace_id ?? sourceConversation.workspace_id;
    if (targetWorkspaceId !== sourceConversation.workspace_id) {
      if (!canAgentAccessWorkspace(ctx, principal.agentId, targetWorkspaceId)) {
        return c.json({ error: 'forbidden' }, 403);
      }
    }
    const result = timedRouteSpan(
      ctx,
      'agent.list_channels',
      {
        sourceConversationId: principal.conversationId,
        sourceWorkspaceId: sourceConversation.workspace_id,
        targetWorkspaceId,
        agentId: principal.agentId,
      },
      () =>
        listAgentVisibleChannels(ctx, targetWorkspaceId, principal.agentId),
      out => ({ channelCount: out.channels.length }),
    );
    return c.json(result);
  });

  app.post(internalPaths.agentSearchMessages(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = AgentSearchMessagesInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const principal = getAgentPrincipal(c);
    if (!canViewConversation(ctx, principal, principal.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const sourceConversation = conversations.get(principal.conversationId);
    if (!sourceConversation) return c.json({ error: 'conversation not found' }, 404);
    // Cross-workspace search is gated up-front by
    // `canAgentAccessWorkspace`. The dto builder still runs the
    // conversation_agents JOIN as the bottom guard, so unjoined
    // channels in the target workspace stay invisible.
    if (
      parsed.data.workspace_id &&
      parsed.data.workspace_id !== sourceConversation.workspace_id
    ) {
      if (
        !canAgentAccessWorkspace(
          ctx,
          principal.agentId,
          parsed.data.workspace_id,
        )
      ) {
        return c.json({ error: 'forbidden' }, 403);
      }
    }
    const result = timedRouteSpan(
      ctx,
      'agent.search_messages',
      {
        sourceConversationId: principal.conversationId,
        agentId: principal.agentId,
        scope: parsed.data.scope,
        workspaceId: parsed.data.workspace_id ?? null,
        channelId: parsed.data.channel_id ?? null,
        limit: parsed.data.limit,
      },
      () =>
        buildAgentSearchMessagesDto(
          ctx,
          conversations,
          sourceConversation,
          principal.agentId,
          parsed.data,
        ),
      out => ({
        hitCount: out.ok ? out.dto.hits.length : 0,
      }),
    );
    if (!result.ok) {
      return c.json({ error: result.message, code: result.code }, result.status);
    }
    return c.json(result.dto);
  });

  app.get(internalPaths.agentListConversationTasks(':cid', ':aid'), c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const conversation = conversations.get(cid);
    if (!conversation) return c.json({ error: 'conversation not found' }, 404);
    if (!agents.get(aid)) return c.json({ error: 'agent not found' }, 404);
    // status / task_number are repeatable; `?status=in_progress&status=in_review`
    // and `?task_number=190&task_number=191` are the canonical multi-value form.
    // 'all' is a legacy alias for no-filter — drop it before validation.
    const statusValues = (c.req.queries('status') ?? []).filter(s => s !== 'all');
    const taskNumberValues = c.req.queries('task_number') ?? [];
    const parsed = ListTasksQuerySchema.safeParse({
      workspace_id: c.req.query('workspace_id') || undefined,
      conversation_id: c.req.query('conversation_id') || undefined,
      status: statusValues.length > 0 ? statusValues : undefined,
      task_number: taskNumberValues.length > 0 ? taskNumberValues : undefined,
      assignee_id: c.req.query('assignee_id') || undefined,
      limit: c.req.query('limit') || undefined,
    });
    if (!parsed.success) {
      return c.json({ error: 'invalid query', issues: parsed.error.issues }, 400);
    }
    const principal = getAgentPrincipal(c);
    if (!canViewConversation(ctx, principal, principal.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    return c.json({
      tasks: ctx.taskService.list({
        actor: principal,
        conversationId: parsed.data.conversation_id,
        status: parsed.data.status,
        taskNumbers: parsed.data.task_number,
        assigneeAgentId: parsed.data.assignee_id,
        limit: parsed.data.limit,
      }),
    });
  });

  app.post(internalPaths.agentCreateConversationTasks(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = CreateTasksInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const principal = getAgentPrincipal(c);
    if (!canPostToConversation(ctx, principal, principal.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const result = await ctx.taskService.createTasks({
      conversationId: principal.conversationId,
      actor: principal,
      tasks: parsed.data.tasks,
    });
    if ('error' in result) {
      return c.json({ error: result.error }, 400);
    }
    return c.json(result);
  });

  app.post(internalPaths.agentClaimConversationTasks(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = ClaimTasksInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const actor = getAgentPrincipal(c);
    if (!canPostToConversation(ctx, actor, actor.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const results = [];
    for (const taskNumber of parsed.data.task_numbers ?? []) {
      results.push(await ctx.taskService.claimTask({ actor, by: { taskNumber } }));
    }
    for (const messageId of parsed.data.message_ids ?? []) {
      results.push(await ctx.taskService.claimTask({ actor, by: { messageId } }));
    }
    return c.json({ results });
  });

  app.post(internalPaths.agentUnclaimConversationTask(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = UnclaimTaskInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const principal = getAgentPrincipal(c);
    if (!canPostToConversation(ctx, principal, principal.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const result = await ctx.taskService.unclaimTask({
      actor: principal,
      by: { taskNumber: parsed.data.task_number },
    });
    return c.json(result);
  });

  app.post(internalPaths.agentUpdateConversationTaskStatus(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = UpdateTaskStatusInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid input', issues: parsed.error.issues }, 400);
    }
    const principal = getAgentPrincipal(c);
    if (!canPostToConversation(ctx, principal, principal.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    const result = await ctx.taskService.updateStatus({
      actor: principal,
      by: { taskNumber: parsed.data.task_number },
      status: parsed.data.status,
    });
    return c.json(result);
  });

  // ── Agent-initiated attachments (silverhand CLI → backend) ────────
  // Agents never specify channel_id; the backend derives it from the
  // calling conversation (cid) so the access boundary is fixed by the
  // agent's runtime context. locateForAgent additionally enforces that
  // the requested attachment belongs to the same channel.

  app.post(internalPaths.agentUploadAttachment(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const conversation = conversations.get(cid);
    if (!conversation) return c.json({ error: 'conversation not found' }, 404);
    if (!agents.get(aid)) return c.json({ error: 'agent not found' }, 404);
    if (!conversations.getAgent(cid, aid)) {
      return c.json({ error: 'agent is not a member of this conversation' }, 403);
    }
    const principal = getAgentPrincipal(c);
    if (!canPostToConversation(ctx, principal, principal.conversationId)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    return uploadAttachment(
      ctx,
      c,
      principal.conversationId,
      'agent',
      principal.agentId,
    );
  });

  app.get(
    internalPaths.agentLocateAttachment(':cid', ':aid', ':attachmentId'),
    async c => {
      const cid = c.req.param('cid');
      const aid = c.req.param('aid');
      const attachmentId = c.req.param('attachmentId');
      if (!cid || !aid || !attachmentId) {
        return c.json(
          { error: 'conversation id, agent id, and attachment id required' },
          400,
        );
      }
      if (!conversations.get(cid)) return c.json({ error: 'conversation not found' }, 404);
      if (!agents.get(aid)) return c.json({ error: 'agent not found' }, 404);
      if (!conversations.getAgent(cid, aid)) {
        return c.json({ error: 'agent is not a member of this conversation' }, 403);
      }
      const principal = getAgentPrincipal(c);
      if (!canViewConversation(ctx, principal, principal.conversationId)) {
        return c.json({ error: 'forbidden' }, 403);
      }
      const located = await ctx.attachmentService.locateForAgent(
        attachmentId,
        principal.conversationId,
      );
      if (!located) {
        // Either does not exist or belongs to a different channel — same
        // shape on purpose so we don't leak existence across channels.
        return c.json({ error: 'attachment not found' }, 404);
      }
      return c.json(located.locate);
    },
  );

  // ── Agent self-edit avatar (silverhand CLI → backend) ────────────
  // The agent token middleware already enforces `principal.agentId ===
  // :aid`, so the URL pin alone makes this self-only. `canEditAgent`
  // is called as a defensive double-check (mirrors the public route)
  // and to keep the agent-self branch of the predicate exercised by
  // tests instead of dead code.
  //
  // Only the avatar is exposed here — display_name / description /
  // runtime / reasoning_effort / fast_mode self-edit is **not**
  // covered by PR-2. Cross-creator profile edits go through the
  // public `PATCH /api/agents/:id` (creator-only); agent self-edit
  // for those fields is a future internal route, not implied by this
  // PR.

  app.post(internalPaths.agentUploadAvatar(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    if (!conversations.get(cid)) return c.json({ error: 'conversation not found' }, 404);
    if (!agents.get(aid)) return c.json({ error: 'agent not found' }, 404);
    const principal = getAgentPrincipal(c);
    if (!canEditAgent(ctx, principal, aid)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    let form: FormData;
    try {
      form = await c.req.formData();
    } catch {
      return c.json({ error: 'multipart/form-data required' }, 400);
    }
    const file = form.get('file');
    if (!(file instanceof File)) {
      return c.json({ error: 'file field required' }, 400);
    }
    const buffer = Buffer.from(await file.arrayBuffer());
    try {
      const dto = await ctx.agentService.uploadAvatar(aid, {
        buffer,
        contentType: file.type || 'application/octet-stream',
        filename: file.name || 'avatar',
      });
      return c.json({ avatar_url: dto.avatar_url, agent: dto });
    } catch (err) {
      const status = agentServiceErrorStatus(err);
      if (status) return c.json(agentServiceErrorBody(err), status);
      throw err;
    }
  });

  app.delete(internalPaths.agentDeleteAvatar(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    if (!conversations.get(cid)) return c.json({ error: 'conversation not found' }, 404);
    if (!agents.get(aid)) return c.json({ error: 'agent not found' }, 404);
    const principal = getAgentPrincipal(c);
    if (!canEditAgent(ctx, principal, aid)) {
      return c.json({ error: 'forbidden' }, 403);
    }
    try {
      const dto = await ctx.agentService.clearAvatar(aid);
      return c.json({ agent: dto });
    } catch (err) {
      const status = agentServiceErrorStatus(err);
      if (status) return c.json(agentServiceErrorBody(err), status);
      throw err;
    }
  });

  // ── Agent-owned schedules (silverhand CLI → backend) ──────────────
  // Per-agent surface. The route hangs off the calling conversation so
  // the existing internal principal middleware (cid+aid → AgentPrincipal)
  // authenticates the request without a second auth path; the calling
  // `cid` is also passed into the service as the default
  // `target_conversation_id` so creating a schedule in DM defaults to
  // firing in DM, in a channel defaults to firing in that channel, etc.
  // (the agent can always override with `target_conversation_id` in the
  // body to fire elsewhere it's a member of).
  // Errors map ScheduleError.code → structured `{ error, code }` JSON
  // via scheduleServiceErrorResponse so the agent sees stable codes.

  app.post(internalPaths.agentScheduleCreate(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = ScheduleCreateInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json(
        { error: 'invalid input', code: 'invalid_input', issues: parsed.error.issues },
        400,
      );
    }
    const principal = getAgentPrincipal(c);
    try {
      const schedule = ctx.scheduleService.create(
        parsed.data,
        principal.agentId,
        cid,
      );
      return c.json({ schedule });
    } catch (err) {
      const mapped = scheduleServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  app.post(internalPaths.agentScheduleList(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = ScheduleListInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json(
        { error: 'invalid input', code: 'invalid_input', issues: parsed.error.issues },
        400,
      );
    }
    const principal = getAgentPrincipal(c);
    const schedules = ctx.scheduleService.list(
      principal.agentId,
      parsed.data.status,
      parsed.data.limit,
    );
    return c.json({ schedules });
  });

  app.post(internalPaths.agentScheduleCancel(':cid', ':aid'), async c => {
    const cid = c.req.param('cid');
    const aid = c.req.param('aid');
    if (!cid || !aid) {
      return c.json({ error: 'conversation id and agent id required' }, 400);
    }
    const body = await c.req.json().catch(() => ({}));
    const parsed = ScheduleCancelInputSchema.safeParse(body);
    if (!parsed.success) {
      return c.json(
        { error: 'invalid input', code: 'invalid_input', issues: parsed.error.issues },
        400,
      );
    }
    const principal = getAgentPrincipal(c);
    try {
      const schedule = ctx.scheduleService.cancel(
        parsed.data.schedule_id,
        principal.agentId,
      );
      return c.json({ schedule });
    } catch (err) {
      const mapped = scheduleServiceErrorResponse(c, err);
      if (mapped) return mapped;
      throw err;
    }
  });

  return app;
}

/** Parse multipart/form-data and hand the file to AttachmentService.
 *  Shared between public (channel/thread) and internal (agent) upload
 *  routes so the response shape and error mapping stay identical. */
async function uploadAttachment(
  ctx: BackendContext,
  c: Context,
  sourceConversationId: string,
  createdByKind: 'user' | 'agent',
  createdById: string,
): Promise<Response> {
  let form: FormData;
  try {
    form = await c.req.formData();
  } catch (err) {
    ctx.logger.warn(
      { sourceConversationId, err },
      'api: attachment upload — multipart parse failed',
    );
    return c.json({ error: 'multipart/form-data required' }, 400);
  }
  const file = form.get('file');
  if (!(file instanceof File)) {
    return c.json({ error: 'file field required' }, 400);
  }
  const buffer = Buffer.from(await file.arrayBuffer());
  try {
    const dto = await ctx.attachmentService.upload({
      sourceConversationId,
      filename: file.name,
      contentType: file.type || null,
      buffer,
      createdByKind,
      createdById,
    });
    return c.json(dto, 201);
  } catch (err) {
    if (err instanceof AttachmentRejection) {
      const status = err.failure.kind === 'too_large' ? 413 : 400;
      return c.json({ error: err.message, code: err.failure.kind }, status);
    }
    throw err;
  }
}
