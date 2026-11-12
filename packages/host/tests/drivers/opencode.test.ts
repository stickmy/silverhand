import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { SILVERHAND_OPENCODE_AGENT } from '@silverhand/shared';
import { describe, expect, it } from 'vitest';

import type { DriverIO, DriverSpawnContext } from '../../src/drivers/index.js';
import { AcpClient, type AcpPermissionPolicy } from '../../src/drivers/acp.js';
import { OpenCodeDriver } from '../../src/drivers/opencode.js';

interface CapturedIO extends DriverIO {
  lines: string[];
}

function makeIO(spawnCtx: DriverSpawnContext): CapturedIO {
  const lines: string[] = [];
  return {
    spawnCtx,
    lines,
    writeLine: async (line: string) => {
      lines.push(line);
    },
  };
}

function makeSpawnCtx(overrides: Partial<DriverSpawnContext> = {}): {
  ctx: DriverSpawnContext;
  cleanup: () => void;
} {
  const tmp = mkdtempSync(join(tmpdir(), 'opencode-driver-test-'));
  const systemPromptPath = join(tmp, 'system-prompt.md');
  writeFileSync(systemPromptPath, 'STANDING PROMPT\n', 'utf-8');

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
    cleanup: () => rmSync(tmp, { recursive: true, force: true }),
  };
}

async function tick(): Promise<void> {
  await new Promise((r) => queueMicrotask(() => r(undefined)));
}

function parseLast(io: CapturedIO): any {
  return JSON.parse(io.lines.at(-1)!);
}

describe('OpenCodeDriver static config', () => {
  it('exposes the expected runtime metadata', () => {
    const d = new OpenCodeDriver();
    expect(d.kind).toBe('opencode');
    expect(d.binary).toBe('opencode');
    expect(d.releaseProfile).toBe('prompt-boundary');
    expect(d.oneShot).toBe(false);
    expect(d.supportsNativeStandingPrompt).toBe(true);
  });

  it('builds opencode acp args without --cwd (removed in opencode 2.x)', () => {
    const { ctx, cleanup } = makeSpawnCtx();
    try {
      expect(new OpenCodeDriver().buildArgs(ctx)).toEqual(['acp']);
    } finally {
      cleanup();
    }
  });

  it('applyEnv adds NO_COLOR without mutating the input', () => {
    const base: NodeJS.ProcessEnv = { PATH: '/usr/bin' };
    const out = new OpenCodeDriver().applyEnv(base);
    expect(out.NO_COLOR).toBe('1');
    expect(out.PATH).toBe('/usr/bin');
    expect(base.NO_COLOR).toBeUndefined();
  });
});

