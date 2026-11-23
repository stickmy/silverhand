import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { CodexDriver } from '../../src/drivers/codex.js';
import type { DriverIO, DriverSpawnContext } from '../../src/drivers/index.js';

/**
 * Pure unit tests for CodexDriver — exercise the JSON-RPC handshake and
 * `parseLine` event mapping without spawning a real `codex` binary.
 *
 * The handshake test uses a fake DriverIO that captures every line the
 * driver tries to write; we assert the issued requests / notifications
 * line up with what `codex app-server` expects.
 */

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
  const tmp = mkdtempSync(join(tmpdir(), 'codex-driver-test-'));
  const systemPromptPath = join(tmp, 'system-prompt.md');
  writeFileSync(systemPromptPath, 'STANDING PROMPT\n', 'utf-8');

  // codex requires a git repo — make this a real one so bindIO doesn't
  // shell out to git.
  writeFileSync(join(tmp, '.git'), '');

  const ctx: DriverSpawnContext = {
    agentId: 'agent-1',
    agentName: 'tester',
    workspacePath: tmp,
    cliSessionId: null,
    model: null,
    reasoningEffort: null,
    serviceTier: null,
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

/** Wait one microtask tick so `bindIO`'s `queueMicrotask` runs. */
async function tick(): Promise<void> {
  await new Promise((r) => queueMicrotask(() => r(undefined)));
}

/** Establish identity through the real request/response handshake. */
async function bindThread(driver: CodexDriver, threadId: string): Promise<void> {
  const built = makeSpawnCtx();
  try {
    const io = makeIO(built.ctx);
    driver.bindIO(io);
    await tick();
    driver.parseLine(JSON.stringify({ id: JSON.parse(io.lines[0]!).id, result: {} }));
    driver.parseLine(
      JSON.stringify({ id: JSON.parse(io.lines[2]!).id, result: { thread: { id: threadId } } }),
    );
  } finally {
    built.cleanup();
  }
}

describe('CodexDriver static config', () => {
  it('exposes the expected runtime metadata', () => {
    const d = new CodexDriver();
    expect(d.kind).toBe('codex');
    expect(d.binary).toBe('codex');
    expect(d.releaseProfile).toBe('native-steer');
    expect(d.oneShot).toBe(false);
    expect(d.supportsNativeStandingPrompt).toBe(true);
  });

  it('applyEnv adds NO_COLOR without mutating the input', () => {
    const d = new CodexDriver();
    const base: NodeJS.ProcessEnv = { PATH: '/usr/bin' };
    const out = d.applyEnv(base);
    expect(out.NO_COLOR).toBe('1');
    expect(out.PATH).toBe('/usr/bin');
    expect(base.NO_COLOR).toBeUndefined();
  });

  it('encodeInterruptControl returns null (no protocol cancel)', () => {
    expect(new CodexDriver().encodeInterruptControl()).toBeNull();
  });
});

describe('CodexDriver.classifyInitProgress', () => {
  it('treats the initialize response as protocol-ready', () => {
    const d = new CodexDriver();
    expect(d.classifyInitProgress(JSON.stringify({ jsonrpc: '2.0', id: 1, result: {} }))).toBe(
      'protocol_ready',
    );
  });

  it('does not treat an unsolicited thread/started as session-ready', () => {
    const d = new CodexDriver();
    expect(
      d.classifyInitProgress(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'thread/started',
          params: { thread: { id: 'thread-1' } },
        }),
      ),
    ).toBe('protocol_ready');
  });

  it('does not treat an unrelated result.thread as session-ready', () => {
    const d = new CodexDriver();
    expect(
      d.classifyInitProgress(
        JSON.stringify({ jsonrpc: '2.0', id: 2, result: { thread: { id: 'thread-1' } } }),
      ),
    ).toBe('protocol_ready');
  });

  it('ignores non-json lines', () => {
    expect(new CodexDriver().classifyInitProgress('not json')).toBeNull();
  });
});

