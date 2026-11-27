import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { AntigravityDriver } from '../../src/drivers/antigravity.js';
import { parseAcpAutoCompactThreshold } from '../../src/drivers/acp.js';
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

  it('parses SILVERHAND_ACP_AUTO_COMPACT formats matching agy', () => {
    expect(parseAcpAutoCompactThreshold('700000')).toEqual({ kind: 'tokens', tokens: 700_000 });
    expect(parseAcpAutoCompactThreshold('800K')).toEqual({ kind: 'tokens', tokens: 800_000 });
    expect(parseAcpAutoCompactThreshold('0.5M')).toEqual({ kind: 'tokens', tokens: 500_000 });
    expect(parseAcpAutoCompactThreshold('80%')).toEqual({ kind: 'percent', ratio: 0.8 });
    expect(parseAcpAutoCompactThreshold(' 200k ')).toEqual({ kind: 'tokens', tokens: 200_000 });
    expect(parseAcpAutoCompactThreshold('')).toBeNull();
    expect(parseAcpAutoCompactThreshold('0')).toBeNull();
    expect(parseAcpAutoCompactThreshold('-100')).toBeNull();
    expect(parseAcpAutoCompactThreshold('101%')).toBeNull();
    expect(parseAcpAutoCompactThreshold('abc')).toBeNull();
  });

  it('triggers /compact at the first idle point after usage crosses the threshold and re-arms only after dropping below', async () => {
    const { driver, sent, response } = await setup({ acpAutoCompactThreshold: '200K' });
    response(sent[1].id, { sessionId: 's' });

    const usage = (used: number, size = 1_048_576) =>
      driver.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'session/update',
          params: { sessionId: 's', update: { sessionUpdate: 'usage_update', used, size } },
        }),
      );

    // Start turn 1 and cross the 200K threshold mid-turn.
    const turn1 = JSON.parse(driver.encodeStdinMessage('hello')!);
    expect(usage(198_000)).toEqual([]);
    expect(usage(203_000)).toEqual([]);
    // Must wait until the turn finishes (never send /compact mid-turn).
    expect(sent.filter((m) => m.method === 'session/prompt')).toHaveLength(0);

    // Turn finishes → emits turn_end + compaction_started and immediately dispatches /compact.
    expect(response(turn1.id, { stopReason: 'end_turn' })).toEqual([
      { kind: 'turn_end', sessionId: 's' },
      { kind: 'compaction_started' },
    ]);
    const compactReq = sent.at(-1);
    expect(compactReq).toMatchObject({
      method: 'session/prompt',
      params: { sessionId: 's', prompt: [{ type: 'text', text: '/compact' }] },
    });
    // While compacting, driver pauses new deliveries and suppresses transient chunks.
    expect(driver.readyToEncode()).toBe(false);
    expect(driver.encodeStdinMessage('queued-by-supervisor')).toBeNull();
    expect(chunk(driver, 'internal compact output')).toEqual([]);

    // Compaction completes → emits compaction_finished + turn_end and reopens readyToEncode.
    expect(response(compactReq.id, { stopReason: 'end_turn' })).toEqual([
      { kind: 'compaction_finished' },
      { kind: 'turn_end', sessionId: 's' },
    ]);
    expect(driver.readyToEncode()).toBe(true);

    // Staying above threshold must NOT loop compaction.
    const turn2 = JSON.parse(driver.encodeStdinMessage('next')!);
    usage(210_000);
    response(turn2.id, { stopReason: 'end_turn' });
    expect(sent.filter((m) => m.params?.prompt?.[0]?.text === '/compact')).toHaveLength(1);

    // Dropping below threshold re-arms; next crossing compacts again.
    const turn3 = JSON.parse(driver.encodeStdinMessage('after drop')!);
    usage(80_000);
    usage(205_000);
    expect(response(turn3.id, { stopReason: 'end_turn' })).toEqual([
      { kind: 'turn_end', sessionId: 's' },
      { kind: 'compaction_started' },
    ]);
    expect(sent.filter((m) => m.params?.prompt?.[0]?.text === '/compact')).toHaveLength(2);
  });

  it('emits compaction lifecycle and error events on idle trigger, RPC failure, and model error chunk', async () => {
    const warnings: Array<{ action: string; message: string }> = [];
    const root = mkdtempSync(join(tmpdir(), 'silverhand-agy-compact-err-'));
    roots.push(root);
    const systemPromptPath = join(root, 'prompt.md');
    writeFileSync(systemPromptPath, 'SILVERHAND IDENTITY');
    const driver = new AntigravityDriver();
    const sent: any[] = [];
    driver.bindIO({
      spawnCtx: {
        agentId: 'a',
        agentName: 'a',
        workspacePath: root,
        cliSessionId: null,
        model: null,
        reasoningEffort: null,
        serviceTier: null,
        systemPromptPath,
        initialPrompt: '',
        acpAutoCompactThreshold: '30k',
      },
      writeLine: async (line) => {
        sent.push(JSON.parse(line));
      },
      onWarning: (ev) => warnings.push({ action: ev.action, message: ev.message }),
    });
    await Promise.resolve();
    const response = (id: number, body: Record<string, unknown>) =>
      driver.parseLine(JSON.stringify({ jsonrpc: '2.0', id, ...body }));
    const usage = (used: number) =>
      driver.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'session/update',
          params: {
            sessionId: 's',
            update: { sessionUpdate: 'usage_update', used, size: 1_048_576 },
          },
        }),
      );

    response(sent[0].id, { result: { protocolVersion: 1, agentCapabilities: CAPABILITIES } });
    response(sent[1].id, { result: { sessionId: 's' } });

    // Idle usage crossing emits compaction_started immediately.
    expect(usage(35_000)).toEqual([{ kind: 'compaction_started' }]);
    const firstCompact = sent.at(-1);
    expect(
      response(firstCompact.id, { error: { code: -32000, message: 'compact rpc failed' } }),
    ).toEqual([
      { kind: 'compaction_finished' },
      { kind: 'error', message: 'compact rpc failed' },
      { kind: 'turn_end', sessionId: 's' },
    ]);
    expect(warnings).toEqual([
      { action: 'acp_auto_compact_failed', message: 'compact rpc failed' },
    ]);
    expect(driver.readyToEncode()).toBe(true);

    // Re-arm and fail via an Antigravity model-error message chunk during /compact.
    expect(usage(10_000)).toEqual([]);
    expect(usage(32_000)).toEqual([{ kind: 'compaction_started' }]);
    const secondCompact = sent.at(-1);
    expect(
      chunk(driver, '\n⚠ RESOURCE_EXHAUSTED (code 429): Quota exceeded while compacting.\n'),
    ).toEqual([]);
    expect(response(secondCompact.id, { result: { stopReason: 'end_turn' } })).toEqual([
      { kind: 'compaction_finished' },
      { kind: 'error', message: 'RESOURCE_EXHAUSTED (code 429): Quota exceeded while compacting.' },
      { kind: 'turn_end', sessionId: 's' },
    ]);
    expect(warnings.at(-1)).toEqual({
      action: 'acp_auto_compact_failed',
      message: 'RESOURCE_EXHAUSTED (code 429): Quota exceeded while compacting.',
    });
  });

  it('supports percentage thresholds against usage_update.size and logs a warning on invalid values', async () => {
    const warnings: string[] = [];
    const root = mkdtempSync(join(tmpdir(), 'silverhand-agy-warn-'));
    roots.push(root);
    const systemPromptPath = join(root, 'prompt.md');
    writeFileSync(systemPromptPath, 'SILVERHAND IDENTITY');
    const invalidDriver = new AntigravityDriver();
    invalidDriver.bindIO({
      spawnCtx: {
        agentId: 'a',
        agentName: 'a',
        workspacePath: root,
        cliSessionId: null,
        model: null,
        reasoningEffort: null,
        serviceTier: null,
        systemPromptPath,
        initialPrompt: '',
        acpAutoCompactThreshold: 'not-a-threshold',
      },
      writeLine: async () => {},
      onWarning: (ev) => warnings.push(ev.action),
    });
    expect(warnings).toEqual(['acp_auto_compact_invalid_threshold']);

    const { driver, sent, response } = await setup({ acpAutoCompactThreshold: '50%' });
    response(sent[1].id, { sessionId: 's' });
    const turn = JSON.parse(driver.encodeStdinMessage('hi')!);
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'session/update',
        params: {
          sessionId: 's',
          update: { sessionUpdate: 'usage_update', used: 500_000, size: 1_000_000 },
        },
      }),
    );
    response(turn.id, { stopReason: 'end_turn' });
    expect(sent.at(-1)).toMatchObject({
      method: 'session/prompt',
      params: { sessionId: 's', prompt: [{ type: 'text', text: '/compact' }] },
    });
  });
});