describe('OpenCodeDriver ACP handshake', () => {
  it('classifies ACP frames as protocol-ready only; parseLine owns session_init', () => {
    const d = new OpenCodeDriver();
    expect(
      d.classifyInitProgress(
        JSON.stringify({
          jsonrpc: '2.0',
          id: 2,
          result: { sessionId: 'ses_1' },
        }),
      ),
    ).toBe('protocol_ready');
    expect(d.classifyInitProgress('not json')).toBeNull();
  });

  it('initializes, creates a session, then accepts turn-boundary prompts', async () => {
    const { ctx, cleanup } = makeSpawnCtx();
    try {
      const d = new OpenCodeDriver();
      const io = makeIO(ctx);
      d.bindIO(io);
      await tick();

      expect(io.lines).toHaveLength(1);
      expect(parseLast(io)).toMatchObject({
        jsonrpc: '2.0',
        id: 1,
        method: 'initialize',
        params: {
          protocolVersion: 1,
          clientInfo: { name: 'silverhand' },
          clientCapabilities: {
            fs: {
              readTextFile: false,
              writeTextFile: false,
            },
            terminal: false,
          },
        },
      });
      expect(d.readyToEncode()).toBe(false);

      expect(d.parseLine(JSON.stringify({ jsonrpc: '2.0', id: 1, result: {} }))).toEqual([]);
      expect(parseLast(io)).toMatchObject({
        id: 2,
        method: 'session/new',
        params: { cwd: ctx.workspacePath, mcpServers: [] },
      });

      expect(
        d.parseLine(
          JSON.stringify({
            jsonrpc: '2.0',
            id: 2,
            result: { sessionId: 'ses_1' },
          }),
        ),
      ).toEqual([]);
      expect(parseLast(io)).toMatchObject({
        id: 3,
        method: 'session/set_config_option',
        params: {
          sessionId: 'ses_1',
          configId: 'mode',
          value: SILVERHAND_OPENCODE_AGENT,
        },
      });
      expect(d.readyToEncode()).toBe(false);

      expect(
        d.parseLine(
          JSON.stringify({
            jsonrpc: '2.0',
            id: 3,
            result: { configOptions: [] },
          }),
        ),
      ).toEqual([{ kind: 'session_init', sessionId: 'ses_1' }]);
      expect(d.readyToEncode()).toBe(true);

      const encoded = d.encodeStdinMessage('hello', null);
      expect(JSON.parse(encoded!)).toMatchObject({
        jsonrpc: '2.0',
        id: 4,
        method: 'session/prompt',
        params: {
          sessionId: 'ses_1',
          prompt: [{ type: 'text', text: 'hello' }],
        },
      });
      expect(d.readyToEncode()).toBe(false);

      expect(
        d.parseLine(
          JSON.stringify({
            jsonrpc: '2.0',
            id: 4,
            result: { stopReason: 'end_turn' },
          }),
        ),
      ).toEqual([{ kind: 'turn_end', sessionId: 'ses_1' }]);
      expect(d.readyToEncode()).toBe(true);
    } finally {
      cleanup();
    }
  });

  it('resumes existing sessions and waits for mode and model config before session_init', async () => {
    const { ctx, cleanup } = makeSpawnCtx({
      cliSessionId: 'ses_existing',
      model: 'opencode/deepseek-v4-flash-free',
    });
    try {
      const d = new OpenCodeDriver();
      const io = makeIO(ctx);
      d.bindIO(io);
      await tick();

      d.parseLine(JSON.stringify({ jsonrpc: '2.0', id: 1, result: {} }));
      expect(parseLast(io)).toMatchObject({
        id: 2,
        method: 'session/resume',
        params: {
          cwd: ctx.workspacePath,
          mcpServers: [],
          sessionId: 'ses_existing',
        },
      });

      expect(
        d.parseLine(
          JSON.stringify({
            jsonrpc: '2.0',
            id: 2,
            // OpenCode 1.17.14 returns only configOptions on session/resume.
            // The requested cliSessionId remains the authoritative session id.
            result: { configOptions: [] },
          }),
        ),
      ).toEqual([]);
      expect(parseLast(io)).toMatchObject({
        id: 3,
        method: 'session/set_config_option',
        params: {
          sessionId: 'ses_existing',
          configId: 'mode',
          value: SILVERHAND_OPENCODE_AGENT,
        },
      });
      expect(d.readyToEncode()).toBe(false);

      expect(
        d.parseLine(
          JSON.stringify({
            jsonrpc: '2.0',
            id: 3,
            result: { configOptions: [] },
          }),
        ),
      ).toEqual([]);
      expect(parseLast(io)).toMatchObject({
        id: 4,
        method: 'session/set_config_option',
        params: {
          sessionId: 'ses_existing',
          configId: 'model',
          value: 'opencode/deepseek-v4-flash-free',
        },
      });
      expect(d.readyToEncode()).toBe(false);

      expect(
        d.parseLine(
          JSON.stringify({
            jsonrpc: '2.0',
            id: 4,
            result: { configOptions: [] },
          }),
        ),
      ).toEqual([{ kind: 'session_init', sessionId: 'ses_existing' }]);
      expect(d.readyToEncode()).toBe(true);
    } finally {
      cleanup();
    }
  });
});

