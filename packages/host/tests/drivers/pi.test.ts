import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { PiDriver } from '../../src/drivers/pi.js';
import type { DriverSpawnContext } from '../../src/drivers/index.js';
import { ParsedEventSchema } from '@silverhand/shared';
const dirs: string[] = [];
afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
});
function fixture(effort: string | null = null) {
  const dir = mkdtempSync(join(tmpdir(), 'pi-driver-'));
  dirs.push(dir);
  const ctx: DriverSpawnContext = {
    agentId: 'agent',
    agentName: 'Pi',
    workspacePath: dir,
    cliSessionId: null,
    model: null,
    reasoningEffort: effort,
    fastMode: null,
    systemPromptPath: join(dir, 'prompt.md'),
    initialPrompt: '',
  };
  writeFileSync(ctx.systemPromptPath, 'standing prompt');
  const d = new PiDriver();
  const args = d.buildArgs(ctx);
  const id = args[args.indexOf('--session-id') + 1];
  const lines: any[] = [];
  d.bindIO({
    spawnCtx: ctx,
    writeLine: async (line) => {
      lines.push(JSON.parse(line));
    },
  });
  const reply = (data: unknown, success = true) =>
    d.parseLine(
      JSON.stringify({
        type: 'response',
        id: lines.at(-1).id,
        success,
        data,
        error: 'fixture failure',
      }),
    );
  const extension = args[args.indexOf('--extension') + 1]!;
  const commands = (path = extension) =>
    reply({
      commands: [{ name: 'silverhand-deliver', source: 'extension', sourceInfo: { path } }],
    });
  const init = () => {
    reply({ sessionId: id });
    return commands();
  };
  return { d, ctx, args, id, lines, reply, extension, commands, init };
}
describe('Pi RPC driver', () => {
  it('isolates exact session coordinates, preserves configuration, and declares its version floor', () => {
    const { d, ctx, args, id } = fixture();
    expect(args).toContain('--append-system-prompt');
    expect(args).not.toContain('--approve');
    expect(args).not.toContain('--session-dir');
    expect(d.buildArgs({ ...ctx, cliSessionId: id! })).toEqual(args);
    expect(d.applyEnv({ PI_CODING_AGENT_DIR: '/configured' }).PI_CODING_AGENT_DIR).toBe(
      '/configured',
    );
    expect(d.minimumVersion).toBe('1.0.0');
  });
  it('requires its own delivery extension before reporting ready', () => {
    const { d, reply, id, lines, extension, commands } = fixture();
    expect(readFileSync(extension, 'utf8')).toContain('registerCommand("silverhand-deliver"');
    expect(reply({ sessionId: id })).toEqual([]);
    expect(lines.at(-1).type).toBe('get_commands');
    expect(commands('/elsewhere/other.mjs')[0]).toMatchObject({ kind: 'error', fatal: true });
    expect(d.readyToEncode()).toBe(false);
    const good = fixture();
    good.reply({ sessionId: good.id });
    expect(good.commands()).toEqual([{ kind: 'session_init', sessionId: good.id }]);
  });
  it('delivers through the extension and settles only on the echoed payload id', () => {
    const { d, init } = fixture();
    expect(d.encodeStdinMessage('early', null, { payloadUuid: 'u0' })).toBeNull();
    init();
    expect(d.admissionPolicy).toBe('lifecycle-start');
    expect(d.encodeStdinMessage('no id', null)).toBeNull();
    const frame = JSON.parse(
      d.encodeStdinMessage('/not-a-command "quoted"\nline', null, { payloadUuid: 'u1' })!,
    );
    expect(frame).toMatchObject({ type: 'prompt', streamingBehavior: 'steer' });
    const [command, ...rest] = frame.message.split(' ');
    expect(command).toBe('/silverhand-deliver');
    expect(JSON.parse(rest.join(' '))).toEqual({
      uuid: 'u1',
      content: '/not-a-command "quoted"\nline',
    });
    expect(
      d.parseLine(
        JSON.stringify({
          type: 'response',
          id: frame.id,
          success: true,
          data: { disposition: 'handled' },
        }),
      ),
    ).toEqual([]);
    const custom = (type: string, details: unknown, customType = 'silverhand_delivery') =>
      d.parseLine(
        JSON.stringify({ type, message: { role: 'custom', customType, content: 'x', details } }),
      );
    expect(custom('message_start', { payloadUuid: 'u1' })).toEqual([
      { kind: 'input_lifecycle', commandUuid: 'u1', state: 'started' },
    ]);
    expect(custom('message_end', { payloadUuid: 'u1' })).toEqual([
      { kind: 'input_lifecycle', commandUuid: 'u1', state: 'completed' },
    ]);
    expect(custom('message_start', { payloadUuid: 'u1' }, 'other')).toEqual([]);
    expect(custom('message_start', {})).toEqual([]);
    expect(
      d.parseLine(
        JSON.stringify({ type: 'message_start', message: { role: 'user', content: 'u1' } }),
      ),
    ).toEqual([]);
    for (const e of [...custom('message_start', { payloadUuid: 'u1' })])
      expect(ParsedEventSchema.safeParse(e).success).toBe(true);
  });
  it('pauses compaction, diagnoses rejection, and ends turns on settle', () => {
    const { d, init } = fixture();
    init();
    const first = JSON.parse(d.encodeStdinMessage('same', null, { payloadUuid: 'a' })!);
    const second = JSON.parse(d.encodeStdinMessage('same', null, { payloadUuid: 'b' })!);
    expect(first.id).not.toBe(second.id);
    expect(
      d.parseLine(
        JSON.stringify({ type: 'response', id: second.id, success: false, error: 'rejected' }),
      )[0],
    ).toMatchObject({ kind: 'error', code: 'pi_rpc_rejected', fatal: false });
    expect(d.parseLine('{"type":"compaction_start"}')).toEqual([{ kind: 'compaction_started' }]);
    expect(d.encodeStdinMessage('blocked', null, { payloadUuid: 'c' })).toBeNull();
    d.parseLine('{"type":"compaction_end"}');
    expect(d.readyToEncode()).toBe(true);
    expect(d.parseLine('{"type":"agent_end"}')).toEqual([]);
    expect(d.parseLine('{"type":"auto_retry_start"}')[0]).toMatchObject({
      kind: 'api_response',
      status: 'waiting',
    });
    expect(d.parseLine('{"type":"auto_retry_end"}')[0]).toMatchObject({
      kind: 'api_response',
      status: 'ready',
    });
    for (let i = 0; i < 300; i++)
      d.encodeStdinMessage('bounded diagnostics', null, { payloadUuid: `p${i}` });
    expect(
      d.parseLine(
        JSON.stringify({ type: 'response', id: first.id, success: false, error: 'late rejection' }),
      )[0],
    ).toMatchObject({ kind: 'error', code: 'pi_rpc_rejected' });
    expect(d.parseLine('{"type":"agent_settled"}')[0]?.kind).toBe('turn_end');
  });
  it('validates actual thinking capability and applied value before initialization', () => {
    const { d, init, lines, reply } = fixture('high');
    expect(init()).toEqual([]);
    expect(lines.at(-1).type).toBe('get_available_thinking_levels');
    expect(reply({ levels: ['off'] })[0]).toMatchObject({ kind: 'error', fatal: true });
    expect(d.readyToEncode()).toBe(false);
    const good = fixture('high');
    good.init();
    good.reply({ levels: ['off', 'high'] });
    expect(good.lines.at(-1)).toMatchObject({ type: 'set_thinking_level', level: 'high' });
    good.reply({});
    expect(good.reply({ sessionId: good.id, thinkingLevel: 'off' })[0]).toMatchObject({
      fatal: true,
    });
  });
  it('deduplicates tool starts, preserves structured input, and never forwards output bodies', () => {
    const { d } = fixture();
    const tool = {
      type: 'tool_execution_start',
      toolCallId: 'call',
      toolName: 'bash',
      args: { command: 'x'.repeat(50_000) },
    };
    const event = d.parseLine(JSON.stringify(tool))[0]!;
    expect(event.kind).toBe('tool_call');
    expect(event).toMatchObject({ input: tool.args });
    expect(d.parseLine(JSON.stringify(tool))).toEqual([]);
    const end = d.parseLine(
      JSON.stringify({
        type: 'tool_execution_end',
        toolCallId: 'call',
        toolName: 'bash',
        isError: false,
        result: { content: 'private result' },
      }),
    );
    expect(end).toEqual([
      { kind: 'tool_output', toolCallId: 'call', name: 'bash', outcome: 'success' },
    ]);
    for (const e of [event, ...end]) expect(ParsedEventSchema.safeParse(e).success).toBe(true);
  });
  it('preserves large arrays and complete text/thinking deltas for common downstream limits', () => {
    const { d } = fixture();
    const input = Array.from({ length: 60 }, (_, i) => ({ index: i, text: 'x'.repeat(1000) }));
    expect(
      d.parseLine(
        JSON.stringify({
          type: 'tool_execution_start',
          toolCallId: 'array',
          toolName: 'tool',
          args: input,
        }),
      )[0],
    ).toMatchObject({ input });
    for (const type of ['text_delta', 'thinking_delta']) {
      const delta = 'x'.repeat(20000) + 'END';
      expect(
        d.parseLine(
          JSON.stringify({ type: 'message_update', assistantMessageEvent: { type, delta } }),
        )[0],
      ).toMatchObject({ text: delta });
    }
  });
  it('cancels extension dialogs explicitly and clears queues before abort', () => {
    const { d, lines, init } = fixture();
    init();
    const event = d.parseLine('{"type":"extension_ui_request","id":"dialog","method":"confirm"}');
    expect(lines.at(-1)).toEqual({ type: 'extension_ui_response', id: 'dialog', cancelled: true });
    expect(event[0]).toMatchObject({ code: 'pi_extension_interaction_cancelled' });
    const controls = d
      .encodeInterruptControl()
      .split('\n')
      .map((x) => JSON.parse(x));
    expect(controls.map((x) => x.type)).toEqual(['clear_queue', 'abort']);
    expect(d.readyToEncode()).toBe(false);
    d.parseLine(JSON.stringify({ type: 'response', id: controls[1].id, success: true }));
    expect(d.readyToEncode()).toBe(true);
  });
});
