import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { GrokDriver } from '../../src/drivers/grok.js';
import type { DriverSpawnContext } from '../../src/drivers/index.js';

const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

async function setup(
  overrides: Partial<DriverSpawnContext> = {},
  capabilities = {
    sessionCapabilities: { resume: {} },
    loadSession: true,
    mcpCapabilities: { http: true, sse: true },
  },
  finalize = true,
) {
  const root = mkdtempSync(join(tmpdir(), 'silverhand-grok-'));
  roots.push(root);
  const systemPromptPath = join(root, 'prompt.md');
  writeFileSync(systemPromptPath, 'SILVERHAND IDENTITY');
  const ctx: DriverSpawnContext = {
    agentId: 'a',
    agentName: 'a',
    workspacePath: root,
    cliSessionId: null,
    model: null,
    reasoningEffort: null,
    serviceTier: null,
    systemPromptPath,
    initialPrompt: '',
    ...overrides,
  };
  const driver = new GrokDriver();
  const sent: any[] = [];
  driver.bindIO({
    spawnCtx: ctx,
    writeLine: async (line) => {
      sent.push(JSON.parse(line));
    },
  });
  await Promise.resolve();
  const rawResponse = (id: number, result: unknown) =>
    driver.parseLine(JSON.stringify({ jsonrpc: '2.0', id, result }));
  const response = (id: number, result: unknown) => {
    const count = sent.length;
    const events = rawResponse(id, result);
    const request = sent.at(-1);
    if (
      finalize &&
      count >= 2 &&
      sent.length > count &&
      ['session/resume', 'session/load'].includes(request.method)
    ) {
      return [...events, ...rawResponse(request.id, result)];
    }
    return events;
  };
  const initialized = response(sent[0].id, {
    protocolVersion: 1,
    agentCapabilities: capabilities,
    authMethods: [{ id: 'grok.com' }],
  });
  return { driver, ctx, sent, response, rawResponse, initialized };
}

function update(driver: GrokDriver, update: unknown) {
  return driver.parseLine(
    JSON.stringify({
      jsonrpc: '2.0',
      method: 'session/update',
      params: { sessionId: 's', update },
    }),
  );
}

