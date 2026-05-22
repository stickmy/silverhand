import { describe, expect, it, vi } from 'vitest';
import type {
  AgentRunTerminalReason,
  ServerConfig,
} from '@silverhand/shared';
import {
  readChildContextMetadata,
  readThreadAnchorMetadata,
} from '@silverhand/shared';
import { AgentRepo } from '../../src/repos/agent-repo.js';
import { ConversationRepo } from '../../src/repos/conversation-repo.js';
import { HostRepo } from '../../src/repos/host-repo.js';
import { MessageRepo } from '../../src/repos/message-repo.js';
import { WorkspaceRepo } from '../../src/repos/workspace-repo.js';
import {
  ActorError,
  MembershipError,
  ParentError,
  SeedAgentError,
  ThreadError,
  ThreadOpenTasksError,
  ThreadService,
} from '../../src/services/thread-service.js';
import type { BackendLogger } from '../../src/context.js';
import type { AgentRuntimeHost } from '../../src/runtime/agent-runtime-host.js';
import { ScopeEventBus } from '../../src/ws/scope-event-bus.js';
import { PrincipalConversationMismatchError } from '../../src/auth/principal.js';
import { makeTempSilverhandDir, openTestDb } from '../utils/db.js';

class FakeConversationManager {
  starts: Array<{ conversationId: string; agentId: string }> = [];
  delivered: Array<{ conversationId: string; agentId: string; messageId: string }> = [];
  stops: Array<{ conversationId: string; agentId: string; mode: 'soft' | 'hard' }> = [];
  resolvedConversations: Array<{ conversationId: string; reason: AgentRunTerminalReason }> = [];
  hangDeliver = false;
  deliverError: Error | null = null;
  onDeliver: (() => void) | null = null;

  async start(conversationId: string, agentId: string): Promise<void> {
    this.starts.push({ conversationId, agentId });
  }

  async deliverMessage(
    conversationId: string,
    agentId: string,
    messageId: string,
  ): Promise<void> {
    this.onDeliver?.();
    this.delivered.push({ conversationId, agentId, messageId });
    if (this.deliverError) throw this.deliverError;
    if (this.hangDeliver) return new Promise<void>(() => {});
  }

  async stop(
    conversationId: string,
    agentId: string,
    mode: 'soft' | 'hard' = 'hard',
  ): Promise<void> {
    this.stops.push({ conversationId, agentId, mode });
  }

  resolveConversationRuns(
    conversationId: string,
    reason: AgentRunTerminalReason = 'conversation_closed',
  ): void {
    this.resolvedConversations.push({ conversationId, reason });
  }

  resolveRun(): void {}
  getAgentRunStates(): never[] {
    return [];
  }
  async stopAll(): Promise<void> {}
  reconcileOnBoot(): void {}
  isRunning(): boolean {
    return false;
  }
  closeActivityLog(): Promise<void> {
    return Promise.resolve();
  }
}

interface FakeTaskInput {
  task_number: number;
  status: 'todo' | 'in_progress' | 'in_review';
  title: string;
  assigneeName: string | null;
  assigneeAgentId: string | null;
}

function makeFakeTaskSummary(
  i: FakeTaskInput,
): import('@silverhand/shared').TaskSummaryDto {
  return {
    message_id: `msg-${i.task_number}`,
    task_number: i.task_number,
    workspace_id: 'w',
    workspace_name: 'w',
    channel_id: 'c',
    channel_name: 'c',
    thread_id: 't',
    conversation: { id: 't', title: 't' },
    status: i.status,
    title: i.title,
    assignee:
      i.assigneeName && i.assigneeAgentId
        ? { type: 'agent', id: i.assigneeAgentId, name: i.assigneeName }
        : null,
    creator: { type: 'agent', id: 'creator', name: 'creator' },
    created_at: 0,
  };
}

