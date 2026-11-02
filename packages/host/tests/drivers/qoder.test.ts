import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';

import type { DriverSpawnContext } from '../../src/drivers/index.js';
import { QoderDriver } from '../../src/drivers/qoder.js';

function makeSpawnCtx(overrides: Partial<DriverSpawnContext> = {}): {
  ctx: DriverSpawnContext;
  cleanup: () => void;
} {
  const tmp = mkdtempSync(join(tmpdir(), 'qoder-driver-test-'));
  const systemPromptPath = join(tmp, 'system-prompt.md');
  writeFileSync(systemPromptPath, 'STANDING QODER PROMPT\n', 'utf-8');

  const ctx: DriverSpawnContext = {
    agentId: 'agent-1',
    agentName: 'tester',
    workspacePath: tmp,
    cliSessionId: null,
    model: null,
    reasoningEffort: null,
    fastMode: null,
    systemPromptPath,
    initialPrompt: '',
    ...overrides,
  };

  return {
    ctx,
    cleanup: () => {
      rmSync(tmp, { recursive: true, force: true });
    },
  };
}

describe('QoderDriver.buildArgs', () => {
  let cleanup: (() => void) | null = null;
  afterEach(() => {
    cleanup?.();
    cleanup = null;
  });

  it('inlines the standing prompt via --append-system-prompt (no file flag)', () => {
    const built = makeSpawnCtx();
    cleanup = built.cleanup;

    const args = new QoderDriver().buildArgs(built.ctx);
    const idx = args.indexOf('--append-system-prompt');

    expect(idx).toBeGreaterThanOrEqual(0);
    expect(args[idx + 1]).toBe('STANDING QODER PROMPT\n');
    // Qoder has no --append-system-prompt-file; the path must NOT leak in.
    expect(args).not.toContain('--append-system-prompt-file');
    expect(args).not.toContain(built.ctx.systemPromptPath);
  });

  it('uses snake_case bypass_permissions and omits claude-only flags', () => {
    const built = makeSpawnCtx();
    cleanup = built.cleanup;

    const args = new QoderDriver().buildArgs(built.ctx);
    const pIdx = args.indexOf('--permission-mode');

    expect(args[pIdx + 1]).toBe('bypass_permissions');
    expect(args).toContain('--dangerously-skip-permissions');
    // Qoder's arg parser rejects these claude-only flags (prints usage).
    expect(args).not.toContain('--verbose');
    expect(args).not.toContain('--allow-dangerously-skip-permissions');
  });

  it('denies Silverhand-conflicting plan / cron / goal / ask-user tools', () => {
    const built = makeSpawnCtx();
    cleanup = built.cleanup;

    const args = new QoderDriver().buildArgs(built.ctx);
    const idx = args.indexOf('--disallowed-tools');
    const list = args[idx + 1];

    for (const tool of [
      'EnterPlanMode',
      'ExitPlanMode',
      'CronCreate',
      'CronList',
      'CronDelete',
      'CreateGoal',
      'GetGoal',
      'UpdateGoal',
      'AskUserQuestion',
    ]) {
      expect(list).toContain(tool);
    }
  });

  it('omits --model when unset and passes it through when set', () => {
    const noModel = makeSpawnCtx();
    expect(new QoderDriver().buildArgs(noModel.ctx)).not.toContain('--model');
    noModel.cleanup();

    const withModel = makeSpawnCtx({ model: 'claude-sonnet-4.5' });
    cleanup = withModel.cleanup;
    const args = new QoderDriver().buildArgs(withModel.ctx);
    const idx = args.indexOf('--model');
    expect(args[idx + 1]).toBe('claude-sonnet-4.5');
  });

  it('adds --resume only when a cliSessionId is present', () => {
    const fresh = makeSpawnCtx();
    expect(new QoderDriver().buildArgs(fresh.ctx)).not.toContain('--resume');
    fresh.cleanup();

    const resumed = makeSpawnCtx({ cliSessionId: 'sess-42' });
    cleanup = resumed.cleanup;
    const args = new QoderDriver().buildArgs(resumed.ctx);
    const idx = args.indexOf('--resume');
    expect(args[idx + 1]).toBe('sess-42');
  });

  it('passes stream-json in both directions and no MCP config (core driver)', () => {
    const built = makeSpawnCtx();
    cleanup = built.cleanup;

    const args = new QoderDriver().buildArgs(built.ctx);
    expect(args[args.indexOf('--output-format') + 1]).toBe('stream-json');
    expect(args[args.indexOf('--input-format') + 1]).toBe('stream-json');
    expect(args).not.toContain('--mcp-config');
  });

  it('writes profile MCP next to the standing prompt and passes --mcp-config', () => {
    const built = makeSpawnCtx({
      profileMcp: { mcpServers: { fs: { command: 'npx', args: ['server-fs'] } } },
    });
    cleanup = built.cleanup;

    const args = new QoderDriver().buildArgs(built.ctx);
    const path = args[args.indexOf('--mcp-config') + 1]!;
    expect(path).toBe(join(built.ctx.workspacePath, 'profile-mcp.json'));
    expect(args).not.toContain('--strict-mcp-config');
    expect(JSON.parse(readFileSync(path, 'utf-8')).mcpServers).toHaveProperty(
      'silverhand_profile__fs',
    );
  });
});