describe('CodexDriver.buildArgs', () => {
  it('passes app-server stdio without inline MCP server config', () => {
    const { ctx, cleanup } = makeSpawnCtx();
    try {
      const args = new CodexDriver().buildArgs(ctx);
      expect(args.slice(0, 3)).toEqual(['app-server', '--listen', 'stdio://']);
      const kv = collectCArgs(args);
      expect([...kv.keys()].some((k) => k.startsWith('mcp_servers.chat'))).toBe(false);
    } finally {
      cleanup();
    }
  });

  it('passes reasoning effort as a codex config override when set', () => {
    const { ctx, cleanup } = makeSpawnCtx({ reasoningEffort: 'xhigh' });
    try {
      const args = new CodexDriver().buildArgs(ctx);
      const kv = collectCArgs(args);
      expect(kv.get('model_reasoning_effort')).toBe('"xhigh"');
    } finally {
      cleanup();
    }
  });

  it('passes Ultra verbatim to Codex app-server', () => {
    const { ctx, cleanup } = makeSpawnCtx({ model: 'gpt-6-astra', reasoningEffort: 'ultra' });
    try {
      const kv = collectCArgs(new CodexDriver().buildArgs(ctx));
      expect(kv.get('model_reasoning_effort')).toBe('"ultra"');
    } finally {
      cleanup();
    }
  });

  it('passes a future token the product enum has never heard of, verbatim (#task:1486)', () => {
    // The wire/spawn-ctx effort is an OPEN string, not the closed product
    // enum. buildArgs must forward any value untouched so a new effort never
    // needs a driver/daemon change — Codex owns the engine-level verdict.
    const { ctx, cleanup } = makeSpawnCtx({ reasoningEffort: 'giga' });
    try {
      const kv = collectCArgs(new CodexDriver().buildArgs(ctx));
      expect(kv.get('model_reasoning_effort')).toBe('"giga"');
    } finally {
      cleanup();
    }
  });

  it('carries no fast_mode feature flags', () => {
    const { ctx, cleanup } = makeSpawnCtx({ serviceTier: 'ultrafast' });
    try {
      const args = new CodexDriver().buildArgs(ctx);
      expect(args).not.toContain('--enable');
      expect(args).not.toContain('--disable');
    } finally {
      cleanup();
    }
  });
});

