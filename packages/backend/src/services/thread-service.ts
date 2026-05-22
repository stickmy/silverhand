import type { Database as Db } from 'better-sqlite3';
import {
  type AgentRunTerminalReason,
  type MessageDto,
  type ServerConfig,
  type ThreadCloseOpenTaskDetail,
  type ThreadDto,
  systemSubtype,
} from '@silverhand/shared';
import type { BackendLogger } from '../context.js';
import type { AgentRuntimeHost } from '../runtime/agent-runtime-host.js';
import type { AgentRepo } from '../repos/agent-repo.js';
import type { ConversationRepo } from '../repos/conversation-repo.js';
import type { MessageRepo } from '../repos/message-repo.js';
import type { WorkspaceRepo } from '../repos/workspace-repo.js';
import type { TaskService } from './task-service.js';
import type { ScopeEventBus } from '../ws/scope-event-bus.js';
import {
  assertActorConversationMatch,
  type AgentPrincipal,
  type UserPrincipal,
} from '../auth/principal.js';

/**
 * ThreadService is the single owner of thread create / close business side
 * effects. Routes translate auth and input shapes, then call wrappers; they
 * must not replicate insertThread, membership fan-out, seed messages,
 * realtime publish, agent delivery/start/stop, or run resolution.
 *
 * Contract:
 * - Routes call public wrappers only. The private core owns common create
 *   side effects.
 * - Thread parents are channel-like conversations only; nested threads stay
 *   out of PR-3.
 * - Seed delivery/fan-out is explicit per seed entry. The core never infers
 *   behavior from message order.
 * - DB mutations are transactional. Realtime publishes happen after commit
 *   and before best-effort delivery/start/stop work.
 */

export type ThreadActor =
  | UserPrincipal
  | AgentPrincipal;

export type MessageErrorCode = 'message_not_found';

export class MessageError extends Error {
  constructor(
    public readonly code: MessageErrorCode,
    message: string,
  ) {
    super(message);
    this.name = 'MessageError';
  }
}

export type ParentErrorCode = 'parent_not_found' | 'parent_closed';

export class ParentError extends Error {
  constructor(
    public readonly code: ParentErrorCode,
    message: string,
  ) {
    super(message);
    this.name = 'ParentError';
  }
}

export type MembershipErrorCode =
  | 'no_members_to_thread'
  | 'agent_not_member_of_parent'
  | 'member_not_in_parent';

export class MembershipError extends Error {
  constructor(
    public readonly code: MembershipErrorCode,
    message: string,
    public readonly agentId?: string,
  ) {
    super(message);
    this.name = 'MembershipError';
  }
}

export type ActorErrorCode = 'agent_not_found' | 'agent_not_member_of_thread';

export class ActorError extends Error {
  constructor(
    public readonly code: ActorErrorCode,
    message: string,
  ) {
    super(message);
    this.name = 'ActorError';
  }
}

export type SeedAgentErrorCode = 'seed_agent_not_found';

export class SeedAgentError extends Error {
  constructor(
    public readonly code: SeedAgentErrorCode,
    message: string,
  ) {
    super(message);
    this.name = 'SeedAgentError';
  }
}

export type ThreadErrorCode =
  | 'thread_not_found'
  | 'already_closed'
  | 'has_active_children';

export class ThreadError extends Error {
  constructor(
    public readonly code: ThreadErrorCode,
    message: string,
  ) {
    super(message);
    this.name = 'ThreadError';
  }
}

/**
 * Specialized close-thread failure: the thread still has tasks in
 * `todo` / `in_progress` / `in_review`. Separate from `ThreadError`
 * because it carries a structured `blockingTasks` payload that the
 * route surfaces as `details.blocking_tasks` for the CLI to render,
 * and because it maps to the top-level `code: 'conflict'` shape
 * (the silverhand-specific reason lives in `details.reason`) so the
 * CLI's stable ERROR_CODES enum keeps working.
 *
 * Override: callers pass `allowOpenTasks: true` to `closeThread` to
 * bypass this check after the human has consciously decided to leave
 * residue. The `already_closed` / `has_active_children` / actor /
 * membership checks are NOT bypassed.
 */