describe('QoderDriver.encode', () => {
  it('encodes a stream-json user turn, threading the session id', () => {
    const line = new QoderDriver().encodeStdinMessage('hello', 'sess-1');
    expect(JSON.parse(line!)).toEqual({
      type: 'user',
      message: { role: 'user', content: [{ type: 'text', text: 'hello' }] },
      session_id: 'sess-1',
    });
  });

  it('omits session_id when null', () => {
    const line = new QoderDriver().encodeStdinMessage('hi', null);
    expect(JSON.parse(line!).session_id).toBeUndefined();
  });

  it('has no verified control-request interrupt (supervisor uses signals)', () => {
    expect(new QoderDriver().encodeInterruptControl()).toBeNull();
  });
});

// ===========================================================================
// CONTRACT (Do Not Weaken) — version-gated release profile, #task:1909
//
// WHY THIS EXISTS: `priority:"next"` (non-destructive mid-turn steer) is
// VERIFIED on qodercli 1.1.17 (#task:1907), and shiki chose that version as
// the conservative enable floor — it is NOT known to be the first release
// that shipped the feature. The one build with negative evidence is 1.0.48
// (#task:1556): it accepts the field, but its headless handler never
// forwards/honours it, and no `command_lifecycle` echo comes back. Anything
// between the two is untested, hence unproven. Claiming
// `tool-boundary-steer` on an unproven build would make the daemon release
// input the runtime cannot admit mid-turn — silently losing the admission
// proof that the freshness CAS depends on. So the gate's job is to admit
// only what is at/above the floor.
//
// INVARIANTS:
//  1. The gate FAILS CLOSED for every input that is not provably >= the
//     floor: null (probe failed), unparseable text, and short forms like
//     `1.1` where a naive string/float compare would say "newer".
//  2. The two decisions move TOGETHER — release profile and wire frame come
//     from one resolved capability. A version that yields
//     `tool-boundary-steer` must also stamp `priority` + the payload uuid,
//     and one that yields `next-turn-batch` must stamp neither.
//  3. An unbound instance behaves like an old CLI. Anything reading
//     `releaseProfile` off a driver nobody bound must get the conservative
//     answer, never a steer claim.
//
// REVIEW GATE for changes here: is the floor still the version whose
// `priority` support was empirically verified? Does every non-provable
// version still fail closed? Do profile and frame still agree? If you raise
// the floor, raise it with evidence, not with a guess.
// ===========================================================================
describe('QoderDriver version-gated release profile (#task:1909)', () => {
  it('defaults to next-turn-batch before any version is bound', () => {
    const driver = new QoderDriver();
    expect(driver.releaseProfile).toBe('next-turn-batch');
    expect(driver.admissionPolicy).toBe('write-success');
  });

  it('claims tool-boundary-steer at and above the verified floor', () => {
    for (const version of ['1.1.17', '1.1.18', '1.2.0', '2.0.0', 'v1.1.17']) {
      const driver = new QoderDriver();
      driver.bindRuntimeVersion(version);
      expect(driver.releaseProfile, version).toBe('tool-boundary-steer');
      expect(driver.admissionPolicy, version).toBe('lifecycle-start');
    }
  });

  it('fails closed below the floor and on anything unprovable', () => {
    // `1.1` and `1.1.9` are the cases a string / float compare gets wrong:
    // missing components count as 0, and components compare numerically.
    for (const version of [
      '1.1.16',
      '1.1.9',
      '1.1',
      '1',
      '0.9.99',
      null,
      '',
      'qodercli (build 9.9.9)',
      'unknown',
    ]) {
      const driver = new QoderDriver();
      driver.bindRuntimeVersion(version);
      expect(driver.releaseProfile, String(version)).toBe('next-turn-batch');
      expect(driver.admissionPolicy, String(version)).toBe('write-success');
    }
  });

  it('stamps priority + payload uuid only on the steer-capable frame', () => {
    const steer = new QoderDriver();
    steer.bindRuntimeVersion('1.1.17');
    const steerFrame = JSON.parse(
      steer.encodeStdinMessage('hello', 'sess-1', { payloadUuid: 'uuid-1' })!,
    );
    expect(steerFrame.priority).toBe('next');
    expect(steerFrame.uuid).toBe('uuid-1');

    const legacy = new QoderDriver();
    legacy.bindRuntimeVersion('1.1.16');
    const legacyFrame = JSON.parse(
      legacy.encodeStdinMessage('hello', 'sess-1', { payloadUuid: 'uuid-1' })!,
    );
    expect(legacyFrame.priority).toBeUndefined();
    expect(legacyFrame.uuid).toBeUndefined();
  });
});