describe('OpenCodeDriver prompt-boundary contract (#task:1565, probed on opencode 1.18.1)', () => {
  // Live-protocol probe findings that make these falsifiers load-bearing:
  //  - opencode ACCEPTS a second session/prompt while one is active (no
  //    error), queues it FIFO, but then HOLDS BOTH RPC responses until the
  //    whole queued run completes (both settled in the same instant with
  //    merged usage). Under runtime-side queuing, "prompt response" stops
  //    being a per-turn boundary — so the client-side single-in-flight
  //    rule is a CORRECTNESS invariant, not politeness.
  //  - session/cancel settles the active prompt's response with
  //    stopReason:'cancelled' (a result, not an error), and the session
  //    accepts new prompts afterward — no wedge.
  async function readyDriver(): Promise<{
    d: OpenCodeDriver;
    io: CapturedIO;
    cleanup: () => void;
  }> {
    const { ctx, cleanup } = makeSpawnCtx();
    const d = new OpenCodeDriver();
    const io = makeIO(ctx);
    d.bindIO(io);
    await tick();
    d.parseLine(JSON.stringify({ jsonrpc: '2.0', id: 1, result: {} }));
    d.parseLine(JSON.stringify({ jsonrpc: '2.0', id: 2, result: { sessionId: 'ses_1' } }));
    d.parseLine(JSON.stringify({ jsonrpc: '2.0', id: 3, result: { configOptions: [] } }));
    return { d, io, cleanup };
  }

  it('single-in-flight falsifier: encode while a prompt is active returns null — a second live prompt would collapse response granularity (probed)', async () => {
    const { d, cleanup } = await readyDriver();
    try {
      const first = d.encodeStdinMessage('prompt A', null);
      expect(first).not.toBeNull();
      // Boundary closed: the daemon MUST NOT be able to mint a second
      // in-flight prompt; deliveries defer instead.
      expect(d.readyToEncode()).toBe(false);
      expect(d.encodeStdinMessage('prompt B', null)).toBeNull();
      // Response settles the boundary; encode works again.
      d.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          id: JSON.parse(first!).id,
          result: { stopReason: 'end_turn' },
        }),
      );
      expect(d.readyToEncode()).toBe(true);
      expect(d.encodeStdinMessage('prompt B', null)).not.toBeNull();
    } finally {
      cleanup();
    }
  });

  it('a cancelled prompt still settles the boundary: stopReason=cancelled result → turn_end, ready again (no wedge)', async () => {
    const { d, cleanup } = await readyDriver();
    try {
      const first = d.encodeStdinMessage('long task', null);
      const events = d.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          id: JSON.parse(first!).id,
          result: {
            stopReason: 'cancelled',
            usage: { inputTokens: 0, outputTokens: 0, totalTokens: 0 },
          },
        }),
      );
      expect(events).toEqual([{ kind: 'turn_end', sessionId: 'ses_1' }]);
      expect(d.readyToEncode()).toBe(true);
    } finally {
      cleanup();
    }
  });

  it('an error response to the active prompt settles the boundary too: error event + turn_end', async () => {
    const { d, cleanup } = await readyDriver();
    try {
      const first = d.encodeStdinMessage('task', null);
      const events = d.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          id: JSON.parse(first!).id,
          error: { code: -32000, message: 'model backend unavailable' },
        }),
      );
      expect(events.some((e) => e.kind === 'error')).toBe(true);
      expect(events.some((e) => e.kind === 'turn_end')).toBe(true);
      expect(d.readyToEncode()).toBe(true);
    } finally {
      cleanup();
    }
  });
});