describe('CodexDriver input lifecycle = userMessage clientId echo, never the RPC ack (#task:1561 round 2)', () => {
  // Countgame round 2: the turn/steer JSON-RPC response is a TRANSPORT
  // ack (codex only promises the input joined the active turn, not that
  // in-flight generation re-conditions on it) — it must emit NO
  // lifecycle. The real generation-boundary evidence is codex's echo of
  // our `clientUserMessageId` on `item/started {type:'userMessage'}`,
  // emitted when pending_input drains into model history right before
  // the next sampling request.
  let cleanup: (() => void) | null = null;
  afterEach(() => {
    cleanup?.();
    cleanup = null;
  });

  async function readyDriver(): Promise<{ driver: CodexDriver; io: ReturnType<typeof makeIO> }> {
    const built = makeSpawnCtx();
    cleanup = built.cleanup;
    const io = makeIO(built.ctx);
    const driver = new CodexDriver();
    driver.bindIO(io);
    await tick();
    driver.parseLine(JSON.stringify({ id: JSON.parse(io.lines[0]!).id, result: {} }));
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        id: JSON.parse(io.lines[2]!).id,
        result: { thread: { id: 'thread-abc' } },
      }),
    );
    return { driver, io };
  }

  it('stamps payloadUuid as clientUserMessageId on turn/steer and emits NO lifecycle from the RPC response', async () => {
    const { driver } = await readyDriver();
    // Enter a turn so encode takes the steer arm.
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'turn/started',
        params: { turn: { id: 'turn-1' } },
      }),
    );
    const encoded = driver.encodeStdinMessage('mid-turn input', 'thread-abc', {
      payloadUuid: 'uuid-A',
    });
    expect(encoded).not.toBeNull();
    const frame = JSON.parse(encoded!);
    expect(frame.method).toBe('turn/steer');
    expect(frame.params.clientUserMessageId).toBe('uuid-A');

    // Transport ack: silent.
    const events = driver.parseLine(JSON.stringify({ jsonrpc: '2.0', id: frame.id, result: {} }));
    expect(events.filter((e) => e.kind === 'input_lifecycle')).toEqual([]);
  });

  it('stamps clientUserMessageId on turn/start too', async () => {
    const { driver } = await readyDriver();
    const encoded = driver.encodeStdinMessage('kick off', 'thread-abc', {
      payloadUuid: 'uuid-K',
    });
    const frame = JSON.parse(encoded!);
    expect(frame.method).toBe('turn/start');
    expect(frame.params.clientUserMessageId).toBe('uuid-K');
    // An unstamped submission omits the field entirely.
    const bare = JSON.parse(driver.encodeStdinMessage('bare', 'thread-abc')!);
    expect('clientUserMessageId' in bare.params).toBe(false);
  });

  it('the userMessage echo maps 1:1 — item/started → started, item/completed → completed (real two-frame timing)', async () => {
    const { driver } = await readyDriver();
    const startedEvents = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/started',
        params: {
          item: {
            type: 'userMessage',
            id: 'item-7',
            clientId: 'uuid-A',
            content: [{ type: 'text', text: 'mid-turn input' }],
          },
        },
      }),
    );
    expect(startedEvents.filter((e) => e.kind === 'input_lifecycle')).toEqual([
      { kind: 'input_lifecycle', commandUuid: 'uuid-A', state: 'started' },
    ]);
    // The real completed arrives as its OWN frame later — mapped then,
    // never fabricated at started (Cindy P2: a faked completed would
    // make the real one arrive uncorrelated after the supervisor prune).
    const completedEvents = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/completed',
        params: {
          item: {
            type: 'userMessage',
            id: 'item-7',
            clientId: 'uuid-A',
            content: [{ type: 'text', text: 'mid-turn input' }],
          },
        },
      }),
    );
    expect(completedEvents.filter((e) => e.kind === 'input_lifecycle')).toEqual([
      { kind: 'input_lifecycle', commandUuid: 'uuid-A', state: 'completed' },
    ]);
  });

  it('a userMessage item without clientId (not ours) emits no lifecycle', async () => {
    const { driver } = await readyDriver();
    const events = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/started',
        params: {
          item: {
            type: 'userMessage',
            id: 'item-8',
            content: [{ type: 'text', text: 'typed by human' }],
          },
        },
      }),
    );
    expect(events.filter((e) => e.kind === 'input_lifecycle')).toEqual([]);
  });

  it('a correlated start response supplies the turn id before notifications', async () => {
    const { driver } = await readyDriver();
    const first = driver.encodeStdinMessage('kick off', 'thread-abc', {
      payloadUuid: 'uuid-K',
    });
    const firstFrame = JSON.parse(first!);
    expect(firstFrame.method).toBe('turn/start');
    // Response carries the new turn id via the generic result branch.
    const events = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        id: firstFrame.id,
        result: { turn: { id: 'turn-9' } },
      }),
    );
    expect(events.filter((e) => e.kind === 'input_lifecycle')).toEqual([]);
    // Next encode steers into the announced turn.
    const second = driver.encodeStdinMessage('mid-turn', 'thread-abc');
    expect(JSON.parse(second!).method).toBe('turn/steer');
    expect(JSON.parse(second!).params.expectedTurnId).toBe('turn-9');
  });

  it('an error response emits only the error event, no lifecycle frames', async () => {
    const { driver } = await readyDriver();
    const encoded = driver.encodeStdinMessage('input', 'thread-abc', {
      payloadUuid: 'uuid-B',
    });
    const frame = JSON.parse(encoded!);
    const events = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        id: frame.id,
        error: { message: 'steer rejected' },
      }),
    );
    expect(events.filter((e) => e.kind === 'input_lifecycle')).toEqual([]);
    expect(events.some((e) => e.kind === 'error')).toBe(true);
  });

  it.each(['threadId', 'sessionId', 'thread'])(
    'ignores foreign %s on lifecycle, text, tools and admission',
    async (field) => {
      const { driver } = await readyDriver();
      driver.parseLine(
        JSON.stringify({
          method: 'turn/started',
          params: { threadId: 'thread-abc', turn: { id: 'main-turn' } },
        }),
      );
      const scope = field === 'thread' ? { thread: { id: 'child' } } : { [field]: 'child' };
      const cases = [
        { method: 'thread/started', params: { thread: { id: 'child' } } },
        { method: 'turn/started', params: { ...scope, turn: { id: 'child-turn' } } },
        {
          method: 'turn/completed',
          params: { ...scope, turn: { id: 'child-turn', status: 'completed' } },
        },
        {
          method: 'item/agentMessage/delta',
          params: { ...scope, itemId: 'child-text', delta: 'foreign' },
        },
        {
          method: 'item/started',
          params: { ...scope, item: { type: 'userMessage', clientId: 'payload' } },
        },
        {
          method: 'item/started',
          params: {
            ...scope,
            item: { type: 'commandExecution', command: 'tool', id: 'child-tool' },
          },
        },
        { method: 'error', params: { ...scope, message: 'thread not found' } },
      ];
      for (const event of cases) {
        expect(driver.parseLine(JSON.stringify(event))).toEqual([]);
      }

      const request = JSON.parse(driver.encodeStdinMessage('next', null)!);
      expect(request.params).toMatchObject({ threadId: 'thread-abc', expectedTurnId: 'main-turn' });
    },
  );

  it('ignores unknown RPC results/errors and correlates each rejection only once', async () => {
    const { driver, io } = await readyDriver();
    io.onInputRejected = vi.fn();
    const request = JSON.parse(
      driver.encodeStdinMessage('next', null, { payloadUuid: 'payload' })!,
    );
    for (const result of [
      { thread: { id: 'child' } },
      { turn: { id: 'child-turn' } },
      { turnId: 'child-turn' },
    ]) {
      expect(driver.parseLine(JSON.stringify({ id: 999, result }))).toEqual([]);
    }
    expect(
      driver.parseLine(JSON.stringify({ id: 999, error: { message: 'thread not found' } })),
    ).toEqual([]);
    expect(io.onInputRejected).not.toHaveBeenCalled();
    const error = JSON.stringify({ id: request.id, error: { message: 'rejected' } });
    expect(driver.parseLine(error)).toEqual([{ kind: 'error', message: 'rejected' }]);
    expect(driver.parseLine(error)).toEqual([]);
    expect(io.onInputRejected).toHaveBeenCalledExactlyOnceWith('payload');
    expect(JSON.parse(driver.encodeStdinMessage('next', null)!).params.threadId).toBe('thread-abc');
  });

  it.each(['turn/start', 'turn/steer'])(
    'a late %s response cannot resurrect a completed turn',
    async (method) => {
      const { driver } = await readyDriver();
      if (method === 'turn/steer')
        driver.parseLine(
          JSON.stringify({
            method: 'turn/started',
            params: { threadId: 'thread-abc', turn: { id: 'old' } },
          }),
        );
      const request = JSON.parse(driver.encodeStdinMessage('next', null)!);
      expect(request.method).toBe(method);
      driver.parseLine(
        JSON.stringify({
          method: 'turn/completed',
          params: { threadId: 'thread-abc', turn: { id: 'old' } },
        }),
      );
      driver.parseLine(JSON.stringify({ id: request.id, result: { turnId: 'old' } }));
      expect(JSON.parse(driver.encodeStdinMessage('next', null)!).method).toBe('turn/start');
    },
  );

  it('a late start response or old completion cannot replace a newer active turn', async () => {
    const { driver } = await readyDriver();
    const request = JSON.parse(driver.encodeStdinMessage('next', null)!);
    driver.parseLine(
      JSON.stringify({
        method: 'turn/started',
        params: { threadId: 'thread-abc', turn: { id: 'new' } },
      }),
    );
    driver.parseLine(JSON.stringify({ id: request.id, result: { turn: { id: 'old' } } }));
    expect(
      driver.parseLine(
        JSON.stringify({
          method: 'turn/completed',
          params: { threadId: 'thread-abc', turn: { id: 'old' } },
        }),
      ),
    ).toEqual([]);
    expect(JSON.parse(driver.encodeStdinMessage('next', null)!).params.expectedTurnId).toBe('new');
  });

  it('lifecycle admission retires pending rejection correlation', async () => {
    const { driver, io } = await readyDriver();
    io.onInputRejected = vi.fn();
    const request = JSON.parse(
      driver.encodeStdinMessage('next', null, { payloadUuid: 'payload' })!,
    );
    driver.parseLine(
      JSON.stringify({
        method: 'item/started',
        params: { threadId: 'thread-abc', item: { type: 'userMessage', clientId: 'payload' } },
      }),
    );
    expect(
      driver.parseLine(JSON.stringify({ id: request.id, error: { message: 'late error' } })),
    ).toEqual([]);
    expect(io.onInputRejected).not.toHaveBeenCalled();
  });
});