export class ThreadOpenTasksError extends Error {
  constructor(
    public readonly blockingTasks: readonly ThreadCloseOpenTaskDetail[],
  ) {
    super(formatThreadOpenTasksMessage(blockingTasks));
    this.name = 'ThreadOpenTasksError';
  }
}

export interface ThreadServiceOptions {
  db: Db;
  conversations: ConversationRepo;
  messages: MessageRepo;
  agents: AgentRepo;
  workspaces: WorkspaceRepo;
  runtimeHost: AgentRuntimeHost;
  events: ScopeEventBus;
  logger: BackendLogger;
  serverConfig: ServerConfig | (() => ServerConfig);
  lastSpawnAtByAgent: Map<string, number>;
  /** Late-bound: TaskService is constructed after ThreadService in
   *  `index.ts`, so we accept a getter and resolve on each call. The
   *  closeThread guard only needs `list(...)`, so we accept the smaller
   *  shape rather than the full TaskService type to make tests easier. */
  taskService: () => Pick<TaskService, 'list'>;
}

interface CreateThreadCoreInput {
  parentChannelId: string;
  title: string;
  spawnedByAgentId: string | null;
  memberAgentIds: string[];
  seedMessages: SeedMessageInput[];
  autoStart:
    | { mode: 'seedOnly'; agentId: string }
    | { mode: 'all'; excludeAgentId?: string };
}

interface SeedMessageInput {
  fromKind: 'system' | 'agent';
  fromAgentId?: string;
  content: string;
  metadata?: Record<string, unknown>;
  deliverTo?: string;
  distributeToOthers?: { excludeAgentId?: string };
}

interface InsertedSeedMessage {
  input: SeedMessageInput;
  message: MessageDto;
}

interface CreateThreadCoreTxResult {
  thread: ThreadDto;
  members: ReturnType<ConversationRepo['listAgents']>;
  insertedSeeds: InsertedSeedMessage[];
}

interface CloseThreadTxResult {
  thread: ThreadDto;
  parentConversationId: string;
  summaryMessage: MessageDto | null;
  parentRecipients: ReturnType<ConversationRepo['listAgents']>;
  threadRecipients: ReturnType<ConversationRepo['listAgents']>;
}

const CONVERSATION_CLOSED: AgentRunTerminalReason = 'conversation_closed';

export class ThreadService {
  private readonly db: Db;
  private readonly conversations: ConversationRepo;
  private readonly messages: MessageRepo;
  private readonly agents: AgentRepo;
  private readonly workspaces: WorkspaceRepo;
  private readonly runtimeHost: AgentRuntimeHost;
  private readonly events: ScopeEventBus;
  private readonly logger: BackendLogger;
  private readonly getServerConfig: () => ServerConfig;
  private readonly lastSpawnAtByAgent: Map<string, number>;
  private readonly getTaskService: () => Pick<TaskService, 'list'>;

  constructor(opts: ThreadServiceOptions) {
    this.db = opts.db;
    this.conversations = opts.conversations;
    this.messages = opts.messages;
    this.agents = opts.agents;
    this.workspaces = opts.workspaces;
    this.runtimeHost = opts.runtimeHost;
    this.events = opts.events;
    this.logger = opts.logger;
    const serverConfig = opts.serverConfig;
    this.getServerConfig =
      typeof serverConfig === 'function'
        ? serverConfig
        : () => serverConfig;
    this.lastSpawnAtByAgent = opts.lastSpawnAtByAgent;
    this.getTaskService = opts.taskService;
  }