describe('OpenCodeDriver native steer (opencode >= 2.2.7, probed)', () => {
  // Probed on 2.2.7: `_session/steering` answers `{outcome:'injected'}` while a
  // turn runs, and `{outcome:'promptRequired'}` once it has ended (the text is
  // not taken). 2.2.6 and earlier have no such method.
  async function readyDriver(version: string | null): Promise<{
    d: OpenCodeDriver;
    io: CapturedIO;
    cleanup: () => void;
  }> {
    const { ctx, cleanup } = makeSpawnCtx();
    const d = new OpenCodeDriver();
    d.bindRuntimeVersion(version);
    const io = makeIO(ctx);
    d.bindIO(io);
    await tick();
    d.parseLine(JSON.stringify({ jsonrpc: '2.0', id: 1, result: {} }));
    d.parseLine(JSON.stringify({ jsonrpc: '2.0', id: 2, result: { sessionId: 'ses_1' } }));
    d.parseLine(JSON.stringify({ jsonrpc: '2.0', id: 3, result: { configOptions: [] } }));
    return { d, io, cleanup };
  }

  const respond = (d: OpenCodeDriver, id: number, result: unknown) =>
    d.parseLine(JSON.stringify({ jsonrpc: '2.0', id, result }));

  it('pins the release profile to the CLI version and fails closed', () => {
    const profile = (v: string | null | undefined) => {
      const d = new OpenCodeDriver();
      if (v !== undefined) d.bindRuntimeVersion(v);
      return d.releaseProfile;
    };
    expect(profile('2.2.7')).toBe('native-steer');
    expect(profile('2.3.0')).toBe('native-steer');
    expect(profile('2.2.6')).toBe('prompt-boundary');
    expect(profile('1.18.1')).toBe('prompt-boundary');
    expect(profile(null)).toBe('prompt-boundary');
    expect(profile(undefined)).toBe('prompt-boundary');
  });

  it('steers busy input into the running turn without taking the prompt slot', async () => {
    const { d, cleanup } = await readyDriver('2.2.7');
    try {
      const prompt = JSON.parse(d.encodeStdinMessage('task', null)!);
      expect(prompt.method).toBe('session/prompt');
      expect(d.readyToEncode()).toBe(true);
      const steer = JSON.parse(d.encodeStdinMessage('more', null)!);
      expect(steer).toMatchObject({
        method: '_session/steering',
        params: { sessionId: 'ses_1', prompt: [{ type: 'text', text: 'more' }] },
      });
      expect(respond(d, steer.id, { outcome: 'injected' })).toEqual([]);
      expect(respond(d, prompt.id, { stopReason: 'end_turn' })).toEqual([
        { kind: 'turn_end', sessionId: 'ses_1' },
      ]);
      expect(JSON.parse(d.encodeStdinMessage('next', null)!).method).toBe('session/prompt');
    } finally {
      cleanup();
    }
  });

  it('redelivers a steer refused after its turn ended as the next prompt', async () => {
    const { d, io, cleanup } = await readyDriver('2.2.7');
    try {
      const prompt = JSON.parse(d.encodeStdinMessage('task', null)!);
      const a = JSON.parse(d.encodeStdinMessage('a', null)!);
      const b = JSON.parse(d.encodeStdinMessage('b', null)!);
      // Refusals can arrive before the refused turn's own response.
      respond(d, a.id, { outcome: 'promptRequired', reason: 'noRunningTurn' });
      respond(d, b.id, { outcome: 'promptRequired', reason: 'noRunningTurn' });
      const before = io.lines.length;
      respond(d, prompt.id, { stopReason: 'end_turn' });
      await tick();
      expect(io.lines.length).toBe(before + 1);
      const next = parseLast(io);
      expect(next.method).toBe('session/prompt');
      expect(next.params.prompt[0].text).toBe('a\n\nb');
      expect(d.readyToEncode()).toBe(true);
      expect(JSON.parse(d.encodeStdinMessage('c', null)!).method).toBe('_session/steering');
    } finally {
      cleanup();
    }
  });

  it('redelivers a refused steer immediately when the turn already settled', async () => {
    const { d, io, cleanup } = await readyDriver('2.2.7');
    try {
      const prompt = JSON.parse(d.encodeStdinMessage('task', null)!);
      const steer = JSON.parse(d.encodeStdinMessage('late', null)!);
      respond(d, prompt.id, { stopReason: 'end_turn' });
      respond(d, steer.id, { outcome: 'promptRequired', reason: 'noRunningTurn' });
      await tick();
      expect(parseLast(io)).toMatchObject({
        method: 'session/prompt',
        params: { prompt: [{ type: 'text', text: 'late' }] },
      });
    } finally {
      cleanup();
    }
  });

  it('a refused steer joins a turn that has since started instead of opening another', async () => {
    const { d, io, cleanup } = await readyDriver('2.2.7');
    try {
      const first = JSON.parse(d.encodeStdinMessage('task', null)!);
      const steer = JSON.parse(d.encodeStdinMessage('late', null)!);
      respond(d, first.id, { stopReason: 'end_turn' });
      JSON.parse(d.encodeStdinMessage('second', null)!);
      respond(d, steer.id, { outcome: 'promptRequired', reason: 'noRunningTurn' });
      await tick();
      expect(parseLast(io)).toMatchObject({
        method: '_session/steering',
        params: { prompt: [{ type: 'text', text: 'late' }] },
      });
    } finally {
      cleanup();
    }
  });

  it('redelivers a steer that fails with an error instead of dropping it', async () => {
    const { d, io, cleanup } = await readyDriver('2.2.7');
    try {
      const prompt = JSON.parse(d.encodeStdinMessage('task', null)!);
      const steer = JSON.parse(d.encodeStdinMessage('more', null)!);
      const events = d.parseLine(
        JSON.stringify({ jsonrpc: '2.0', id: steer.id, error: { code: -32000, message: 'x' } }),
      );
      expect(events).toEqual([{ kind: 'error', message: 'x' }]);
      respond(d, prompt.id, { stopReason: 'end_turn' });
      await tick();
      expect(parseLast(io)).toMatchObject({
        method: 'session/prompt',
        params: { prompt: [{ type: 'text', text: 'more' }] },
      });
    } finally {
      cleanup();
    }
  });

  it('a failed redelivery write is swallowed, not an unhandled rejection', async () => {
    const { ctx, cleanup } = makeSpawnCtx();
    try {
      const d = new OpenCodeDriver();
      d.bindRuntimeVersion('2.2.7');
      const lines: string[] = [];
      let broken = false;
      d.bindIO({
        spawnCtx: ctx,
        writeLine: async (line: string) => {
          if (broken) throw new Error('EPIPE');
          lines.push(line);
        },
      });
      await tick();
      d.parseLine(JSON.stringify({ jsonrpc: '2.0', id: 1, result: {} }));
      d.parseLine(JSON.stringify({ jsonrpc: '2.0', id: 2, result: { sessionId: 'ses_1' } }));
      d.parseLine(JSON.stringify({ jsonrpc: '2.0', id: 3, result: { configOptions: [] } }));
      const prompt = JSON.parse(d.encodeStdinMessage('task', null)!);
      const steer = JSON.parse(d.encodeStdinMessage('late', null)!);
      respond(d, steer.id, { outcome: 'promptRequired', reason: 'noRunningTurn' });
      broken = true;
      respond(d, prompt.id, { stopReason: 'end_turn' });
      await new Promise((r) => setTimeout(r, 0));
      expect(lines.some((l) => l.includes('late'))).toBe(false);
    } finally {
      cleanup();
    }
  });

  it('below 2.2.7 keeps single-in-flight prompts', async () => {
    const { d, cleanup } = await readyDriver('2.2.6');
    try {
      expect(d.encodeStdinMessage('task', null)).not.toBeNull();
      expect(d.readyToEncode()).toBe(false);
      expect(d.encodeStdinMessage('more', null)).toBeNull();
    } finally {
      cleanup();
    }
  });
});