describe('CodexDriver JSON-RPC handshake', () => {
  let cleanup: (() => void) | null = null;
  afterEach(() => {
    cleanup?.();
    cleanup = null;
  });

  it('binds session_init only to the requested thread/start response', async () => {
    const built = makeSpawnCtx();
    cleanup = built.cleanup;
    const io = makeIO(built.ctx);
    const driver = new CodexDriver();

    driver.bindIO(io);
    await tick();

    expect(io.lines).toHaveLength(1);
    const init = JSON.parse(io.lines[0]!);
    expect(init).toMatchObject({
      jsonrpc: '2.0',
      method: 'initialize',
      params: {
        clientInfo: { name: 'silverhand', version: '1.0.0' },
        capabilities: { experimentalApi: true },
      },
    });
    expect(typeof init.id).toBe('number');

    // Server replies to initialize → driver should send `initialized`
    // notification + `thread/start` request.
    const events1 = driver.parseLine(JSON.stringify({ jsonrpc: '2.0', id: init.id, result: {} }));
    expect(events1).toEqual([]);
    expect(io.lines).toHaveLength(3);
    const initialized = JSON.parse(io.lines[1]!);
    expect(initialized).toMatchObject({
      jsonrpc: '2.0',
      method: 'initialized',
      params: {},
    });
    expect(initialized.id).toBeUndefined();
    const threadStart = JSON.parse(io.lines[2]!);
    expect(threadStart).toMatchObject({
      jsonrpc: '2.0',
      method: 'thread/start',
      params: {
        cwd: built.ctx.workspacePath,
        approvalPolicy: 'never',
        sandbox: 'danger-full-access',
        developerInstructions: 'STANDING PROMPT\n',
      },
    });
    expect(threadStart.params).not.toHaveProperty('model');
    expect(threadStart.params).not.toHaveProperty('excludeTurns');

    // A notification may arrive first, but cannot choose the bound identity.
    const events2 = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'thread/started',
        params: { thread: { id: 'thread-abc' } },
      }),
    );
    expect(events2).toEqual([]);
    expect(driver.readyToEncode()).toBe(false);
    const reply = JSON.stringify({ id: threadStart.id, result: { thread: { id: 'thread-abc' } } });
    expect(driver.classifyInitProgress(reply)).toBe('session_ready');
    expect(driver.parseLine(reply)).toEqual([{ kind: 'session_init', sessionId: 'thread-abc' }]);

    // Subsequent thread/started with the SAME id should not re-announce.
    const events3 = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'thread/started',
        params: { thread: { id: 'thread-abc' } },
      }),
    );
    expect(events3.find((e) => e.kind === 'session_init')).toBeUndefined();
  });

  it('resumes the exact session without returning history or replacing its context', async () => {
    const built = makeSpawnCtx({ cliSessionId: 'thread-resumed' });
    cleanup = built.cleanup;
    const io = makeIO(built.ctx);
    const driver = new CodexDriver();

    driver.bindIO(io);
    await tick();
    const init = JSON.parse(io.lines[0]!);

    driver.parseLine(JSON.stringify({ jsonrpc: '2.0', id: init.id, result: {} }));
    const threadCall = JSON.parse(io.lines[2]!);
    expect(threadCall.method).toBe('thread/resume');
    expect(threadCall.params).toEqual({
      threadId: 'thread-resumed',
      cwd: built.ctx.workspacePath,
      approvalPolicy: 'never',
      sandbox: 'danger-full-access',
      developerInstructions: 'STANDING PROMPT\n',
      excludeTurns: true,
    });
    expect(threadCall.params).not.toHaveProperty('model');
  });

  it.each(['gpt-5.4', 'gpt-6-astra'])('uses %s for thread/start', async (model) => {
    const built = makeSpawnCtx({ model });
    cleanup = built.cleanup;
    const io = makeIO(built.ctx);
    const driver = new CodexDriver();

    driver.bindIO(io);
    await tick();
    const init = JSON.parse(io.lines[0]!);

    driver.parseLine(JSON.stringify({ jsonrpc: '2.0', id: init.id, result: {} }));
    const threadCall = JSON.parse(io.lines[2]!);
    expect(threadCall).toMatchObject({
      method: 'thread/start',
      params: {
        model,
      },
    });
  });

  it('passes the service tier on thread/start and omits it when unset', async () => {
    for (const serviceTier of ['ultrafast', null]) {
      const built = makeSpawnCtx({ serviceTier });
      const io = makeIO(built.ctx);
      const driver = new CodexDriver();
      driver.bindIO(io);
      await tick();
      const init = JSON.parse(io.lines[0]!);
      driver.parseLine(JSON.stringify({ jsonrpc: '2.0', id: init.id, result: {} }));
      const threadCall = JSON.parse(io.lines[2]!);
      if (serviceTier) expect(threadCall.params.serviceTier).toBe(serviceTier);
      else expect(threadCall.params).not.toHaveProperty('serviceTier');
      built.cleanup();
    }
  });

  it('does not send turn/start to a resume target before thread/resume is ready', async () => {
    const built = makeSpawnCtx({ cliSessionId: 'thread-resumed' });
    cleanup = built.cleanup;
    const io = makeIO(built.ctx);
    const driver = new CodexDriver();

    driver.bindIO(io);
    await tick();
    const init = JSON.parse(io.lines[0]!);
    driver.parseLine(JSON.stringify({ jsonrpc: '2.0', id: init.id, result: {} }));
    const resume = JSON.parse(io.lines[2]!);
    expect(resume.method).toBe('thread/resume');

    expect(driver.encodeStdinMessage('queued while starting', 'thread-resumed')).toBeNull();

    const events = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        id: resume.id,
        result: { thread: { id: 'thread-resumed' } },
      }),
    );
    expect(events).toEqual([{ kind: 'session_init', sessionId: 'thread-resumed' }]);

    const line = driver.encodeStdinMessage('after resume', 'thread-resumed');
    expect(line).not.toBeNull();
    const turn = JSON.parse(line!);
    expect(turn).toMatchObject({
      method: 'turn/start',
      params: {
        threadId: 'thread-resumed',
        input: [{ type: 'text', text: 'after resume' }],
      },
    });
  });

  it('fires the initial turn/start automatically when initialPrompt is set', async () => {
    const built = makeSpawnCtx({ initialPrompt: 'hello there' });
    cleanup = built.cleanup;
    const io = makeIO(built.ctx);
    const driver = new CodexDriver();

    driver.bindIO(io);
    await tick();
    const init = JSON.parse(io.lines[0]!);
    driver.parseLine(JSON.stringify({ jsonrpc: '2.0', id: init.id, result: {} }));
    // Correlated thread response triggers session_init AND startInitialTurn.
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        id: JSON.parse(io.lines[2]!).id,
        result: { thread: { id: 'thread-xyz' } },
      }),
    );

    const last = JSON.parse(io.lines[io.lines.length - 1]!);
    expect(last.method).toBe('turn/start');
    expect(last.params).toMatchObject({
      threadId: 'thread-xyz',
      input: [{ type: 'text', text: 'hello there' }],
    });
  });

  it('emits an error event when initialize fails', async () => {
    const built = makeSpawnCtx();
    cleanup = built.cleanup;
    const io = makeIO(built.ctx);
    const driver = new CodexDriver();

    driver.bindIO(io);
    await tick();
    const init = JSON.parse(io.lines[0]!);

    const events = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        id: init.id,
        error: { code: -32000, message: 'bad config' },
      }),
    );
    expect(events).toEqual([{ kind: 'error', message: 'bad config' }]);
    // No follow-up requests should be issued.
    expect(io.lines).toHaveLength(1);
  });

  it('marks thread-not-found resume errors as fatal', async () => {
    const built = makeSpawnCtx({ cliSessionId: 'thread-stale' });
    cleanup = built.cleanup;
    const io = makeIO(built.ctx);
    const driver = new CodexDriver();

    driver.bindIO(io);
    await tick();
    const init = JSON.parse(io.lines[0]!);
    driver.parseLine(JSON.stringify({ jsonrpc: '2.0', id: init.id, result: {} }));

    const resume = JSON.parse(io.lines[2]!);
    expect(resume.method).toBe('thread/resume');
    const events = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        id: resume.id,
        error: { code: -32602, message: 'thread not found: thread-stale' },
      }),
    );
    expect(events).toEqual([
      {
        kind: 'error',
        message: 'thread not found: thread-stale',
        code: 'thread_not_found',
        fatal: true,
      },
    ]);
  });

  it('marks archived resume session errors as fatal', async () => {
    const built = makeSpawnCtx({
      cliSessionId: '019f3ba4-a772-7010-a52a-8cdac98ddfd5',
    });
    cleanup = built.cleanup;
    const io = makeIO(built.ctx);
    const driver = new CodexDriver();

    driver.bindIO(io);
    await tick();
    const init = JSON.parse(io.lines[0]!);
    driver.parseLine(JSON.stringify({ jsonrpc: '2.0', id: init.id, result: {} }));

    const resume = JSON.parse(io.lines[2]!);
    expect(resume.method).toBe('thread/resume');
    const message =
      'session 019f3ba4-a772-7010-a52a-8cdac98ddfd5 is archived. Run codex unarchive 019f3ba4-a772-7010-a52a-8cdac98ddfd5 to unarchive it first.';
    const events = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        id: resume.id,
        error: { code: -32602, message },
      }),
    );
    expect(events).toEqual([
      {
        kind: 'error',
        message,
        code: 'session_archived',
        fatal: true,
      },
    ]);
  });
});