  async createThreadFromMessage(input: {
    messageId: string;
    actor: UserPrincipal;
  }): Promise<{ thread: ThreadDto; reused: boolean }> {
    const original = this.messages.getById(input.messageId);
    if (!original) {
      throw new MessageError('message_not_found', 'message not found');
    }
    const parent = this.conversations.getChannelLike(original.conversation_id);
    if (!parent) {
      throw new ParentError('parent_not_found', 'parent channel not found');
    }
    if (parent.closed_at !== null) {
      throw new ParentError('parent_closed', 'parent channel is closed');
    }

    const existing = this.conversations.findOpenThreadChild(parent.id, original.id);
    if (existing) {
      return { thread: existing, reused: true };
    }

    const parentAgentIds = this.conversations
      .listAgents(parent.id)
      .map(ca => ca.agent_id);
    const fallbackWorkspaceAgentIds =
      parentAgentIds.length === 0
        ? this.workspaces.listMemberAgents(parent.workspace_id)
        : [];
    const memberAgentIds = dedupe([
      original.from_kind === 'agent' ? original.from_agent_id : null,
      ...parentAgentIds,
      ...fallbackWorkspaceAgentIds,
    ]);
    if (memberAgentIds.length === 0) {
      throw new MembershipError(
        'no_members_to_thread',
        'channel has no members to start a thread with',
      );
    }

    const seedAgentId =
      original.from_kind === 'agent' && original.from_agent_id
        ? original.from_agent_id
        : memberAgentIds[0]!;
    const seedAgent = this.agents.get(seedAgentId);
    if (!seedAgent) {
      throw new SeedAgentError('seed_agent_not_found', 'seed agent not found');
    }

    let quotedSenderName: string | null = null;
    let quotedSenderLabel: string;
    if (original.from_kind === 'user') {
      quotedSenderLabel = userLabel(original.from_user_id);
    } else if (original.from_agent_id) {
      const originalAgent = this.agents.get(original.from_agent_id);
      quotedSenderName = originalAgent?.name ?? null;
      quotedSenderLabel = originalAgent ? `@${originalAgent.name}` : '@agent';
    } else {
      quotedSenderLabel = '@agent';
    }

    const flat = original.content.replace(/\s+/g, ' ').trim();
    const title = 'Thread: ' + (flat.length > 50 ? flat.slice(0, 50) + '…' : flat);
    const workspace = this.workspaces.get(parent.workspace_id);
    const parentLabel = workspace?.name ?? parent.title;
    const quoteText = [
      `Quoted from #${parentLabel} · ${quotedSenderLabel}:`,
      ``,
      ...original.content.split('\n').map(line => `> ${line}`),
    ].join('\n');
    const onboarding = [
      `You (@${seedAgent.name}) just joined a thread in #${parentLabel}.`,
      `The user wants to dig deeper into the quoted message above.`,
      ``,
      `Read the quote, send one short greeting acknowledging the topic, then wait for the user.`,
      `If you need parent context, ask the parent channel via \`target='parent'\`. When you think the thread has reached a conclusion, post a visible summary in this thread and ask the user whether to close it. Only call \`close_thread\` after the user explicitly confirms.`,
    ].join('\n');

    const thread = await this.createThreadCore({
      parentChannelId: parent.id,
      title,
      spawnedByAgentId: null,
      memberAgentIds,
      seedMessages: [
        {
          fromKind: 'system',
          content: quoteText,
          metadata: {
            subtype: systemSubtype('thread_anchor'),
            quoted_message_id: original.id,
            quoted_conversation_id: parent.id,
            quoted_channel_name: parentLabel,
            quoted_content: original.content,
            quoted_from_kind: original.from_kind,
            quoted_from_agent_id: original.from_agent_id,
            quoted_from_user_id: original.from_user_id,
            quoted_agent_name: quotedSenderName,
            quoted_created_at: original.created_at,
          },
        },
        {
          fromKind: 'system',
          content: onboarding,
          metadata: { subtype: systemSubtype('onboarding') },
          deliverTo: seedAgentId,
        },
      ],
      autoStart: { mode: 'seedOnly', agentId: seedAgentId },
    });

    return { thread, reused: false };
  }