describe('OpenCodeDriver ACP event mapping', () => {
  it('maps message chunks and tool calls from session/update', () => {
    const d = new OpenCodeDriver();
    expect(
      d.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'session/update',
          params: {
            sessionId: 'ses_1',
            update: {
              sessionUpdate: 'agent_message_chunk',
              content: { type: 'text', text: 'hello' },
            },
          },
        }),
      ),
    ).toEqual([{ kind: 'text', text: 'hello' }]);

    expect(
      d.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'session/update',
          params: {
            sessionId: 'ses_1',
            update: {
              sessionUpdate: 'agent_thought_chunk',
              content: { type: 'text', text: 'thinking' },
            },
          },
        }),
      ),
    ).toEqual([{ kind: 'thinking', text: 'thinking' }]);

    expect(
      d.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'session/update',
          params: {
            sessionId: 'ses_1',
            update: {
              sessionUpdate: 'tool_call',
              toolCallId: 'tool_1',
              title: 'Bash',
              rawInput: { command: 'pnpm test' },
              status: 'pending',
            },
          },
        }),
      ),
    ).toEqual([
      {
        kind: 'tool_call',
        name: 'Bash',
        input: { command: 'pnpm test' },
        toolCallId: 'tool_1',
      },
    ]);

    expect(
      d.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'session/update',
          params: {
            sessionId: 'ses_1',
            update: {
              sessionUpdate: 'tool_call_update',
              toolCallId: 'tool_1',
              status: 'completed',
            },
          },
        }),
      ),
    ).toEqual([{ kind: 'tool_output', toolCallId: 'tool_1', outcome: 'success' }]);
  });

  it('refines a tool call with the params that arrive in tool_call_update, without re-announcing the call (#task:2101)', () => {
    const d = new OpenCodeDriver();
    const update = (u: Record<string, unknown>) =>
      d.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'session/update',
          params: { sessionId: 'ses_1', update: u },
        }),
      );

    // The real OpenCode sequence: the creating frame carries no command, only
    // the scaffolding `cwd` — which is why every Silverhand command rendered
    // as a bare `bash`.
    expect(
      update({
        sessionUpdate: 'tool_call',
        toolCallId: 'tool_1',
        title: 'bash',
        rawInput: { cwd: '/tmp/agent' },
        status: 'pending',
      }),
    ).toEqual([
      {
        kind: 'tool_call',
        name: 'bash',
        input: { cwd: '/tmp/agent' },
        toolCallId: 'tool_1',
      },
    ]);

    // The command lands in a LATER update for the same id. It must come back
    // as a refinement of that row, never as a second `tool_call`: `tool_call`
    // is what increments `outstandingToolUses` / `toolCallCounts`.
    const refined = update({
      sessionUpdate: 'tool_call_update',
      toolCallId: 'tool_1',
      rawInput: { cwd: '/tmp/agent', command: 'silverhand message send' },
      status: 'in_progress',
    });
    expect(refined).toEqual([
      {
        kind: 'tool_call_input',
        toolCallId: 'tool_1',
        input: { cwd: '/tmp/agent', command: 'silverhand message send' },
      },
    ]);
    expect(refined.some((e) => e.kind === 'tool_call')).toBe(false);

    // A terminal update carrying revised params emits BOTH — the refinement
    // for the row and the existing completion marker.
    expect(
      update({
        sessionUpdate: 'tool_call_update',
        toolCallId: 'tool_1',
        rawInput: { cwd: '/tmp/agent', command: 'silverhand message send --anyway' },
        status: 'completed',
      }),
    ).toEqual([
      {
        kind: 'tool_call_input',
        toolCallId: 'tool_1',
        input: { cwd: '/tmp/agent', command: 'silverhand message send --anyway' },
      },
      { kind: 'tool_output', toolCallId: 'tool_1', outcome: 'success' },
    ]);
  });

  it('emits no refinement for a tool call it never announced, or for an update with no params (#task:2101)', () => {
    const d = new OpenCodeDriver();
    const update = (u: Record<string, unknown>) =>
      d.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'session/update',
          params: { sessionId: 'ses_1', update: u },
        }),
      );

    // Unknown id: there is no row to refine, so the backend would have nothing
    // to update. Gated on what we announced, not on the payload.
    expect(
      update({
        sessionUpdate: 'tool_call_update',
        toolCallId: 'never_seen',
        rawInput: { command: 'ls' },
        status: 'in_progress',
      }),
    ).toEqual([]);

    update({
      sessionUpdate: 'tool_call',
      toolCallId: 'tool_2',
      title: 'bash',
      rawInput: { cwd: '/tmp/agent' },
      status: 'pending',
    });

    // `rawInput` absent = the runtime revised something else (status, output).
    // Absent is NOT the same as an empty object: `{}` is a real reported input.
    expect(
      update({
        sessionUpdate: 'tool_call_update',
        toolCallId: 'tool_2',
        status: 'in_progress',
      }),
    ).toEqual([]);
    expect(
      update({
        sessionUpdate: 'tool_call_update',
        toolCallId: 'tool_2',
        rawInput: {},
        status: 'in_progress',
      }),
    ).toEqual([{ kind: 'tool_call_input', toolCallId: 'tool_2', input: {} }]);
  });

  it('auto-approves permission requests to match full-access runtime posture', async () => {
    const { ctx, cleanup } = makeSpawnCtx();
    try {
      const d = new OpenCodeDriver();
      const io = makeIO(ctx);
      d.bindIO(io);
      await tick();

      d.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          id: 9,
          method: 'session/request_permission',
          params: {
            sessionId: 'ses_1',
            toolCall: { toolCallId: 'tool_1', title: 'Bash' },
            options: [
              { optionId: 'allow', kind: 'allow_once', name: 'Allow' },
              { optionId: 'reject', kind: 'reject_once', name: 'Reject' },
            ],
          },
        }),
      );
      await tick();

      expect(parseLast(io)).toEqual({
        jsonrpc: '2.0',
        id: 9,
        result: {
          outcome: { outcome: 'selected', optionId: 'allow' },
        },
      });
    } finally {
      cleanup();
    }
  });
});

