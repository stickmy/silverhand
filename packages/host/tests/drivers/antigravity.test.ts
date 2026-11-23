import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { AntigravityDriver } from '../../src/drivers/antigravity.js';
import type { DriverSpawnContext } from '../../src/drivers/index.js';

const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

const CAPABILITIES = {
  loadSession: true,
  sessionCapabilities: { resume: {}, load: {} },
  _meta: { antigravity: { systemPrompt: { append: true } } },
};

async function setup(
  overrides: Partial<DriverSpawnContext> = {},
  capabilities: Record<string, unknown> = CAPABILITIES,
) {
  const root = mkdtempSync(join(tmpdir(), 'silverhand-agy-'));
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
  const driver = new AntigravityDriver();
  const sent: any[] = [];
  driver.bindIO({
    spawnCtx: ctx,
    writeLine: async (line) => {
      sent.push(JSON.parse(line));
    },
  });
  await Promise.resolve();
  const response = (id: number, result: unknown) =>
    driver.parseLine(JSON.stringify({ jsonrpc: '2.0', id, result }));
  const initialized = response(sent[0].id, { protocolVersion: 1, agentCapabilities: capabilities });
  return { driver, sent, response, initialized };
}

function chunk(driver: AntigravityDriver, text: string) {
  return driver.parseLine(
    JSON.stringify({
      jsonrpc: '2.0',
      method: 'session/update',
      params: {
        sessionId: 's',
        update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text } },
      },
    }),
  );
}