  async createThreadFromAgent(input: {
    parentConversationId: string;
    actor: AgentPrincipal;
    title: string;
    contextSummary: string;
    initialPrompt: string;
    requestedMembers?: string[];
  }): Promise<{ thread: ThreadDto }> {
    assertActorConversationMatch(input.actor, input.parentConversationId);
    const parent = this.conversations.getChannelLike(input.parentConversationId);
    if (!parent) {
      throw new ParentError('parent_not_found', 'parent channel not found');
    }
    const spawnerAgentId = input.actor.agentId;
    if (!this.agents.get(spawnerAgentId)) {
      throw new ActorError('agent_not_found', 'agent not found');
    }
    const parentMembers = new Set(
      this.conversations.listAgents(parent.id).map(member => member.agent_id),
    );
    if (!parentMembers.has(spawnerAgentId)) {
      throw new MembershipError(
        'agent_not_member_of_parent',
        'agent is not a member of the parent channel',
      );
    }
    const requestedMembers =
      input.requestedMembers && input.requestedMembers.length > 0
        ? input.requestedMembers
        : [...parentMembers];
    for (const memberId of requestedMembers) {
      if (!parentMembers.has(memberId)) {
        throw new MembershipError(
          'member_not_in_parent',
          `agent ${memberId} is not a member of the parent channel`,
          memberId,
        );
      }
    }
    const thread = await this.createThreadCore({
      parentChannelId: parent.id,
      title: input.title,
      spawnedByAgentId: spawnerAgentId,
      memberAgentIds: dedupe([...requestedMembers, spawnerAgentId]),
      seedMessages: [
        {
          fromKind: 'system',
          content:
            `Inherited context from parent channel '${parent.title}': ${input.contextSummary}`,
          metadata: {
            subtype: systemSubtype('child_context'),
            parent_conversation_id: parent.id,
            parent_title: parent.title,
            summary: input.contextSummary,
          },
          deliverTo: spawnerAgentId,
        },
        {
          fromKind: 'agent',
          fromAgentId: spawnerAgentId,
          content: input.initialPrompt,
          distributeToOthers: { excludeAgentId: spawnerAgentId },
        },
      ],
      autoStart: { mode: 'all' },
    });
    return { thread };
  }

  async closeThread(input: {
    threadId: string;
    actor: ThreadActor;
    summary: string;
    postedToParent: boolean;
    /** Explicit escape hatch — bypass the "thread has no open tasks"
     *  guard only. Other pre-close checks (`already_closed`,
     *  `has_active_children`, actor / membership) still apply. */
    allowOpenTasks?: boolean;
  }): Promise<{ thread: ThreadDto }> {
    assertActorConversationMatch(input.actor, input.threadId);
    const thread = this.conversations.getThread(input.threadId);
    if (!thread) {
      throw new ThreadError('thread_not_found', 'thread not found');
    }
    if (thread.closed_at !== null) {
      throw new ThreadError('already_closed', 'already closed');
    }
    if (this.conversations.countActiveChildren(thread.id) > 0) {
      throw new ThreadError(
        'has_active_children',
        'close all child threads first',
      );
    }
    if (input.actor.kind === 'agent') {
      if (!this.agents.get(input.actor.agentId)) {
        throw new ActorError('agent_not_found', 'agent not found');
      }
      if (!this.conversations.getAgent(thread.id, input.actor.agentId)) {
        throw new ActorError(
          'agent_not_member_of_thread',
          'agent is not a member of this thread',
        );
      }
    }
    // Open-task guard. Placed AFTER actor + membership so a non-member
    // or unauthorized actor never sees a `blocking_tasks` summary
    // (which is just task titles/assignees, but is still leakage we
    // don't want gated only by HTTP 409 vs 403 timing). The check
    // queries the thread by id directly — TaskService.list itself
    // scopes agent actors by workspace, which is wider than needed
    // here, so the AND on `conversationId` does the final narrowing.
    if (!input.allowOpenTasks) {
      const openTasks = this.getTaskService().list({
        actor: input.actor,
        conversationId: thread.id,
        status: OPEN_TASK_STATUSES,
      });
      if (openTasks.length > 0) {
        throw new ThreadOpenTasksError(
          openTasks.map(t => ({
            task_number: t.task_number,
            status: t.status as ThreadCloseOpenTaskDetail['status'],
            title: t.title,
            assignee_name: t.assignee?.name ?? null,
            assignee_agent_id:
              t.assignee?.type === 'agent' ? t.assignee.id : null,
          })),
        );
      }
    }

    const closed = this.db.transaction((): CloseThreadTxResult => {
      this.conversations.close(thread.id, input.summary);
      const closedThread = this.conversations.getThread(thread.id);
      if (!closedThread) {
        throw new ThreadError('thread_not_found', 'thread not found');
      }
      const parent = this.conversations.get(thread.channel_id);
      const parentOpen = parent !== null && parent.closed_at === null;
      let summaryMessage: MessageDto | null = null;
      if (!input.postedToParent && parentOpen && parent) {
        const closedBy = this.actorLabel(input.actor);
        // The summary is user/agent-supplied markdown, often with
        // headings, lists or code. Put it on its own line(s) so block
        // markdown parses, and ship the raw fields in metadata so the
        // renderer can lay it out as a structured card without having
        // to re-parse the prose header.
        summaryMessage = this.messages.insertForConversation({
          conversationId: parent.id,
          fromKind: 'system',
          content:
            `Thread "${thread.title}" closed by ${closedBy}.\n\n` +
            input.summary,
          metadata: {
            subtype: systemSubtype('thread_summary'),
            thread_id: thread.id,
            thread_title: thread.title,
            closed_by: closedBy,
            summary: input.summary,
          },
        });
      }
      return {
        thread: closedThread,
        parentConversationId: thread.channel_id,
        summaryMessage,
        parentRecipients: parent ? this.conversations.listAgents(parent.id) : [],
        threadRecipients: this.conversations.listAgents(thread.id),
      };
    })();

    if (closed.summaryMessage) {
      this.events.publish(closed.parentConversationId, {
        kind: 'message',
        payload: { message: closed.summaryMessage },
      });
    }
    this.events.publish(closed.parentConversationId, {
      kind: 'thread_updated',
      payload: {
        parent_conversation_id: closed.parentConversationId,
        thread_id: closed.thread.id,
      },
    });
    this.runtimeHost.resolveConversationRuns(
      closed.thread.id,
      CONVERSATION_CLOSED,
    );

    if (closed.summaryMessage) {
      void this.deliverBestEffort(
        closed.parentConversationId,
        closed.summaryMessage.id,
        closed.parentRecipients,
        null,
        'thread-service: parent summary delivery failed',
      );
    }
    void this.stopThreadMembersBestEffort(closed.thread.id, closed.threadRecipients);

    return { thread: closed.thread };
  }