it('keeps ACP result bodies out of parsed events', () => {
  for (const sessionUpdate of ['tool_call', 'tool_call_update']) {
    const events = new OpenCodeDriver().parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'session/update',
        params: {
          sessionId: 'marker-session',
          update: {
            sessionUpdate,
            toolCallId: 'marker-call',
            status: 'completed',
            rawOutput: 'private body',
            content: [{ type: 'text', text: 'private body' }],
          },
        },
      }),
    );
    expect(events.some((event) => event.kind === 'tool_output')).toBe(true);
    expect(JSON.stringify(events)).not.toContain('private body');
  }
});

it.each([
  { kinds: ['allow_once', 'allow_always', 'reject_once'], selected: 'allow_always' },
  { kinds: ['allow_always', 'allow_once'], selected: 'allow_always' },
  { kinds: ['allow_once'], selected: 'allow_once' },
  { kinds: ['reject_once'], selected: null },
  { kinds: [], selected: null },
])('OpenCode permission preference: $kinds', async ({ kinds, selected }) => {
  const { ctx, cleanup } = makeSpawnCtx();
  try {
    const driver = new OpenCodeDriver();
    const io = makeIO(ctx);
    driver.bindIO(io);
    await tick();
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        id: 91,
        method: 'session/request_permission',
        params: {
          options: kinds.map((kind) => ({ kind, optionId: kind, name: kind })),
        },
      }),
    );
    await tick();
    expect(parseLast(io)).toEqual({
      jsonrpc: '2.0',
      id: 91,
      result: {
        outcome: selected ? { outcome: 'selected', optionId: selected } : { outcome: 'cancelled' },
      },
    });
  } finally {
    cleanup();
  }
});

it.each([
  { policy: { shouldAllow: () => true }, selected: 'allow_always' },
  { policy: { shouldAllow: () => false }, selected: 'reject_once' },
  { policy: undefined, selected: 'reject_once' },
] satisfies Array<{ policy: AcpPermissionPolicy | undefined; selected: string }>)(
  'uses the common ACP allow order while preserving denial: $selected',
  async ({ policy, selected }) => {
    const { ctx, cleanup } = makeSpawnCtx();
    try {
      const client = new AcpClient({
        runtimeLabel: 'test adapter',
        permissionPolicy: policy,
        buildInitializeParams: () => ({}),
        buildSessionParams: () => ({}),
      });
      const io = makeIO(ctx);
      client.bindIO(io);
      await tick();
      client.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          id: 92,
          method: 'session/request_permission',
          params: {
            options: ['allow_always', 'allow_once', 'reject_always', 'reject_once'].map((kind) => ({
              kind,
              optionId: kind,
              name: kind,
            })),
          },
        }),
      );
      await tick();
      expect(parseLast(io)).toEqual({
        jsonrpc: '2.0',
        id: 92,
        result: { outcome: { outcome: 'selected', optionId: selected } },
      });
    } finally {
      cleanup();
    }
  },
);