describe('CodexDriver.parseLine event mapping', () => {
  it('returns kind=unknown for non-JSON lines', () => {
    const events = new CodexDriver().parseLine('not json at all');
    expect(events).toEqual([{ kind: 'unknown', raw: 'not json at all' }]);
  });

  it('ignores empty / whitespace-only lines', () => {
    expect(new CodexDriver().parseLine('   ')).toEqual([]);
  });

  it('emits a zero-width thinking event on turn/started', () => {
    const events = new CodexDriver().parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'turn/started',
        params: { turn: { id: 'turn-1' } },
      }),
    );
    expect(events).toEqual([{ kind: 'thinking', text: '' }]);
  });

  it('emits text on item/completed agentMessage when no delta was streamed', () => {
    const events = new CodexDriver().parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/completed',
        params: { item: { type: 'agentMessage', id: 'msg-2', text: 'Whole message' } },
      }),
    );
    expect(events).toEqual([{ kind: 'text', text: 'Whole message' }]);
  });

  it('translates mcpToolCall items with the chat-server prefix shorthand', () => {
    const driver = new CodexDriver();
    const started = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/started',
        params: {
          item: {
            type: 'mcpToolCall',
            server: 'chat',
            tool: 'send_message',
            arguments: { text: 'hi' },
          },
        },
      }),
    );
    expect(started).toEqual([
      {
        kind: 'tool_call',
        name: 'mcp_chat_send_message',
        input: { text: 'hi' },
        tool: { type: 'mcp', server: 'chat', tool: 'send_message' },
      },
    ]);

    const completed = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/completed',
        params: {
          item: {
            type: 'mcpToolCall',
            server: 'chat',
            tool: 'send_message',
          },
        },
      }),
    );
    expect(completed).toEqual([
      { kind: 'tool_output', name: 'mcp_chat_send_message', outcome: 'completed' },
    ]);
  });

  it('translates shell command execution items', () => {
    const driver = new CodexDriver();
    const start = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/started',
        params: { item: { type: 'commandExecution', command: 'ls -al' } },
      }),
    );
    expect(start).toEqual([
      {
        kind: 'tool_call',
        name: 'shell',
        input: { command: 'ls -al' },
        tool: { type: 'command', command: 'ls -al' },
      },
    ]);
    const end = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/completed',
        params: { item: { type: 'commandExecution', command: 'ls -al' } },
      }),
    );
    expect(end).toEqual([{ kind: 'tool_output', name: 'shell', outcome: 'completed' }]);
  });

  it('emits compaction lifecycle events', () => {
    const driver = new CodexDriver();
    expect(
      driver.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'item/started',
          params: { item: { type: 'contextCompaction' } },
        }),
      ),
    ).toEqual([{ kind: 'compaction_started' }]);
    expect(
      driver.parseLine(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'item/completed',
          params: { item: { type: 'contextCompaction' } },
        }),
      ),
    ).toEqual([{ kind: 'compaction_finished' }]);
  });

  it('emits turn_end on turn/completed and surfaces failed turns as errors', async () => {
    const driver = new CodexDriver();
    await bindThread(driver, 'thread-q');
    // Need a threadId so the sessionId field is populated.
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'thread/started',
        params: { thread: { id: 'thread-q' } },
      }),
    );
    const failed = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'turn/completed',
        params: {
          turn: {
            id: 't-1',
            status: 'failed',
            error: { message: 'rate limited' },
          },
        },
      }),
    );
    expect(failed).toEqual([
      { kind: 'error', message: 'rate limited' },
      { kind: 'turn_end', sessionId: 'thread-q' },
    ]);
  });

  it('pairs webSearch start with a tool_output on completion', () => {
    const driver = new CodexDriver();
    const start = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/started',
        params: { item: { type: 'webSearch', id: 'ws-1', query: 'silverhand' } },
      }),
    );
    expect(start).toEqual([
      {
        kind: 'tool_call',
        name: 'web_search',
        input: { query: 'silverhand' },
        tool: { type: 'web_search', query: 'silverhand' },
        toolCallId: 'ws-1',
      },
    ]);

    const end = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/completed',
        params: { item: { type: 'webSearch', id: 'ws-1' } },
      }),
    );
    expect(end).toEqual([
      { kind: 'tool_output', name: 'web_search', toolCallId: 'ws-1', outcome: 'completed' },
    ]);
  });

  it('clears pending tool counts on turn/completed to bound state across turns', () => {
    const driver = new CodexDriver();
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'thread/started',
        params: { thread: { id: 'thread-clean' } },
      }),
    );
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/started',
        params: { item: { type: 'webSearch', id: 'ws-leak', query: 'q' } },
      }),
    );
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/started',
        params: {
          item: { type: 'fileChange', id: 'fc-leak', changes: [{ path: 'x.ts' }] },
        },
      }),
    );
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'turn/completed',
        params: { turn: { id: 't-clean', status: 'completed' } },
      }),
    );

    // Re-firing item/completed for the same ids should not re-emit tool_output;
    // the pending counts were cleared when the turn ended.
    const reCompleted = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/completed',
        params: { item: { type: 'webSearch', id: 'ws-leak' } },
      }),
    );
    expect(reCompleted).toEqual([]);
    const reCompletedFc = driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/completed',
        params: { item: { type: 'fileChange', id: 'fc-leak' } },
      }),
    );
    expect(reCompletedFc).toEqual([]);
  });
});