  private async createThreadCore(input: CreateThreadCoreInput): Promise<ThreadDto> {
    const memberAgentIds = dedupe(input.memberAgentIds);
    if (memberAgentIds.length === 0) {
      throw new MembershipError(
        'no_members_to_thread',
        'thread requires at least one member',
      );
    }
    for (const seed of input.seedMessages) {
      if (seed.fromKind === 'agent' && !seed.fromAgentId) {
        throw new Error('agent seed message requires fromAgentId');
      }
      if (seed.deliverTo && seed.distributeToOthers) {
        throw new Error('seed message cannot both deliverTo and distributeToOthers');
      }
    }

    const created = this.db.transaction((): CreateThreadCoreTxResult => {
      const thread = this.conversations.insertThread({
        channelId: input.parentChannelId,
        title: input.title,
        spawnedByAgentId: input.spawnedByAgentId,
        spawnCtx: {
          limits: this.getServerConfig().child_sessions,
          lastSpawnAtByAgent: this.lastSpawnAtByAgent,
        },
      });
      for (const memberId of memberAgentIds) {
        this.conversations.addAgent(thread.id, memberId);
      }
      // v25 human membership: threads inherit from their parent
      // conversation's `conversation_users`. This works for both
      // user-created and agent-created threads — the parent already
      // carries the right narrowing (channel snapshotted from
      // workspace_users for user creators, or from a source
      // conversation for agent creators).
      for (const userId of this.conversations
        .listUsers(input.parentChannelId)
        .map(u => u.user_id)) {
        this.conversations.addUser(thread.id, userId);
      }
      const insertedSeeds = input.seedMessages.map(seed => ({
        input: seed,
        message: this.messages.insertForConversation({
          conversationId: thread.id,
          fromKind: seed.fromKind,
          fromAgentId: seed.fromKind === 'agent' ? seed.fromAgentId ?? null : null,
          content: seed.content,
          metadata: seed.metadata ?? null,
        }),
      }));
      return {
        thread,
        members: this.conversations.listAgents(thread.id),
        insertedSeeds,
      };
    })();

    this.events.publish(input.parentChannelId, {
      kind: 'thread_updated',
      payload: {
        parent_conversation_id: input.parentChannelId,
        thread_id: created.thread.id,
      },
    });

    for (const seed of created.insertedSeeds) {
      if (seed.input.deliverTo) {
        void this.deliverOneBestEffort(
          created.thread.id,
          seed.input.deliverTo,
          seed.message.id,
          'thread-service: seed delivery failed',
        );
      }
      if (seed.input.distributeToOthers) {
        void this.deliverBestEffort(
          created.thread.id,
          seed.message.id,
          created.members,
          seed.input.distributeToOthers.excludeAgentId ?? null,
          'thread-service: seed distribution failed',
        );
      }
    }
    void this.startForPolicyBestEffort(created.thread.id, created.members, input.autoStart);

    return created.thread;
  }