function bootstrap() {
  const { db, localHostId } = openTestDb();
  const tmp = makeTempSilverhandDir();
  new HostRepo(db).ensureLocal();
  const agents = new AgentRepo(db);
  const workspaces = new WorkspaceRepo(db, { workspacesRoot: tmp.workspacesRoot });
  const conversations = new ConversationRepo(db);
  const messages = new MessageRepo(db);
  const events = new ScopeEventBus('test-epoch');
  const fake = new FakeConversationManager();
  // FakeTaskService: by default the close-thread guard sees zero open
  // tasks. Tests that exercise the guard mutate `fakeTaskService.tasks`
  // before calling `closeThread`.
  const fakeTaskService = {
    tasks: [] as Array<import('@silverhand/shared').TaskSummaryDto>,
    list(_filters: unknown) {
      return this.tasks;
    },
  };
  const logger: BackendLogger = {
    info: vi.fn(),
    warn: vi.fn(),
    error: vi.fn(),
  };
  const serverConfig: ServerConfig = {
    mode: 'embedded',
    url: 'http://127.0.0.1:0',
    auth_token: 'a'.repeat(32),
    refresh_token: null,
    child_sessions: {
      max_concurrent_per_parent: null,
      spawn_cooldown_seconds: null,
      max_depth: 1,
    },
  };
  const service = new ThreadService({
    db,
    conversations,
    messages,
    agents,
    workspaces,
    runtimeHost: fake as unknown as AgentRuntimeHost,
    events,
    logger,
    serverConfig,
    lastSpawnAtByAgent: new Map(),
    taskService: () => fakeTaskService,
  });
  const workspace = workspaces.insert({
    name: 'eng',
    description: null,
    workspacePath: tmp.workspacesRoot,
    workspaceHostId: localHostId,
  });
  const alice = agents.create({ name: 'alice', runtime: 'claude' }, localHostId, 'local');
  const bob = agents.create({ name: 'bob', runtime: 'claude' }, localHostId, 'local');
  const carol = agents.create({ name: 'carol', runtime: 'claude' }, localHostId, 'local');
  for (const agent of [alice, bob, carol]) {
    workspaces.addMember(workspace.id, agent.id);
  }
  const channel = conversations.insertChannel({
    workspaceId: workspace.id,
    workspacePath: workspace.workspace_path,
    title: 'source',
  });
  conversations.addAgent(channel.id, alice.id);
  conversations.addAgent(channel.id, bob.id);

  return {
    db,
    tmp,
    agents,
    workspaces,
    conversations,
    messages,
    events,
    fake,
    logger,
    service,
    workspace,
    channel,
    alice,
    bob,
    carol,
    fakeTaskService,
    cleanup: tmp.cleanup,
  };
}