describe('QoderDriver.classifyInitProgress', () => {
  it('treats any complete stream-json frame as protocol-ready', () => {
    expect(
      new QoderDriver().classifyInitProgress(
        JSON.stringify({ type: 'system', subtype: 'hook_started' }),
      ),
    ).toBe('protocol_ready');
  });

  it('treats system init as session-ready', () => {
    expect(
      new QoderDriver().classifyInitProgress(
        JSON.stringify({ type: 'system', subtype: 'init', session_id: 'q-1' }),
      ),
    ).toBe('session_ready');
  });
});

describe('QoderDriver.parseLine', () => {
  it('drops Qoder hook frames silently (unmodeled system subtypes)', () => {
    expect(
      new QoderDriver().parseLine(JSON.stringify({ type: 'system', subtype: 'hook_started' })),
    ).toEqual([]);
  });

  it('projects model queue updates as normalized api_response events (#task:1582)', () => {
    const driver = new QoderDriver();

    expect(
      driver.parseLine(
        JSON.stringify({
          type: 'system',
          subtype: 'model_queue_status',
          status: 'queued',
          queue_count: 573,
          wait_time_ms: 747_048,
          request_id: 'must-not-surface',
        }),
      ),
    ).toEqual([
      {
        kind: 'api_response',
        status: 'waiting',
        message: 'Qoder model request queued (queue size 573, estimated wait 12m 27s)',
      },
    ]);

    expect(
      driver.parseLine(
        JSON.stringify({
          type: 'system',
          subtype: 'model_queue_status',
          status: 'ready',
        }),
      ),
    ).toEqual([
      {
        kind: 'api_response',
        status: 'ready',
        message: 'Qoder model is available; resuming request',
      },
    ]);
  });

  it('keeps malformed queue metadata bounded and does not expose identifiers', () => {
    expect(
      new QoderDriver().parseLine(
        JSON.stringify({
          type: 'system',
          subtype: 'model_queue_status',
          status: 'queued',
          queue_count: 'many',
          wait_time_ms: -1,
          request_id: 'request-secret',
        }),
      ),
    ).toEqual([
      {
        kind: 'api_response',
        status: 'waiting',
        message: 'Qoder model request queued',
      },
    ]);
  });

  it('projects assistant thinking / text / tool_use blocks', () => {
    const events = new QoderDriver().parseLine(
      JSON.stringify({
        type: 'assistant',
        message: {
          role: 'assistant',
          content: [
            { type: 'thinking', thinking: 'pondering' },
            { type: 'text', text: 'answer' },
            {
              type: 'tool_use',
              id: 'call_1',
              name: 'Bash',
              input: { command: 'echo hi' },
            },
          ],
        },
      }),
    );

    expect(events).toEqual([
      { kind: 'thinking', text: 'pondering' },
      { kind: 'text', text: 'answer' },
      {
        kind: 'tool_call',
        name: 'Bash',
        input: { command: 'echo hi' },
        toolCallId: 'call_1',
      },
    ]);
  });

  it('projects a user tool_result frame (no name field)', () => {
    const events = new QoderDriver().parseLine(
      JSON.stringify({
        type: 'user',
        message: {
          role: 'user',
          content: [
            {
              type: 'tool_result',
              tool_use_id: 'call_1',
              content: 'hi',
              is_error: false,
            },
          ],
        },
      }),
    );

    expect(events).toEqual([
      { kind: 'tool_output', name: undefined, toolCallId: 'call_1', outcome: 'success' },
    ]);
  });

  it('labels api_error snippets with the Qoder runtime and redacts the endpoint', () => {
    const events = new QoderDriver().parseLine(
      JSON.stringify({
        type: 'system',
        subtype: 'api_error',
        error: { cause: { code: 'ConnectionRefused', path: 'http://127.0.0.1:9090/v1' } },
        retryAttempt: 2,
      }),
    );

    expect(events).toEqual([
      {
        kind: 'error',
        code: 'api_connection_refused',
        message: 'Qoder API gateway unreachable; retrying (attempt 2)',
      },
    ]);
    expect(JSON.stringify(events)).not.toContain('127.0.0.1');
  });

  it('emits turn_end on a successful result frame', () => {
    const events = new QoderDriver().parseLine(
      JSON.stringify({
        type: 'result',
        subtype: 'success',
        is_error: false,
        session_id: 'q-1',
      }),
    );
    expect(events).toEqual([{ kind: 'turn_end', sessionId: 'q-1' }]);
  });

  it('does NOT classify errors as fatal (Qoder resume-failure is stderr, not a result frame)', () => {
    const events = new QoderDriver().parseLine(
      JSON.stringify({
        type: 'result',
        subtype: 'error_during_execution',
        is_error: true,
        stop_reason: null,
        session_id: 'q-1',
        result: 'Some execution error',
      }),
    );

    expect(events[0]).toEqual({ kind: 'error', message: 'Some execution error' });
    expect(events[0]).not.toHaveProperty('fatal');
    expect(events[1]).toEqual({ kind: 'turn_end', sessionId: 'q-1' });
  });
});