describe('CodexDriver.encodeStdinMessage', () => {
  it('returns null until threadId is established', () => {
    const driver = new CodexDriver();
    expect(driver.encodeStdinMessage('hi', null)).toBeNull();
  });

  it('encodes turn/start when no turn is active', async () => {
    const driver = new CodexDriver();
    await bindThread(driver, 'thread-1');
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'thread/started',
        params: { thread: { id: 'thread-1' } },
      }),
    );
    const line = driver.encodeStdinMessage('hello', null);
    expect(line).not.toBeNull();
    const msg = JSON.parse(line!);
    expect(msg).toMatchObject({
      jsonrpc: '2.0',
      method: 'turn/start',
      params: {
        threadId: 'thread-1',
        input: [{ type: 'text', text: 'hello' }],
      },
    });
  });

  it('encodes turn/steer when a turn is active', async () => {
    const driver = new CodexDriver();
    await bindThread(driver, 'thread-1');
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'thread/started',
        params: { thread: { id: 'thread-1' } },
      }),
    );
    driver.parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'turn/started',
        params: { turn: { id: 'turn-9' } },
      }),
    );
    const line = driver.encodeStdinMessage('please pivot', null);
    const msg = JSON.parse(line!);
    expect(msg).toMatchObject({
      method: 'turn/steer',
      params: {
        threadId: 'thread-1',
        expectedTurnId: 'turn-9',
        input: [{ type: 'text', text: 'please pivot' }],
      },
    });
  });
});

/** Walk an argv list collecting every `-c key=value` pair into a map. */
function collectCArgs(args: string[]): Map<string, string> {
  const out = new Map<string, string>();
  for (let i = 0; i < args.length; i++) {
    if (args[i] === '-c' && i + 1 < args.length) {
      const next = args[i + 1]!;
      const eq = next.indexOf('=');
      if (eq > 0) out.set(next.slice(0, eq), next.slice(eq + 1));
      i++;
    }
  }
  return out;
}

it('emits only terminal markers for command and MCP results', () => {
  for (const item of [
    { type: 'commandExecution', aggregatedOutput: 'private body' },
    { type: 'mcpToolCall', server: 'example', tool: 'read', result: { content: 'private body' } },
  ]) {
    const events = new CodexDriver().parseLine(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'item/completed',
        params: { item: { id: 'marker-call', status: 'completed', exitCode: 0, ...item } },
      }),
    );
    const marker = events.find((event) => event.kind === 'tool_output');
    expect(marker).toBeDefined();
    expect(marker).not.toHaveProperty('output');
    expect(JSON.stringify(events)).not.toContain('private body');
  }
});