describe('ThreadService.createThreadFromMessage', () => {
  it('creates a quoted-message thread, publishes first, delivers onboarding, and starts the seed agent', async () => {
    const t = bootstrap();
    try {
      const order: string[] = [];
      const unsub = t.events.subscribe(t.channel.id, e => {
        if (e.kind === 'thread_updated') order.push('publish');
      });
      t.fake.onDeliver = () => order.push('deliver');
      const original = t.messages.insertForConversation({
        conversationId: t.channel.id,
        fromKind: 'user',
        fromUserId: 'local',
        content: 'Please inspect\nthis regression',
      });

      const result = await t.service.createThreadFromMessage({
        messageId: original.id,
        actor: { kind: 'user', userId: 'local' },
      });
      unsub();

      expect(result.reused).toBe(false);
      expect(order[0]).toBe('publish');
      const seeded = t.messages.listForConversation(result.thread.id);
      expect(seeded).toHaveLength(2);
      expect(seeded[0]).toMatchObject({
        from_kind: 'system',
        metadata: expect.objectContaining({ subtype: 'thread_anchor' }),
      });
      // PR-5 producer round-trip: thread_anchor metadata must satisfy
      // ThreadAnchorMetadataSchema so the renderer can pull a Quoted
      // card without re-parsing message.content.
      const anchorParsed = readThreadAnchorMetadata(
        seeded[0]!.metadata as Record<string, unknown>,
      );
      expect(anchorParsed).not.toBeNull();
      expect(anchorParsed!.quoted_message_id).toBe(original.id);
      expect(anchorParsed!.quoted_conversation_id).toBe(t.channel.id);
      expect(anchorParsed!.quoted_content).toBe('Please inspect\nthis regression');
      expect(anchorParsed!.quoted_from_kind).toBe('user');
      expect(seeded[1]).toMatchObject({
        from_kind: 'system',
        metadata: { subtype: 'onboarding' },
      });
      expect(t.fake.delivered).toHaveLength(1);
      expect(t.fake.delivered[0]).toMatchObject({
        conversationId: result.thread.id,
        messageId: seeded[1]!.id,
      });
      expect([t.alice.id, t.bob.id]).toContain(t.fake.delivered[0]!.agentId);
      expect(t.fake.starts).toEqual([
        { conversationId: result.thread.id, agentId: t.fake.delivered[0]!.agentId },
      ]);
    } finally {
      t.cleanup();
    }
  });

  it('reuses an open anchored thread without side effects', async () => {
    const t = bootstrap();
    try {
      const original = t.messages.insertForConversation({
        conversationId: t.channel.id,
        fromKind: 'agent',
        fromAgentId: t.bob.id,
        content: 'drill down here',
      });
      const first = await t.service.createThreadFromMessage({
        messageId: original.id,
        actor: { kind: 'user', userId: 'local' },
      });
      t.fake.delivered = [];
      t.fake.starts = [];
      const events: string[] = [];
      const unsub = t.events.subscribe(t.channel.id, e => events.push(e.kind));

      const second = await t.service.createThreadFromMessage({
        messageId: original.id,
        actor: { kind: 'user', userId: 'local' },
      });
      unsub();

      expect(second.reused).toBe(true);
      expect(second.thread.id).toBe(first.thread.id);
      expect(second.thread.anchor_message_id).toBe(original.id);
      expect(t.fake.delivered).toHaveLength(0);
      expect(t.fake.starts).toHaveLength(0);
      expect(events).toHaveLength(0);
    } finally {
      t.cleanup();
    }
  });

  it('falls back to workspace members when a channel has no conversation agents', async () => {
    const t = bootstrap();
    try {
      const empty = t.conversations.insertChannel({
        workspaceId: t.workspace.id,
        workspacePath: t.workspace.workspace_path,
        title: 'empty',
      });
      const original = t.messages.insertForConversation({
        conversationId: empty.id,
        fromKind: 'user',
        content: 'needs a thread',
      });

      const result = await t.service.createThreadFromMessage({
        messageId: original.id,
        actor: { kind: 'user', userId: 'local' },
      });

      expect(t.conversations.listAgents(result.thread.id).map(a => a.agent_id).sort())
        .toEqual([t.alice.id, t.bob.id, t.carol.id].sort());
    } finally {
      t.cleanup();
    }
  });

  it('rejects a missing seed agent before inserting', async () => {
    const t = bootstrap();
    try {
      const msg = t.messages.insertForConversation({
        conversationId: t.channel.id,
        fromKind: 'agent',
        fromAgentId: t.carol.id,
        content: 'stale agent author',
      });
      t.db.pragma('foreign_keys = OFF');
      try {
        t.db.prepare('UPDATE messages SET from_agent_id = ? WHERE id = ?')
          .run('missing-agent', msg.id);
      } finally {
        t.db.pragma('foreign_keys = ON');
      }

      await expect(
        t.service.createThreadFromMessage({
          messageId: msg.id,
          actor: { kind: 'user', userId: 'local' },
        }),
      ).rejects.toBeInstanceOf(SeedAgentError);
    } finally {
      t.cleanup();
    }
  });
});