describe('QoderDriver SDK mode (>= 1.1.81, #task:2957)', () => {
  function sdkDriver(overrides: Partial<DriverSpawnContext> = {}) {
    const built = makeSpawnCtx(overrides);
    const driver = new QoderDriver();
    driver.bindRuntimeVersion('1.1.81');
    const writes: Array<Record<string, any>> = [];
    driver.bindIO({
      writeLine: async (line) => {
        writes.push(JSON.parse(line));
      },
      spawnCtx: built.ctx,
    });
    return { driver, writes, built };
  }
  const respond = (requestId: string, response: unknown, sessionId?: string) =>
    JSON.stringify({
      type: 'control_response',
      ...(sessionId ? { session_id: sessionId } : {}),
      response: { subtype: 'success', request_id: requestId, response },
    });
  const handshake = (driver: QoderDriver, sessionId = 'sess-1') => [
    ...driver.parseLine(
      respond('daemon', { daemon_protocol_version: 2, capabilities: ['multi_session_v2'] }),
    ),
    ...driver.parseLine(respond('open', undefined, sessionId)),
    ...driver.parseLine(respond('session', { capabilities: ['msg_lifecycle_v1'] }, sessionId)),
  ];

  it('keeps headless argv and kickoff at 1.1.80', () => {
    const driver = new QoderDriver();
    driver.bindRuntimeVersion('1.1.80');
    const built = makeSpawnCtx();
    try {
      expect(driver.buildArgs(built.ctx)).toContain('--input-format');
      expect(driver.kickoffMode).toBe('requires_first_message');
      expect(driver.readyToEncode()).toBe(true);
    } finally {
      built.cleanup();
    }
  });

  it('spawns --sdk, opens the session with the headless args, and gates encode on ready', () => {
    const { driver, writes, built } = sdkDriver({ cliSessionId: 'sess-1', model: 'Ultimate' });
    try {
      expect(driver.buildArgs(built.ctx)).toEqual(['--sdk']);
      expect(driver.kickoffMode).toBe('self_inits');
      expect(driver.admissionPolicy).toBe('lifecycle-start');
      expect(driver.releaseProfile).toBe('tool-boundary-steer');
      expect(writes[0]!.request.type).toBe('daemon.initialize');
      expect(writes[0]!.request.auth).toEqual({ type: 'qodercli' });
      expect(driver.readyToEncode()).toBe(false);
      expect(driver.encodeStdinMessage('hi', null, { payloadUuid: 'u1' })).toBeNull();

      const events = handshake(driver);
      const open = writes[1]!.request;
      expect(open.type).toBe('daemon.open_session');
      expect(open.cwd).toBe(built.ctx.workspacePath);
      expect(open.worker_args).toContain('--resume');
      expect(open.worker_args).toContain('Ultimate');
      expect(writes[2]).toMatchObject({ session_id: 'sess-1', request: { type: 'initialize' } });
      expect(events).toEqual([{ kind: 'session_init', sessionId: 'sess-1' }]);

      const frame = JSON.parse(driver.encodeStdinMessage('hi', null, { payloadUuid: 'u1' })!);
      expect(frame).toMatchObject({
        type: 'user',
        session_id: 'sess-1',
        uuid: 'u1',
        priority: 'next',
      });
    } finally {
      built.cleanup();
    }
  });

  it('passes profile MCP through the worker args', () => {
    const { driver, writes, built } = sdkDriver({
      profileMcp: { mcpServers: { fs: { command: 'npx', args: ['server-fs'] } } },
    });
    try {
      handshake(driver);
      const args: string[] = writes[1]!.request.worker_args;
      const path = args[args.indexOf('--mcp-config') + 1]!;
      expect(JSON.parse(readFileSync(path, 'utf-8')).mcpServers).toHaveProperty(
        'silverhand_profile__fs',
      );
    } finally {
      built.cleanup();
    }
  });

  it('ignores worker system/init and passes lifecycle frames through once ready', () => {
    const { driver, built } = sdkDriver();
    try {
      handshake(driver);
      const init = JSON.stringify({ type: 'system', subtype: 'init', session_id: 'sess-1' });
      expect(driver.parseLine(init)).toEqual([]);
      const lifecycle = JSON.stringify({
        type: 'command_lifecycle',
        command_uuid: 'u1',
        state: 'started',
        session_id: 'sess-1',
      });
      expect(driver.parseLine(lifecycle)).toEqual([
        { kind: 'input_lifecycle', commandUuid: 'u1', state: 'started' },
      ]);
    } finally {
      built.cleanup();
    }
  });

  it('fails fatally and shuts the daemon down on a rejected handshake', () => {
    const { driver, writes, built } = sdkDriver({ cliSessionId: 'gone' });
    try {
      driver.parseLine(
        respond('daemon', { daemon_protocol_version: 2, capabilities: ['multi_session_v2'] }),
      );
      const events = driver.parseLine(
        JSON.stringify({
          type: 'control_response',
          response: { subtype: 'error', request_id: 'open', error: 'transcript missing' },
        }),
      );
      expect(events).toEqual([
        expect.objectContaining({ kind: 'error', code: 'qoder_sdk_rejected', fatal: true }),
      ]);
      expect(writes.at(-1)!.request.type).toBe('daemon.shutdown');
      expect(driver.readyToEncode()).toBe(false);
    } finally {
      built.cleanup();
    }
  });

  it('refuses a resumed session that comes back under another id', () => {
    const { driver, built } = sdkDriver({ cliSessionId: 'sess-1' });
    try {
      const events = handshake(driver, 'other');
      expect(events[0]).toMatchObject({ code: 'qoder_sdk_session_mismatch', fatal: true });
      expect(driver.readyToEncode()).toBe(false);
    } finally {
      built.cleanup();
    }
  });

  it('gives control only to the response for the request in flight', () => {
    const { driver, writes, built } = sdkDriver();
    try {
      handshake(driver);
      const writesBefore = writes.length;
      const stray = (requestId: string, subtype: string) =>
        JSON.stringify({
          type: 'control_response',
          session_id: 'sess-1',
          response: { subtype, request_id: requestId, error: 'late' },
        });
      expect(driver.parseLine(stray('open', 'error'))).toEqual([]);
      expect(driver.parseLine(stray('unknown', 'error'))).toEqual([]);
      expect(driver.parseLine(stray('session', 'success'))).toEqual([]);
      expect(driver.readyToEncode()).toBe(true);
      expect(writes).toHaveLength(writesBefore);
    } finally {
      built.cleanup();
    }
  });

  it('recognizes daemon frames by field, not by serialization', () => {
    const { driver, writes, built } = sdkDriver();
    try {
      driver.parseLine(
        '{ "type": "control_response", "response": { "subtype": "success", "request_id": "daemon", "response": { "daemon_protocol_version": 2, "capabilities": ["multi_session_v2"] } } }',
      );
      expect(writes[1]!.request.type).toBe('daemon.open_session');
    } finally {
      built.cleanup();
    }
  });

  it('fails a ready session on a daemon session failure', () => {
    const { driver, writes, built } = sdkDriver();
    try {
      handshake(driver);
      const events = driver.parseLine(
        JSON.stringify({
          type: 'status_event',
          status: 'daemon.session_updated',
          data: { local_session_id: 'sess-1', state: 'failed', error: {} },
        }),
      );
      expect(events).toEqual([
        expect.objectContaining({ code: 'qoder_sdk_session_failed', fatal: true }),
      ]);
      expect(writes.at(-1)!.request.type).toBe('daemon.shutdown');
      expect(driver.readyToEncode()).toBe(false);
    } finally {
      built.cleanup();
    }
  });

  it('stops encoding when a control write fails', async () => {
    const built = makeSpawnCtx();
    const driver = new QoderDriver();
    driver.bindRuntimeVersion('1.1.81');
    driver.bindIO({ writeLine: () => Promise.reject(new Error('EPIPE')), spawnCtx: built.ctx });
    try {
      await new Promise((resolve) => setImmediate(resolve));
      handshake(driver);
      expect(driver.readyToEncode()).toBe(false);
    } finally {
      built.cleanup();
    }
  });

  it('refuses a session without input lifecycle', () => {
    const { driver, built } = sdkDriver();
    try {
      driver.parseLine(
        respond('daemon', { daemon_protocol_version: 2, capabilities: ['multi_session_v2'] }),
      );
      driver.parseLine(respond('open', undefined, 'sess-1'));
      const events = driver.parseLine(respond('session', { capabilities: [] }, 'sess-1'));
      expect(events[0]).toMatchObject({ code: 'qoder_sdk_unsupported', fatal: true });
    } finally {
      built.cleanup();
    }
  });
});