describe('Grok ACP lifecycle', () => {
  it('does not authenticate; session/new authentication errors remain ordinary activity errors', async () => {
    const { driver, sent } = await setup();
    expect(sent.map((r) => r.method)).toEqual(['initialize', 'session/new']);
    const events = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        id: sent[1].id,
        error: { code: -32000, message: 'Authentication required' },
      }),
    );
    expect(events).toEqual([{ kind: 'error', message: 'Authentication required' }]);
    expect(driver.readyToEncode()).toBe(false);
    expect(driver.encodeStdinMessage('not delivered')).toBeNull();
  });

  it('uses independent busy interjection identities without consuming the active prompt slot', async () => {
    const { driver, sent, response } = await setup();
    expect(response(sent[1].id, { sessionId: 's' })).toEqual([
      { kind: 'session_init', sessionId: 's' },
    ]);
    const first = JSON.parse(driver.encodeStdinMessage('first')!);
    const left = JSON.parse(driver.encodeStdinMessage('same')!);
    const right = JSON.parse(driver.encodeStdinMessage('same')!);
    expect(first.method).toBe('session/prompt');
    expect(left.method).toBe('_x.ai/interject');
    expect(left.params.interjectionId).not.toBe(right.params.interjectionId);
    expect(left.params.text).toBe(right.params.text);
    expect(response(left.id, { status: 'queued' })).toEqual([]);
    expect(JSON.parse(driver.encodeInterruptControl()!).method).toBe('session/cancel');
    expect(
      driver.parseLine(
        JSON.stringify({ id: right.id, error: { code: -32602, message: 'session not found' } }),
      ),
    ).toEqual([{ kind: 'error', message: 'session not found' }]);
    expect(response(first.id, { stopReason: 'cancelled' })).toEqual([
      { kind: 'turn_end', sessionId: 's' },
    ]);
    expect(JSON.parse(driver.encodeStdinMessage('next')!).method).toBe('session/prompt');
  });

  it('resumes the exact identity without replaying historical activity', async () => {
    const { driver, sent, response } = await setup({ cliSessionId: 'saved-id' });
    expect(sent[1].method).toBe('session/resume');
    expect(sent[1].params.sessionId).toBe('saved-id');
    expect(sent[1].params._meta.systemPromptOverride).toBe('SILVERHAND IDENTITY');
    expect(
      update(driver, {
        sessionUpdate: 'agent_message_chunk',
        content: { type: 'text', text: 'old' },
      }),
    ).toEqual([]);
    expect(response(sent[1].id, {})).toEqual([{ kind: 'session_init', sessionId: 'saved-id' }]);
    expect(
      update(driver, {
        sessionUpdate: 'agent_message_chunk',
        content: { type: 'text', text: 'new' },
      }),
    ).toEqual([{ kind: 'text', text: 'new' }]);
  });

  it('negotiates load fallback and fails closed when neither restore method exists', async () => {
    const fallback = await setup({ cliSessionId: 'saved' }, { loadSession: true } as any);
    expect(fallback.sent[1].method).toBe('session/load');
    const refused = await setup({ cliSessionId: 'saved' }, {} as any);
    expect(refused.sent).toHaveLength(1);
    expect(refused.initialized).toEqual([expect.objectContaining({ kind: 'error', fatal: true })]);
  });

  it('injects additive MCP transports and identity for each fresh process', async () => {
    const { sent, driver, ctx } = await setup({
      profileMcp: {
        mcpServers: {
          local: { command: 'fixture', args: ['arg'], env: { LOCAL: 'value' } },
          remote: {
            type: 'http',
            url: 'http://127.0.0.1/mcp',
            headers: { Authorization: 'fixture' },
          },
          events: { type: 'sse', url: 'http://127.0.0.1/sse' },
        },
      },
    });
    expect(sent[1].params.mcpServers).toEqual([
      {
        name: 'silverhand_profile__local',
        command: 'fixture',
        args: ['arg'],
        env: [{ name: 'LOCAL', value: 'value' }],
      },
      {
        name: 'silverhand_profile__remote',
        type: 'http',
        url: 'http://127.0.0.1/mcp',
        headers: [{ name: 'Authorization', value: 'fixture' }],
      },
      { name: 'silverhand_profile__events', type: 'sse', url: 'http://127.0.0.1/sse', headers: [] },
    ]);
    expect(driver.buildArgs(ctx)).toEqual(['agent', '--no-leader', 'stdio']);
  });

  it('applies model then effort, checks the actual response, and refuses silent downgrade', async () => {
    const { driver, sent, response } = await setup({ model: 'grok-4.6', reasoningEffort: 'xhigh' });
    response(sent[1].id, { sessionId: 's' });
    expect(sent[2].params).toEqual({ sessionId: 's', configId: 'model', value: 'grok-4.6' });
    response(sent[2].id, { configOptions: [{ id: 'model', currentValue: 'grok-4.6' }] });
    expect(sent[3].params.configId).toBe('reasoning_effort');
    const events = response(sent[3].id, {
      configOptions: [{ id: 'model', currentValue: 'grok-4.6' }],
    });
    expect(events).toEqual([expect.objectContaining({ kind: 'error', fatal: true })]);
    expect(driver.readyToEncode()).toBe(false);
  });

  it('becomes ready only after the requested model and effort are confirmed', async () => {
    const { driver, sent, response } = await setup({ model: 'grok-4.6', reasoningEffort: 'high' });
    response(sent[1].id, { sessionId: 's' });
    expect(driver.readyToEncode()).toBe(false);

    response(sent[2].id, { configOptions: [{ id: 'model', currentValue: 'grok-4.6' }] });
    expect(driver.readyToEncode()).toBe(false);
    expect(
      response(sent[3].id, {
        configOptions: [
          { id: 'model', currentValue: 'grok-4.6' },
          { id: 'reasoning_effort', currentValue: 'high' },
        ],
      }),
    ).toEqual([{ kind: 'session_init', sessionId: 's' }]);
    expect(driver.readyToEncode()).toBe(true);
  });

  it('rejects an unadvertised remote MCP transport before session creation', async () => {
    const result = await setup(
      {
        profileMcp: { mcpServers: { remote: { type: 'http', url: 'http://127.0.0.1/mcp' } } },
      },
      { mcpCapabilities: { http: false } } as any,
    );
    expect(result.sent.map((request) => request.method)).toEqual(['initialize']);
    expect(result.initialized).toEqual([
      expect.objectContaining({
        kind: 'error',
        fatal: true,
        message: 'Grok does not advertise MCP http support.',
      }),
    ]);
  });

  it('preserves structured tool input and markers without output body', async () => {
    const { driver, sent, response } = await setup();
    response(sent[1].id, { sessionId: 's' });
    const input = { commands: Array.from({ length: 500 }, () => 'x'.repeat(100)) };
    const events = update(driver, {
      sessionUpdate: 'tool_call',
      toolCallId: 't',
      title: 'shell',
      rawInput: input,
    });
    expect(events[0]).toMatchObject({ kind: 'tool_call', input });
    const output = update(driver, {
      sessionUpdate: 'tool_call_update',
      toolCallId: 't',
      status: 'completed',
      content: [{ type: 'content', content: { type: 'text', text: 'PRIVATE OUTPUT' } }],
    });
    expect(JSON.stringify(output)).not.toContain('PRIVATE OUTPUT');
    expect(output).toContainEqual(expect.objectContaining({ kind: 'tool_output' }));
  });

  it('keeps input blocked until identity reattachment completes, and refuses reattachment errors', async () => {
    const { driver, sent, response } = await setup({ model: 'grok-4.6' }, undefined, false);
    response(sent[1].id, { sessionId: 's' });
    expect(
      response(sent[2].id, { configOptions: [{ id: 'model', currentValue: 'grok-4.6' }] }),
    ).toEqual([]);
    expect(sent[3]).toMatchObject({
      method: 'session/resume',
      params: {
        sessionId: 's',
        _meta: { systemPromptOverride: 'SILVERHAND IDENTITY', 'x.ai/restore_code': false },
      },
    });
    expect(driver.encodeStdinMessage('too early')).toBeNull();
    expect(
      driver.parseLine(
        JSON.stringify({ id: sent[3].id, error: { code: -32000, message: 'attach failed' } }),
      ),
    ).toEqual([{ kind: 'error', message: 'attach failed' }]);
    expect(driver.readyToEncode()).toBe(false);
  });

  it('uses one new session without config or restore capabilities for the default model', async () => {
    const { driver, sent, response } = await setup({}, {} as any, false);
    expect(response(sent[1].id, { sessionId: 'default' })).toEqual([
      { kind: 'session_init', sessionId: 'default' },
    ]);
    expect(sent.map((request) => request.method)).toEqual(['initialize', 'session/new']);
    expect(driver.readyToEncode()).toBe(true);
  });

  it('applies reasoning-only configuration without reattaching', async () => {
    const { driver, sent, response } = await setup({ reasoningEffort: 'high' }, {} as any, false);
    expect(response(sent[1].id, { sessionId: 'default' })).toEqual([]);
    expect(sent[2].params).toEqual({
      sessionId: 'default',
      configId: 'reasoning_effort',
      value: 'high',
    });
    expect(
      response(sent[2].id, { configOptions: [{ id: 'reasoning_effort', currentValue: 'high' }] }),
    ).toEqual([{ kind: 'session_init', sessionId: 'default' }]);
    expect(sent.map((request) => request.method)).toEqual([
      'initialize',
      'session/new',
      'session/set_config_option',
    ]);
    expect(driver.readyToEncode()).toBe(true);
  });

  it('declares its version floor for the supervisor gate', () => {
    expect(new GrokDriver().minimumVersion).toBe('1.0.24');
  });
});