describe('ThreadService.createThreadFromAgent', () => {
  it('rejects agent principal conversation mismatch before inserting', async () => {
    const t = bootstrap();
    try {
      const other = t.conversations.insertChannel({
        workspaceId: t.workspace.id,
        workspacePath: t.workspace.workspace_path,
        title: 'other',
      });
      await expect(
        t.service.createThreadFromAgent({
          parentConversationId: t.channel.id,
          actor: { kind: 'agent', agentId: t.alice.id, conversationId: other.id },
          title: 'bad',
          contextSummary: 'x',
          initialPrompt: 'go',
        }),
      ).rejects.toBeInstanceOf(PrincipalConversationMismatchError);
      expect(t.conversations.listChildren(t.channel.id)).toEqual([]);
    } finally {
      t.cleanup();
    }
  });

  it('creates an agent thread, publishes before delivery, distributes prompt, and starts all members including the spawner', async () => {
    const t = bootstrap();
    try {
      const order: string[] = [];
      const unsub = t.events.subscribe(t.channel.id, e => {
        if (e.kind === 'thread_updated') order.push('publish');
      });
      t.fake.onDeliver = () => order.push('deliver');

      const result = await t.service.createThreadFromAgent({
        parentConversationId: t.channel.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
        title: 'investigate',
        contextSummary: 'perf regression',
        initialPrompt: 'profile the hot path',
        requestedMembers: [t.bob.id],
      });
      unsub();

      expect(order[0]).toBe('publish');
      expect(t.conversations.listAgents(result.thread.id).map(a => a.agent_id).sort())
        .toEqual([t.alice.id, t.bob.id].sort());
      const seeded = t.messages.listForConversation(result.thread.id);
      expect(seeded.map(m => m.content)).toEqual([
        "Inherited context from parent channel 'source': perf regression",
        'profile the hot path',
      ]);
      // PR-5 producer round-trip: child_context metadata must satisfy
      // ChildContextMetadataSchema so the renderer can pull an
      // Inherited card without re-parsing message.content.
      const childParsed = readChildContextMetadata(
        seeded[0]!.metadata as Record<string, unknown>,
      );
      expect(childParsed).not.toBeNull();
      expect(childParsed!.parent_conversation_id).toBe(t.channel.id);
      expect(childParsed!.parent_title).toBe('source');
      expect(childParsed!.summary).toBe('perf regression');
      expect(t.fake.delivered).toEqual([
        {
          conversationId: result.thread.id,
          agentId: t.alice.id,
          messageId: seeded[0]!.id,
        },
        {
          conversationId: result.thread.id,
          agentId: t.bob.id,
          messageId: seeded[1]!.id,
        },
      ]);
      // The spawner needs its own (thread, agent) process to receive
      // future thread messages and the inherited-context system message
      // (delivered to the spawner). Without this start, the spawner
      // stays 'idle' in the thread and shows as 'away' in the UI.
      expect(
        t.fake.starts
          .filter(s => s.conversationId === result.thread.id)
          .map(s => s.agentId)
          .sort(),
      ).toEqual([t.alice.id, t.bob.id].sort());
    } finally {
      t.cleanup();
    }
  });

  it('defaults members to all parent members and rejects non-parent members', async () => {
    const t = bootstrap();
    try {
      const result = await t.service.createThreadFromAgent({
        parentConversationId: t.channel.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
        title: 'all',
        contextSummary: 'x',
        initialPrompt: 'go',
      });
      expect(t.conversations.listAgents(result.thread.id).map(a => a.agent_id).sort())
        .toEqual([t.alice.id, t.bob.id].sort());

      await expect(
        t.service.createThreadFromAgent({
          parentConversationId: t.channel.id,
          actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
          title: 'bad',
          contextSummary: 'x',
          initialPrompt: 'go',
          requestedMembers: [t.carol.id],
        }),
      ).rejects.toBeInstanceOf(MembershipError);
    } finally {
      t.cleanup();
    }
  });

  it('rejects invalid parent and spawner membership before inserting', async () => {
    const t = bootstrap();
    try {
      const thread = t.conversations.insertThread({
        channelId: t.channel.id,
        title: 'not-parent',
      });
      await expect(
        t.service.createThreadFromAgent({
          parentConversationId: thread.id,
          actor: { kind: 'agent', agentId: t.alice.id, conversationId: thread.id },
          title: 'bad',
          contextSummary: 'x',
          initialPrompt: 'go',
        }),
      ).rejects.toBeInstanceOf(ParentError);

      await expect(
        t.service.createThreadFromAgent({
          parentConversationId: t.channel.id,
          actor: { kind: 'agent', agentId: t.carol.id, conversationId: t.channel.id },
          title: 'bad',
          contextSummary: 'x',
          initialPrompt: 'go',
        }),
      ).rejects.toMatchObject({ code: 'agent_not_member_of_parent' });
    } finally {
      t.cleanup();
    }
  });
});

