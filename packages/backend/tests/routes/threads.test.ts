import { afterEach, describe, expect, it } from 'vitest';
import { buildTestApp, http, type TestApp } from '../utils/app.js';

async function makeAgent(t: TestApp, name: string): Promise<string> {
  const created = await (
    await http(t).post('/api/agents', { name, runtime: 'claude' })
  ).json() as any;
  return created.agent.id;
}

async function createWorkspaceChannel(
  t: TestApp,
  names: string[],
): Promise<{ workspaceId: string; channelId: string; agentIds: Record<string, string> }> {
  const agentIds: Record<string, string> = {};
  for (const name of names) {
    agentIds[name] = await makeAgent(t, name);
  }
  const workspace = await (
    await http(t).post('/api/workspaces', {
      name: `thread-ws-${names.join('-')}`,
      member_agent_ids: Object.values(agentIds),
    })
  ).json() as any;
  t.fake.starts = [];
  t.fake.delivered = [];
  t.fake.stops = [];
  t.fake.resolvedConversations = [];
  return {
    workspaceId: workspace.workspace.id,
    channelId: workspace.onboarding_channel_id,
    agentIds,
  };
}

function authHeaders(token: string): Record<string, string> {
  return { authorization: `Bearer ${token}` };
}

describe('thread routes', () => {
  let t: TestApp | null = null;
  afterEach(() => {
    t?.cleanup();
    t = null;
  });

  it('message thread route creates once and reuses without a second publish', async () => {
    t = buildTestApp();
    const { channelId } = await createWorkspaceChannel(t, ['alice']);
    const posted = await (
      await http(t).post(`/api/channels/${channelId}/messages`, {
        content: 'open a focused thread',
      })
    ).json() as any;
    const events: Array<{ kind: string; payload: any }> = [];
    const unsub = t.ctx.events.subscribe(channelId, e => {
      if (e.kind === 'thread_updated') events.push({ kind: e.kind, payload: e.payload });
    });

    const first = await http(t).post(`/api/messages/${posted.message.id}/thread`);
    const firstBody = await first.json() as any;
    const second = await http(t).post(`/api/messages/${posted.message.id}/thread`);
    const secondBody = await second.json() as any;
    unsub();

    expect(first.status).toBe(201);
    expect(second.status).toBe(200);
    expect(secondBody).toMatchObject({
      thread_id: firstBody.thread_id,
      reused: true,
    });
    expect(events).toHaveLength(1);
    expect(events[0]).toEqual({
      kind: 'thread_updated',
      payload: {
        parent_conversation_id: channelId,
        thread_id: firstBody.thread_id,
      },
    });
  });

  it('internal create-thread uses service side effects and starts only non-spawner members', async () => {
    t = buildTestApp();
    const { channelId, agentIds } = await createWorkspaceChannel(t, ['alice', 'bob']);

    const response = await http(t).post(
      `/internal/conversations/${channelId}/agents/${agentIds.alice}/create-thread`,
      {
        title: 'investigate',
        members: [agentIds.bob],
        context_summary: 'looking at perf',
        initial_prompt: 'profile the hot path',
      },
      { headers: authHeaders(t.authToken) },
    );
    expect(response.status).toBe(200);
    const body = await response.json() as any;
    const messages = await (
      await http(t).get(`/api/threads/${body.thread_id}/messages`)
    ).json() as any;

    expect(messages.messages).toHaveLength(2);
    expect(t.fake.delivered).toEqual([
      {
        conversationId: body.thread_id,
        agentId: agentIds.alice,
        messageId: messages.messages[0].id,
      },
      {
        conversationId: body.thread_id,
        agentId: agentIds.bob,
        messageId: messages.messages[1].id,
      },
    ]);
    expect(
      t.fake.starts
        .filter(s => s.conversationId === body.thread_id)
        .map(s => s.agentId)
        .sort(),
    ).toEqual([agentIds.alice, agentIds.bob].sort());
  });

  it('internal close-thread validates actor membership and posts parent summary', async () => {
    t = buildTestApp();
    const { channelId, agentIds } = await createWorkspaceChannel(t, ['alice', 'bob']);
    const carolId = await makeAgent(t, 'carol');
    const child = await (
      await http(t).post(
        `/internal/conversations/${channelId}/agents/${agentIds.alice}/create-thread`,
        {
          title: 'inv',
          members: [agentIds.bob],
          context_summary: 'x',
          initial_prompt: 'go',
        },
        { headers: authHeaders(t.authToken) },
      )
    ).json() as any;

    const forged = await http(t).post(
      `/internal/conversations/${child.thread_id}/agents/${carolId}/close-thread`,
      { summary: 'not allowed' },
      { headers: authHeaders(t.authToken) },
    );
    expect(forged.status).toBe(403);

    const events: Array<{ kind: string; payload: any }> = [];
    const unsub = t.ctx.events.subscribe(channelId, e => {
      events.push({ kind: e.kind, payload: e.payload });
    });
    const close = await http(t).post(
      `/internal/conversations/${child.thread_id}/agents/${agentIds.bob}/close-thread`,
      { summary: 'done' },
      { headers: authHeaders(t.authToken) },
    );
    unsub();

    expect(close.status).toBe(200);
    const parentMessages = await (
      await http(t).get(`/api/channels/${channelId}/messages`)
    ).json() as any;
    const summary = parentMessages.messages.find(
      (m: any) => m.metadata?.subtype === 'thread_summary',
    );
    expect(summary.content).toContain('@bob');
    expect(summary.content).toContain('done');
    expect(events.map(e => e.kind)).toEqual(
      expect.arrayContaining(['message', 'thread_updated']),
    );
    expect(t.fake.resolvedConversations).toEqual([
      { conversationId: child.thread_id, reason: 'conversation_closed' },
    ]);
  });

  it('internal close-thread rejects with HTTP 409 + structured blocking_tasks when open tasks remain', async () => {
    t = buildTestApp();
    const { channelId, agentIds } = await createWorkspaceChannel(t, ['alice', 'bob']);
    const child = await (
      await http(t).post(
        `/internal/conversations/${channelId}/agents/${agentIds.alice}/create-thread`,
        {
          title: 'inv',
          members: [agentIds.bob],
          context_summary: 'x',
          initial_prompt: 'go',
        },
        { headers: authHeaders(t.authToken) },
      )
    ).json() as any;
    const threadId = child.thread_id as string;

    // Plant an in_progress task in the thread (created by bob, who is
    // a member). create → claim → status moves it from todo →
    // in_progress.
    const create = await (
      await http(t).post(
        `/internal/conversations/${threadId}/agents/${agentIds.bob}/tasks/create`,
        { tasks: [{ title: 'finish the spike' }] },
        { headers: authHeaders(t.authToken) },
      )
    ).json() as any;
    const taskNumber = create.tasks[0].task_number as number;
    await http(t).post(
      `/internal/conversations/${threadId}/agents/${agentIds.bob}/tasks/claim`,
      { task_numbers: [taskNumber] },
      { headers: authHeaders(t.authToken) },
    );
    await http(t).post(
      `/internal/conversations/${threadId}/agents/${agentIds.bob}/tasks/update-status`,
      { task_number: taskNumber, status: 'in_progress' },
      { headers: authHeaders(t.authToken) },
    );

    const rejected = await http(t).post(
      `/internal/conversations/${threadId}/agents/${agentIds.bob}/close-thread`,
      { summary: 'done' },
      { headers: authHeaders(t.authToken) },
    );
    expect(rejected.status).toBe(409);
    const body = await rejected.json() as any;
    expect(body.error.code).toBe('conflict');
    expect(body.error.details.reason).toBe('has_open_tasks');
    expect(Array.isArray(body.error.details.blocking_tasks)).toBe(true);
    expect(body.error.details.blocking_tasks).toHaveLength(1);
    expect(body.error.details.blocking_tasks[0]).toMatchObject({
      task_number: taskNumber,
      status: 'in_progress',
      assignee_name: 'bob',
      assignee_agent_id: agentIds.bob,
    });
    // Thread-not-closed is asserted in the service unit test
    // (`rejects close when thread still has open tasks ...`). Here we
    // confirm the override path works back through the HTTP boundary.
    const overridden = await http(t).post(
      `/internal/conversations/${threadId}/agents/${agentIds.bob}/close-thread`,
      { summary: 'accepting residue', allow_open_tasks: true },
      { headers: authHeaders(t.authToken) },
    );
    expect(overridden.status).toBe(200);
  });
});