  private actorLabel(actor: ThreadActor): string {
    if (actor.kind === 'user') return userLabel(actor.userId);
    const agent = this.agents.get(actor.agentId);
    return `@${agent?.name ?? 'agent'}`;
  }

  private async deliverOneBestEffort(
    conversationId: string,
    agentId: string,
    messageId: string,
    reason: string,
  ): Promise<void> {
    try {
      await this.runtimeHost.deliverMessage(conversationId, agentId, messageId);
    } catch (err) {
      this.logger.warn({ conversationId, agentId, messageId, err }, reason);
    }
  }

  private async deliverBestEffort(
    conversationId: string,
    messageId: string,
    recipients: ReturnType<ConversationRepo['listAgents']>,
    excludeAgentId: string | null,
    reason: string,
  ): Promise<void> {
    const selected = recipients.filter(ca => ca.agent_id !== excludeAgentId);
    try {
      await Promise.all(
        selected.map(ca =>
          this.runtimeHost.deliverMessage(
            conversationId,
            ca.agent_id,
            messageId,
          ),
        ),
      );
    } catch (err) {
      this.logger.warn(
        { conversationId, messageId, excludeAgentId, err },
        reason,
      );
    }
  }

  private async startForPolicyBestEffort(
    conversationId: string,
    members: ReturnType<ConversationRepo['listAgents']>,
    policy: CreateThreadCoreInput['autoStart'],
  ): Promise<void> {
    if (policy.mode === 'seedOnly') {
      await this.runtimeHost.start(conversationId, policy.agentId).catch(err => {
        this.logger.warn(
          { conversationId, agentId: policy.agentId, err },
          'thread-service: auto-start on user-created thread failed',
        );
      });
      return;
    }
    await Promise.all(
      members
        .filter(ca => ca.agent_id !== (policy.excludeAgentId ?? null))
        .filter(ca => ca.status !== 'active' && ca.status !== 'starting')
        .map(ca =>
          this.runtimeHost.start(conversationId, ca.agent_id).catch(err => {
            this.logger.warn(
              { conversationId, agentId: ca.agent_id, err },
              'thread-service: auto-start on agent-created thread failed',
            );
          }),
        ),
    );
  }

  private async stopThreadMembersBestEffort(
    conversationId: string,
    members: ReturnType<ConversationRepo['listAgents']>,
  ): Promise<void> {
    await Promise.all(
      members.map(ca =>
        this.runtimeHost.stop(conversationId, ca.agent_id, 'hard').catch(err => {
          this.logger.warn(
            { conversationId, agentId: ca.agent_id, err },
            'thread-service: stop closed thread member failed',
          );
        }),
      ),
    );
  }
}

function dedupe(ids: Array<string | null | undefined>): string[] {
  return Array.from(new Set(ids.filter((id): id is string => !!id)));
}

function userLabel(userId: string | null | undefined): string {
  return userId ?? 'user';
}

const OPEN_TASK_STATUSES = ['todo', 'in_progress', 'in_review'] as const;

function formatThreadOpenTasksMessage(
  blocking: readonly ThreadCloseOpenTaskDetail[],
): string {
  const n = blocking.length;
  // Keep the human-readable message terse — the CLI renders the full
  // task list from `details.blocking_tasks`. This string is the
  // fallback for tools that ignore details and only show .message.
  return (
    `cannot close thread: ${n} open task${n === 1 ? '' : 's'} ` +
    `(status in todo/in_progress/in_review). ` +
    `Close or move tasks to done first, ` +
    `or pass --allow-open-tasks to override.`
  );
}