describe('ThreadService.closeThread', () => {
  it('rejects agent principal conversation mismatch before closing', async () => {
    const t = bootstrap();
    try {
      const created = await t.service.createThreadFromAgent({
        parentConversationId: t.channel.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
        title: 'inv',
        contextSummary: 'x',
        initialPrompt: 'go',
        requestedMembers: [t.bob.id],
      });
      await expect(
        t.service.closeThread({
          threadId: created.thread.id,
          actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
          summary: 'done',
          postedToParent: false,
        }),
      ).rejects.toBeInstanceOf(PrincipalConversationMismatchError);
      expect(t.conversations.getThread(created.thread.id)?.closed_at).toBe(null);
    } finally {
      t.cleanup();
    }
  });

  it('closes a thread, publishes summary before delivery, resolves runs, and stops members', async () => {
    const t = bootstrap();
    try {
      const created = await t.service.createThreadFromAgent({
        parentConversationId: t.channel.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
        title: 'inv',
        contextSummary: 'x',
        initialPrompt: 'go',
        requestedMembers: [t.bob.id],
      });
      t.fake.delivered = [];
      t.fake.stops = [];
      const order: string[] = [];
      const unsub = t.events.subscribe(t.channel.id, e => {
        if (e.kind === 'message') order.push('publish-message');
        if (e.kind === 'thread_updated') order.push('publish-thread');
      });
      t.fake.onDeliver = () => order.push('deliver');

      const closed = await t.service.closeThread({
        threadId: created.thread.id,
        actor: { kind: 'agent', agentId: t.bob.id, conversationId: created.thread.id },
        summary: 'done',
        postedToParent: false,
      });
      unsub();

      expect(closed.thread.closed_at).not.toBeNull();
      expect(order.slice(0, 2)).toEqual(['publish-message', 'publish-thread']);
      expect(t.fake.resolvedConversations).toEqual([
        { conversationId: created.thread.id, reason: 'conversation_closed' },
      ]);
      const parentMessages = t.messages.listForConversation(t.channel.id);
      const summary = parentMessages.find(m => m.metadata?.subtype === 'thread_summary');
      expect(summary?.content).toContain('@bob');
      expect(summary?.content).toContain('done');
      expect(t.fake.delivered.map(d => d.agentId).sort()).toEqual(
        [t.alice.id, t.bob.id].sort(),
      );
      expect(t.fake.stops.map(s => s.agentId).sort()).toEqual(
        [t.alice.id, t.bob.id].sort(),
      );
    } finally {
      t.cleanup();
    }
  });

  it('skips parent summary when already posted', async () => {
    const t = bootstrap();
    try {
      const created = await t.service.createThreadFromAgent({
        parentConversationId: t.channel.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
        title: 'inv',
        contextSummary: 'x',
        initialPrompt: 'go',
        requestedMembers: [t.bob.id],
      });
      const before = t.messages.listForConversation(t.channel.id).length;

      await t.service.closeThread({
        threadId: created.thread.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: created.thread.id },
        summary: 'already posted',
        postedToParent: true,
      });

      expect(t.messages.listForConversation(t.channel.id)).toHaveLength(before);
    } finally {
      t.cleanup();
    }
  });

  it('rejects forged agent close actors without mutating the thread', async () => {
    const t = bootstrap();
    try {
      const created = await t.service.createThreadFromAgent({
        parentConversationId: t.channel.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
        title: 'inv',
        contextSummary: 'x',
        initialPrompt: 'go',
        requestedMembers: [t.bob.id],
      });

      await expect(
        t.service.closeThread({
          threadId: created.thread.id,
          actor: { kind: 'agent', agentId: t.carol.id, conversationId: created.thread.id },
          summary: 'done',
          postedToParent: false,
        }),
      ).rejects.toBeInstanceOf(ActorError);

      expect(t.conversations.getThread(created.thread.id)?.closed_at).toBeNull();
    } finally {
      t.cleanup();
    }
  });

  it('rejects already closed threads without mutating them again', async () => {
    const t = bootstrap();
    try {
      const created = await t.service.createThreadFromAgent({
        parentConversationId: t.channel.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
        title: 'inv',
        contextSummary: 'x',
        initialPrompt: 'go',
        requestedMembers: [t.bob.id],
      });
      await t.service.closeThread({
        threadId: created.thread.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: created.thread.id },
        summary: 'done',
        postedToParent: true,
      });

      await expect(
        t.service.closeThread({
          threadId: created.thread.id,
          actor: { kind: 'agent', agentId: t.alice.id, conversationId: created.thread.id },
          summary: 'again',
          postedToParent: true,
        }),
      ).rejects.toBeInstanceOf(ThreadError);
    } finally {
      t.cleanup();
    }
  });

  it('rejects close when thread still has open tasks and surfaces blocking_tasks detail', async () => {
    const t = bootstrap();
    try {
      const created = await t.service.createThreadFromAgent({
        parentConversationId: t.channel.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
        title: 'inv',
        contextSummary: 'x',
        initialPrompt: 'go',
        requestedMembers: [t.bob.id],
      });
      t.fakeTaskService.tasks = [
        makeFakeTaskSummary({
          task_number: 901,
          status: 'in_review',
          title: 'finish review',
          assigneeName: 'bob',
          assigneeAgentId: t.bob.id,
        }),
        makeFakeTaskSummary({
          task_number: 902,
          status: 'todo',
          title: 'pending work',
          assigneeName: null,
          assigneeAgentId: null,
        }),
      ];

      let captured: ThreadOpenTasksError | null = null;
      try {
        await t.service.closeThread({
          threadId: created.thread.id,
          actor: { kind: 'agent', agentId: t.alice.id, conversationId: created.thread.id },
          summary: 'done',
          postedToParent: false,
        });
      } catch (err) {
        if (err instanceof ThreadOpenTasksError) captured = err;
        else throw err;
      }
      expect(captured).not.toBeNull();
      expect(captured!.blockingTasks).toHaveLength(2);
      expect(captured!.blockingTasks[0]).toMatchObject({
        task_number: 901,
        status: 'in_review',
        assignee_name: 'bob',
        assignee_agent_id: t.bob.id,
      });
      expect(captured!.blockingTasks[1]).toMatchObject({
        task_number: 902,
        status: 'todo',
        assignee_name: null,
        assignee_agent_id: null,
      });
      expect(captured!.message).toContain('2 open tasks');
      // thread must NOT be closed
      expect(t.conversations.getThread(created.thread.id)?.closed_at).toBeNull();
      // no parent summary published; no runs resolved
      expect(t.fake.resolvedConversations).toEqual([]);
    } finally {
      t.cleanup();
    }
  });

  it('allowOpenTasks bypasses the open-task guard and closes the thread', async () => {
    const t = bootstrap();
    try {
      const created = await t.service.createThreadFromAgent({
        parentConversationId: t.channel.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
        title: 'inv',
        contextSummary: 'x',
        initialPrompt: 'go',
        requestedMembers: [t.bob.id],
      });
      t.fakeTaskService.tasks = [
        makeFakeTaskSummary({
          task_number: 903,
          status: 'in_progress',
          title: 'still going',
          assigneeName: 'alice',
          assigneeAgentId: t.alice.id,
        }),
      ];

      const closed = await t.service.closeThread({
        threadId: created.thread.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: created.thread.id },
        summary: 'accepting residue',
        postedToParent: true,
        allowOpenTasks: true,
      });

      expect(closed.thread.closed_at).not.toBeNull();
    } finally {
      t.cleanup();
    }
  });

  it('non-member actor hits ActorError BEFORE the open-task query (no leakage of blocking_tasks)', async () => {
    const t = bootstrap();
    try {
      const created = await t.service.createThreadFromAgent({
        parentConversationId: t.channel.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
        title: 'inv',
        contextSummary: 'x',
        initialPrompt: 'go',
        requestedMembers: [t.bob.id],
      });
      // Populate the fake task service with tasks. If the guard fired
      // BEFORE the actor check we'd surface them; the order assertion
      // below proves we don't.
      t.fakeTaskService.tasks = [
        makeFakeTaskSummary({
          task_number: 904,
          status: 'in_review',
          title: 'should not leak',
          assigneeName: 'bob',
          assigneeAgentId: t.bob.id,
        }),
      ];
      let saw: unknown = null;
      const listSpy = vi.spyOn(t.fakeTaskService, 'list');
      try {
        await t.service.closeThread({
          threadId: created.thread.id,
          // carol is NOT a member of the thread
          actor: { kind: 'agent', agentId: t.carol.id, conversationId: created.thread.id },
          summary: 'forged',
          postedToParent: false,
        });
      } catch (err) {
        saw = err;
      }
      expect(saw).toBeInstanceOf(ActorError);
      expect(listSpy).not.toHaveBeenCalled();
      listSpy.mockRestore();
    } finally {
      t.cleanup();
    }
  });

  it('closes cleanly when every task in the thread is done (no open-task block)', async () => {
    const t = bootstrap();
    try {
      const created = await t.service.createThreadFromAgent({
        parentConversationId: t.channel.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
        title: 'inv',
        contextSummary: 'x',
        initialPrompt: 'go',
        requestedMembers: [t.bob.id],
      });
      // empty fakeTaskService.tasks already; matches "all done / no
      // active tasks in this thread"
      const closed = await t.service.closeThread({
        threadId: created.thread.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: created.thread.id },
        summary: 'all done',
        postedToParent: true,
      });
      expect(closed.thread.closed_at).not.toBeNull();
    } finally {
      t.cleanup();
    }
  });
});