describe('Antigravity ACP driver', () => {
  it('spawns agy acp without SSH markers', () => {
    const driver = new AntigravityDriver();
    expect(driver.buildArgs()).toEqual(['acp', '--dangerously-skip-permissions']);
    const env = driver.applyEnv({
      PATH: '/bin',
      SSH_CLIENT: 'x',
      SSH_CONNECTION: 'y',
      SSH_TTY: 'z',
    });
    expect(env).toEqual({ PATH: '/bin', NO_COLOR: '1' });
  });

  it('appends the identity and profile MCP on a new session', async () => {
    const { sent } = await setup({
      profileMcp: {
        mcpServers: { docs: { type: 'http', url: 'https://mcp.test', headers: { A: 'b' } } },
      },
    });
    expect(sent[1].method).toBe('session/new');
    expect(sent[1].params._meta).toEqual({
      antigravity: { systemPrompt: { append: 'SILVERHAND IDENTITY' } },
    });
    expect(sent[1].params.mcpServers).toEqual([
      expect.objectContaining({
        type: 'http',
        url: 'https://mcp.test',
        headers: [{ name: 'A', value: 'b' }],
      }),
    ]);
  });

  it('refuses a runtime that cannot take the system prompt', async () => {
    const { initialized } = await setup({}, { loadSession: true });
    expect(initialized).toEqual([expect.objectContaining({ kind: 'error', fatal: true })]);
  });

  it('resumes the saved session with the identity reapplied', async () => {
    const { sent, response } = await setup({ cliSessionId: 'saved' });
    expect(sent[1].method).toBe('session/resume');
    expect(sent[1].params.sessionId).toBe('saved');
    expect(sent[1].params._meta.antigravity.systemPrompt.append).toBe('SILVERHAND IDENTITY');
    expect(response(sent[1].id, {})).toEqual([{ kind: 'session_init', sessionId: 'saved' }]);
  });

  it('sets the model, then its thinking level, and checks both took effect', async () => {
    const { sent, response } = await setup({ model: 'gemini-3.8-flash', reasoningEffort: 'low' });
    expect(response(sent[1].id, { sessionId: 's' })).toEqual([]);
    expect(sent[2]).toMatchObject({
      method: 'session/set_config_option',
      params: { sessionId: 's', configId: 'model', value: 'gemini-3.8-flash' },
    });
    expect(response(sent[2].id, {})).toEqual([]);
    expect(sent[3]).toMatchObject({
      method: 'session/set_config_option',
      params: { sessionId: 's', configId: 'thought_level', value: 'low' },
    });
    const options = [
      { id: 'model', currentValue: 'gemini-3.8-flash' },
      { id: 'thought_level', currentValue: 'low' },
    ];
    expect(response(sent[3].id, { configOptions: options })).toEqual([
      { kind: 'session_init', sessionId: 's' },
    ]);
  });

  it('fails the session when the requested model was not applied', async () => {
    const { sent, response } = await setup({ model: 'claude-opus-4-6-thinking' });
    response(sent[1].id, { sessionId: 's' });
    expect(
      response(sent[2].id, { configOptions: [{ id: 'model', currentValue: 'gemini-3.8-flash' }] }),
    ).toEqual([expect.objectContaining({ kind: 'error', fatal: true })]);
  });

  it('a mid-turn prompt joins the turn, which ends with the latest prompt', async () => {
    const { driver, sent, response } = await setup();
    response(sent[1].id, { sessionId: 's' });
    const first = JSON.parse(driver.encodeStdinMessage('first')!);
    expect(driver.readyToEncode()).toBe(true);
    const steer = JSON.parse(driver.encodeStdinMessage('steer')!);
    expect(steer.method).toBe('session/prompt');
    expect(steer.params.prompt).toEqual([{ type: 'text', text: 'steer' }]);
    // agy answers the joined prompt at once while the turn keeps running.
    expect(response(first.id, { stopReason: 'end_turn' })).toEqual([]);
    expect(response(steer.id, { stopReason: 'end_turn' })).toEqual([
      { kind: 'turn_end', sessionId: 's' },
    ]);
    expect(JSON.parse(driver.encodeInterruptControl() ?? 'null')).toBeNull();
  });

  it('reports a model failure chunk as an error, not a reply', async () => {
    const { driver, sent, response } = await setup();
    response(sent[1].id, { sessionId: 's' });
    expect(
      chunk(
        driver,
        '\n⚠ FAILED_PRECONDITION (code 400): User location is not supported for the API use.\n',
      ),
    ).toEqual([
      {
        kind: 'error',
        message: 'FAILED_PRECONDITION (code 400): User location is not supported for the API use.',
      },
    ]);
    expect(chunk(driver, 'All good ⚠ (code 400) here')).toEqual([
      { kind: 'text', text: 'All good ⚠ (code 400) here' },
    ]);
  });

  it('maps native tool frames to the shared tool names', async () => {
    const { driver, sent, response } = await setup();
    response(sent[1].id, { sessionId: 's' });
    const tool = (toolCallId: string, kind: string, rawInput: unknown, title = 't') =>
      driver.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'session/update',
          params: {
            sessionId: 's',
            update: {
              sessionUpdate: 'tool_call',
              toolCallId,
              kind,
              title,
              rawInput,
              status: 'pending',
            },
          },
        }),
      )[0];
    expect(
      tool('1', 'execute', { CommandLine: 'echo hi', WaitMsBeforeAsync: 5000, toolSummary: 'Run' }),
    ).toMatchObject({ name: 'Bash', input: { command: 'echo hi' } });
    expect(tool('2', 'read', { Path: '/w/a.txt' })).toMatchObject({
      name: 'Read',
      input: { file_path: '/w/a.txt' },
    });
    expect(tool('3', 'edit', { Path: '/w/b.txt', Content: 'x' })).toMatchObject({
      name: 'Write',
      input: { file_path: '/w/b.txt', content: 'x' },
    });
    expect(tool('4', 'edit', { Path: '/w/b.txt', Target: 'x', Replacement: 'y' })).toMatchObject({
      name: 'Edit',
      input: { file_path: '/w/b.txt', old_string: 'x', new_string: 'y' },
    });
    expect(tool('5', 'search', { query: 'q' })).toMatchObject({
      name: 'WebSearch',
      input: { query: 'q' },
    });
    expect(tool('6', 'other', { Foo: 1 }, 'Custom')).toMatchObject({
      name: 'Custom',
      input: { Foo: 1 },
    });
  });
});