describe('ThreadService user-membership inheritance (v25)', () => {
  it('inherits user members from the parent conversation when creating a thread', async () => {
    const t = bootstrap();
    try {
      // Drop test-fixture autoseed triggers so observed user
      // membership reflects only what ThreadService writes.
      t.db.exec(
        `DROP TRIGGER IF EXISTS test_seed_workspace_users;
         DROP TRIGGER IF EXISTS test_seed_conversation_users;`,
      );
      // Pin parent conversation_users to a specific subset.
      t.db.exec(
        `DELETE FROM conversation_users WHERE conversation_id = '${t.channel.id}'`,
      );
      t.db
        .prepare(
          `INSERT INTO conversation_users (conversation_id, user_id, joined_at)
             VALUES (?, 'local', ?), (?, 'alice', ?)`,
        )
        .run(t.channel.id, Date.now(), t.channel.id, Date.now());

      const result = await t.service.createThreadFromAgent({
        parentConversationId: t.channel.id,
        actor: { kind: 'agent', agentId: t.alice.id, conversationId: t.channel.id },
        title: 'child',
        requestedMembers: [t.alice.id],
        contextSummary: 'ctx',
        initialPrompt: 'go',
      });

      const userMembers = t.conversations
        .listUsers(result.thread.id)
        .map(u => u.user_id)
        .sort();
      expect(userMembers).toEqual(['alice', 'local']);
    } finally {
      t.cleanup();
    }
  });
});
