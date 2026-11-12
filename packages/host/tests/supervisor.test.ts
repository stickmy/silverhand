/**
 * Supervisor 3a — spawn / output / hard stop / exit / status.
 *
 * Scope locked with Cindy in `jhH-vC1s_1J_jTLu`: this suite covers
 * execution-plane behavior only. Parse / deliver / watchdog
 * / lifecycle-lock land in 3b/3c with their own test suites.
 *
 * Test strategy: substitute a fake `Driver` whose `binary` points at
 * `tests/_fixtures/echo-runtime.cjs` (driven via argv flags) so we
 * can exercise real child_process.spawn against a controlled
 * binary. The supervisor itself is unmodified — only the test seam
 * is the driver factory + binary resolver.
 *
 * Timing: every test uses tight `sigtermToSigkillMs` / `sigkillTimeoutMs`
 * (50ms / 200ms) so the SIGKILL cascade completes inside the vitest
 * default 5s budget even when we deliberately exercise the
 * `--ignore-sigterm` path.
 */
import { afterAll, describe, expect, it, vi } from 'vitest';
import { EventEmitter } from 'node:events';
import { join } from 'node:path';
import { setTimeout as wait } from 'node:timers/promises';
import { execPath } from 'node:process';
import { spawn, type ChildProcess, type SpawnOptions } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { composeRuntimeDeliveryPresentation, parseDeliveryTarget } from '@silverhand/shared';

import { Supervisor } from '../src/daemon/supervisor/index.js';
import type { DeliveryOwnerContext } from '../src/daemon/supervisor-contract.js';
import { CodexDriver } from '../src/drivers/codex.js';
import type { Driver, DriverIO, DriverSpawnContext, ParsedEvent } from '../src/drivers/index.js';
import { QoderDriver } from '../src/drivers/qoder.js';
import { PiDriver } from '../src/drivers/pi.js';
import { GrokDriver } from '../src/drivers/grok.js';
import { createDaemonLogControl, type DaemonLogger } from '../src/daemon/logger.js';
import type { EventFrame, RuntimeStartResult, StartOptions } from '../src/protocol.js';
import { runtimePidFilePath } from '../src/daemon/runtime-pidfiles.js';
import { testStartOptions, testRuntimeStartResolveDeps } from './_helpers/runtime-start-options.js';

const FIXTURE = join(__dirname, '_fixtures', 'echo-runtime.cjs');
/** 3c-b watchdog fixture — produces zero bytes until killed. See
 *  `tests/_fixtures/no-output-runtime.cjs` for rationale (Cindy
 *  `HyC05fC-FEs94uoZ` B1). Tests that need to observe silence-tier
 *  firing must point `driver.binary` + `buildArgs` at this file so the
 *  spawn seam (`realSpawn(execPath, [bin, ...args.slice(1)])`)
 *  executes `node no-output-runtime.cjs` without any positional
 *  arguments leaking through. */
const NO_OUTPUT_FIXTURE = join(__dirname, '_fixtures', 'no-output-runtime.cjs');

/**
 * relocation-v1 §3.2 / §7 task 4: the daemon now resolves a regular
 * workspace's cwd to the deterministic agent-centric bundle dir
 * (`<agentsRoot>/<agentId>/workspaces/<workspaceId>`), NOT the
 * old registry workspace path. shiki + Cindy 2026-08-04: the session cwd is the
 * ONLY hard-ensured runtime base — the supervisor's pre-spawn
 * `ensureRuntimeStartWorkspaceCwd` does `mkdir -p` then requires a USABLE cwd
 * (`bundleCwdUsable`: a directory this process can enter). L1/L2 CONTENT
 * (MEMORY.md) is best-effort and NEVER gates the spawn. These suites default
 * `testRuntimeStartResolveDeps` to a shared temp AGENTS ROOT (via
 * `makeSupervisor`; #task:1813 — the root is always explicit, never derived from
 * a home) and pre-create the bundle dirs for the two agent ids that boot
 * runtimes here (`a1` from `baseStartParams`, `aX` from the deliver-SPAWN test)
 * so the shared root is realized; the pre-spawn ensure would mkdir them anyway.
 * Tests that need an UNUSABLE cwd (path occupied by a non-dir file) pass their
 * own agents root and skip this. */
const SUPERVISOR_AGENTS_ROOT = mkdtempSync(join(tmpdir(), 'sh-sup-agents-'));
function ensureSupervisorBundle(agentId: string, workspaceId = 'ws1'): void {
  const bundleDir = join(SUPERVISOR_AGENTS_ROOT, agentId, 'workspaces', workspaceId);
  // The cwd is the only hard prerequisite — a directory the process can enter.
  // MEMORY.md is best-effort enrichment and is intentionally NOT written here.
  mkdirSync(bundleDir, { recursive: true });
}
ensureSupervisorBundle('a1');
ensureSupervisorBundle('aX');
afterAll(() => {
  try {
    rmSync(SUPERVISOR_AGENTS_ROOT, { recursive: true, force: true });
  } catch {
    /* best-effort */
  }
});

/**
 * TS assertion helper: narrow a `RuntimeStartResult` to the success
 * arm so subsequent `.pid` / `.started_at` accesses typecheck. Throws
 * if the result is the cooldown arm — every existing 3a/3b test
 * expects a successful start, so the failure mode is "test setup is
 * wrong, not the code under test".
 *
 * 3c (Cindy `-pdNvRYOWip5e9yA`) replaced the flat result shape with a
 * discriminated union: `{started:true, pid, started_at, ...}` vs
 * `{started:false, reason:'init_cooldown', cooldown_until}`. Cooldown-
 * specific tests use the discriminator directly.
 */
function assertStarted(
  res: RuntimeStartResult,
): asserts res is Extract<RuntimeStartResult, { started: true }> {
  if (!res.started) {
    throw new Error(`expected start() to succeed, got cooldown: ${JSON.stringify(res)}`);
  }
}

/** Build a noop logger that satisfies the `DaemonLogger` interface
 *  without actually writing. Tests don't assert on log contents; the
 *  §3.4 typed-helper tests live in `daemon-logging.test.ts`. */
function silentLogger(): DaemonLogger {
  const noop = (): void => {};
  return {
    filePath: '/dev/null',
    logControl: createDaemonLogControl('debug'),
    debug: noop,
    info: noop,
    warn: noop,
    error: noop,
    close: async () => {},
  };
}

/**
 * Fake driver that wraps the echo-runtime fixture. `extraArgs` lets
 * a single test customize the fixture's behavior (e.g. ask it to
 * write to stderr, or to ignore SIGTERM).
 *
 * Option A v2 (Cindy `UMGu36zoTDVuKWXi`): daemon now calls
 * `encodeStdinMessage` itself for `deliver` (spawn + write arms).
 * The previous default of returning `null` would now
 * surface as `driver_refused` and break every supervisor test that
 * exercises stdin writes. We mimic the real claude driver by
 * encoding the text into a stream-json `{"type":"user", …}` line so
 * the echo-stdin fixture's `STDIN:{"type":"user"` assertions keep
 * passing without coupling the fake to claude's exact schema.
 * `readyToEncode()` defaults to true (matches the stateless claude
 * driver) so the `ready_for_drain` / deliver gates fire immediately;
 * tests that need a stateful pre-`thread/started` driver build a
 * separate factory.
 */
function encodeStreamJsonUser(text: string): string {
  return JSON.stringify({
    type: 'user',
    message: { role: 'user', content: text },
  });
}

function testDeliveryPresentation(body: string) {
  return { body, instruction: 'other' as const };
}

/**
 * The two coordinates a send response OWES the daemon since #task:2021 P4-4:
 * WHICH stream the address resolved to, and which reply cluster the message
 * landed in. Derived here from the request's own `target` token with the shared
 * parser — the same split the backend resolver performs — so a stub can never
 * answer for a stream the test did not address, and so a send whose address this
 * grammar cannot carry fails the test loudly instead of quietly taking the
 * unresolved first-touch path.
 *
 * Which is also why the conversation and message ids in this suite are spelled in
 * the real id domain (16 base64url chars) rather than `c1` / `mRoot`: an address
 * only carries that domain, and `delivery-target.ts` states the rule these
 * fixtures now follow — "fixtures do not get a vote".
 */
function landedStream(target: unknown): {
  conversation_id: string;
  thread_root_id: string | null;
} {
  const parsed = typeof target === 'string' ? parseDeliveryTarget(target) : null;
  if (parsed === null) {
    throw new Error(
      `test stub: send target is not an addressable stream: ${JSON.stringify(target)}`,
    );
  }
  return {
    conversation_id: parsed.conversationId,
    thread_root_id: parsed.threadRootId,
  };
}

function makeFakeDriver(extraArgs: string[] = []): Driver {
  return {
    kind: 'claude',
    binary: FIXTURE, // resolveRuntime stub returns this verbatim
    kickoffMode: 'self_inits',
    releaseProfile: 'native-steer',
    admissionPolicy: 'write-success',
    oneShot: false,
    supportsNativeStandingPrompt: true,
    initLivenessSilenceMs: 30_000,
    initPreProtocolCapMs: 5 * 60_000,
    initPostProtocolMs: 60_000,
    buildArgs: (_ctx: DriverSpawnContext): string[] => [FIXTURE, ...extraArgs],
    applyEnv: (env) => ({ ...env }),
    encodeStdinMessage: (text: string) => encodeStreamJsonUser(text),
    encodeInterruptControl: () => null,
    parseLine: (_line: string): ParsedEvent[] => [],
    classifyInitProgress: () => null,
  };
}

/** Spy logger that records every event emit so individual assertions
 *  can scan for runtime_state / runtime_output / runtime_exit. */
function makeEmitSpy(): { emit: (e: EventFrame) => void; events: EventFrame[] } {
  const events: EventFrame[] = [];
  return { emit: (e) => events.push(e), events };
}

/** Controls only child I/O and exit timing; Supervisor still owns admission. */
class ControlledRuntime extends EventEmitter {
  readonly pid = 45_100;
  readonly stdout = new EventEmitter();
  readonly stderr = new EventEmitter();
  readonly writes: Array<{ line: string; finish: (error?: Error) => void }> = [];
  private exited = false;
  writeResolved = false;
  constructor(private readonly autoFinish = false) {
    super();
  }
  readonly stdin = Object.assign(new EventEmitter(), {
    destroyed: false,
    end: () => {},
    write: (line: string, callback?: (error?: Error | null) => void): boolean => {
      let finished = false;
      const write = {
        line,
        finish: (error?: Error) => {
          if (finished) return;
          finished = true;
          this.writeResolved = true;
          callback?.(error);
        },
      };
      this.writes.push(write);
      if (this.autoFinish) setImmediate(() => write.finish());
      return true;
    },
  });

  exit(code = 0): void {
    if (this.exited) return;
    this.exited = true;
    this.emit('exit', code, null);
    this.stdout.emit('end');
    this.stdout.emit('close');
    this.emit('close', code, null);
  }

  kill(): boolean {
    setImmediate(() => this.exit());
    return true;
  }

  dispose(): void {
    for (const write of this.writes) write.finish(new Error('fixture closed'));
    this.exit();
  }
}

function makeSupervisor(
  opts: {
    driverArgs?: string[];
    sigtermToSigkillMs?: number;
    sigkillTimeoutMs?: number;
    spawnFn?: (b: string, a: ReadonlyArray<string>, o: SpawnOptions) => ChildProcess;
    daemonInstanceId?: string;
    runtimePidRootDir?: string | null;
    runtimeStartResolveDeps?: ReturnType<typeof testRuntimeStartResolveDeps>;
    emitRawRuntimeOutput?: boolean;
    logger?: DaemonLogger;
    /** Observe the DriverSpawnContext the supervisor hands to buildArgs. */
    onSpawnCtx?: (ctx: DriverSpawnContext) => void;
  } = {},
): {
  supervisor: Supervisor;
  events: EventFrame[];
} {
  const spy = makeEmitSpy();
  const baseDriver = makeFakeDriver(opts.driverArgs);
  const driver: Driver = opts.onSpawnCtx
    ? {
        ...baseDriver,
        buildArgs: (ctx) => {
          opts.onSpawnCtx!(ctx);
          return baseDriver.buildArgs(ctx);
        },
      }
    : baseDriver;
  // Node fixture is a .cjs file; invoke via the same node binary the
  // test runner uses so we don't depend on the fixture being chmod +x.
  const realSpawn = spawn;
  const spawnFn =
    opts.spawnFn ?? ((bin, args, options) => realSpawn(execPath, [bin, ...args.slice(1)], options));
  const supervisor = new Supervisor({
    emit: spy.emit,
    logger: opts.logger ?? silentLogger(),
    driverFor: () => driver,
    resolveRuntime: async (_kind, fallback) => ({ binary: fallback, version: null }),
    spawnFn,
    daemonInstanceId: opts.daemonInstanceId,
    runtimePidRootDir: opts.runtimePidRootDir,
    runtimeStartResolveDeps:
      opts.runtimeStartResolveDeps ??
      testRuntimeStartResolveDeps({ agentHomeRootDir: SUPERVISOR_AGENTS_ROOT }),
    controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
    emitRawRuntimeOutput: opts.emitRawRuntimeOutput ?? true,
    sigtermToSigkillMs: opts.sigtermToSigkillMs ?? 50,
    sigkillTimeoutMs: opts.sigkillTimeoutMs ?? 200,
  });
  return { supervisor, events: spy.events };
}

const baseStartParams = {
  conversation_id: 'Conversation0001',
  agent_id: 'a1',
  runtime_generation: 1,
  options: testStartOptions({
    conversationId: 'Conversation0001',
    agentId: 'a1',
    agentName: 'tester',
    runtime: 'claude',
  }),
};

async function waitFor(cond: () => boolean, timeoutMs = 2_000, intervalMs = 10): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (cond()) return;
    await wait(intervalMs);
  }
  throw new Error(`waitFor timed out after ${timeoutMs}ms`);
}

/**
 * Wait until a `deliver()` WRITE-arm batch has been consumed. Under
 * `runtime_deliver` the WRITE arm returns `admitted` SYNC but emits
 * `runtime_delivery_consumed` ASYNC (from `consumeStartupDelivery`, once
 * the batch is written to stdin). Tests that then act on the delivered
 * turn must wait for that consumed ack first.
 */
async function waitForDeliveryConsumed(events: EventFrame[], deliveryId: number): Promise<void> {
  await waitFor(() =>
    events.some(
      (e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(deliveryId),
    ),
  );
}

/**
 * Wait until the spawned runtime has surfaced its `READY` line, which
 * (for the self-init echo-runtime fixture) means the handle has left
 * the `spawning`/`startup_pending` phase and reached `ready`. A
 * `deliver()` WRITE arm only admits on a ready handle — delivering
 * before READY would hit the `startup_pending` refuse arm.
 */
async function waitForRuntimeReady(events: EventFrame[]): Promise<void> {
  await waitFor(() =>
    events.some((e) => e.event === 'runtime_output' && e.chunk.includes('READY')),
  );
}

describe('Supervisor 3a — spawn / status / phase emit', () => {
  it('logs required Silverhand env presence for both child and CLI shim without values', async () => {
    const infoRecords: unknown[] = [];
    const logger: DaemonLogger = {
      ...silentLogger(),
      info: (obj) => infoRecords.push(obj),
    };
    const { supervisor } = makeSupervisor({ logger });

    try {
      const res = await supervisor.start(baseStartParams);
      assertStarted(res);
      const attempt = infoRecords.find(
        (record) =>
          typeof record === 'object' &&
          record !== null &&
          (record as { action?: unknown }).action === 'runtime_spawn_attempt',
      ) as
        | {
            silverhand_env_presence?: {
              child: unknown;
              shim: unknown;
            };
          }
        | undefined;

      const expectedPresence = {
        all_present: true,
        present_keys: [
          'SILVERHAND_CONVERSATION_ID',
          'SILVERHAND_AGENT_ID',
          'SILVERHAND_SERVER_URL',
          'SILVERHAND_AUTH_TOKEN',
        ],
        missing_keys: [],
      };
      expect(attempt?.silverhand_env_presence).toEqual({
        child: expectedPresence,
        shim: expectedPresence,
      });
    } finally {
      await supervisor
        .stop({
          runtime_scope: {
            kind: 'conversation',
            conversation_id: 'Conversation0001',
            agent_id: 'a1',
          },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => {});
    }
  });

  it('mkdirs a missing session cwd then spawns — L1/L2 content is never a spawn gate', async () => {
    // shiki + Cindy 2026-08-04: the session cwd is the ONLY hard-ensured
    // runtime base. The daemon resolves the regular workspace cwd to the
    // deterministic bundle dir under the agent home (relocation-v1 §3.2). We
    // give a real agents root but DON'T pre-create the bundle: the pre-spawn
    // `ensureRuntimeStartWorkspaceCwd` must `mkdir -p` it and proceed to spawn.
    // A missing MEMORY.md (best-effort L2) must NOT block the start.
    const agentsRoot = mkdtempSync(join(tmpdir(), 'silverhand-missing-cwd-'));
    const bundleDir = join(agentsRoot, 'a1', 'workspaces', 'ws1');
    expect(existsSync(bundleDir)).toBe(false);
    const { supervisor } = makeSupervisor({
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: agentsRoot,
      }),
    });

    try {
      const res = await supervisor.start(baseStartParams);
      assertStarted(res);
      // The cwd was hard-ensured (created) even though it did not pre-exist,
      // and no MEMORY.md was needed.
      expect(existsSync(bundleDir)).toBe(true);
      expect(existsSync(join(bundleDir, 'MEMORY.md'))).toBe(false);
    } finally {
      await supervisor
        .stop({
          runtime_scope: {
            kind: 'conversation',
            conversation_id: 'Conversation0001',
            agent_id: 'a1',
          },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => {});
      rmSync(agentsRoot, { recursive: true, force: true });
    }
  });

  it('fails typed before spawn ONLY when the session cwd cannot be created (non-dir file occupies the path), without leaking the host path onto the wire', async () => {
    // The one hard-gate on the forward path: the OS genuinely cannot spawn a
    // child in the cwd. Occupy the bundle path with a regular FILE so `mkdir`
    // throws (ENOTDIR/EEXIST). This must fail typed with
    // `workspace_bundle_missing` and never spawn.
    const agentsRoot = mkdtempSync(join(tmpdir(), 'silverhand-unusable-cwd-'));
    const bundleDir = join(agentsRoot, 'a1', 'workspaces', 'ws1');
    // Create the parent chain, then plant a FILE where the cwd dir must be.
    mkdirSync(join(agentsRoot, 'a1', 'workspaces'), { recursive: true });
    writeFileSync(bundleDir, 'not a directory\n', 'utf8');
    let spawnCalled = false;
    const { supervisor } = makeSupervisor({
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: agentsRoot,
      }),
      spawnFn: () => {
        spawnCalled = true;
        throw new Error('spawn should not be called');
      },
    });

    try {
      // relocation-v1 §7 task 3 (Cindy nail 1): the typed error's wire
      // `message` MUST NOT carry the absolute cwd path (it is forwarded
      // verbatim to the API/renderer). It names only the semantic id + kind;
      // the concrete path stays a host-only log diagnostic.
      const err = await supervisor.start(baseStartParams).then(
        () => {
          throw new Error('expected start to reject');
        },
        (e: unknown) => e as { code?: string; message?: string },
      );
      expect(err.code).toBe('workspace_bundle_missing');
      expect(err.message).not.toContain(bundleDir);
      expect(err.message).not.toContain(agentsRoot);
      expect(err.message).toContain(baseStartParams.options.workspace!.id);
      expect(spawnCalled).toBe(false);
    } finally {
      rmSync(agentsRoot, { recursive: true, force: true });
    }
  });

  it('contains spawn children that report no pid so their async error cannot crash the daemon', async () => {
    const fakeChild = Object.assign(new EventEmitter(), {
      pid: undefined,
      stdout: null,
      stderr: null,
      stdin: null,
      kill: () => true,
    }) as unknown as ChildProcess;
    const { supervisor } = makeSupervisor({
      spawnFn: () => fakeChild,
    });

    await expect(supervisor.start(baseStartParams)).rejects.toThrow(/child spawned without a pid/);
    expect(() => {
      fakeChild.emit('error', new Error('spawn failed after return'));
    }).not.toThrow();
  });

  it('start spawns a child with a real pid and emits runtime_state (starting → idle) + runtime_ready_for_drain', async () => {
    const { supervisor, events } = makeSupervisor();
    const res = await supervisor.start(baseStartParams);

    assertStarted(res);
    expect(res.pid).toBeGreaterThan(0);
    expect(typeof res.started_at).toBe('number');

    // After setImmediate ticks, the ready-for-drain event should land.
    await waitFor(() => events.some((e) => e.event === 'runtime_ready_for_drain'));

    const states = events.filter((e) => e.event === 'runtime_state');
    const startingStates = states.filter(
      (e) => e.event === 'runtime_state' && e.state.status === 'starting',
    );
    const idleStates = states.filter(
      (e) => e.event === 'runtime_state' && e.state.status === 'idle',
    );
    expect(startingStates.length).toBeGreaterThanOrEqual(1);
    expect(idleStates.length).toBeGreaterThanOrEqual(1);

    // status() reflects the live handle.
    const snap = supervisor.status({});
    expect(snap.runtimes).toHaveLength(1);
    expect(snap.runtimes[0]).toMatchObject({
      conversation_id: 'Conversation0001',
      agent_id: 'a1',
      phase: 'ready',
      pid: res.pid,
      started_at: res.started_at,
    });

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('hydrates prompt/spawn/env on daemon side from typed runtime_start options and drains the deliver SPAWN batch', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'silverhand-d2g-hydrate-'));
    // D4.e-6: options are semantic-only; the daemon resolves home_dir as ONE
    // join onto the explicit agents root (`resolveAgentDir`) and the regular
    // workspace via the injected resolver. Lay out L1 under that same
    // coordinate so the resolved home_dir points at the system_prompt.md we
    // write.
    const agentHomeRootDir = join(dir, 'agents');
    const agentHome = join(agentHomeRootDir, 'a1');
    // The L2/cwd is the agent-centric bundle dir under the agent home
    // (relocation-v1 §3.2), keyed by workspace id 'ws1' and derived from the
    // agents root. The pre-spawn ensure only requires the cwd to be a usable dir;
    // MEMORY.md is best-effort L2 whose CONTENT this test asserts is hydrated
    // into the prompt below.
    const memoryDir = join(agentHome, 'workspaces', 'ws1');
    mkdirSync(agentHome, { recursive: true });
    mkdirSync(join(memoryDir, 'notes'), { recursive: true });
    writeFileSync(join(agentHome, 'system_prompt.md'), 'ROLE_D2G', 'utf8');
    writeFileSync(join(memoryDir, 'MEMORY.md'), 'MEMORY_D2G', 'utf8');
    writeFileSync(
      join(memoryDir, 'notes', 'runtime.md'),
      [
        '---',
        'name: runtime-evidence',
        'description: Runtime spawn evidence; read before changing hydration.',
        'metadata:',
        '  category: project',
        '---',
        '# Runtime evidence',
        'NOTE_BODY_MUST_NOT_BE_INJECTED',
      ].join('\n'),
      'utf8',
    );

    let promptText = '';
    const captured: { ctx?: DriverSpawnContext } = {};
    const driver: Driver = {
      ...makeFakeDriver(['--echo-stdin']),
      buildArgs: (ctx) => {
        captured.ctx = ctx;
        promptText = readFileSync(ctx.systemPromptPath, 'utf8');
        return [FIXTURE, '--echo-stdin'];
      },
    };
    const spy = makeEmitSpy();
    const supervisor = new Supervisor({
      emit: spy.emit,
      logger: silentLogger(),
      driverFor: () => driver,
      resolveRuntime: async (_kind, fallback) => ({ binary: fallback, version: null }),
      spawnFn: (bin, args, options) => spawn(execPath, [bin, ...args.slice(1)], options),
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir,
      }),
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
      sigtermToSigkillMs: 50,
      sigkillTimeoutMs: 200,
    });

    try {
      const res = await supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          spawn_options: testStartOptions({
            conversationId: 'Conversation0001',
            agentId: 'a1',
          }),
          delivery_ids: [11],
          kind: 'message',
          text: 'startup delivery D2G',
          deliveries: [
            {
              delivery_id: 11,
              seq: 11,
              presentation: testDeliveryPresentation('startup delivery D2G'),
            },
          ],
        },
        { owner: 'test-owner' },
      );
      expect(res.disposition).toBe('spawning');
      expect(captured.ctx).toBeDefined();
      const ctx = captured.ctx!;
      // The spawn cwd is the resolved agent-centric bundle dir (== memoryDir),
      // no longer the registry establishment-gate path.
      expect(ctx.workspacePath).toBe(memoryDir);
      expect(ctx.systemPromptPath).toMatch(/spawn-.+system-prompt\.md$/);
      expect(promptText).toContain('ROLE_D2G');
      expect(promptText).toContain('MEMORY_D2G');
      expect(promptText).toContain('## Memory Notes Catalog');
      expect(promptText).toContain('runtime-evidence');
      expect(promptText).toContain('Runtime spawn evidence');
      expect(promptText).not.toContain('NOTE_BODY_MUST_NOT_BE_INJECTED');
      expect(promptText).not.toContain('CONSENSUS_D2G');
      await waitFor(() => spy.events.some((e) => e.event === 'runtime_delivery_consumed'));
      expect(
        spy.events.some((e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids[0] === 11),
      ).toBe(true);
      await supervisor.stop({
        runtime_scope: {
          kind: 'conversation',
          conversation_id: 'Conversation0001',
          agent_id: 'a1',
        },
        runtime_generation: 1,
        mode: 'hard',
      });
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('AgentSlot spawn → clear/reset → respawn preserves one L1 cwd/prompt and never exports an ambient conversation env', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'silverhand-agent-slot-respawn-'));
    const agentHomeRootDir = join(dir, 'agents');
    const agentHome = join(agentHomeRootDir, 'a1');
    mkdirSync(agentHome, { recursive: true });
    writeFileSync(join(agentHome, 'system_prompt.md'), 'AGENT_SLOT_ROLE', 'utf8');
    writeFileSync(join(agentHome, 'MEMORY.md'), 'AGENT_SLOT_MEMORY', 'utf8');
    const spawnContexts: DriverSpawnContext[] = [];
    const promptTexts: string[] = [];
    const childEnvs: Array<NodeJS.ProcessEnv> = [];
    const { supervisor, events } = makeSupervisor({
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({ agentHomeRootDir }),
      onSpawnCtx: (ctx) => {
        spawnContexts.push(ctx);
        promptTexts.push(readFileSync(ctx.systemPromptPath, 'utf8'));
      },
      spawnFn: (bin, args, options) => {
        childEnvs.push(options.env ?? {});
        return spawn(execPath, [bin, ...args.slice(1)], options);
      },
    });
    const options = testStartOptions({
      agentId: 'a1',
      sessionScope: 'agent',
    });
    const deliver = (conversationId: string, generation: number, deliveryId: number) =>
      supervisor.deliver(
        {
          runtime_generation: generation,
          conversation_id: conversationId,
          target: {
            conversation_id: conversationId,
            workspace:
              conversationId === 'Conversation0002'
                ? { id: 'ws-action', name: 'Action Workspace' }
                : null,
          },
          agent_id: 'a1',
          delivery_ids: [deliveryId],
          kind: 'message',
          text: `delivery ${deliveryId}`,
          deliveries: [
            {
              delivery_id: deliveryId,
              seq: deliveryId,
              presentation: testDeliveryPresentation(`delivery ${deliveryId}`),
            },
          ],
          spawn_options: options,
        },
        { owner: `owner-${generation}` },
      );

    try {
      expect(await deliver('Conversation0001', 1, 11)).toMatchObject({
        disposition: 'spawning',
      });
      await waitForDeliveryConsumed(events, 11);
      await expect(
        supervisor.clearSession({
          runtime_scope: { kind: 'agent', agent_id: 'a1' },
          runtime_generation: 1,
        }),
      ).resolves.toEqual({ cleared: true });

      expect(await deliver('Conversation0002', 2, 22)).toMatchObject({
        disposition: 'spawning',
      });
      await waitForDeliveryConsumed(events, 22);

      expect(existsSync(join(agentHome, 'conversations'))).toBe(false);
      expect(existsSync(join(agentHome, 'workspaces'))).toBe(false);
      expect(spawnContexts).toHaveLength(2);
      expect(spawnContexts.map((ctx) => ctx.workspacePath)).toEqual([agentHome, agentHome]);
      expect(promptTexts).toHaveLength(2);
      for (const prompt of promptTexts) {
        expect(prompt).toContain('AGENT_SLOT_ROLE');
        expect(prompt).toContain('AGENT_SLOT_MEMORY');
        expect(prompt).not.toMatch(/runtime_scope|workdir|\bcwd\b/u);
        expect(prompt).toContain('## Memory and files');
        expect(prompt).not.toMatch(/\b(?:AgentSlot|agent-global|L1|L2)\b/u);
        expect(prompt).not.toContain('Workspace Memory Index');
      }
      expect(childEnvs).toHaveLength(2);
      for (const env of childEnvs) {
        expect(env['SILVERHAND_AGENT_ID']).toBe('a1');
        expect(env).not.toHaveProperty('SILVERHAND_CONVERSATION_ID');
      }
    } finally {
      await supervisor
        .stop({
          runtime_scope: { kind: 'agent', agent_id: 'a1' },
          runtime_generation: 2,
          mode: 'hard',
        })
        .catch(() => {});
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('writes a runtime pidfile after spawn and unlinks it when the child exits', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'silverhand-supervisor-pidfile-'));
    const runtimePidRootDir = join(dir, 'runtime-pids');
    const daemonInstanceId = '1000-42';
    let supervisor: Supervisor | null = null;
    try {
      ({ supervisor } = makeSupervisor({
        runtimePidRootDir,
        daemonInstanceId,
      }));
      const res = await supervisor.start(baseStartParams);
      assertStarted(res);

      const filePath = runtimePidFilePath(runtimePidRootDir, daemonInstanceId, {
        kind: 'conversation',
        conversation_id: 'Conversation0001',
        agent_id: 'a1',
      });
      expect(existsSync(filePath)).toBe(true);
      const record = JSON.parse(readFileSync(filePath, 'utf8'));
      expect(record).toMatchObject({
        pid: res.pid,
        started_at: res.started_at,
        runtime_scope: {
          kind: 'conversation',
          conversation_id: 'Conversation0001',
          agent_id: 'a1',
        },
        agent: 'a1',
        runtime: 'claude',
        cmd: FIXTURE,
      });

      await supervisor.stop({
        runtime_scope: {
          kind: 'conversation',
          conversation_id: 'Conversation0001',
          agent_id: 'a1',
        },
        runtime_generation: 1,
        mode: 'hard',
      });
      expect(existsSync(filePath)).toBe(false);
    } finally {
      await supervisor
        ?.stop({
          runtime_scope: {
            kind: 'conversation',
            conversation_id: 'Conversation0001',
            agent_id: 'a1',
          },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => {});
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('start is idempotent — second start while running returns the same pid + started_at', async () => {
    const { supervisor } = makeSupervisor();
    const first = await supervisor.start(baseStartParams);
    const second = await supervisor.start(baseStartParams);
    assertStarted(first);
    assertStarted(second);
    expect(second.pid).toBe(first.pid);
    expect(second.started_at).toBe(first.started_at);
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('rejects a runtime_start custody generation that differs from the live handle', async () => {
    const { supervisor } = makeSupervisor();
    await supervisor.start({ ...baseStartParams, runtime_generation: 4 });
    await expect(supervisor.start({ ...baseStartParams, runtime_generation: 5 })).rejects.toThrow(
      /runtime generation mismatch/,
    );
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 4,
      mode: 'hard',
    });
  });

  it('start throws on malformed direct-call options', async () => {
    const { supervisor } = makeSupervisor();
    await expect(
      supervisor.start({
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        agent_id: 'a1',
        options: {} as never,
      }),
    ).rejects.toThrow();
  });

  it('start refuses a missing binary with a typed code', async () => {
    const supervisor = new Supervisor({
      emit: makeEmitSpy().emit,
      logger: silentLogger(),
      driverFor: () => makeFakeDriver(),
      resolveRuntime: async () => null,
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
      }),
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
    });
    await expect(supervisor.start(baseStartParams)).rejects.toMatchObject({
      code: 'runtime_binary_missing',
      message: 'Claude Code is not installed on this computer.',
    });
  });

  // The gate is the supervisor's, driven only by each driver's declared floor.
  describe.each([
    ['pi', new PiDriver().minimumVersion, '0.99.0'],
    ['grok', new GrokDriver().minimumVersion, '1.0.23'],
  ] as const)('version gate for %s (floor %s)', (_kind, minimum, below) => {
    const startWith = async (version: string | null) => {
      let spawned = false;
      const supervisor = new Supervisor({
        emit: makeEmitSpy().emit,
        logger: silentLogger(),
        driverFor: () => ({
          ...makeFakeDriver(),
          minimumVersion: minimum,
          buildArgs: () => {
            spawned = true;
            throw new Error('spawn reached');
          },
        }),
        resolveRuntime: async (_kind, fallback) => ({ binary: fallback, version }),
        runtimeStartResolveDeps: testRuntimeStartResolveDeps({
          agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
        }),
        controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
      });
      const error = await supervisor.start(baseStartParams).catch((err: unknown) => err);
      return { error, spawned };
    };

    it('refuses a parsed version below the floor', async () => {
      const { error, spawned } = await startWith(`${below} (build 7)`);
      expect(spawned).toBe(false);
      expect(error).toMatchObject({
        code: 'runtime_version_unsupported',
        message: `Claude Code ${below} is not supported. Update to ${minimum} or newer.`,
      });
    });

    it.each([null, 'garbage', ''])(
      'lets an unknown version (%j) through to spawn',
      async (version) => {
        expect((await startWith(version)).spawned).toBe(true);
      },
    );

    it('lets the floor itself through', async () => {
      expect((await startWith(minimum)).spawned).toBe(true);
    });
  });
});

describe('Supervisor 3a — output pump', () => {
  it('does not emit raw runtime_output by default', async () => {
    const child = new ControlledRuntime();
    const { emit, events } = makeEmitSpy();
    const supervisor = new Supervisor({
      emit,
      logger: silentLogger(),
      driverFor: () => ({
        ...makeFakeDriver(),
        parseLine: (text) => [{ kind: 'text', text }],
      }),
      resolveRuntime: async (_kind, binary) => ({ binary, version: null }),
      spawnFn: () => child as unknown as ChildProcess,
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
      }),
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
    });
    try {
      await supervisor.start(baseStartParams);
      child.stdout.emit('data', 'structured-output\n');
      child.stderr.emit('data', 'local-diagnostic\n');
      expect(events.filter((e) => e.event === 'runtime_output')).toEqual([]);
      expect(events).toContainEqual(
        expect.objectContaining({
          event: 'runtime_parsed_event',
          parsed: { kind: 'text', text: 'structured-output' },
        }),
      );
    } finally {
      child.dispose();
    }
  });

  it('pauseOutput silently drops chunks; resumeOutput restores pumping', async () => {
    const child = new ControlledRuntime();
    const { supervisor, events } = make3cSupervisor({
      spawnFn: () => child as unknown as ChildProcess,
      driverOverride: (base) => ({
        ...base,
        parseLine: (text) => [{ kind: 'text', text }],
      }),
    });
    const params = {
      runtime_scope: {
        kind: 'conversation',
        conversation_id: 'Conversation0001',
        agent_id: 'a1',
      },
    } as const;
    try {
      await supervisor.start(baseStartParams);
      child.stdout.emit('data', 'before-pause\n');
      child.stderr.emit('data', 'before-error\n');
      expect(supervisor.pauseOutput(params)).toEqual({ paused: true });
      expect(supervisor.pauseOutput(params)).toEqual({ paused: true });
      child.stdout.emit('data', 'during-pause\n');
      child.stderr.emit('data', 'during-error\n');
      expect(events).toContainEqual(
        expect.objectContaining({
          event: 'runtime_parsed_event',
          parsed: { kind: 'text', text: 'during-pause' },
        }),
      );
      expect(supervisor.resumeOutput(params)).toEqual({ resumed: true });
      expect(supervisor.resumeOutput(params)).toEqual({ resumed: true });
      child.stdout.emit('data', 'after-resume\n');
      child.stderr.emit('data', 'after-error\n');
      expect(
        events.filter((e) => e.event === 'runtime_output').map((e) => [e.stream, e.chunk]),
      ).toEqual([
        ['stdout', 'before-pause\n'],
        ['stderr', 'before-error\n'],
        ['stdout', 'after-resume\n'],
        ['stderr', 'after-error\n'],
      ]);
      expect(events.filter((e) => e.event === 'runtime_output_paused')).toHaveLength(1);
      expect(events.filter((e) => e.event === 'runtime_output_resumed')).toHaveLength(1);
    } finally {
      child.dispose();
    }
  });

  it('pauseOutput / resumeOutput on unknown (conv, agent) return paused:false / resumed:false without emitting acks', async () => {
    const { supervisor, events } = makeSupervisor();
    const p = supervisor.pauseOutput({
      runtime_scope: {
        kind: 'conversation',
        conversation_id: 'ConversationX001',
        agent_id: 'aX',
      },
    });
    const r = supervisor.resumeOutput({
      runtime_scope: {
        kind: 'conversation',
        conversation_id: 'ConversationX001',
        agent_id: 'aX',
      },
    });
    expect(p.paused).toBe(false);
    expect(r.resumed).toBe(false);
    expect(
      events.filter(
        (e) => e.event === 'runtime_output_paused' || e.event === 'runtime_output_resumed',
      ),
    ).toEqual([]);
  });
});

describe('Supervisor 3a — hard stop / exit', () => {
  it('hard stop sends SIGTERM, child exits, runtime_state(failed, stopped) + runtime_exit(intentional=true) land', async () => {
    const { supervisor, events } = makeSupervisor();
    await supervisor.start(baseStartParams);
    await waitFor(() => events.some((e) => e.event === 'runtime_ready_for_drain'));

    const stopRes = await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
    expect(stopRes).toMatchObject({ mode: 'hard', stopped: true });
    // exit_code may be null on Linux where SIGTERM-killed children
    // report code=null + signal='SIGTERM'; macOS often reports 143.
    // Both are valid wire shapes.
    if (stopRes.mode === 'hard') {
      expect(stopRes.exit_code === null || stopRes.exit_code === 143).toBe(true);
    }

    const exitEvt = events.find((e) => e.event === 'runtime_exit');
    expect(exitEvt).toBeDefined();
    if (exitEvt && exitEvt.event === 'runtime_exit') {
      expect(exitEvt.intentional).toBe(true);
    }

    // Final state event carries the `stopped` terminal reason; per
    // Cindy `GMbjEkGUHrB1BBMe` `stopped` maps to status:'failed' (not
    // 'idle') — the agent did NOT complete cleanly, it was forcibly
    // terminated. Only `replied|turn_ended|replaced` map to idle.
    const lastState = [...events].reverse().find((e) => e.event === 'runtime_state');
    expect(lastState).toBeDefined();
    if (lastState && lastState.event === 'runtime_state') {
      expect(lastState.state.terminalReason).toBe('stopped');
      expect(lastState.state.status).toBe('failed');
    }

    // status() now shows the handle as exited with pid=null.
    const snap = supervisor.status({});
    expect(snap.runtimes[0]?.phase).toBe('exited');
    expect(snap.runtimes[0]?.pid).toBeNull();
  });

  it('hard stop on a never-started runtime returns stopped:false + exit_code:null', async () => {
    const { supervisor } = makeSupervisor();
    const res = await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'ConversationX001', agent_id: 'aX' },
      runtime_generation: 1,
      mode: 'hard',
    });
    expect(res).toEqual({ mode: 'hard', stopped: false, exit_code: null });
  });

  it('hard stop escalates to SIGKILL when the child ignores SIGTERM', async () => {
    const { supervisor, events } = makeSupervisor({
      driverArgs: ['--ignore-sigterm'],
      sigtermToSigkillMs: 100,
      sigkillTimeoutMs: 1_000,
    });
    await supervisor.start(baseStartParams);
    // Wait for the fixture's READY line so we know its SIGTERM
    // handler is installed before we send the signal — otherwise
    // SIGTERM lands during node's startup window and kills the
    // process before the trap takes effect.
    await waitFor(() =>
      events.some((e) => e.event === 'runtime_output' && e.chunk.includes('READY')),
    );

    const t0 = Date.now();
    const stopRes = await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
    const elapsed = Date.now() - t0;

    // Cascade must take at least the SIGTERM grace before SIGKILL
    // lands — proves we actually wait, not bail early.
    expect(elapsed).toBeGreaterThanOrEqual(80);
    expect(stopRes.mode).toBe('hard');
    if (stopRes.mode === 'hard') {
      // SIGKILL-terminated processes report code=null + signal='SIGKILL'
      // on every platform we care about.
      expect(stopRes.stopped).toBe(true);
      expect(stopRes.exit_code).toBeNull();
    }
  });

  it('hard stop safety-timeout resolves with stopped:false + exit_code:null when the child never reports exit', async () => {
    // Cindy `OQCA05pivtB8nSTC` B2: if the SIGKILL window expires and
    // the child still hasn't fired `exit`, we must NOT lie that it
    // stopped. Caller would otherwise treat its resources as freed
    // while the OS handle persists. Honest answer: `stopped:false,
    // exit_code:null`; leave the handle in `draining`; let the real
    // `attachChildListeners.once('exit')` fire later if/when the
    // child actually goes away.
    //
    // We exercise this with a stub ChildProcess that swallows kill()
    // and never emits 'exit' — no real OS process is involved.
    class StubChild extends EventEmitter {
      readonly pid = 999_999;
      readonly stdout = new EventEmitter();
      readonly stderr = new EventEmitter();
      readonly stdin = { write: () => true, end: () => {} };
      kill(_sig?: NodeJS.Signals | number): boolean {
        // Pretend we delivered the signal but the child ignores it.
        return true;
      }
    }
    const stub = new StubChild();
    const spawnFn = (_b: string, _a: ReadonlyArray<string>, _o: SpawnOptions): ChildProcess =>
      stub as unknown as ChildProcess;

    const { supervisor, events } = makeSupervisor({
      sigtermToSigkillMs: 30,
      sigkillTimeoutMs: 80,
      spawnFn,
    });
    await supervisor.start(baseStartParams);

    const t0 = Date.now();
    const stopRes = await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
    const elapsed = Date.now() - t0;

    expect(stopRes).toEqual({ mode: 'hard', stopped: false, exit_code: null });
    // Must have waited the full SIGKILL window — otherwise we bailed
    // before giving the child a chance to exit.
    expect(elapsed).toBeGreaterThanOrEqual(60);

    // Handle must stay in `draining` (not flipped to `exited`); no
    // `runtime_exit` event should have been emitted yet.
    const snap = supervisor.status({});
    expect(snap.runtimes[0]?.phase).toBe('draining');
    expect(snap.runtimes[0]?.pid).toBe(999_999);
    expect(events.some((e) => e.event === 'runtime_exit')).toBe(false);

    // Cleanup: pretend the child finally exits so the supervisor's
    // primary listener fires and we leave no dangling handles.
    stub.emit('exit', null, 'SIGKILL');
    await waitFor(() => events.some((e) => e.event === 'runtime_exit'));
  });

  it('oneShot clean exit code 0 (no stop request) marks intentional=false; reason maps to replied → status:idle (Cindy `5nruk1IPaEHSab0T` B4)', async () => {
    // Cindy `5nruk1IPaEHSab0T` B4: a clean `exit(0)` is NOT a crash.
    // CM's terminal mapping uses code===0 → `replied/idle`; the
    // distinction matters for oneShot drivers (cursor) where a
    // single-turn process exits cleanly after producing its reply.
    // Mapping it to `crashed/failed` would let the UI / RunStateCache
    // mark every successful cursor run as failed.
    //
    // #task:1460 sharpened this: the `code === 0 → replied` arm is for a
    // runtime that completed its work — a oneShot single-turn run, or a
    // long-lived runtime that reached `session_init`. A LONG-LIVED
    // runtime that exits (even code 0) BEFORE `session_init` never
    // replied and is now an init failure (`crashed`), so this contract is
    // pinned with a oneShot driver — the class whose normal completion is
    // a pre-init-looking clean exit. (See the #task:1460 describe block
    // for the long-lived pre-init → crashed counterpart.)
    const { supervisor, events } = make3cSupervisor({
      // Sleep briefly then voluntarily exit(0). The supervisor never
      // gets a stop() call — this exercises the clean-exit path.
      driverArgs: ['--sleep', '50'],
      driverOverride: (base) => ({ ...base, oneShot: true }),
    });
    await supervisor.start(baseStartParams);
    await waitFor(() => events.some((e) => e.event === 'runtime_exit'), 3_000);
    const exitEvt = events.find((e) => e.event === 'runtime_exit');
    expect(exitEvt && exitEvt.event === 'runtime_exit' && exitEvt.intentional).toBe(false);
    if (exitEvt && exitEvt.event === 'runtime_exit') {
      expect(exitEvt.code).toBe(0);
    }
    const lastState = [...events].reverse().find((e) => e.event === 'runtime_state');
    expect(lastState && lastState.event === 'runtime_state' && lastState.state.terminalReason).toBe(
      'replied',
    );
    expect(lastState && lastState.event === 'runtime_state' && lastState.state.status).toBe('idle');
  });

  it('voluntary child exit with NON-ZERO code marks intentional=false; reason maps to crashed → status:failed (Cindy `5nruk1IPaEHSab0T` B4)', async () => {
    // Counterpart to the clean-exit test: a non-zero `exit(N)` IS a
    // crash. We use a stub child so we can control the exit code
    // precisely (the echo-runtime.cjs fixture always exits 0).
    class CodeExitStub extends EventEmitter {
      readonly pid = 34_567;
      readonly stdout = new EventEmitter();
      readonly stderr = new EventEmitter();
      readonly stdin = {
        write: (_l: string, cb?: (err?: Error | null) => void): boolean => {
          setImmediate(() => cb?.());
          return true;
        },
        end: () => {},
        destroyed: false,
        once: () => {},
      };
      kill(_sig?: NodeJS.Signals | number): boolean {
        return true;
      }
    }
    const stub = new CodeExitStub();
    const spawnFn = (_b: string, _a: ReadonlyArray<string>, _o: SpawnOptions): ChildProcess =>
      stub as unknown as ChildProcess;

    const { supervisor, events } = makeSupervisor({ spawnFn });
    await supervisor.start(baseStartParams);
    // Simulate a non-zero exit (e.g. binary crash on uncaught throw).
    stub.emit('exit', 137, null);
    await waitFor(() => events.some((e) => e.event === 'runtime_exit'));

    const exitEvt = events.find((e) => e.event === 'runtime_exit');
    expect(exitEvt && exitEvt.event === 'runtime_exit' && exitEvt.intentional).toBe(false);
    if (exitEvt && exitEvt.event === 'runtime_exit') {
      expect(exitEvt.code).toBe(137);
    }
    const lastState = [...events].reverse().find((e) => e.event === 'runtime_state');
    expect(lastState && lastState.event === 'runtime_state' && lastState.state.terminalReason).toBe(
      'crashed',
    );
    expect(lastState && lastState.event === 'runtime_state' && lastState.state.status).toBe(
      'failed',
    );
  });

  it('stdoutLineBuffer trailing partial is flushed at child exit (Cindy `5nruk1IPaEHSab0T` B5)', async () => {
    // Node's readline emits a final `line` for input that ends
    // without a trailing `\n`. The supervisor's chunk-buffered pump
    // must mirror that or the last JSON frame on quick exit (often
    // session_init / turn_end / error) silently vanishes from
    // parsed-event downstream. We stub a child that writes
    // `final-line` (no newline) to stdout, then exits — the parsed
    // event for that line must land in the event stream BEFORE the
    // runtime_exit frame.
    class TailFlushStub extends EventEmitter {
      readonly pid = 45_678;
      readonly stdout = new EventEmitter();
      readonly stderr = new EventEmitter();
      readonly stdin = {
        write: (_l: string, cb?: (err?: Error | null) => void): boolean => {
          setImmediate(() => cb?.());
          return true;
        },
        end: () => {},
        destroyed: false,
        once: () => {},
      };
      kill(_sig?: NodeJS.Signals | number): boolean {
        return true;
      }
    }
    const stub = new TailFlushStub();
    const spawnFn = (_b: string, _a: ReadonlyArray<string>, _o: SpawnOptions): ChildProcess =>
      stub as unknown as ChildProcess;

    const { driver } = makeProgrammableDriver();
    // Pre-arm parseLine to return a session_init for whatever line
    // the supervisor hands it.
    driver.parseLine = (line: string) =>
      line === 'final-line' ? [{ kind: 'session_init', sessionId: 'sess-tail' }] : [];

    const spy = makeEmitSpy();
    const supervisor = new Supervisor({
      emit: spy.emit,
      logger: silentLogger(),
      driverFor: () => driver,
      resolveRuntime: async (_kind, fallback) => ({ binary: fallback, version: null }),
      spawnFn,
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
      }),
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
      sigtermToSigkillMs: 50,
      sigkillTimeoutMs: 200,
    });
    await supervisor.start(baseStartParams);
    // Write an unterminated chunk to stdout (no `\n`). The supervisor
    // must keep it in `stdoutLineBuffer` and flush on exit.
    stub.stdout.emit('data', 'final-line');
    // Now exit cleanly.
    stub.emit('exit', 0, null);
    await waitFor(() => spy.events.some((e) => e.event === 'runtime_exit'));

    const idxParsed = spy.events.findIndex(
      (e) =>
        e.event === 'runtime_parsed_event' &&
        e.parsed.kind === 'session_init' &&
        e.parsed.sessionId === 'sess-tail',
    );
    const idxExit = spy.events.findIndex((e) => e.event === 'runtime_exit');
    expect(idxParsed).toBeGreaterThanOrEqual(0);
    expect(idxExit).toBeGreaterThanOrEqual(0);
    // Flush happens before phase='exited' is set + before the
    // runtime_exit frame is emitted, so the parsed event lands
    // first.
    expect(idxParsed).toBeLessThan(idxExit);
  });
});

describe('Supervisor 3a — stopAll / closeActivityLog', () => {
  it('shutdown drains once, joins concurrent callers and rejects later starts', async () => {
    const { supervisor, events } = makeSupervisor();
    await supervisor.start(baseStartParams);
    await waitFor(() => events.some((e) => e.event === 'runtime_ready_for_drain'));
    const first = supervisor.shutdown();
    expect(supervisor.shutdown()).toBe(first);
    await first;
    expect(supervisor.status({}).runtimes.every((r) => r.phase === 'exited')).toBe(true);
    await expect(supervisor.start(baseStartParams)).rejects.toThrow('shutting down');
  });

  it('shutdown fences a start already awaiting preparation before its handle exists', async () => {
    const spawnFn = vi.fn((bin: string, args: ReadonlyArray<string>, options: SpawnOptions) =>
      spawn(execPath, [bin, ...args.slice(1)], options),
    );
    const { supervisor } = makeSupervisor({
      spawnFn,
      onSpawnCtx: () => {
        void supervisor.shutdown();
      },
    });
    await expect(supervisor.start(baseStartParams)).rejects.toThrow('shutting down');
    await supervisor.shutdown();
    expect(spawnFn).not.toHaveBeenCalled();
  });

  it('stopAll hard-terminates every live handle and returns the count', async () => {
    const { supervisor, events } = makeSupervisor();
    await supervisor.start({ ...baseStartParams, agent_id: 'a1' });
    await supervisor.start({
      ...baseStartParams,
      conversation_id: 'Conversation0002',
      agent_id: 'a2',
      options: testStartOptions({
        conversationId: 'Conversation0002',
        agentId: 'a2',
      }),
    });
    await waitFor(() => events.filter((e) => e.event === 'runtime_ready_for_drain').length >= 2);

    const res = await supervisor.stopAll({});
    expect(res.stopped_count).toBe(2);

    const snap = supervisor.status({});
    expect(snap.runtimes.every((r) => r.phase === 'exited')).toBe(true);
  });

  it('closeActivityLog returns flushed:true (host has no activity log to flush in 3a)', () => {
    const { supervisor } = makeSupervisor();
    const res = supervisor.closeActivityLog({});
    expect(res).toEqual({ flushed: true });
  });
});

describe('Supervisor 3b — deliver WRITE arm writes payload to stdin', () => {
  it('AgentSlot reuses one child across conversation targets and acks each origin independently', async () => {
    let spawnCount = 0;
    let lastDeliveredSeqPath: string | undefined;
    const { supervisor, events } = makeSupervisor({
      driverArgs: ['--echo-stdin'],
      spawnFn: (bin, args, options) => {
        spawnCount += 1;
        lastDeliveredSeqPath = options.env?.['SILVERHAND_LAST_DELIVERED_SEQ_FILE'];
        return spawn(execPath, [bin, ...args.slice(1)], options);
      },
    });
    const agentOptions = testStartOptions({
      agentId: 'a1',
      sessionScope: 'agent',
    });
    const deliver = (conversationId: string, deliveryId: number, seq: number, body: string) => {
      const presentation = testDeliveryPresentation(body);
      return supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: conversationId,
          target: { conversation_id: conversationId, workspace: null },
          agent_id: 'a1',
          delivery_ids: [deliveryId],
          deliveries: [
            {
              delivery_id: deliveryId,
              seq,
              presentation,
            },
          ],
          kind: 'message',
          text: composeRuntimeDeliveryPresentation([presentation], 'silverhand message send'),
          spawn_options: agentOptions,
        },
        { owner: 'agent-slot-owner' },
      );
    };

    try {
      expect(await deliver('Conversation0001', 101, 100, 'first target')).toMatchObject({
        disposition: 'spawning',
      });
      await waitForDeliveryConsumed(events, 101);
      expect(await deliver('Conversation0002', 202, 1, 'second target')).toMatchObject({
        disposition: 'admitted',
      });
      await waitForDeliveryConsumed(events, 202);
      await waitFor(() =>
        events.some(
          (event) => event.event === 'runtime_output' && event.chunk.includes('second target'),
        ),
      );

      expect(
        supervisor.pauseOutput({
          runtime_scope: { kind: 'agent', agent_id: 'a1' },
        }),
      ).toEqual({ paused: true });
      expect(
        supervisor.resumeOutput({
          runtime_scope: { kind: 'agent', agent_id: 'a1' },
        }),
      ).toEqual({ resumed: true });
      expect(
        events
          .filter(
            (
              event,
            ): event is Extract<
              EventFrame,
              { event: 'runtime_output_paused' | 'runtime_output_resumed' }
            > =>
              event.event === 'runtime_output_paused' || event.event === 'runtime_output_resumed',
          )
          .map((event) => ({
            event: event.event,
            scope: event.runtime_scope,
            focusedConversationId: event.conversation_id,
          })),
      ).toEqual([
        {
          event: 'runtime_output_paused',
          scope: { kind: 'agent', agent_id: 'a1' },
          focusedConversationId: 'Conversation0002',
        },
        {
          event: 'runtime_output_resumed',
          scope: { kind: 'agent', agent_id: 'a1' },
          focusedConversationId: 'Conversation0002',
        },
      ]);

      expect(spawnCount).toBe(1);
      const rows = supervisor.status({ agent_id: 'a1' }).runtimes;
      expect(rows).toHaveLength(1);
      expect(rows[0]).toMatchObject({
        runtime_scope: { kind: 'agent', agent_id: 'a1' },
        conversation_id: 'Conversation0002',
        runtime_generation: 1,
      });
      const consumed = events.filter(
        (event): event is Extract<EventFrame, { event: 'runtime_delivery_consumed' }> =>
          event.event === 'runtime_delivery_consumed',
      );
      expect(
        consumed.map((event) => ({
          conversationId: event.conversation_id,
          deliveryIds: event.delivery_ids,
          scope: event.runtime_scope,
        })),
      ).toEqual([
        {
          conversationId: 'Conversation0001',
          deliveryIds: [101],
          scope: { kind: 'agent', agent_id: 'a1' },
        },
        {
          conversationId: 'Conversation0002',
          deliveryIds: [202],
          scope: { kind: 'agent', agent_id: 'a1' },
        },
      ]);
      const output = events
        .filter(
          (event): event is Extract<EventFrame, { event: 'runtime_output' }> =>
            event.event === 'runtime_output',
        )
        .map((event) => event.chunk)
        .join('');
      expect(output).toContain('first target');
      expect(output).toContain('second target');
      expect(output).not.toContain('target=');
      expect(output).not.toMatch(/runtime_scope|workdir=/u);
      expect(output).not.toContain(SUPERVISOR_AGENTS_ROOT);
      // #task:2095: the sidecar is a bare `<seq>\n` scalar (the per-stream
      // `{version:1,streams}` ledger is retired). On an AgentSlot it is the
      // LAST value written, NOT a per-origin cursor — the two delivers above
      // hop seq spaces (Conversation0001 seq 100 → Conversation0002 seq 1), so
      // the file ends on the last write's seq. Per-origin independent ack is
      // proven by the `runtime_delivery_consumed` stream asserted above, which
      // is the real ack surface — not this scalar.
      expect(lastDeliveredSeqPath).toBeDefined();
      expect(readFileSync(lastDeliveredSeqPath!, 'utf8')).toBe('1\n');
    } finally {
      await supervisor
        .stop({
          runtime_scope: { kind: 'agent', agent_id: 'a1' },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => {});
    }
  });

  it('deliver on a live runtime writes the encoded payload to stdin and returns admitted, settling onto runtime_delivery_consumed', async () => {
    // 3b lifts the 3a stub: the WRITE arm now writes the opaque
    // payload to the child's stdin (+ '\n') and returns admitted on
    // write success. We verify via the fixture's --echo-stdin mode,
    // which surfaces each received line back on stdout prefixed
    // with "STDIN:".
    const { supervisor, events } = makeSupervisor({
      driverArgs: ['--echo-stdin'],
    });
    await supervisor.start(baseStartParams);
    await waitFor(() =>
      events.some((e) => e.event === 'runtime_output' && e.chunk.includes('READY')),
    );
    const res = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [42, 43],
        kind: 'message',
        text: 'hi',
        deliveries: [
          { delivery_id: 42, seq: 42, presentation: testDeliveryPresentation('hi') },
          { delivery_id: 43, seq: 43, presentation: testDeliveryPresentation('hi') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(res.disposition).toBe('admitted');
    if (res.disposition !== 'admitted') throw new Error('expected live admission');
    expect(res.pid).toBeGreaterThan(0);
    // Echo from fixture confirms the bytes reached the child.
    await waitFor(() =>
      events.some((e) => e.event === 'runtime_output' && e.chunk.includes('STDIN:{"type":"user"')),
    );
    // Custody ack settles on the unified consumed event.
    await waitFor(() =>
      events.some(
        (e) =>
          e.event === 'runtime_delivery_consumed' &&
          e.delivery_ids.length === 2 &&
          e.delivery_ids[0] === 42 &&
          e.delivery_ids[1] === 43,
      ),
    );
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });
});

describe('Supervisor 3b — clearSession behavior', () => {
  it('late generation-N stop and clear cannot change a live generation N+1 handle', async () => {
    const { supervisor } = makeSupervisor();
    await supervisor.start(baseStartParams);
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
    await supervisor.start({ ...baseStartParams, runtime_generation: 2 });

    await expect(
      supervisor.stop({
        runtime_scope: {
          kind: 'conversation',
          conversation_id: 'Conversation0001',
          agent_id: 'a1',
        },
        runtime_generation: 1,
        mode: 'hard',
      }),
    ).resolves.toEqual({ mode: 'hard', stopped: false, exit_code: null });
    await expect(
      supervisor.clearSession({
        runtime_scope: {
          kind: 'conversation',
          conversation_id: 'Conversation0001',
          agent_id: 'a1',
        },
        runtime_generation: 1,
      }),
    ).resolves.toEqual({ cleared: false });
    expect(supervisor.status({}).runtimes).toMatchObject([{ runtime_generation: 2 }]);

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 2,
      mode: 'hard',
    });
  });

  it('clearSession on unknown returns cleared:false; on live hard-stops the runtime and returns cleared:true (resume-state clear remains a 3b/backend concern)', async () => {
    const { supervisor, events } = makeSupervisor();
    const miss = await supervisor.clearSession({
      runtime_scope: { kind: 'conversation', conversation_id: 'ConversationX001', agent_id: 'aX' },
      runtime_generation: 1,
    });
    expect(miss).toEqual({ cleared: false });

    await supervisor.start(baseStartParams);
    const hit = await supervisor.clearSession({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
    });
    expect(hit).toEqual({ cleared: true });
    expect(events.some((e) => e.event === 'runtime_exit')).toBe(true);
  });
});

// ── 3b parse fanout / run-state / startup_delivery tests ───────────

/** Build a fake driver whose `parseLine` returns whatever the test
 *  pre-loads via `setNextParse`. Lets us drive the supervisor's
 *  parse fanout without depending on a real driver's JSON shape.
 *
 *  `kickoffMode` defaults to `'self_inits'` (real claude is
 *  `requires_first_message`; pick the mode that matches the surface
 *  under test — this default is chosen for round-trip test simplicity,
 *  not driver realism). */
function makeProgrammableDriver(
  extraArgs: string[] = [],
  kickoffMode: 'self_inits' | 'requires_first_message' = 'self_inits',
  admissionPolicy: 'lifecycle-start' | 'write-success' = 'write-success',
): {
  driver: Driver;
  setNextParse: (events: ParsedEvent[]) => void;
} {
  let next: ParsedEvent[] = [];
  const driver: Driver = {
    kind: 'claude',
    binary: FIXTURE,
    kickoffMode,
    releaseProfile: 'native-steer',
    admissionPolicy,
    oneShot: false,
    supportsNativeStandingPrompt: true,
    initLivenessSilenceMs: 30_000,
    initPreProtocolCapMs: 5 * 60_000,
    initPostProtocolMs: 60_000,
    buildArgs: () => [FIXTURE, '--echo-stdin', ...extraArgs],
    applyEnv: (env) => ({ ...env }),
    // Option A v2 (Cindy `UMGu36zoTDVuKWXi`): daemon calls
    // encodeStdinMessage directly. A null return surfaces as
    // `driver_refused`, which would break tests that exercise the
    // stdin write path (deferred startup_delivery consume, etc).
    // Mirror the claude driver's stream-json shape.
    encodeStdinMessage: (text: string) => encodeStreamJsonUser(text),
    encodeInterruptControl: () => null,
    parseLine: () => {
      const out = next;
      next = [];
      return out;
    },
    classifyInitProgress: () => null,
  };
  return { driver, setNextParse: (events) => (next = events) };
}

function makeSupervisorWithDriver(
  driver: Driver,
  opts: {
    sigtermToSigkillMs?: number;
    sigkillTimeoutMs?: number;
    emitRawRuntimeOutput?: boolean;
    logger?: DaemonLogger;
  } = {},
): {
  supervisor: Supervisor;
  events: EventFrame[];
} {
  const spy = makeEmitSpy();
  const realSpawn = spawn;
  const spawnFn = (bin: string, args: ReadonlyArray<string>, options: SpawnOptions): ChildProcess =>
    realSpawn(execPath, [bin, ...args.slice(1)], options);
  const supervisor = new Supervisor({
    emit: spy.emit,
    logger: opts.logger ?? silentLogger(),
    driverFor: () => driver,
    resolveRuntime: async (_kind, fallback) => ({ binary: fallback, version: null }),
    spawnFn,
    runtimeStartResolveDeps: testRuntimeStartResolveDeps({
      agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
    }),
    controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
    emitRawRuntimeOutput: opts.emitRawRuntimeOutput ?? true,
    sigtermToSigkillMs: opts.sigtermToSigkillMs ?? 50,
    sigkillTimeoutMs: opts.sigkillTimeoutMs ?? 200,
  });
  return { supervisor, events: spy.events };
}

// ParsedEvent / Driver are imported at the top of the file (via
// `../src/drivers/index.js`, which re-exports `ParsedEvent` from
// `@silverhand/shared`).

describe('Supervisor 3b — parse fanout (Cindy `OQCA05pivtB8nSTC` + `C2BZGDbQWfnq-bKJ`)', () => {
  it('every stdout line is passed to driver.parseLine and each returned ParsedEvent is fanned out as runtime_parsed_event', async () => {
    const { driver, setNextParse } = makeProgrammableDriver(['--emit-line', 'PROBE']);
    const { supervisor, events } = makeSupervisorWithDriver(driver);
    // Pre-load parse results so when the fixture's READY / PROBE
    // lines arrive, parseLine returns concrete events.
    setNextParse([
      { kind: 'session_init', sessionId: 'sess-abc' },
      { kind: 'text', text: 'hi from driver' },
    ]);
    await supervisor.start(baseStartParams);

    await waitFor(() =>
      events.some(
        (e) =>
          e.event === 'runtime_parsed_event' &&
          e.parsed.kind === 'session_init' &&
          e.parsed.sessionId === 'sess-abc',
      ),
    );
    expect(
      events.some(
        (e) =>
          e.event === 'runtime_parsed_event' &&
          e.parsed.kind === 'text' &&
          e.parsed.text === 'hi from driver',
      ),
    ).toBe(true);
    // Raw `runtime_output` MUST also carry the same bytes (Cindy:
    // parse fanout is a sidecar, not a replacement for raw output).
    expect(events.some((e) => e.event === 'runtime_output' && e.chunk.includes('READY'))).toBe(
      true,
    );
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('correlates runtime lifecycle frames to the minted payload uuid end-to-end (#task:1557)', async () => {
    const captured: Array<string | undefined> = [];
    const { driver, setNextParse } = makeProgrammableDriver();
    const baseEncode = driver.encodeStdinMessage.bind(driver);
    driver.encodeStdinMessage = (text, sess, opts) => {
      captured.push(opts?.payloadUuid);
      return baseEncode(text, sess, opts);
    };
    const logs: Array<Record<string, unknown>> = [];
    const logger: DaemonLogger = {
      info: (fields: unknown) => {
        if (fields && typeof fields === 'object') logs.push(fields as Record<string, unknown>);
      },
      warn: (fields: unknown) => {
        if (fields && typeof fields === 'object') logs.push(fields as Record<string, unknown>);
      },
      error: () => {},
    } as unknown as DaemonLogger;
    const { supervisor, events } = makeSupervisorWithDriver(driver, { logger });

    await supervisor.start(baseStartParams);
    await waitForRuntimeReady(events);

    // Delivery #1: the supervisor mints a uuid for the lifecycle-capable
    // driver and hands it to the encoder.
    await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [21],
        kind: 'message',
        text: 'first payload',
        deliveries: [
          { delivery_id: 21, seq: 21, presentation: testDeliveryPresentation('first payload') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    await waitForDeliveryConsumed(events, 21);
    const uuidA = captured[0];
    expect(uuidA).toBeTruthy();

    // Delivery #2's stdin echo triggers parseLine, which replays the
    // runtime's lifecycle for payload A (queued → started → completed).
    setNextParse([
      { kind: 'input_lifecycle', commandUuid: uuidA!, state: 'queued' },
      { kind: 'input_lifecycle', commandUuid: uuidA!, state: 'started' },
      { kind: 'input_lifecycle', commandUuid: uuidA!, state: 'completed' },
    ]);
    await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [22],
        kind: 'message',
        text: 'second payload',
        deliveries: [
          { delivery_id: 22, seq: 22, presentation: testDeliveryPresentation('second payload') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    await waitFor(() =>
      logs.some((l) => l['action'] === 'input_lifecycle' && l['state'] === 'completed'),
    );
    const lifecycleLogs = logs.filter((l) => l['action'] === 'input_lifecycle');
    expect(lifecycleLogs).toHaveLength(3);
    for (const entry of lifecycleLogs) {
      expect(entry['command_uuid']).toBe(uuidA);
      expect(entry['correlated']).toBe(true);
      expect(entry['delivery_ids']).toEqual([21]);
    }

    // completed pruned the mapping: replaying `started` for uuidA on
    // delivery #3's echo must log correlated:false.
    setNextParse([{ kind: 'input_lifecycle', commandUuid: uuidA!, state: 'started' }]);
    await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [23],
        kind: 'message',
        text: 'third payload',
        deliveries: [
          { delivery_id: 23, seq: 23, presentation: testDeliveryPresentation('third payload') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    await waitFor(() =>
      logs.some(
        (l) =>
          l['action'] === 'input_lifecycle' &&
          l['state'] === 'started' &&
          l['correlated'] === false,
      ),
    );
    const uncorrelated = logs.find(
      (l) => l['action'] === 'input_lifecycle' && l['correlated'] === false,
    );
    expect(uncorrelated?.['delivery_ids']).toBeNull();

    // Zero behavior change: lifecycle frames drove no run-state tool
    // transition of their own — the three lifecycle frames add exactly
    // nothing on top of the delivery that preceded them.
    expect(
      events.some((e) => e.event === 'runtime_state' && e.state?.status === 'tool_running'),
    ).toBe(false);
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('parsed events drive AgentRunState transitions (thinking → tool_running → thinking → idle)', async () => {
    const { driver } = makeProgrammableDriver([
      '--emit-line',
      'A',
      '--emit-line',
      'B',
      '--emit-line',
      'C',
      '--emit-line',
      'D',
      '--emit-line',
      'E',
    ]);
    const { supervisor, events } = makeSupervisorWithDriver(driver);
    // 6 stdout lines arrive: READY, A, B, C, D, E. The first call
    // (READY) consumes step 0; the remaining five drive the
    // lifecycle. step beyond the sequence returns [].
    let step = 0;
    const sequence: ParsedEvent[][] = [
      [], // READY (no-op)
      [{ kind: 'thinking', text: 'pondering' }], // A → thinking
      [{ kind: 'tool_call', name: 'Bash', input: {} }], // B → tool_running(Bash)
      [{ kind: 'tool_output', name: 'Bash', outcome: 'success' }], // C → thinking (outstanding=0)
      [{ kind: 'text', text: 'hello' }], // D → text snippet, still thinking
      [{ kind: 'turn_end' }], // E → idle (turn_ended)
    ];
    driver.parseLine = () => sequence[step++] ?? [];

    await supervisor.start(baseStartParams);

    await waitFor(
      () =>
        events.some(
          (e) =>
            e.event === 'runtime_state' &&
            e.state.status === 'idle' &&
            e.state.terminalReason === 'turn_ended',
        ),
      3_000,
    );

    const statusSeq = events
      .filter((e) => e.event === 'runtime_state')
      .map((e) => (e.event === 'runtime_state' ? e.state.status : ''));
    // Must contain the lifecycle in order; allow other status emits
    // (starting, idle, etc.) interleaved.
    const required = ['thinking', 'tool_running', 'thinking', 'idle'];
    let cursor = 0;
    for (const s of statusSeq) {
      if (s === required[cursor]) cursor++;
      if (cursor === required.length) break;
    }
    expect(cursor).toBe(required.length);

    const snippetState = events.find(
      (e) =>
        e.event === 'runtime_state' &&
        e.state.activitySnippet === 'pondering' &&
        e.state.activitySnippetKind === 'thinking',
    );
    expect(snippetState).toBeDefined();

    const textSnippetState = events.find(
      (e) =>
        e.event === 'runtime_state' &&
        e.state.activitySnippet === 'hello' &&
        e.state.activitySnippetKind === 'text',
    );
    expect(textSnippetState).toBeDefined();

    const toolState = events.find(
      (e) =>
        e.event === 'runtime_state' &&
        e.state.status === 'tool_running' &&
        e.state.detail === 'Bash',
    );
    expect(
      toolState && toolState.event === 'runtime_state' && toolState.state.toolCallCounts,
    ).toEqual({ Bash: 1 });

    const postToolThinking = events.find(
      (e) =>
        e.event === 'runtime_state' &&
        e.state.status === 'thinking' &&
        e.state.toolCallCounts?.Bash === 1,
    );
    expect(postToolThinking).toBeDefined();

    const lastIdle = [...events]
      .reverse()
      .find((e) => e.event === 'runtime_state' && e.state.status === 'idle');
    expect(lastIdle && lastIdle.event === 'runtime_state' && lastIdle.state.terminalReason).toBe(
      'turn_ended',
    );

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('a tool_call_input refinement is forwarded but touches NO run state: counts stay at one call and a single tool_output still ends the tool (#task:2101)', async () => {
    const { driver } = makeProgrammableDriver([
      '--emit-line',
      'A',
      '--emit-line',
      'B',
      '--emit-line',
      'C',
      '--emit-line',
      'D',
    ]);
    const { supervisor, events } = makeSupervisorWithDriver(driver);
    let step = 0;
    const sequence: ParsedEvent[][] = [
      [], // READY (no-op)
      [{ kind: 'tool_call', name: 'Bash', input: { cwd: '/w' } }],
      // The late params for the SAME call. If this were counted as a call,
      // `toolCallCounts.Bash` would reach 2 and `outstandingToolUses` 2 —
      // leaving the runtime stuck in tool_running after the single output.
      [{ kind: 'tool_call_input', toolCallId: 't1', input: { command: 'ls' } }],
      [{ kind: 'tool_output', name: 'Bash', outcome: 'success' }],
      [{ kind: 'turn_end' }],
    ];
    driver.parseLine = () => sequence[step++] ?? [];

    await supervisor.start(baseStartParams);

    await waitFor(
      () =>
        events.some(
          (e) =>
            e.event === 'runtime_state' &&
            e.state.status === 'idle' &&
            e.state.terminalReason === 'turn_ended',
        ),
      3_000,
    );

    // Forwarded to the backend like any other parsed event — the refinement is
    // real runtime output, it just is not run-state evidence.
    expect(
      events.some(
        (e) =>
          e.event === 'runtime_parsed_event' &&
          e.parsed.kind === 'tool_call_input' &&
          e.parsed.toolCallId === 't1',
      ),
    ).toBe(true);

    // Exactly one call was ever counted, at creation.
    for (const e of events) {
      if (e.event !== 'runtime_state') continue;
      if (!e.state.toolCallCounts) continue;
      expect(e.state.toolCallCounts).toEqual({ Bash: 1 });
    }

    // One `tool_output` was enough to close the tool: outstanding was never
    // double-incremented.
    const backToThinking = events.find(
      (e) =>
        e.event === 'runtime_state' &&
        e.state.status === 'thinking' &&
        e.state.toolCallCounts?.Bash === 1,
    );
    expect(backToThinking).toBeDefined();

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('reclaims a runtime that stays alive after a post-init fatal error', async () => {
    const { driver } = makeProgrammableDriver([
      '--emit-line',
      'INIT',
      '--emit-line',
      'FATAL',
      '--ignore-sigterm',
    ]);
    driver.parseLine = (line) =>
      line === 'INIT'
        ? [{ kind: 'session_init', sessionId: 's1' }]
        : line === 'FATAL'
          ? [{ kind: 'error', code: 'session_failed', message: 'session failed', fatal: true }]
          : [];
    const { supervisor, events } = makeSupervisorWithDriver(driver, { sigtermToSigkillMs: 300 });

    await supervisor.start(baseStartParams);
    await waitFor(() =>
      events.some((e) => e.event === 'runtime_parsed_event' && e.parsed.kind === 'error'),
    );
    // The grace window refuses new input instead of accepting it and killing it.
    const during = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [31],
        kind: 'message',
        text: 'late payload',
        deliveries: [
          { delivery_id: 31, seq: 31, presentation: testDeliveryPresentation('late payload') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(during).toMatchObject({ disposition: 'refused', reason: 'draining' });
    await waitFor(() => events.some((e) => e.event === 'runtime_exit'), 3_000);
    await wait(200);

    const exits = events.filter((e) => e.event === 'runtime_exit');
    expect(exits).toHaveLength(1);
    expect(exits[0]).toMatchObject({ intentional: false, code: null });
  });

  it('non-fatal parsed errors surface as thinking activity instead of leaving the UI idle', async () => {
    const { driver } = makeProgrammableDriver(['--emit-line', 'A']);
    const { supervisor, events } = makeSupervisorWithDriver(driver);
    let step = 0;
    const sequence: ParsedEvent[][] = [
      [],
      [
        {
          kind: 'error',
          code: 'api_connection_refused',
          message: 'Claude API gateway unreachable; retrying (attempt 1)',
        },
      ],
    ];
    driver.parseLine = () => sequence[step++] ?? [];

    await supervisor.start(baseStartParams);

    await waitFor(() =>
      events.some(
        (e) =>
          e.event === 'runtime_state' &&
          e.state.status === 'thinking' &&
          e.state.activitySnippet === 'Claude API gateway unreachable; retrying (attempt 1)' &&
          e.state.activitySnippetKind === undefined,
      ),
    );

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('a parsed event whose encoded frame exceeds MAX_FRAME_BYTES is downgraded to runtime_parsed_event_dropped (no payload leaked)', async () => {
    const { driver } = makeProgrammableDriver(['--emit-line', 'A']);
    const { supervisor, events } = makeSupervisorWithDriver(driver);
    // Build a tool_call with a huge `input` so the encoded frame
    // blows past MAX_FRAME_BYTES (1 MiB).
    const huge = 'x'.repeat(2 * 1024 * 1024);
    driver.parseLine = () => [{ kind: 'tool_call', name: 'Bash', input: { command: huge } }];

    await supervisor.start(baseStartParams);
    await waitFor(() => events.some((e) => e.event === 'runtime_parsed_event_dropped'), 3_000);

    // The dropped frame is metadata-only — no payload fields.
    const dropped = events.find((e) => e.event === 'runtime_parsed_event_dropped');
    expect(dropped).toBeDefined();
    if (dropped && dropped.event === 'runtime_parsed_event_dropped') {
      expect(dropped.parsed_kind).toBe('tool_call');
      expect(dropped.reason).toBe('frame_too_large');
      expect(dropped.encoded_bytes).toBeGreaterThan(1 * 1024 * 1024);
      // Hard rule: no payload leakage.
      const keys = new Set(Object.keys(dropped));
      expect(keys.has('parsed')).toBe(false);
      expect(keys.has('input')).toBe(false);
      expect(keys.has('raw')).toBe(false);
      expect(keys.has('text')).toBe(false);
      expect(keys.has('name')).toBe(false);
    }

    // No full runtime_parsed_event for the same kind should have
    // landed (we'd otherwise be double-counting).
    const fullToolCallEvents = events.filter(
      (e) => e.event === 'runtime_parsed_event' && e.parsed.kind === 'tool_call',
    );
    expect(fullToolCallEvents).toHaveLength(0);
    expect(events).toContainEqual(
      expect.objectContaining({
        event: 'runtime_state',
        state: expect.objectContaining({ status: 'tool_running', detail: 'Bash' }),
      }),
    );

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('passes the initialized CLI session ID to subsequent driver input encoding', async () => {
    const child = new ControlledRuntime(true);
    const sessions: Array<string | null | undefined> = [];
    const { supervisor, events } = make3cSupervisor({
      spawnFn: () => child as unknown as ChildProcess,
      driverOverride: (base) => ({
        ...base,
        parseLine: () => [{ kind: 'session_init', sessionId: 'sess-xyz' }],
        encodeStdinMessage: (text, session) => {
          sessions.push(session);
          return encodeStreamJsonUser(text);
        },
      }),
    });
    const deliver = (id: number) =>
      supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          delivery_ids: [id],
          kind: 'message',
          text: 'hello',
          deliveries: [
            { delivery_id: id, seq: id, presentation: testDeliveryPresentation('hello') },
          ],
          spawn_options: baseStartParams.options,
        },
        { owner: 'test-owner' },
      );
    try {
      await supervisor.start(baseStartParams);
      expect((await deliver(1)).disposition).toBe('admitted');
      await waitForDeliveryConsumed(events, 1);
      child.stdout.emit('data', 'initialized\n');
      expect((await deliver(2)).disposition).toBe('admitted');
      await waitForDeliveryConsumed(events, 2);
      expect(sessions).toEqual([null, 'sess-xyz']);
    } finally {
      child.dispose();
    }
  });
});

// ── #task:2210 lifecycle-start admission gate (regression: #task:2095
//    degraded #task:1561's per-payload gate to pure observation, so a
//    successful stdin write re-flipped rows to consumed — "countgame round
//    2"). Two falsifier sets:
//      Set 1 (started-gate): write-ack alone must NOT settle (test 1);
//        the correlated `started` MUST settle (test 2). A settle-at-write
//        mis-impl fails test 1; a never-settle mis-impl fails test 2.
//      Set 2 (uuid correlation): a `started` for a DIFFERENT commandUuid
//        must NOT settle (test 3). A "settle on any started" mis-impl
//        fails test 3.
//    All three drive a `lifecycle-start` programmable driver; the write's
//    `--echo-stdin` echo triggers parseLine, and we queue the lifecycle
//    frame from inside the encode wrapper so it correlates to the uuid the
//    supervisor just minted for THIS batch.
describe('Supervisor 3b — #task:2210 lifecycle-start admission gate', () => {
  const deliverBatch = (supervisor: Supervisor, id: number) =>
    supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [id],
        kind: 'message',
        text: `payload ${id}`,
        deliveries: [
          { delivery_id: id, seq: id, presentation: testDeliveryPresentation(`payload ${id}`) },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );

  const hardStop = (supervisor: Supervisor) =>
    supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });

  it('a written batch stays UNCONSUMED on the stdin-write ack alone — no input_lifecycle started ⇒ no runtime_delivery_consumed (regression falsifier)', async () => {
    const { driver, setNextParse } = makeProgrammableDriver([], 'self_inits', 'lifecycle-start');
    // The write's stdin echo triggers parseLine; queue NOTHING so the
    // transport ack is the ONLY signal the daemon sees.
    const baseEncode = driver.encodeStdinMessage.bind(driver);
    driver.encodeStdinMessage = (t, s, o) => {
      setNextParse([]);
      return baseEncode(t, s, o);
    };
    const logs: Array<Record<string, unknown>> = [];
    const logger: DaemonLogger = {
      info: (f: unknown) => {
        if (f && typeof f === 'object') logs.push(f as Record<string, unknown>);
      },
      warn: (f: unknown) => {
        if (f && typeof f === 'object') logs.push(f as Record<string, unknown>);
      },
      error: () => {},
    } as unknown as DaemonLogger;
    const { supervisor, events } = makeSupervisorWithDriver(driver, { logger });

    await supervisor.start(baseStartParams);
    await waitForRuntimeReady(events);
    const res = await deliverBatch(supervisor, 77);
    // Transport write succeeded — disposition is admitted (row handed to the
    // runtime, in_flight_owner set) — but that is NOT admission.
    expect(res.disposition).toBe('admitted');
    // The gate recorded the awaiting-admission sub-state for THIS batch.
    await waitFor(() =>
      logs.some(
        (l) =>
          l['action'] === 'startup_delivery_awaiting_admission' &&
          Array.isArray(l['delivery_ids']) &&
          (l['delivery_ids'] as number[]).includes(77),
      ),
    );
    // consumed is WITHHELD: a bounded negative wait proves it never fires on
    // the write ack. (Pre-#2210 settle-at-write behavior fails HERE.)
    await wait(150);
    expect(
      events.some((e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(77)),
    ).toBe(false);
    await hardStop(supervisor);
  });

  it('input_lifecycle{started} whose commandUuid == the minted payloadUuid settles the batch onto runtime_delivery_consumed', async () => {
    const { driver, setNextParse } = makeProgrammableDriver([], 'self_inits', 'lifecycle-start');
    // Queue a correlated `started` keyed to the uuid the supervisor mints
    // for THIS write, so the echo replays the real admission proof.
    const baseEncode = driver.encodeStdinMessage.bind(driver);
    driver.encodeStdinMessage = (t, s, o) => {
      setNextParse([
        { kind: 'input_lifecycle', commandUuid: o?.payloadUuid ?? '', state: 'started' },
      ]);
      return baseEncode(t, s, o);
    };
    const { supervisor, events } = makeSupervisorWithDriver(driver);

    await supervisor.start(baseStartParams);
    await waitForRuntimeReady(events);
    const res = await deliverBatch(supervisor, 88);
    expect(res.disposition).toBe('admitted');
    // The correlated `started` opens the gate — consumed now fires.
    await waitForDeliveryConsumed(events, 88);
    await hardStop(supervisor);
  });

  it('input_lifecycle{started} with a NON-matching commandUuid does NOT settle the batch (uuid-correlated, not any-started)', async () => {
    const { driver, setNextParse } = makeProgrammableDriver([], 'self_inits', 'lifecycle-start');
    const baseEncode = driver.encodeStdinMessage.bind(driver);
    driver.encodeStdinMessage = (t, s, o) => {
      setNextParse([
        { kind: 'input_lifecycle', commandUuid: 'not-the-minted-uuid', state: 'started' },
      ]);
      return baseEncode(t, s, o);
    };
    const { supervisor, events } = makeSupervisorWithDriver(driver);

    await supervisor.start(baseStartParams);
    await waitForRuntimeReady(events);
    const res = await deliverBatch(supervisor, 99);
    expect(res.disposition).toBe('admitted');
    // The mismatched `started` was parsed & fanned out...
    await waitFor(() =>
      events.some((e) => e.event === 'runtime_parsed_event' && e.parsed.kind === 'input_lifecycle'),
    );
    // ...but it must NOT have opened the gate for delivery 99.
    await wait(100);
    expect(
      events.some((e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(99)),
    ).toBe(false);
    await hardStop(supervisor);
  });

  const consumedCount = (events: EventFrame[], id: number) =>
    events.filter((e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(id))
      .length;
  const failedCount = (events: EventFrame[], id: number) =>
    events.filter((e) => e.event === 'runtime_delivery_failed' && e.delivery_ids.includes(id))
      .length;

  it.each(['success', 'failure'] as const)(
    'early started waits for stdin completion; %s settles the delivery exactly once',
    async (outcome) => {
      const { supervisor, events, stub, deliverGate, injectStarted } = makeControlledGate(true);
      try {
        await supervisor.start(baseStartParams);
        expect((await deliverGate(55)).disposition).toBe('admitted');
        await waitFor(() => stub.writes.length === 1);
        injectStarted();
        expect(events).toContainEqual(
          expect.objectContaining({
            event: 'runtime_parsed_event',
            parsed: expect.objectContaining({ kind: 'input_lifecycle', state: 'started' }),
          }),
        );
        expect(consumedCount(events, 55)).toBe(0);
        expect(failedCount(events, 55)).toBe(0);

        stub.writes[0]!.finish(outcome === 'failure' ? new Error('EPIPE') : undefined);
        await waitFor(() => consumedCount(events, 55) + failedCount(events, 55) > 0);
        injectStarted(); // A duplicate proof cannot settle the same delivery twice.
        expect(consumedCount(events, 55)).toBe(outcome === 'success' ? 1 : 0);
        expect(failedCount(events, 55)).toBe(outcome === 'failure' ? 1 : 0);
        if (outcome === 'failure') {
          expect(events).toContainEqual(
            expect.objectContaining({
              event: 'runtime_delivery_failed',
              delivery_ids: [55],
              reason: 'write_failed',
            }),
          );
        }
      } finally {
        stub.dispose();
      }
    },
  );

  it('early started does not flush a deferred batch before the first stdin write completes', async () => {
    const { supervisor, events, stub, deliverGate, injectStarted } = makeControlledGate(true);
    try {
      await supervisor.start(baseStartParams);
      expect((await deliverGate(71)).disposition).toBe('admitted');
      await waitFor(() => stub.writes.length === 1);
      injectStarted();
      expect(consumedCount(events, 71)).toBe(0);
      expect((await deliverGate(72)).disposition).toBe('deferred');
      expect(stub.writes).toHaveLength(1);
      expect(consumedCount(events, 72)).toBe(0);

      stub.writes[0]!.finish();
      await waitForDeliveryConsumed(events, 71);
      await waitFor(() => stub.writes.length === 2);
      expect(consumedCount(events, 72)).toBe(0);
      injectStarted();
      expect(consumedCount(events, 72)).toBe(0);
      stub.writes[1]!.finish();
      await waitForDeliveryConsumed(events, 72);
      expect(stub.writes).toHaveLength(2);
      expect(consumedCount(events, 71)).toBe(1);
      expect(consumedCount(events, 72)).toBe(1);
      expect(events.filter((e) => e.event === 'runtime_delivery_failed')).toEqual([]);
    } finally {
      stub.dispose();
    }
  });

  // ── P0 exit-vs-stdout-drain (Cindy `dfxZOd5LJ17FibYs`): a pending
  //    lifecycle-start gate's failed/ROLLBACK must wait for the stdout
  //    admission channel to reach EOF/`close`, NEVER fire on process `exit`.
  //    Node delivers buffered stdout up to the stream's own end, which can land
  //    AFTER `exit`; rolling an already-admitted row back to pending on `exit`
  //    alone re-opens it for the next runtime = product double delivery. A
  //    fully-controlled stub child lets us order exit → late-`started` stdout →
  //    close EXACTLY (a real spawn can't sequence that deterministically).
  const makeControlledGate = (holdWrites = false) => {
    const { driver } = makeProgrammableDriver([], 'self_inits', 'lifecycle-start');
    let capturedUuid = '';
    const baseEncode = driver.encodeStdinMessage.bind(driver);
    driver.encodeStdinMessage = (t, s, o) => {
      capturedUuid = o?.payloadUuid ?? '';
      return baseEncode(t, s, o);
    };
    const stub = new ControlledRuntime(!holdWrites);
    const spawnFn = (_b: string, _a: ReadonlyArray<string>, _o: SpawnOptions): ChildProcess =>
      stub as unknown as ChildProcess;
    const spy = makeEmitSpy();
    const supervisor = new Supervisor({
      emit: spy.emit,
      logger: silentLogger(),
      driverFor: () => driver,
      resolveRuntime: async (_kind, fallback) => ({ binary: fallback, version: null }),
      spawnFn,
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
      }),
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
      sigtermToSigkillMs: 50,
      sigkillTimeoutMs: 200,
    });
    const deliverGate = (id: number) =>
      supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          delivery_ids: [id],
          kind: 'message',
          text: 'gate batch',
          deliveries: [
            { delivery_id: id, seq: id, presentation: testDeliveryPresentation('gate batch') },
          ],
          spawn_options: baseStartParams.options,
        },
        { owner: 'test-owner' },
      );
    // Drive a matching `started` onto stdout: swap parseLine to emit it for the
    // captured payload uuid, then push any non-empty line (parseLine ignores
    // content). Used AFTER `exit` to exercise the post-exit/pre-close window.
    const injectStarted = () => {
      driver.parseLine = () => [
        { kind: 'input_lifecycle', commandUuid: capturedUuid, state: 'started' },
      ];
      stub.stdout.emit('data', 'lifecycle-line\n');
    };
    return {
      supervisor,
      events: spy.events,
      stub,
      deliverGate,
      injectStarted,
      driver,
      uuid: () => capturedUuid,
    };
  };

  it('P0-B1: a matching started arriving on stdout AFTER exit (before close) still settles consumed; exit does NOT roll back first', async () => {
    const { events, stub, deliverGate, injectStarted } = makeControlledGate();
    await deliverGate(81);
    await waitFor(() => stub.writeResolved, 1_000);
    await wait(20);
    // Write confirmed; gate armed, still awaiting the `started` proof.
    expect(consumedCount(events, 81)).toBe(0);
    expect(failedCount(events, 81)).toBe(0);

    // Child exits BEFORE the started chunk is delivered. The gate's rollback
    // must be DEFERRED — no failed at exit.
    stub.emit('exit', 0, null);
    await wait(20);
    expect(failedCount(events, 81)).toBe(0);
    expect(consumedCount(events, 81)).toBe(0);

    // The matching `started` now lands on stdout (post-exit, pre-close) and
    // settles the batch to consumed — the whole point of deferring.
    injectStarted();
    await waitForDeliveryConsumed(events, 81);
    expect(consumedCount(events, 81)).toBe(1);

    // Channel closes with the gate already settled → still no failed.
    stub.stdout.emit('close');
    await wait(20);
    expect(consumedCount(events, 81)).toBe(1);
    expect(failedCount(events, 81)).toBe(0);
  });

  it('P0-B2: exit then stdout close with NO started finalizes failed exactly once (child_exit); duplicate close/exit does not re-settle', async () => {
    const { events, stub, deliverGate } = makeControlledGate();
    await deliverGate(82);
    await waitFor(() => stub.writeResolved, 1_000);
    await wait(20);
    expect(consumedCount(events, 82)).toBe(0);
    expect(failedCount(events, 82)).toBe(0);

    // Exit defers finalization; no rollback yet.
    stub.emit('exit', 0, null);
    await wait(20);
    expect(failedCount(events, 82)).toBe(0);

    // The channel drains with no admission proof → single failed rollback,
    // reason preserved from exit time (`child_exit`).
    stub.stdout.emit('close');
    await wait(20);
    expect(failedCount(events, 82)).toBe(1);
    expect(consumedCount(events, 82)).toBe(0);
    const failed = events.find(
      (e) => e.event === 'runtime_delivery_failed' && e.delivery_ids.includes(82),
    );
    expect(failed && failed.event === 'runtime_delivery_failed' && failed.reason).toBe(
      'child_exit',
    );

    // A duplicate close/end and a re-fired exit must NOT settle a second time.
    stub.stdout.emit('close');
    stub.stdout.emit('end');
    stub.emit('exit', 0, null);
    await wait(20);
    expect(failedCount(events, 82)).toBe(1);
    expect(consumedCount(events, 82)).toBe(0);
  });

  it('P0-B3: a matching started SPLIT across the exit boundary re-stitches at EOF and settles consumed (exit must NOT flush the half-frame)', async () => {
    // Cindy `CbJ59KTcL69Z6rJa` A: `exit` ≠ stdout EOF, so a `started` JSON can
    // arrive in two chunks straddling exit. If `onChildExit` flushed the buffer,
    // the prefix would be parsed as a broken complete line and cleared, and the
    // suffix arriving post-exit could never be re-stitched → wrong failed. The
    // fix preserves the buffer for a deferred gate and drains it at EOF.
    const { events, stub, deliverGate, driver, uuid } = makeControlledGate();
    // Content-sensitive parseLine: emit the `started` proof ONLY for the FULLY
    // stitched line, never for either half alone. So a premature flush of the
    // prefix produces no proof (and the bug would fail the batch).
    driver.parseLine = (line: string) =>
      line.includes('LIFECYCLE-STARTED-COMPLETE')
        ? [{ kind: 'input_lifecycle', commandUuid: uuid(), state: 'started' }]
        : [];
    await deliverGate(83);
    await waitFor(() => stub.writeResolved, 1_000);
    await wait(20);
    expect(consumedCount(events, 83)).toBe(0);

    // Prefix (no newline) lands in the buffer BEFORE exit.
    stub.stdout.emit('data', 'LIFECYCLE-');
    // Child exits with only the half-frame buffered. Rollback deferred; the
    // half-frame must NOT be flushed/cleared.
    stub.emit('exit', 0, null);
    await wait(20);
    expect(failedCount(events, 83)).toBe(0);
    expect(consumedCount(events, 83)).toBe(0);

    // Suffix arrives post-exit, completing the line; then the channel closes.
    stub.stdout.emit('data', 'STARTED-COMPLETE\n');
    await waitForDeliveryConsumed(events, 83);
    expect(consumedCount(events, 83)).toBe(1);
    stub.stdout.emit('close');
    await wait(20);
    expect(consumedCount(events, 83)).toBe(1);
    expect(failedCount(events, 83)).toBe(0);
  });

  it('P0-B4: post-exit ordinary frames (turn_end) before the matching started are drained admission-ONLY — consumed fires, but no new parsed event / second terminal after exit', async () => {
    // Cindy `CbJ59KTcL69Z6rJa` B: after `runtime_exit`, the post-exit window may
    // only extract the matching `started` admission proof. An ordinary frame
    // (turn_end / tool / session / error) that precedes it must be dropped
    // silently — running the full dispatch would resolve the already-terminal
    // run twice, rebuild run-state, or publish an ordinary parsed event.
    const { events, stub, deliverGate, driver, uuid } = makeControlledGate();
    driver.parseLine = (line: string) => {
      if (line.includes('TURNEND')) return [{ kind: 'turn_end' }];
      if (line.includes('STARTED'))
        return [{ kind: 'input_lifecycle', commandUuid: uuid(), state: 'started' }];
      return [];
    };
    await deliverGate(84);
    await waitFor(() => stub.writeResolved, 1_000);
    await wait(20);

    stub.emit('exit', 0, null);
    await wait(20);
    const parsedFor84 = () =>
      events.filter((e) => e.event === 'runtime_parsed_event' && e.agent_id === 'a1').length;
    const exitCount = () =>
      events.filter((e) => e.event === 'runtime_exit' && e.agent_id === 'a1').length;
    const stateCount = () =>
      events.filter((e) => e.event === 'runtime_state' && e.agent_id === 'a1').length;
    const parsedBefore = parsedFor84();
    const exitBefore = exitCount();
    const stateBefore = stateCount();
    expect(exitBefore).toBe(1); // the single terminal from onChildExit

    // Post-exit chunk: an ordinary turn_end line, THEN the matching started.
    stub.stdout.emit('data', 'TURNEND-line\nSTARTED-line\n');
    await waitForDeliveryConsumed(events, 84);
    stub.stdout.emit('close');
    await wait(20);

    // Admission settled exactly once…
    expect(consumedCount(events, 84)).toBe(1);
    expect(failedCount(events, 84)).toBe(0);
    // …and NONE of the ordinary post-exit side effects fired: no new
    // runtime_parsed_event (the turn_end was dropped), no second terminal,
    // no new run-state transition after exit.
    expect(parsedFor84()).toBe(parsedBefore);
    expect(exitCount()).toBe(exitBefore);
    expect(stateCount()).toBe(stateBefore);
  });
});

describe('Supervisor 3b — deliver SPAWN batch step-0 mutex', () => {
  it('deliver SPAWN arm delays runtime_ready_for_drain until the batch settles (runtime_delivery_consumed comes first)', async () => {
    // Cindy `wuiwdLKrLFyGip_s`: settle order is
    //   consumed/failed → clear pending → ready_for_drain.
    // We verify by enforcing the relative event ordering.
    const { supervisor, events } = makeSupervisor({
      driverArgs: ['--echo-stdin'],
    });
    await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [101, 102],
        kind: 'message',
        text: 'boot batch',
        deliveries: [
          { delivery_id: 101, seq: 101, presentation: testDeliveryPresentation('boot batch') },
          { delivery_id: 102, seq: 102, presentation: testDeliveryPresentation('boot batch') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    await waitFor(() => events.some((e) => e.event === 'runtime_ready_for_drain'));

    const idxConsumed = events.findIndex((e) => e.event === 'runtime_delivery_consumed');
    const idxReady = events.findIndex((e) => e.event === 'runtime_ready_for_drain');
    expect(idxConsumed).toBeGreaterThanOrEqual(0);
    expect(idxReady).toBeGreaterThan(idxConsumed);

    const consumed = events.find((e) => e.event === 'runtime_delivery_consumed');
    if (consumed && consumed.event === 'runtime_delivery_consumed') {
      expect(consumed.delivery_ids).toEqual([101, 102]);
    }

    // ready_for_drain must be emitted exactly once.
    const readyCount = events.filter((e) => e.event === 'runtime_ready_for_drain').length;
    expect(readyCount).toBe(1);

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('SPAWN batch gate is cleared even if the child exits before the supervisor writes the batch (no startup_pending leak)', async () => {
    // Cindy `wuiwdLKrLFyGip_s`: failed settle must still clear the
    // gate. The invariant is "any subsequent deliver never returns
    // `startup_pending` after the start batch lands one of
    // consumed/failed". We exercise that via a child that exits
    // fast (--sleep 1): the supervisor's consume path races with
    // the exit handler — either path clears the gate. The
    // `runtime_ready_for_drain` event MAY or MAY NOT fire depending
    // on who won the race; the relevant assertion is gate-clear,
    // not the absence of ready_for_drain.
    const { supervisor, events } = makeSupervisor({
      driverArgs: ['--sleep', '1'],
    });
    await supervisor
      .deliver(
        {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          delivery_ids: [201],
          kind: 'message',
          text: 'boot batch',
          deliveries: [
            { delivery_id: 201, seq: 201, presentation: testDeliveryPresentation('boot batch') },
          ],
          spawn_options: baseStartParams.options,
        },
        { owner: 'test-owner' },
      )
      .catch(() => {});
    await waitFor(
      () =>
        events.some((e) => e.event === 'runtime_exit') ||
        events.some((e) => e.event === 'runtime_delivery_failed') ||
        events.some((e) => e.event === 'runtime_delivery_consumed'),
      3_000,
    );
    // After the dust settles, a deliver must NOT refuse startup_pending
    // (it'll usually SPAWN again because the child already exited; if
    // consume won the race it may be admitted — both are fine. The key
    // invariant Cindy `wuiwdLKrLFyGip_s` locked is the gate cleared,
    // NOT a particular post-clear outcome).
    const deliverRes = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [202],
        kind: 'message',
        text: 'after-death',
        deliveries: [
          { delivery_id: 202, seq: 202, presentation: testDeliveryPresentation('after-death') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    if (deliverRes.disposition === 'refused') {
      expect(deliverRes.reason).not.toBe('startup_pending');
    }

    // If the supervisor took the explicit failed path, verify the
    // delivery_ids round-trip and reason=child_exit (this is the
    // path that runs when the exit handler beats the consume).
    const failed = events.find((e) => e.event === 'runtime_delivery_failed');
    if (failed && failed.event === 'runtime_delivery_failed') {
      expect(failed.delivery_ids).toEqual([201]);
      expect(failed.reason).toBe('child_exit');
    }

    await supervisor
      .stop({
        runtime_scope: {
          kind: 'conversation',
          conversation_id: 'Conversation0001',
          agent_id: 'a1',
        },
        runtime_generation: 1,
        mode: 'hard',
      })
      .catch(() => {});
  });
});

describe('Supervisor 3b — Cindy `YW2BGWryWjfymPkU` B2: deferred SPAWN-batch consume for requires_first_message drivers', () => {
  it('write success alone does NOT settle the gate when driver.kickoffMode === requires_first_message; consumed fires only after session_init', async () => {
    // CM's `initPendingInboxIds` defers `markDelivered` for codex-like
    // (`requires_first_message`) drivers until `session_init` arrives,
    // so the supervisor must mirror that. Until session_init,
    // `runtime_delivery_consumed` and `runtime_ready_for_drain`
    // are bottled up, and a subsequent `deliver` refuses `startup_pending`.
    // Use a stub child so we control stdin completion AND session_init
    // arrival timing precisely.
    class ControlledStub extends EventEmitter {
      readonly pid = 23_456;
      readonly stdout = new EventEmitter();
      readonly stderr = new EventEmitter();
      writeResolved = false;
      readonly stdin = {
        write: (_line: string, cb?: (err?: Error | null) => void): boolean => {
          // Resolve on next tick so `writeStdinPayload` settles, but
          // the caller can observe state in between.
          setImmediate(() => {
            this.writeResolved = true;
            cb?.();
          });
          return true;
        },
        end: () => {},
        destroyed: false,
        once: () => {},
      };
      kill(_sig?: NodeJS.Signals | number): boolean {
        setImmediate(() => this.emit('exit', 0, null));
        return true;
      }
    }
    const stub = new ControlledStub();
    const spawnFn = (_b: string, _a: ReadonlyArray<string>, _o: SpawnOptions): ChildProcess =>
      stub as unknown as ChildProcess;

    const { driver } = makeProgrammableDriver([], 'requires_first_message');
    const spy = makeEmitSpy();
    const supervisor = new Supervisor({
      emit: spy.emit,
      logger: silentLogger(),
      driverFor: () => driver,
      resolveRuntime: async (_kind, fallback) => ({ binary: fallback, version: null }),
      spawnFn,
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
      }),
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
      sigtermToSigkillMs: 50,
      sigkillTimeoutMs: 200,
    });

    await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [501, 502],
        kind: 'message',
        text: 'boot batch',
        deliveries: [
          { delivery_id: 501, seq: 501, presentation: testDeliveryPresentation('boot batch') },
          { delivery_id: 502, seq: 502, presentation: testDeliveryPresentation('boot batch') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );

    // Wait for the stdin write to have actually completed (the consume
    // path is async — deliver() returned before the write settled).
    await waitFor(() => stub.writeResolved, 1_000);
    // Give the supervisor a couple of ticks to process the post-write
    // branch. Since requires_first_message + !initObserved, the gate
    // must NOT clear and ready_for_drain must NOT fire.
    await wait(40);

    expect(spy.events.some((e) => e.event === 'runtime_delivery_consumed')).toBe(false);
    expect(spy.events.some((e) => e.event === 'runtime_ready_for_drain')).toBe(false);
    expect(spy.events.some((e) => e.event === 'runtime_delivery_failed')).toBe(false);

    // #task:1417 Stage 3 — deliver during the write-in-flight window now
    // DEFERS (the handle is `ready` but the first batch's write is parked
    // awaiting session_init, so `handle.startupDelivery !== null`). Custody
    // transfers to the daemon's mid-turn queue; [777] flushes once the
    // parked write settles. (Stage 2 refused `startup_pending` here and made
    // the backend re-drive; Stage 3 keeps it, which is strictly better.)
    const deliverMid = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [777],
        kind: 'message',
        text: 'stub',
        deliveries: [
          { delivery_id: 777, seq: 777, presentation: testDeliveryPresentation('stub') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(deliverMid).toEqual({
      disposition: 'deferred',
      pid: expect.any(Number),
      started_at: expect.any(Number),
    });

    // Now drive a `session_init` parsed event by emitting a stdout
    // line; the programmable driver returns whatever its parseLine
    // hook returns. The line content itself is irrelevant — parseLine
    // sees any non-empty line.
    driver.parseLine = () => [{ kind: 'session_init', sessionId: 'sess-deferred' }];
    stub.stdout.emit('data', 'irrelevant-line-content\n');

    // After session_init, settle order must be:
    //   consumed → clear gate → ready_for_drain
    // (the `runtime_parsed_event` sidecar is emitted by
    // `emitParsedEvent` AFTER `updateRunStateFromParsed`, so its
    // index is later than both consumed and ready — that's a
    // cosmetic ordering and not the invariant. The semantic
    // invariant is `consumed < ready`.)
    await waitFor(() => spy.events.some((e) => e.event === 'runtime_ready_for_drain'), 1_000);
    const idxConsumed = spy.events.findIndex((e) => e.event === 'runtime_delivery_consumed');
    const idxReady = spy.events.findIndex((e) => e.event === 'runtime_ready_for_drain');
    const idxSessionInit = spy.events.findIndex(
      (e) => e.event === 'runtime_parsed_event' && e.parsed.kind === 'session_init',
    );
    expect(idxSessionInit).toBeGreaterThanOrEqual(0);
    expect(idxConsumed).toBeGreaterThanOrEqual(0);
    expect(idxReady).toBeGreaterThan(idxConsumed);

    const consumed = spy.events.find((e) => e.event === 'runtime_delivery_consumed');
    if (consumed && consumed.event === 'runtime_delivery_consumed') {
      expect(consumed.delivery_ids).toEqual([501, 502]);
    }

    // ready_for_drain fires exactly once even with the deferred path.
    const readyCount = spy.events.filter((e) => e.event === 'runtime_ready_for_drain').length;
    expect(readyCount).toBe(1);

    // #task:1417 Stage 3 — the deferred [777] flushes once the parked
    // startup write settled + the boundary reopened. Its own consumed lands
    // AFTER the startup batch's, covering the deferred delivery_id.
    await waitFor(
      () =>
        spy.events.some(
          (e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(777),
        ),
      1_000,
    );

    // Deliver after everything settled now succeeds (WRITE arm → admitted).
    const deliverAfter = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [999],
        kind: 'message',
        text: 'stub',
        deliveries: [
          { delivery_id: 999, seq: 999, presentation: testDeliveryPresentation('stub') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(deliverAfter.disposition).toBe('admitted');

    // Cleanup.
    setImmediate(() => stub.emit('exit', 0, null));
  });
});

describe('Supervisor 3b — Cindy `YW2BGWryWjfymPkU` B3: resolveRun terminal semantics', () => {
  it('turn_end resolves the run; the next thinking event creates a fresh runId with no leaked terminalReason', async () => {
    // resolveRun() clears `handle.runState` after emitting the terminal
    // so the next observable activity must allocate a NEW runId via
    // `transitionRunState`'s lazy creation path. Without B3's fix the
    // old runState would keep accumulating with a `terminalReason`
    // stuck on it.
    const { driver } = makeProgrammableDriver([
      '--emit-line',
      'A',
      '--emit-line',
      'B',
      '--emit-line',
      'C',
    ]);
    const { supervisor, events } = makeSupervisorWithDriver(driver);
    let step = 0;
    const sequence: ParsedEvent[][] = [
      [], // READY
      [{ kind: 'thinking', text: 't1' }], // A — run 1 starts
      [{ kind: 'turn_end' }], // B — run 1 resolves
      [{ kind: 'thinking', text: 't2' }], // C — run 2 starts
    ];
    driver.parseLine = () => sequence[step++] ?? [];

    await supervisor.start(baseStartParams);
    await waitFor(
      () =>
        events.filter((e) => e.event === 'runtime_state' && e.state.status === 'thinking').length >=
        2,
      3_000,
    );

    const thinkingStates = events
      .filter((e) => e.event === 'runtime_state' && e.state.status === 'thinking')
      .map((e) => (e.event === 'runtime_state' ? e.state : null!));
    expect(thinkingStates.length).toBeGreaterThanOrEqual(2);

    // First and last thinking states belong to different runs.
    const firstThinking = thinkingStates[0]!;
    const lastThinking = thinkingStates[thinkingStates.length - 1]!;
    expect(firstThinking.runId).not.toBe(lastThinking.runId);

    // Neither thinking carries a terminalReason (it would only land
    // on the resolved terminal frame).
    expect(firstThinking.terminalReason).toBeUndefined();
    expect(lastThinking.terminalReason).toBeUndefined();

    // The terminal frame for turn_end carries status:idle (CM rule:
    // turn_ended → idle) and reuses the first run's runId.
    const terminal = events.find(
      (e) => e.event === 'runtime_state' && e.state.terminalReason === 'turn_ended',
    );
    expect(terminal).toBeDefined();
    if (terminal && terminal.event === 'runtime_state') {
      expect(terminal.state.status).toBe('idle');
      expect(terminal.state.runId).toBe(firstThinking.runId);
    }

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('hard stop mid-tool_running emits terminal {status:failed, terminalReason:stopped} reusing the active runId', async () => {
    // Before B3 the supervisor would emit a terminal `runtime_state`
    // by attaching `terminalReason` via `emitState(handle, reason)`
    // WITHOUT flipping `status` — leaving a tool_running terminal,
    // which is incoherent. resolveRun maps `stopped → failed` (per
    // CM's terminalStatus table) and preserves the existing runId so
    // the consumer can correlate the terminal with the in-flight run.
    const { driver } = makeProgrammableDriver(['--emit-line', 'A']);
    const { supervisor, events } = makeSupervisorWithDriver(driver);
    let step = 0;
    const sequence: ParsedEvent[][] = [
      [], // READY
      [{ kind: 'tool_call', name: 'Bash', input: {} }], // A → tool_running
    ];
    driver.parseLine = () => sequence[step++] ?? [];

    await supervisor.start(baseStartParams);
    await waitFor(
      () =>
        events.some(
          (e) =>
            e.event === 'runtime_state' &&
            e.state.status === 'tool_running' &&
            e.state.detail === 'Bash',
        ),
      3_000,
    );
    const toolRunning = events.find(
      (e) =>
        e.event === 'runtime_state' &&
        e.state.status === 'tool_running' &&
        e.state.detail === 'Bash',
    );
    const inflightRunId =
      toolRunning && toolRunning.event === 'runtime_state' ? toolRunning.state.runId : null;
    expect(inflightRunId).toBeTruthy();

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });

    // Find the FIRST terminal `runtime_state` (resolveRun emits one
    // before runtime_exit). It must carry status:failed +
    // terminalReason:stopped + the same runId as the in-flight run.
    const terminal = events.find(
      (e) => e.event === 'runtime_state' && e.state.terminalReason === 'stopped',
    );
    expect(terminal).toBeDefined();
    if (terminal && terminal.event === 'runtime_state') {
      expect(terminal.state.status).toBe('failed');
      expect(terminal.state.runId).toBe(inflightRunId);
    }
  });

  it('status() after a terminal-resolved run returns run_state:null (no stale terminalReason leaks via the status surface)', async () => {
    // resolveRun clears `handle.runState` so the next status() poll
    // returns null until a fresh event creates a new run. Without B3
    // the terminal AgentRunState would still be hanging on the handle,
    // and status() would project it as if the run were still live.
    const { driver } = makeProgrammableDriver(['--emit-line', 'A', '--emit-line', 'B']);
    const { supervisor, events } = makeSupervisorWithDriver(driver);
    let step = 0;
    const sequence: ParsedEvent[][] = [
      [], // READY
      [{ kind: 'thinking', text: 'pondering' }], // A → run starts
      [{ kind: 'turn_end' }], // B → run resolves
    ];
    driver.parseLine = () => sequence[step++] ?? [];

    await supervisor.start(baseStartParams);
    await waitFor(
      () =>
        events.some((e) => e.event === 'runtime_state' && e.state.terminalReason === 'turn_ended'),
      3_000,
    );
    // Give a tick for runState to be nulled out after the terminal
    // emit.
    await wait(20);

    const snap = supervisor.status({ conversation_id: 'Conversation0001', agent_id: 'a1' });
    expect(snap.runtimes).toHaveLength(1);
    expect(snap.runtimes[0]?.run_state).toBeNull();

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });
});

// ─────────────────────────────────────────────────────────────────────
// 3c-b: bindIO + init watchdog + retry/cooldown + lifecycle lock +
//       compaction watchdog
//
// All 3c-b tests use the same fake-driver / fixture pattern as 3a/3b
// — Supervisor is exercised against a real child process via
// echo-runtime.cjs. Watchdog timers default to ~5 minutes; tests
// override the supervisor's `initWatchdogMs` (global override per
// Cindy `-pdNvRYOWip5e9yA`) to ~50-200ms so the cascades complete
// inside vitest's 5s budget.
// ─────────────────────────────────────────────────────────────────────

import {
  DEFAULT_COMPACTION_STALE_MS,
  DEFAULT_INIT_LIVENESS_SILENCE_MS,
  DEFAULT_INIT_POST_PROTOCOL_MS,
  DEFAULT_INIT_PRE_PROTOCOL_CAP_MS,
  type SupervisorOptions,
} from '../src/daemon/supervisor/index.js';

/** Build a supervisor variant that lets each 3c-b test pass extra
 *  watchdog / retry / cooldown knobs without rebuilding the spawn /
 *  driver wiring. Keeps the rest of the makeSupervisor() shape
 *  identical to 3a/3b. */
function make3cSupervisor(
  opts: {
    driverArgs?: string[];
    driverOverride?: (base: Driver) => Driver;
    spawnFn?: (b: string, a: ReadonlyArray<string>, o: SpawnOptions) => ChildProcess;
  } & Partial<
    Pick<
      SupervisorOptions,
      | 'sigtermToSigkillMs'
      | 'sigkillTimeoutMs'
      | 'initWatchdogMs'
      | 'initLivenessSilenceMs'
      | 'initPreProtocolCapMs'
      | 'initPostProtocolMs'
      | 'initMaxRetries'
      | 'initFailureCooldownThreshold'
      | 'initFailureCooldownMs'
      | 'compactionStaleMs'
      | 'emitRawRuntimeOutput'
    >
  > = {},
): { supervisor: Supervisor; events: EventFrame[]; driver: Driver } {
  const spy = makeEmitSpy();
  const baseDriver = makeFakeDriver(opts.driverArgs);
  const driver = opts.driverOverride ? opts.driverOverride(baseDriver) : baseDriver;
  const realSpawn = spawn;
  const spawnFn =
    opts.spawnFn ?? ((bin, args, options) => realSpawn(execPath, [bin, ...args.slice(1)], options));
  const supervisor = new Supervisor({
    emit: spy.emit,
    logger: silentLogger(),
    driverFor: () => driver,
    resolveRuntime: async (_kind, fallback) => ({ binary: fallback, version: null }),
    spawnFn,
    runtimeStartResolveDeps: testRuntimeStartResolveDeps({
      agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
    }),
    controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
    sigtermToSigkillMs: opts.sigtermToSigkillMs ?? 50,
    sigkillTimeoutMs: opts.sigkillTimeoutMs ?? 200,
    initWatchdogMs: opts.initWatchdogMs,
    initLivenessSilenceMs: opts.initLivenessSilenceMs,
    initPreProtocolCapMs: opts.initPreProtocolCapMs,
    initPostProtocolMs: opts.initPostProtocolMs,
    initMaxRetries: opts.initMaxRetries,
    initFailureCooldownThreshold: opts.initFailureCooldownThreshold,
    initFailureCooldownMs: opts.initFailureCooldownMs,
    compactionStaleMs: opts.compactionStaleMs,
    emitRawRuntimeOutput: opts.emitRawRuntimeOutput ?? true,
  });
  return { supervisor, events: spy.events, driver };
}

describe('Supervisor 3c-b — driver.bindIO', () => {
  it('calls driver.bindIO once after spawn with writeLine + spawnCtx', async () => {
    const calls: Array<{ writeLine: (line: string) => Promise<void>; ctx: DriverSpawnContext }> =
      [];
    const { supervisor } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        bindIO(io) {
          calls.push({ writeLine: io.writeLine, ctx: io.spawnCtx });
        },
      }),
    });
    const res = await supervisor.start(baseStartParams);
    assertStarted(res);
    expect(calls).toHaveLength(1);
    expect(typeof calls[0]!.writeLine).toBe('function');
    expect(calls[0]!.ctx.agentName).toBe('tester');
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('bindIO.writeLine writes the payload + "\\n" to child stdin', async () => {
    let writeLine: ((line: string) => Promise<void>) | null = null;
    const { supervisor, events } = make3cSupervisor({
      driverArgs: ['--echo-stdin'],
      driverOverride: (base) => ({
        ...base,
        bindIO(io) {
          writeLine = io.writeLine;
        },
      }),
    });
    await supervisor.start(baseStartParams);
    await waitFor(() => writeLine !== null);
    // wait for child READY so stdin is plumbed
    await waitFor(() =>
      events.some(
        (e) => e.event === 'runtime_output' && e.stream === 'stdout' && e.chunk.includes('READY'),
      ),
    );
    await writeLine!('jsonrpc-line-no-trailing-newline');
    await waitFor(
      () =>
        events.some(
          (e) =>
            e.event === 'runtime_output' &&
            e.stream === 'stdout' &&
            e.chunk.includes('STDIN:jsonrpc-line-no-trailing-newline'),
        ),
      2_000,
    );
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('bindIO.writeLine throws synchronously when the driver line already ends with "\\n" (Cindy `-pdNvRYOWip5e9yA` guard, `HyC05fC-FEs94uoZ` sync follow-up)', async () => {
    let writeLine: ((line: string) => Promise<void>) | null = null;
    const { supervisor } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        bindIO(io) {
          writeLine = io.writeLine;
        },
      }),
    });
    await supervisor.start(baseStartParams);
    await waitFor(() => writeLine !== null);
    // Synchronous throw — Codex drivers fire-and-forget via
    // `void this.io?.writeLine(...)`, so a rejected Promise would
    // become an unhandled rejection. The seam throws inside the
    // driver's bound callsite instead, where the driver author sees
    // it during local testing.
    expect(() => writeLine!('already-terminated\n')).toThrow(/must not end with/);
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });
});

describe('Supervisor 3c-b — 3-tier init watchdog', () => {
  it('silence tier fires when no bytes arrive within livenessSilenceMs', async () => {
    // Cindy `HyC05fC-FEs94uoZ` B1: prior version pointed buildArgs at
    // `[execPath, '-e', 'setInterval(...);']` to try to coerce
    // echo-runtime into emitting nothing, but the test spawn seam
    // collapses to `[bin, ...args.slice(1)]` so the actual invocation
    // was `node echo-runtime.cjs -e setInterval(...);` — echo-runtime
    // ignored the unknown args and still emitted `READY\n`, refreshing
    // the silence timer. Use the dedicated no-output fixture so the
    // spawn seam runs `node no-output-runtime.cjs` cleanly and the
    // child emits zero bytes until the watchdog kills it.
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        kickoffMode: 'self_inits', // arm immediately at spawn
        binary: NO_OUTPUT_FIXTURE,
        buildArgs: () => [NO_OUTPUT_FIXTURE],
      }),
      initLivenessSilenceMs: 60,
      // Big pre-protocol cap so the silence timer fires first.
      initPreProtocolCapMs: 60_000,
      sigtermToSigkillMs: 30,
    });
    await supervisor.start(baseStartParams);
    await waitFor(
      () =>
        events.some((e) => e.event === 'runtime_init_timeout' && e.timeout_reason === 'silence'),
      2_000,
    );
    const timeout = events.find((e) => e.event === 'runtime_init_timeout');
    expect(timeout && timeout.event === 'runtime_init_timeout').toBe(true);
    if (timeout && timeout.event === 'runtime_init_timeout') {
      expect(timeout.timeout_reason).toBe('silence');
      expect(timeout.retry_attempt).toBe(0);
    }
  });

  it('pre_protocol_cap tier fires when raw bytes arrive but no protocol line lands within the cap', async () => {
    // echo-runtime emits READY (raw bytes → silence timer refreshes)
    // but classifyInitProgress is `() => null` (default fake), so the
    // pre-protocol cap is the next timer to fire.
    const { supervisor, events } = make3cSupervisor({
      initLivenessSilenceMs: 60_000,
      initPreProtocolCapMs: 80,
      sigtermToSigkillMs: 30,
    });
    await supervisor.start(baseStartParams);
    await waitFor(
      () =>
        events.some(
          (e) => e.event === 'runtime_init_timeout' && e.timeout_reason === 'pre_protocol_cap',
        ),
      2_000,
    );
  });

  it('post_protocol tier fires after protocol_ready when session_ready never lands', async () => {
    // Driver classifyInitProgress returns 'protocol_ready' for any
    // line. After READY arrives, the post-protocol timer arms; we
    // never emit session_ready, so it fires.
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        classifyInitProgress: () => 'protocol_ready' as const,
      }),
      initPostProtocolMs: 80,
      sigtermToSigkillMs: 30,
    });
    await supervisor.start(baseStartParams);
    await waitFor(
      () =>
        events.some(
          (e) => e.event === 'runtime_init_timeout' && e.timeout_reason === 'post_protocol',
        ),
      2_000,
    );
  });

  it('classifyInitProgress=session_ready clears every init watchdog (no timeout fires)', async () => {
    // The fixture's READY line drives `classifyInitProgress`, so we
    // need the silence/pre-protocol windows to be wider than node's
    // cold-start latency — otherwise the silence timer fires before
    // READY arrives and the watchdog kills the child before
    // `session_ready` has any chance to clear it. Cindy
    // `HyC05fC-FEs94uoZ` B1: original 80ms window was racing the
    // child boot. 600ms is wide enough on macOS/Linux dev hosts and
    // CI while still keeping the "wait past every window" assertion
    // honest.
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        classifyInitProgress: () => 'session_ready' as const,
      }),
      initWatchdogMs: 600, // every tier
      sigtermToSigkillMs: 30,
    });
    await supervisor.start(baseStartParams);
    // Wait past every watchdog window; no timeout should ever fire.
    await wait(1_200);
    expect(events.find((e) => e.event === 'runtime_init_timeout')).toBeUndefined();
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('a parsed session_init event clears the watchdog', async () => {
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        parseLine: (line) => {
          // Treat any line containing "READY" as session_init for this test.
          if (line.includes('READY')) {
            return [{ kind: 'session_init' as const, sessionId: 'sess-1' }];
          }
          return [];
        },
      }),
      // Same cold-start-aware window rationale as the
      // classifyInitProgress=session_ready test above. session_init
      // arrives via parseLine on the READY line; the silence/pre-cap
      // windows must outlast node's cold start.
      initWatchdogMs: 600,
      sigtermToSigkillMs: 30,
    });
    await supervisor.start(baseStartParams);
    await wait(1_200);
    expect(events.find((e) => e.event === 'runtime_init_timeout')).toBeUndefined();
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('deferred-init: requires_first_message + no startup batch → no watchdog at spawn; deliver arms it', async () => {
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        kickoffMode: 'requires_first_message',
      }),
      initWatchdogMs: 80,
      sigtermToSigkillMs: 30,
    });
    await supervisor.start(baseStartParams);
    // Let every default-armed window pass; nothing should fire because
    // the watchdog isn't armed yet.
    await wait(250);
    expect(events.find((e) => e.event === 'runtime_init_timeout')).toBeUndefined();
    // Now deliver — that arms the watchdog. echo-runtime won't emit
    // session_init, so a silence/pre-cap timeout follows.
    await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [1],
        kind: 'message',
        text: 'hi',
        deliveries: [{ delivery_id: 1, seq: 1, presentation: testDeliveryPresentation('hi') }],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    await waitFor(() => events.some((e) => e.event === 'runtime_init_timeout'), 2_000);
  });

  it('deferred-init: requires_first_message + SPAWN batch arms watchdog only after the first stdin write', async () => {
    class DelayedWriteStub extends EventEmitter {
      readonly pid = 34_567;
      readonly stdout = new EventEmitter();
      readonly stderr = new EventEmitter();
      pendingWrite: ((err?: Error | null) => void) | null = null;
      readonly stdin = {
        write: (_line: string, cb?: (err?: Error | null) => void): boolean => {
          this.pendingWrite = cb ?? null;
          return true;
        },
        end: () => {},
        destroyed: false,
        once: () => {},
      };
      kill(_sig?: NodeJS.Signals | number): boolean {
        setImmediate(() => this.emit('exit', null, 'SIGTERM'));
        return true;
      }
    }
    const stub = new DelayedWriteStub();
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        kickoffMode: 'requires_first_message',
      }),
      spawnFn: () => stub as unknown as ChildProcess,
      initWatchdogMs: 60,
      initMaxRetries: 0,
      sigtermToSigkillMs: 30,
    });

    await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [901],
        kind: 'message',
        text: 'boot batch',
        deliveries: [
          { delivery_id: 901, seq: 901, presentation: testDeliveryPresentation('boot batch') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    await waitFor(() => stub.pendingWrite !== null, 1_000);

    // Wait past the watchdog window while the SPAWN-batch write is
    // still unresolved. A spawn-time watchdog would fire here; the
    // correct boundary for Claude-like runtimes is the first completed
    // stdin write, not process spawn.
    await wait(150);
    expect(events.find((e) => e.event === 'runtime_init_timeout')).toBeUndefined();

    stub.pendingWrite?.();
    await waitFor(() => events.some((e) => e.event === 'runtime_init_timeout'), 2_000);
  });

  it('override precedence: opts.initLivenessSilenceMs overrides driver.initLivenessSilenceMs', async () => {
    // Use the no-output fixture so silence fires from a true "no
    // bytes received" state (not "echo-runtime READY refreshed the
    // timer once, then went idle"). Cindy `HyC05fC-FEs94uoZ` B1
    // applies here too.
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        initLivenessSilenceMs: 60_000, // driver opinion would never fire in test budget
        binary: NO_OUTPUT_FIXTURE,
        buildArgs: () => [NO_OUTPUT_FIXTURE],
      }),
      initLivenessSilenceMs: 80, // supervisor override → wins
      initPreProtocolCapMs: 60_000,
      sigtermToSigkillMs: 30,
    });
    await supervisor.start(baseStartParams);
    await waitFor(
      () =>
        events.some((e) => e.event === 'runtime_init_timeout' && e.timeout_reason === 'silence'),
      2_000,
    );
  });

  it('override precedence: opts.initWatchdogMs (global) wins over per-tier override AND driver', async () => {
    // Same fixture swap as the per-tier-override test: no-output
    // fixture so silence fires deterministically from zero bytes.
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        initLivenessSilenceMs: 60_000,
        binary: NO_OUTPUT_FIXTURE,
        buildArgs: () => [NO_OUTPUT_FIXTURE],
      }),
      initWatchdogMs: 80, // global override
      initLivenessSilenceMs: 60_000, // per-tier — must be IGNORED
      sigtermToSigkillMs: 30,
    });
    await supervisor.start(baseStartParams);
    await waitFor(
      () =>
        events.some((e) => e.event === 'runtime_init_timeout' && e.timeout_reason === 'silence'),
      2_000,
    );
  });
});

describe('Supervisor 3c-b — retry / cooldown state machine', () => {
  it('first timeout with maxRetries=1 emits retrying=true + runtime_init_retrying for attempt 1', async () => {
    const { supervisor, events } = make3cSupervisor({
      initWatchdogMs: 80,
      sigtermToSigkillMs: 30,
      initMaxRetries: 1,
      // High threshold so cooldown does NOT engage during the retry window.
      initFailureCooldownThreshold: 10,
    });
    await supervisor.start(baseStartParams);
    await waitFor(
      () => events.some((e) => e.event === 'runtime_init_retrying' && e.retry_attempt === 1),
      2_000,
    );
    // The timeout that triggered the retry carried retry_attempt=0
    // (0-indexed, "the one that just failed") and retrying=true.
    const firstTimeout = events.find((e) => e.event === 'runtime_init_timeout');
    expect(firstTimeout && firstTimeout.event === 'runtime_init_timeout').toBe(true);
    if (firstTimeout && firstTimeout.event === 'runtime_init_timeout') {
      expect(firstTimeout.retry_attempt).toBe(0);
      expect(firstTimeout.retrying).toBe(true);
      expect(firstTimeout.max_retries).toBe(1);
      expect(firstTimeout.cooldown_until).toBeUndefined();
    }
    // Stop any pending retry process.
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('second timeout (attempt=1) emits retrying=false; no runtime_init_retrying follows', async () => {
    const { supervisor, events } = make3cSupervisor({
      initWatchdogMs: 80,
      sigtermToSigkillMs: 30,
      initMaxRetries: 1,
      initFailureCooldownThreshold: 10,
    });
    await supervisor.start(baseStartParams);
    await waitFor(
      () => events.filter((e) => e.event === 'runtime_init_timeout').length >= 2,
      4_000,
    );
    const timeouts = events.filter((e) => e.event === 'runtime_init_timeout');
    if (timeouts[1] && timeouts[1].event === 'runtime_init_timeout') {
      expect(timeouts[1].retry_attempt).toBe(1);
      expect(timeouts[1].retrying).toBe(false);
    }
    // Exactly ONE retrying event was emitted across the whole sequence.
    expect(events.filter((e) => e.event === 'runtime_init_retrying')).toHaveLength(1);
  });

  it('after threshold failures cooldown engages: runtime_init_timeout carries cooldown_until + runtime_init_cooldown event fires on exit', async () => {
    const { supervisor, events } = make3cSupervisor({
      initWatchdogMs: 60,
      sigtermToSigkillMs: 30,
      initMaxRetries: 0, // no retry — every failure is terminal
      initFailureCooldownThreshold: 1, // cooldown after the first failure
      initFailureCooldownMs: 60_000,
    });
    await supervisor.start(baseStartParams);
    await waitFor(() => events.some((e) => e.event === 'runtime_init_cooldown'), 3_000);
    const timeout = events.find((e) => e.event === 'runtime_init_timeout');
    if (timeout && timeout.event === 'runtime_init_timeout') {
      expect(typeof timeout.cooldown_until).toBe('number');
      expect((timeout.cooldown_until ?? 0) > Date.now()).toBe(true);
    }
    const cooldown = events.find((e) => e.event === 'runtime_init_cooldown');
    if (cooldown && cooldown.event === 'runtime_init_cooldown') {
      expect(cooldown.attempts_exhausted).toBeGreaterThanOrEqual(1);
      expect(cooldown.cooldown_until > Date.now()).toBe(true);
    }
  });

  it('start at attempt=0 while cooldown is active returns the cooldown result arm', async () => {
    const { supervisor } = make3cSupervisor({
      initWatchdogMs: 60,
      sigtermToSigkillMs: 30,
      initMaxRetries: 0,
      initFailureCooldownThreshold: 1,
      initFailureCooldownMs: 60_000,
    });
    await supervisor.start(baseStartParams);
    // Wait for the cooldown bucket to land.
    await waitFor(
      () =>
        supervisor.status({ conversation_id: 'Conversation0001', agent_id: 'a1' }).runtimes
          .length === 0 ||
        supervisor
          .status({ conversation_id: 'Conversation0001', agent_id: 'a1' })
          .runtimes.some((r) => r.phase === 'exited'),
      3_000,
    );
    // Give the exit handler a tick to mark cooldown.
    await wait(50);
    const second = await supervisor.start(baseStartParams);
    expect(second.started).toBe(false);
    if (!second.started) {
      expect(second.reason).toBe('init_cooldown');
      expect(second.cooldown_until > Date.now()).toBe(true);
    }
  });

  it('start at attempt=0 after cooldown expires returns the success arm again', async () => {
    const { supervisor } = make3cSupervisor({
      initWatchdogMs: 60,
      sigtermToSigkillMs: 30,
      initMaxRetries: 0,
      initFailureCooldownThreshold: 1,
      initFailureCooldownMs: 40, // very short cooldown
    });
    await supervisor.start(baseStartParams);
    await wait(300); // outlast cooldown
    const second = await supervisor.start(baseStartParams);
    assertStarted(second);
    expect(second.pid).toBeGreaterThan(0);
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('successful session_init clears prior failures before counting a new cooldown incident', async () => {
    const children: ControlledRuntime[] = [];
    const { supervisor, events } = make3cSupervisor({
      spawnFn: () => {
        const child = new ControlledRuntime();
        children.push(child);
        return child as unknown as ChildProcess;
      },
      driverOverride: (base) => ({
        ...base,
        parseLine: () => [{ kind: 'session_init', sessionId: 'healthy-session' }],
      }),
      initMaxRetries: 0,
      initFailureCooldownThreshold: 2,
      initFailureCooldownMs: 60_000,
    });
    try {
      assertStarted(await supervisor.start(baseStartParams));
      children[0]!.exit(1); // First failure really exists before the successful init.
      assertStarted(await supervisor.start(baseStartParams));
      children[1]!.stdout.emit('data', 'session-init\n');
      expect(events).toContainEqual(
        expect.objectContaining({
          event: 'runtime_parsed_event',
          parsed: { kind: 'session_init', sessionId: 'healthy-session' },
        }),
      );
      children[1]!.exit(1); // A post-init exit must not count as an init failure.
      assertStarted(await supervisor.start(baseStartParams));
      children[2]!.exit(1);
      expect(events.filter((e) => e.event === 'runtime_init_cooldown')).toEqual([]);
      assertStarted(await supervisor.start(baseStartParams));
      children[3]!.exit(1); // The second NEW failure must still engage cooldown.
      expect(events.filter((e) => e.event === 'runtime_init_cooldown')).toHaveLength(1);
      expect(await supervisor.start(baseStartParams)).toEqual(
        expect.objectContaining({
          started: false,
          reason: 'init_cooldown',
        }),
      );
      expect(children).toHaveLength(4);
    } finally {
      for (const child of children) child.dispose();
    }
  });
});

describe('Supervisor #task:1460 — pre-init exit folds into init-failure cooldown', () => {
  // Incident #1459: a long-lived runtime (claude) was SIGKILL'd by EDR
  // BEFORE `session_init`. The init watchdog never fired (the child died
  // faster than any tier), so the failure bypassed the retry/cooldown
  // counter entirely and backend re-drove the pending inbox row without
  // bound — 1105 spawns in 10 min. These tests pin the fold: a pre-init
  // abnormal exit of a long-lived runtime is accounted in the SAME
  // sticky counter, engages the same cooldown, and the cooldown then
  // refuses the next fresh start — WITHOUT the watchdog ever firing.
  //
  // The child crashes via `--sleep N --exit-code 1` (an abnormal exit
  // that isn't user-intentional); the predicate is exit-code/signal
  // agnostic (it keys off `driver.oneShot`), so a fast crash stands in
  // for the external SIGKILL. `initWatchdogMs` is left at its 5-min
  // default so the watchdog provably does NOT fire inside the test.

  it('a long-lived pre-init crash engages cooldown with NO runtime_init_timeout (not disguised as a watchdog timeout)', async () => {
    const { supervisor, events } = make3cSupervisor({
      driverArgs: ['--sleep', '30', '--exit-code', '1'],
      // parseLine never returns session_init → initObserved stays false.
      initFailureCooldownThreshold: 1, // cooldown on the first failure
      initFailureCooldownMs: 60_000,
    });
    await supervisor.start(baseStartParams);
    await waitFor(() => events.some((e) => e.event === 'runtime_init_cooldown'), 3_000);
    // The watchdog must NOT have fired — this is the pre-init path, not a
    // timeout re-labelled. (Core of Cindy's "don't disguise as timeout".)
    expect(events.some((e) => e.event === 'runtime_init_timeout')).toBe(false);
    const cooldown = events.find((e) => e.event === 'runtime_init_cooldown');
    if (cooldown && cooldown.event === 'runtime_init_cooldown') {
      expect(cooldown.attempts_exhausted).toBe(1);
      expect(cooldown.cooldown_until > Date.now()).toBe(true);
    }
  });

  it('cooldown from a pre-init crash refuses the next fresh start (the loop-stopping gate)', async () => {
    const { supervisor } = make3cSupervisor({
      driverArgs: ['--sleep', '30', '--exit-code', '1'],
      initFailureCooldownThreshold: 1,
      initFailureCooldownMs: 60_000,
    });
    await supervisor.start(baseStartParams);
    await waitFor(
      () =>
        supervisor
          .status({ conversation_id: 'Conversation0001', agent_id: 'a1' })
          .runtimes.every((r) => r.phase === 'exited') ||
        supervisor.status({ conversation_id: 'Conversation0001', agent_id: 'a1' }).runtimes
          .length === 0,
      3_000,
    );
    await wait(50); // let the exit handler engage cooldown
    const second = await supervisor.start(baseStartParams);
    expect(second.started).toBe(false);
    if (!second.started) {
      expect(second.reason).toBe('init_cooldown');
      expect(second.cooldown_until > Date.now()).toBe(true);
    }
  });

  it('a long-lived runtime that exits code 0 BEFORE session_init still counts (lifecycle, not exit code)', async () => {
    // Cindy point 1: for a long-lived driver a clean code-0 exit before
    // `session_init` is STILL an init failure — it never replied and
    // would re-loop on the pending inbox — so it must engage cooldown.
    const { supervisor, events } = make3cSupervisor({
      driverArgs: ['--sleep', '30', '--exit-code', '0'],
      initFailureCooldownThreshold: 1,
      initFailureCooldownMs: 60_000,
    });
    await supervisor.start(baseStartParams);
    await waitFor(() => events.some((e) => e.event === 'runtime_init_cooldown'), 3_000);
    expect(events.some((e) => e.event === 'runtime_init_timeout')).toBe(false);
  });

  it('a oneShot runtime that exits CLEAN code-0 pre-init is EXCLUDED — no cooldown, next start still succeeds', async () => {
    // A oneShot / cursor `--print` runtime's compatible completion is a
    // clean code-0 exit before `initObserved`; counting it would cooldown
    // a healthy runtime. The predicate excludes ONLY the clean case, so
    // even a threshold of 1 never engages here.
    const { supervisor, events } = make3cSupervisor({
      driverArgs: ['--sleep', '30', '--exit-code', '0'],
      driverOverride: (base) => ({ ...base, oneShot: true }),
      initFailureCooldownThreshold: 1,
      initFailureCooldownMs: 60_000,
    });
    await supervisor.start(baseStartParams);
    await waitFor(
      () =>
        supervisor
          .status({ conversation_id: 'Conversation0001', agent_id: 'a1' })
          .runtimes.every((r) => r.phase === 'exited') ||
        supervisor.status({ conversation_id: 'Conversation0001', agent_id: 'a1' }).runtimes
          .length === 0,
      3_000,
    );
    await wait(50);
    expect(events.some((e) => e.event === 'runtime_init_cooldown')).toBe(false);
    // Not gated — a oneShot's clean exit left the failure counter clean.
    const second = await supervisor.start(baseStartParams);
    expect(second.started).toBe(true);
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('a oneShot runtime that exits ABNORMALLY (non-zero) pre-init IS counted (oneShot excuses success, not a kill)', async () => {
    // Cindy P1: `oneShot` only explains "exits after a successful turn",
    // NOT "gets signal-killed / exits non-zero before init". EDR can
    // SIGKILL cursor too, and backend's generic pending re-drive would
    // loop it just the same. A non-zero pre-init exit therefore counts
    // and engages cooldown. (A SIGKILL delivers code=null / signal set,
    // which takes the same `code !== 0` disjunct as this non-zero case.)
    const { supervisor, events } = make3cSupervisor({
      driverArgs: ['--sleep', '30', '--exit-code', '1'],
      driverOverride: (base) => ({ ...base, oneShot: true }),
      initFailureCooldownThreshold: 1,
      initFailureCooldownMs: 60_000,
    });
    await supervisor.start(baseStartParams);
    await waitFor(() => events.some((e) => e.event === 'runtime_init_cooldown'), 3_000);
    expect(events.some((e) => e.event === 'runtime_init_timeout')).toBe(false);
    const cooldown = events.find((e) => e.event === 'runtime_init_cooldown');
    if (cooldown && cooldown.event === 'runtime_init_cooldown') {
      expect(cooldown.attempts_exhausted).toBe(1);
    }
    // And the cooldown gate now refuses the next fresh start.
    await wait(20);
    const second = await supervisor.start(baseStartParams);
    expect(second.started).toBe(false);
  });

  it('threshold=3: the sticky counter accumulates across 3 independent pre-init crashes; only the 3rd engages cooldown; the 4th fresh start is refused (#1459 acceptance)', async () => {
    // The real #1459 acceptance: prove the per-(conv, agent) counter is
    // STICKY across separate handles (each `start()` is a fresh
    // user-initiated attempt=0, purging the prior exited handle), that
    // the first two crashes do NOT cooldown, that the 3rd emits exactly
    // one `runtime_init_cooldown` with `attempts_exhausted === 3`, and
    // that the 4th start is gated. Guards off-by-one and any per-handle
    // counter reset that a threshold=1 test cannot catch.
    const { supervisor, events } = make3cSupervisor({
      driverArgs: ['--sleep', '25', '--exit-code', '1'],
      initFailureCooldownThreshold: 3,
      initFailureCooldownMs: 60_000,
    });

    // Crash 1 and 2: fresh start each time, wait for the terminal exit so
    // the counter is bumped before the next start. Neither should cooldown.
    for (let attempt = 1; attempt <= 2; attempt++) {
      const res = await supervisor.start(baseStartParams);
      expect(res.started).toBe(true); // not gated yet
      await waitFor(
        () => events.filter((e) => e.event === 'runtime_exit').length === attempt,
        3_000,
      );
      expect(events.some((e) => e.event === 'runtime_init_cooldown')).toBe(false);
    }

    // Crash 3: still a permitted fresh start, but this exit crosses the
    // threshold and engages cooldown.
    const third = await supervisor.start(baseStartParams);
    expect(third.started).toBe(true);
    await waitFor(() => events.some((e) => e.event === 'runtime_init_cooldown'), 3_000);
    const cooldowns = events.filter((e) => e.event === 'runtime_init_cooldown');
    expect(cooldowns).toHaveLength(1);
    const cd = cooldowns[0];
    if (cd && cd.event === 'runtime_init_cooldown') {
      expect(cd.attempts_exhausted).toBe(3);
      expect(cd.cooldown_until > Date.now()).toBe(true);
    }
    // The watchdog never fired across the whole sequence.
    expect(events.some((e) => e.event === 'runtime_init_timeout')).toBe(false);

    // 4th fresh start is refused by the cooldown gate.
    await wait(20);
    const fourth = await supervisor.start(baseStartParams);
    expect(fourth.started).toBe(false);
    if (!fourth.started) {
      expect(fourth.reason).toBe('init_cooldown');
      expect(fourth.cooldown_until > Date.now()).toBe(true);
    }
  });
});

describe('Supervisor 3c-b — round-2 regressions (Cindy `HyC05fC-FEs94uoZ` B2/B3)', () => {
  it('B2: hard stop during init clears the watchdog so no runtime_init_timeout fires and terminal reason is "stopped"', async () => {
    // CM (`conversation-manager.ts` lines 1020-1023) clears the init
    // watchdog AT DRAINING ENTRY so a user-initiated hard stop is not
    // mismarked as init timeout. Without the fix:
    //   1. user calls `stop({mode:'hard'})` while init is in flight
    //   2. handle.phase → draining, intentionalExit=true, but the
    //      watchdog timer is still ticking
    //   3. silence/pre-cap fires before the child actually exits
    //   4. failInitWatchdog sets initTimedOut=true
    //   5. onChildExit's reason mapping picks 'timeout' (precedence
    //      over 'stopped'), pollutes terminal state + retry/cooldown
    //      bucket + startup_delivery `failed` reason
    // With the fix: hardStopHandle clears every watchdog at draining
    // entry; the timer never fires; exit handler maps `intentional`
    // → terminal `stopped`.
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        binary: NO_OUTPUT_FIXTURE,
        buildArgs: () => [NO_OUTPUT_FIXTURE],
      }),
      // Tight watchdog so the timer would fire well within the
      // SIGTERM grace if it weren't cleared.
      initWatchdogMs: 80,
      sigtermToSigkillMs: 200,
      sigkillTimeoutMs: 600,
    });
    await supervisor.start(baseStartParams);
    // Trigger hard stop BEFORE the silence window elapses.
    const stopPromise = supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
    // Wait well past the original 80ms silence window to prove the
    // timer was actually cleared, not just delayed.
    await wait(300);
    const stopRes = await stopPromise;
    expect(stopRes).toMatchObject({ mode: 'hard', stopped: true });

    // NO runtime_init_timeout — proves clearInitWatchdog ran at
    // hardStopHandle entry.
    expect(events.find((e) => e.event === 'runtime_init_timeout')).toBeUndefined();
    // Terminal reason is 'stopped' (not 'timeout'), proving the
    // exit-handler reason mapping picked the user intent.
    const terminal = events.find(
      (e) => e.event === 'runtime_state' && e.state.terminalReason !== undefined,
    );
    expect(terminal).toBeDefined();
    if (terminal && terminal.event === 'runtime_state') {
      expect(terminal.state.terminalReason).toBe('stopped');
      expect(terminal.state.status).toBe('failed');
    }
    // Exit event marked intentional.
    const exitEvt = events.find((e) => e.event === 'runtime_exit');
    expect(exitEvt && exitEvt.event === 'runtime_exit' && exitEvt.intentional).toBe(true);
  });

  it('B3: init watchdog fire flips phase=draining so deliver during the kill window refuses draining', async () => {
    // Without B3 the watchdog kill window leaves `deliver()` open: the
    // timer fires, `failInitWatchdog` SIGTERMs the child but does NOT
    // touch phase, so deliver's `exited/startup_pending/draining` gate
    // doesn't catch it. Backend writes stdin to a dying process and
    // markDelivered's the inbox row to a runtime that never
    // processes it. With B3, failInitWatchdog sets phase='draining'
    // immediately, so the deliver gate's draining arm refuses.
    //
    // Test setup: no-output fixture with --ignore-sigterm so the
    // child stays alive in the SIGTERM grace window. initWatchdogMs
    // is widened to 1000ms so node has time to cold-boot the fixture
    // AND register the SIGTERM handler before the timer fires (a
    // signal that arrives mid-bootstrap falls back to default
    // action — terminate — defeating --ignore-sigterm). Long
    // SIGTERM/SIGKILL grace keeps the dying handle observable while
    // deliver races against the retry respawn.
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        binary: NO_OUTPUT_FIXTURE,
        buildArgs: () => [NO_OUTPUT_FIXTURE, '--ignore-sigterm'],
      }),
      initWatchdogMs: 1_000,
      initMaxRetries: 0,
      initFailureCooldownThreshold: 10,
      sigtermToSigkillMs: 800,
      sigkillTimeoutMs: 1_500,
    });
    await supervisor.start(baseStartParams);
    await waitFor(() => events.some((e) => e.event === 'runtime_init_timeout'), 3_000);
    // At this instant phase should be 'draining' (the child is alive
    // because --ignore-sigterm swallows SIGTERM; SIGKILL won't land
    // for another 800ms).
    const deliverRes = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [11],
        kind: 'message',
        text: 'during-kill-window',
        deliveries: [
          {
            delivery_id: 11,
            seq: 11,
            presentation: testDeliveryPresentation('during-kill-window'),
          },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(deliverRes.disposition).toBe('refused');
    if (deliverRes.disposition === 'refused') {
      // 'draining' is the deliver-gate signal that the runtime is being
      // torn down — backend MUST NOT markDelivered against this
      // outcome.
      expect(deliverRes.reason).toBe('draining');
    }
    // Sanity: the child eventually exits via SIGKILL and the
    // supervisor emits a terminal runtime_exit so the test cleanup
    // doesn't hang.
    await waitFor(() => events.some((e) => e.event === 'runtime_exit'), 4_000);
  });

  it('B3 + retry: phase=draining after watchdog fire does NOT block onChildExit retry-respawn (init_retrying still fires + new handle spawns)', async () => {
    // Cindy `eHnfQ2a2_l5x-YoQ` note 2: the B3 phase=draining flip
    // must not interfere with the retry branch in onChildExit. The
    // retry branch deletes the old handle and calls
    // `internalRespawnForRetry`; it does NOT gate on phase. This
    // test proves the full sequence:
    //   1. watchdog fires → phase=draining + initTimedOut + initRetryPlanned=true
    //   2. deliver during kill window → refused with reason='draining'
    //   3. child exits → onChildExit detects retry → emits
    //      runtime_init_retrying + deletes old handle + spawns new
    //   4. new handle has a different pid; status() reports a fresh
    //      ready runtime
    //
    // Same fixture configuration rationale as the B3 test above:
    // --ignore-sigterm + 1000ms watchdog so node boots and registers
    // the handler before the timer fires; 800ms SIGTERM grace so the
    // dying handle stays observable.
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        binary: NO_OUTPUT_FIXTURE,
        buildArgs: () => [NO_OUTPUT_FIXTURE, '--ignore-sigterm'],
      }),
      initWatchdogMs: 1_000,
      initMaxRetries: 1, // first failure → retry
      initFailureCooldownThreshold: 10, // don't engage cooldown
      sigtermToSigkillMs: 800,
      sigkillTimeoutMs: 1_500,
    });
    const startRes = await supervisor.start(baseStartParams);
    assertStarted(startRes);
    const originalPid = startRes.pid;
    await waitFor(() => events.some((e) => e.event === 'runtime_init_timeout'), 3_000);
    // Deliver inside the draining window must refuse.
    const midDeliver = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [42],
        kind: 'message',
        text: 'x',
        deliveries: [{ delivery_id: 42, seq: 42, presentation: testDeliveryPresentation('x') }],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(midDeliver.disposition).toBe('refused');
    if (midDeliver.disposition === 'refused') expect(midDeliver.reason).toBe('draining');
    // Wait for the retry event + the new spawn to happen (SIGKILL
    // will eventually take the SIGTERM-ignoring child down).
    await waitFor(
      () => events.some((e) => e.event === 'runtime_init_retrying' && e.retry_attempt === 1),
      4_000,
    );
    await waitFor(() => {
      const snap = supervisor.status({
        conversation_id: 'Conversation0001',
        agent_id: 'a1',
      });
      // The new handle must be live (different pid; phase ready
      // OR — if its own watchdog already fired — at least visibly
      // present and not the original exited handle).
      return (
        snap.runtimes.length === 1 &&
        snap.runtimes[0]!.pid !== null &&
        snap.runtimes[0]!.pid !== originalPid
      );
    }, 4_000);
    // The retry's new handle inherits no draining state from the old
    // one — confirms phase=draining set by failInitWatchdog did NOT
    // leak across the respawn boundary.
    const retryingEvent = events.find((e) => e.event === 'runtime_init_retrying');
    expect(
      retryingEvent && retryingEvent.event === 'runtime_init_retrying'
        ? retryingEvent.retry_attempt
        : null,
    ).toBe(1);
    // The terminal runtime_state for the FAILED attempt carries
    // terminalReason='timeout', mapped via resolveRun (Cindy: init
    // watchdog precedence over intentional/code in reason mapping).
    const timeoutTerminal = events.find(
      (e) => e.event === 'runtime_state' && e.state.terminalReason === 'timeout',
    );
    expect(timeoutTerminal).toBeDefined();
    // Cleanup: tear down the retry's runtime.
    await supervisor.stopAll({});
  });
});

describe('Supervisor 3c-b — lifecycle lock', () => {
  it('concurrent start+start on the same key returns the idempotent success arm (lock serializes)', async () => {
    const { supervisor } = make3cSupervisor();
    const [a, b] = await Promise.all([
      supervisor.start(baseStartParams),
      supervisor.start(baseStartParams),
    ]);
    assertStarted(a);
    assertStarted(b);
    expect(a.pid).toBe(b.pid);
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('concurrent start+stop on the same key serializes (stop sees the live handle)', async () => {
    const { supervisor } = make3cSupervisor();
    const startP = supervisor.start(baseStartParams);
    const stopP = supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
    const [s, st] = await Promise.all([startP, stopP]);
    assertStarted(s);
    expect(st).toMatchObject({ mode: 'hard', stopped: true });
  });

  it('stopAll fans out per-key in parallel (two different keys both stop)', async () => {
    const signals: Array<[ControlledRuntime, NodeJS.Signals | number | undefined]> = [];
    class WaitingRuntime extends ControlledRuntime {
      override kill(signal?: NodeJS.Signals | number): boolean {
        signals.push([this, signal]);
        return true;
      }
    }
    const children: ControlledRuntime[] = [];
    const { supervisor } = make3cSupervisor({
      spawnFn: () => {
        const child = new WaitingRuntime();
        children.push(child);
        return child as unknown as ChildProcess;
      },
    });
    try {
      await supervisor.start(baseStartParams);
      await supervisor.start({
        ...baseStartParams,
        conversation_id: 'Conversation0002',
        options: testStartOptions({ conversationId: 'Conversation0002', agentId: 'a1' }),
      });
      const stopping = supervisor.stopAll({});
      await waitFor(() => signals.length >= 2);
      // Neither child has exited: serial shutdown would escalate the first
      // one instead of immediately signalling the second scope.
      expect(signals).toEqual(children.map((child) => [child, 'SIGTERM']));
      for (const child of children) child.exit();
      expect(await stopping).toEqual({ stopped_count: 2 });
      expect(supervisor.status({}).runtimes.every((r) => r.phase === 'exited')).toBe(true);
    } finally {
      for (const child of children) child.dispose();
    }
  });
});

describe('Supervisor 3c-b — compaction watchdog', () => {
  it('compaction_started + no compaction_finished within compactionStaleMs → runtime_compaction_stale + run-state stacks compactionStale:true (keeps compacting:true)', async () => {
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        parseLine: (line) => {
          if (line.includes('COMPACT_START')) return [{ kind: 'compaction_started' as const }];
          return [];
        },
      }),
      driverArgs: ['--emit-line', 'COMPACT_START'],
      compactionStaleMs: 80,
      sigtermToSigkillMs: 30,
    });
    await supervisor.start(baseStartParams);
    await waitFor(() => events.some((e) => e.event === 'runtime_compaction_stale'), 2_000);
    const stale = events.find((e) => e.event === 'runtime_compaction_stale');
    if (stale && stale.event === 'runtime_compaction_stale') {
      expect(stale.elapsed_ms).toBeGreaterThanOrEqual(80);
    }
    // The most recent runtime_state carries both compacting:true and
    // compactionStale:true. (CM contract — UI shows "still compacting,
    // stale" not "no longer compacting".)
    const stateEmits = events.filter((e) => e.event === 'runtime_state');
    const last = stateEmits[stateEmits.length - 1];
    if (last && last.event === 'runtime_state') {
      expect(last.state.compacting).toBe(true);
      expect(last.state.compactionStale).toBe(true);
    }
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('compaction_stale does NOT kill the child (status() still shows the live pid)', async () => {
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        parseLine: (line) => {
          if (line.includes('COMPACT_START')) return [{ kind: 'compaction_started' as const }];
          return [];
        },
      }),
      driverArgs: ['--emit-line', 'COMPACT_START'],
      compactionStaleMs: 60,
      sigtermToSigkillMs: 30,
    });
    const res = await supervisor.start(baseStartParams);
    assertStarted(res);
    await waitFor(() => events.some((e) => e.event === 'runtime_compaction_stale'), 2_000);
    const snap = supervisor.status({ conversation_id: 'Conversation0001', agent_id: 'a1' });
    expect(snap.runtimes[0]?.pid).toBe(res.pid);
    expect(snap.runtimes[0]?.phase).toBe('ready');
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('compaction_finished cancels the watchdog (no stale event fires)', async () => {
    const { supervisor, events } = make3cSupervisor({
      driverOverride: (base) => ({
        ...base,
        parseLine: (line) => {
          if (line.includes('COMPACT_START')) return [{ kind: 'compaction_started' as const }];
          if (line.includes('COMPACT_END')) return [{ kind: 'compaction_finished' as const }];
          return [];
        },
      }),
      driverArgs: ['--emit-line', 'COMPACT_START', '--emit-line', 'COMPACT_END'],
      compactionStaleMs: 200,
      sigtermToSigkillMs: 30,
    });
    await supervisor.start(baseStartParams);
    // Wait longer than compactionStaleMs; the finished event should
    // have cleared the watchdog so no stale fires.
    await wait(400);
    expect(events.find((e) => e.event === 'runtime_compaction_stale')).toBeUndefined();
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });
});

describe('Supervisor 3c-b — defaults exported for the bridge', () => {
  it('exports CM-aligned defaults so backend can render UI hints without re-deriving the values', () => {
    expect(DEFAULT_INIT_LIVENESS_SILENCE_MS).toBe(30 * 1000);
    expect(DEFAULT_INIT_PRE_PROTOCOL_CAP_MS).toBe(5 * 60 * 1000);
    expect(DEFAULT_INIT_POST_PROTOCOL_MS).toBe(30 * 1000);
    expect(DEFAULT_COMPACTION_STALE_MS).toBe(5 * 60 * 1000);
  });
});

// ── Option A v2 arms (Cindy `UMGu36zoTDVuKWXi`) ────────────────────
//
// Daemon now owns encoding via its in-process driver. Three new arms
// fall out of that:
//
//   1. `startup_pending` (deliver, readyToEncode() === false) —
//      → recoverable arm; backend keeps DB row pending and will
//      redeliver on `runtime_ready_for_drain`.
//   2. `driver_refused` (deliver WRITE arm) — readyToEncode() returned
//      true but encodeStdinMessage() returned null → driver bug; the
//      WRITE arm admits SYNC then fails ASYNC on runtime_delivery_failed
//      (Cindy `0G5uJBydO_4qt_2r`).
//   3. Codex stateful gate — readyToEncode() flips false → true on
//      `session_init` (production codex flips on `thread/started`).
//      ready_for_drain and a deferred SPAWN-batch consume both
//      latch off that transition.
//
// These tests build a "stateful" fake driver that mirrors codex's
// shape: a mutable `threadId`-like flag the test toggles in lockstep
// with parsed events, gated by `readyToEncode()`. Encoding remains
// the stream-json shape so the echo-stdin fixture's assertions stay
// readable.

/** Build a codex-shaped fake driver whose `readyToEncode()` is gated
 *  by an external mutable flag. session_init parses to a single event
 *  that flips the flag, exactly like codex's `thread/started` does in
 *  production. `kickoffMode` defaults to `'self_inits'` so this isn't
 *  confused with the B2 deferred-consume path (which gates on
 *  `requires_first_message + !initObserved`). */
function makeStatefulCodexLikeDriver(): {
  driver: Driver;
  flipReady: () => void;
} {
  const state = { ready: false };
  const driver: Driver = {
    kind: 'codex',
    binary: FIXTURE,
    kickoffMode: 'self_inits',
    releaseProfile: 'native-steer',
    admissionPolicy: 'write-success',
    oneShot: false,
    supportsNativeStandingPrompt: true,
    initLivenessSilenceMs: 30_000,
    initPreProtocolCapMs: 5 * 60_000,
    initPostProtocolMs: 60_000,
    buildArgs: () => [FIXTURE, '--echo-stdin'],
    applyEnv: (env) => ({ ...env }),
    readyToEncode: () => state.ready,
    encodeStdinMessage: (text: string) => encodeStreamJsonUser(text),
    encodeInterruptControl: () => null,
    parseLine: () => [],
    classifyInitProgress: () => null,
  };
  return {
    driver,
    flipReady: () => {
      state.ready = true;
    },
  };
}

describe('Supervisor 3b — Option A v2 deliver arms (Cindy `UMGu36zoTDVuKWXi`)', () => {
  it('deliver on a driver whose readyToEncode()=false refuses startup_pending (recoverable)', async () => {
    // Codex pre-`thread/started`: backend will keep the DB row pending
    // and redeliver on the next `runtime_ready_for_drain` once
    // readyToEncode() flips true.
    const { driver } = makeStatefulCodexLikeDriver();
    const { supervisor, events } = makeSupervisorWithDriver(driver);
    await supervisor.start(baseStartParams);
    await waitFor(() =>
      events.some((e) => e.event === 'runtime_output' && e.chunk.includes('READY')),
    );

    // ready_for_drain MUST NOT have fired yet — readyToEncode() is
    // false so the gate is held closed.
    expect(events.some((e) => e.event === 'runtime_ready_for_drain')).toBe(false);

    const res = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [201, 202],
        kind: 'message',
        text: 'hi from backend',
        deliveries: [
          { delivery_id: 201, seq: 201, presentation: testDeliveryPresentation('hi from backend') },
          { delivery_id: 202, seq: 202, presentation: testDeliveryPresentation('hi from backend') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(res).toEqual({
      disposition: 'refused',
      reason: 'startup_pending',
    });
    // No stdin write should have surfaced — fixture would echo
    // STDIN:{...} on every received line.
    const echoed = events.some((e) => e.event === 'runtime_output' && e.chunk.includes('STDIN:'));
    expect(echoed).toBe(false);
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('deliver post-ready: encodeStdinMessage returns null → admitted then async runtime_delivery_failed reason:driver_refused (fail loud)', async () => {
    // readyToEncode() returns true but the driver's encoder produces
    // null. This is the driver-bug arm — Cindy `0G5uJBydO_4qt_2r`:
    // fail loudly rather than silently dropping bytes. Under
    // runtime_deliver the WRITE arm admits SYNC (driverReadyToEncode
    // defaults/returns true) and the failure surfaces ASYNC on the
    // unified runtime_delivery_failed family.
    const driver: Driver = {
      kind: 'codex',
      binary: FIXTURE,
      kickoffMode: 'self_inits',
      releaseProfile: 'native-steer',
      admissionPolicy: 'write-success',
      oneShot: false,
      supportsNativeStandingPrompt: true,
      initLivenessSilenceMs: 30_000,
      initPreProtocolCapMs: 5 * 60_000,
      initPostProtocolMs: 60_000,
      buildArgs: () => [FIXTURE, '--echo-stdin'],
      applyEnv: (env) => ({ ...env }),
      readyToEncode: () => true,
      encodeStdinMessage: () => null, // driver bug
      encodeInterruptControl: () => null,
      parseLine: () => [],
      classifyInitProgress: () => null,
    };
    const { supervisor, events } = makeSupervisorWithDriver(driver);
    await supervisor.start(baseStartParams);
    await waitFor(() =>
      events.some((e) => e.event === 'runtime_output' && e.chunk.includes('READY')),
    );
    // ready_for_drain fires because readyToEncode()=true and no
    // startup-batch gate.
    await waitFor(() => events.some((e) => e.event === 'runtime_ready_for_drain'));

    const res = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [301],
        kind: 'message',
        text: 'bytes',
        deliveries: [
          { delivery_id: 301, seq: 301, presentation: testDeliveryPresentation('bytes') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(res.disposition).toBe('admitted');
    await waitFor(() =>
      events.some(
        (e) =>
          e.event === 'runtime_delivery_failed' &&
          e.delivery_ids.includes(301) &&
          e.reason === 'driver_refused',
      ),
    );
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });
});

describe('Supervisor 3b — Option A v2 ready_for_drain gate by readyToEncode (Cindy `UMGu36zoTDVuKWXi`)', () => {
  it('ready_for_drain is held until readyToEncode() flips true (codex stateful gate)', async () => {
    // No startup batch; gate is purely driver.readyToEncode().
    // Pre-flip: ready_for_drain MUST NOT fire even after start
    // settles. Post-flip (via a parsed event that the test injects):
    // ready_for_drain fires exactly once.
    const { driver, flipReady } = makeStatefulCodexLikeDriver();
    // Replace parseLine so an injected stdout line triggers flipReady
    // synchronously — this mirrors how codex flips its threadId on
    // `thread/started`.
    driver.parseLine = (line: string) => {
      if (line.includes('THREAD_STARTED')) {
        flipReady();
        return [{ kind: 'session_init', sessionId: 'sess-codex-1' }];
      }
      return [];
    };
    const { supervisor, events } = makeSupervisorWithDriver(driver);
    await supervisor.start(baseStartParams);
    await waitFor(() =>
      events.some((e) => e.event === 'runtime_output' && e.chunk.includes('READY')),
    );
    // Pre-flip: gate stays closed.
    await wait(40);
    expect(events.some((e) => e.event === 'runtime_ready_for_drain')).toBe(false);

    // Flip via stdin echo trick: feed a line through the fixture's
    // echo path so the supervisor sees a stdout line that parseLine
    // converts to session_init + flipReady.
    // The simplest way to inject a line is to deliver to the supervisor;
    // but that would refuse startup_pending. Instead use the existing
    // echo-stdin fixture and write a phantom line via stdin: the
    // supervisor only sees its OWN stdout, and the only way to push
    // a synthetic stdout line is the fixture's behavior. So we use a
    // driver-arg fixture instead: --emit-line THREAD_STARTED makes
    // the fixture write that line on stdout shortly after READY.
    // (Switch driverArgs by rebuilding the driver inline.)
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });

    // ── second supervisor with the same driver but the fixture
    //    configured to actually emit the THREAD_STARTED line.
    const { driver: driver2, flipReady: flip2 } = makeStatefulCodexLikeDriver();
    driver2.buildArgs = () => [FIXTURE, '--emit-line', 'THREAD_STARTED'];
    driver2.parseLine = (line: string) => {
      if (line.includes('THREAD_STARTED')) {
        flip2();
        return [{ kind: 'session_init', sessionId: 'sess-codex-2' }];
      }
      return [];
    };
    const second = makeSupervisorWithDriver(driver2);
    await second.supervisor.start(baseStartParams);
    await waitFor(() => second.events.some((e) => e.event === 'runtime_ready_for_drain'), 2_000);
    // Exactly one ready_for_drain after the flip.
    expect(second.events.filter((e) => e.event === 'runtime_ready_for_drain').length).toBe(1);
    await second.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });
});

describe('Supervisor 3b — Option A v2 deferred SPAWN batch on stateful driver (Cindy `UMGu36zoTDVuKWXi`)', () => {
  it('SPAWN-batch encode is deferred until readyToEncode() flips true (codex pre-thread/started)', async () => {
    // Codex case: deliver SPAWN arm with a batch while the driver's
    // readyToEncode() is still false. The supervisor MUST defer the
    // encode (no stdin write yet, no consumed event, no ready_for_drain).
    // Once a parsed event flips readyToEncode() true,
    // `tryConsumeStartupDelivery` re-enters and the batch settles:
    // consumed → ready_for_drain.
    const { driver, flipReady } = makeStatefulCodexLikeDriver();
    driver.buildArgs = () => [FIXTURE, '--echo-stdin', '--emit-line', 'THREAD_STARTED'];
    driver.parseLine = (line: string) => {
      if (line.includes('THREAD_STARTED')) {
        flipReady();
        return [{ kind: 'session_init', sessionId: 'sess-codex-3' }];
      }
      return [];
    };
    const { supervisor, events } = makeSupervisorWithDriver(driver);
    await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [701, 702],
        kind: 'message',
        text: 'boot codex',
        deliveries: [
          { delivery_id: 701, seq: 701, presentation: testDeliveryPresentation('boot codex') },
          { delivery_id: 702, seq: 702, presentation: testDeliveryPresentation('boot codex') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );

    // Eventually the fixture emits THREAD_STARTED → parseLine flips
    // ready → tryConsumeStartupDelivery encodes + writes → consumed +
    // ready_for_drain fire in that order.
    await waitFor(() => events.some((e) => e.event === 'runtime_ready_for_drain'), 2_000);
    const consumed = events.find((e) => e.event === 'runtime_delivery_consumed');
    const idxConsumed = events.findIndex((e) => e.event === 'runtime_delivery_consumed');
    const idxReady = events.findIndex((e) => e.event === 'runtime_ready_for_drain');
    expect(consumed).toBeDefined();
    if (consumed && consumed.event === 'runtime_delivery_consumed') {
      expect(consumed.delivery_ids).toEqual([701, 702]);
    }
    expect(idxConsumed).toBeGreaterThanOrEqual(0);
    expect(idxReady).toBeGreaterThan(idxConsumed);
    // Exactly one ready_for_drain across the deferred path.
    expect(events.filter((e) => e.event === 'runtime_ready_for_drain').length).toBe(1);
    // The stdin write actually happened (echo confirms).
    await waitFor(
      () =>
        events.some(
          (e) => e.event === 'runtime_output' && e.chunk.includes('STDIN:{"type":"user"'),
        ),
      2_000,
    );

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });
});

describe('Supervisor — runtime_deliver metadata validation', () => {
  it('hard-rejects a deliver whose deliveries[] id-set does not match delivery_ids', async () => {
    const { supervisor } = makeSupervisor();
    try {
      await supervisor.start(baseStartParams);
      await expect(
        supervisor.deliver(
          {
            runtime_generation: 1,
            conversation_id: 'Conversation0001',
            target: { conversation_id: 'Conversation0001', workspace: null },
            agent_id: 'a1',
            delivery_ids: [1, 2],
            kind: 'message',
            text: 'mismatched batch',
            deliveries: [
              {
                delivery_id: 1,
                seq: 1,
                presentation: testDeliveryPresentation('delivery-text'),
              },
            ],
            spawn_options: baseStartParams.options,
          },
          { owner: 'test-owner' },
        ),
      ).rejects.toThrow(/delivery metadata mismatch/);
    } finally {
      await supervisor
        .stop({
          runtime_scope: {
            kind: 'conversation',
            conversation_id: 'Conversation0001',
            agent_id: 'a1',
          },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => undefined);
    }
  });
});

describe('Supervisor Stage 2 — runtime_deliver (#task:1417)', () => {
  it('SPAWN arm: no live handle → spawns with the batch as startup delivery, returns spawning, and settles onto the UNIFIED runtime_delivery_consumed', async () => {
    const { supervisor, events } = makeSupervisor({
      driverArgs: ['--echo-stdin'],
    });
    const res = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [7, 8],
        kind: 'message',
        text: 'boot via deliver',
        deliveries: [
          { delivery_id: 7, seq: 7, presentation: testDeliveryPresentation('boot via deliver') },
          { delivery_id: 8, seq: 8, presentation: testDeliveryPresentation('boot via deliver') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(res.disposition).toBe('spawning');
    if (res.disposition !== 'spawning') throw new Error('unreachable');
    expect(typeof res.pid).toBe('number');
    expect(typeof res.started_at).toBe('number');

    // The daemon wrote the batch to stdin (fixture echoes it back).
    await waitFor(() =>
      events.some((e) => e.event === 'runtime_output' && e.chunk.includes('STDIN:{"type":"user"')),
    );
    // Custody is ack'd on the UNIFIED family, keyed by delivery_ids.
    await waitFor(() =>
      events.some(
        (e) =>
          e.event === 'runtime_delivery_consumed' &&
          e.delivery_ids.length === 2 &&
          e.delivery_ids[0] === 7 &&
          e.delivery_ids[1] === 8,
      ),
    );

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('response-loss ownership union: expanded, subset, and deferred-overlap retries release each id exactly once in FIFO order', async () => {
    class ControlledWriteChild extends EventEmitter {
      readonly pid = 42_042;
      readonly stdout = new EventEmitter();
      readonly stderr = new EventEmitter();
      readonly writes: string[] = [];
      readonly writeCallbacks: Array<(err?: Error | null) => void> = [];
      readonly stdin = {
        write: (line: string, cb?: (err?: Error | null) => void): boolean => {
          this.writes.push(line);
          if (cb) this.writeCallbacks.push(cb);
          return true;
        },
        end: () => {},
        destroyed: false,
        once: () => {},
      };
      kill(_sig?: NodeJS.Signals | number): boolean {
        setImmediate(() => this.emit('exit', 0, null));
        return true;
      }
    }
    const child = new ControlledWriteChild();
    const { supervisor, events } = makeSupervisor({
      spawnFn: () => child as unknown as ChildProcess,
    });
    try {
      await supervisor.start(baseStartParams);
      await waitFor(
        () =>
          supervisor.status({
            conversation_id: 'Conversation0001',
            agent_id: 'a1',
          }).runtimes[0]?.phase === 'ready',
      );
      const batch = (ids: number[]) => {
        const presentations = ids.map((id) => ({
          body: `msg-${id}`,
          instruction: 'other' as const,
        }));
        return {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          delivery_ids: ids,
          kind: 'message' as const,
          text: composeRuntimeDeliveryPresentation(presentations, 'silverhand message send'),
          deliveries: ids.map((id, index) => ({
            delivery_id: id,
            seq: id,
            presentation: presentations[index]!,
          })),
          spawn_options: baseStartParams.options,
        };
      };
      const writtenText = (index: number): string =>
        (
          JSON.parse(child.writes[index]!) as {
            message: { content: string };
          }
        ).message.content;

      // [42] is already admitted and writing when its response disappears.
      expect(await supervisor.deliver(batch([42]), { owner: 'conn-1' })).toMatchObject({
        disposition: 'admitted',
      });
      expect(child.writes).toHaveLength(1);

      // Backend-style retry expands to [42,43]. 42 remains in the release
      // slot; only novel 43 enters the FIFO, but the disposition says daemon
      // custody covers the whole submitted batch.
      expect(await supervisor.deliver(batch([42, 43]), { owner: 'conn-2' })).toMatchObject({
        disposition: 'deferred',
      });
      expect(child.writes).toHaveLength(1);

      child.writeCallbacks.shift()?.();
      await waitForDeliveryConsumed(events, 42);
      await waitFor(() => child.writes.length === 2);
      expect(writtenText(0)).toBe(batch([42]).text);
      expect(writtenText(1)).toBe(batch([43]).text);
      expect(writtenText(1).match(/You have one message waiting/g)).toHaveLength(1);
      expect(writtenText(1).match(/Respond as appropriate\./g)).toHaveLength(1);
      child.writeCallbacks.shift()?.();
      await waitForDeliveryConsumed(events, 43);
      expect(
        events.filter(
          (event) => event.event === 'runtime_delivery_consumed' && event.delivery_ids.includes(42),
        ),
      ).toHaveLength(1);
      expect(
        events.filter(
          (event) => event.event === 'runtime_delivery_consumed' && event.delivery_ids.includes(43),
        ),
      ).toHaveLength(1);

      // A retry that is a strict SUBSET of the release slot takes no novel
      // custody and therefore cannot create another write.
      expect(await supervisor.deliver(batch([50, 51]), { owner: 'conn-2' })).toMatchObject({
        disposition: 'admitted',
      });
      expect(child.writes).toHaveLength(3);
      expect(await supervisor.deliver(batch([50]), { owner: 'conn-3' })).toMatchObject({
        disposition: 'admitted',
      });
      expect(child.writes).toHaveLength(3);
      child.writeCallbacks.shift()?.();
      await waitForDeliveryConsumed(events, 50);
      expect(
        events.filter(
          (event) =>
            event.event === 'runtime_delivery_consumed' &&
            (event.delivery_ids.includes(50) || event.delivery_ids.includes(51)),
        ),
      ).toHaveLength(1);

      // Cross-deferred partial overlap: 60 holds the release slot, 61 is
      // already FIFO-owned, and retry [61,62] appends only novel 62. Release
      // order stays 60 → 61 → 62 and every id is consumed once.
      expect(await supervisor.deliver(batch([60]), { owner: 'conn-3' })).toMatchObject({
        disposition: 'admitted',
      });
      expect(await supervisor.deliver(batch([61]), { owner: 'conn-3' })).toMatchObject({
        disposition: 'deferred',
      });
      expect(await supervisor.deliver(batch([61, 62]), { owner: 'conn-4' })).toMatchObject({
        disposition: 'deferred',
      });
      child.writeCallbacks.shift()?.();
      await waitForDeliveryConsumed(events, 60);
      await waitFor(() => child.writes.length === 5);
      child.writeCallbacks.shift()?.();
      await waitForDeliveryConsumed(events, 61);
      await waitFor(() => child.writes.length === 6);
      child.writeCallbacks.shift()?.();
      await waitForDeliveryConsumed(events, 62);
      expect(
        child.writes
          .slice(3)
          .map((write) => [60, 61, 62].find((id) => write.includes(`msg-${id}`))),
      ).toEqual([60, 61, 62]);
      for (const id of [60, 61, 62]) {
        expect(
          events.filter(
            (event) =>
              event.event === 'runtime_delivery_consumed' && event.delivery_ids.includes(id),
          ),
        ).toHaveLength(1);
      }
    } finally {
      await supervisor
        .stop({
          runtime_scope: {
            kind: 'conversation',
            conversation_id: 'Conversation0001',
            agent_id: 'a1',
          },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => undefined);
    }
  });

  it('REFUSE arm: startup batch still in flight → refused startup_pending, no ready_for_drain (step-0 mutex)', async () => {
    // Same pinned-gate technique as the delivery step-0 test: a stdin stub
    // whose write callback never fires keeps consumeStartupDelivery
    // hung, so `startupDelivery !== null` stays true.
    class HungStdinStub extends EventEmitter {
      readonly pid = 22_345;
      readonly stdout = new EventEmitter();
      readonly stderr = new EventEmitter();
      readonly stdin = {
        write: (_line: string, _cb?: (err?: Error | null) => void): boolean => true,
        end: () => {},
        destroyed: false,
        once: () => {},
      };
      kill(_sig?: NodeJS.Signals | number): boolean {
        setImmediate(() => this.emit('exit', 0, null));
        return true;
      }
    }
    const stub = new HungStdinStub();
    const spawnFn = (_b: string, _a: ReadonlyArray<string>, _o: SpawnOptions): ChildProcess =>
      stub as unknown as ChildProcess;
    const { supervisor, events } = makeSupervisor({ spawnFn });
    // Put a batch in flight via the deliver SPAWN arm; the hung stdin
    // stub keeps consumeStartupDelivery from settling, so
    // `startupDelivery !== null` stays true.
    await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [1, 2],
        kind: 'message',
        text: 'boot',
        deliveries: [
          { delivery_id: 1, seq: 1, presentation: testDeliveryPresentation('boot') },
          { delivery_id: 2, seq: 2, presentation: testDeliveryPresentation('boot') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    const res = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [99],
        kind: 'message',
        text: 'race',
        deliveries: [{ delivery_id: 99, seq: 99, presentation: testDeliveryPresentation('race') }],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(res).toEqual({ disposition: 'refused', reason: 'startup_pending' });
    expect(events.some((e) => e.event === 'runtime_ready_for_drain')).toBe(false);

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('REFUSE arm: handle stuck draining (SIGKILL-timeout safety net) → refused draining', async () => {
    // `deliver()` holds the per-(conv,agent) lifecycle lock, and every
    // normal stop path holds it until the child exits — so a deliver
    // racing a hard stop simply serializes behind it and respawns. The
    // ONE way a live handle sits in `draining` with the lock free is the
    // SIGKILL-timeout safety net: the child ignores both signals, so
    // `hardStopHandle` resolves stop() (stopped:false) and releases the
    // lock while `phase` is still `draining`. A stub whose `kill()`
    // never emits `exit` reproduces that window deterministically.
    class NoExitStub extends EventEmitter {
      readonly pid = 32_345;
      readonly stdout = new EventEmitter();
      readonly stderr = new EventEmitter();
      readonly stdin = {
        write: (_line: string, cb?: (err?: Error | null) => void): boolean => {
          cb?.();
          return true;
        },
        end: () => {},
        destroyed: false,
        once: () => {},
      };
      kill(_sig?: NodeJS.Signals | number): boolean {
        return true; // never emits 'exit' → forces the safety-net path
      }
    }
    const stub = new NoExitStub();
    const spawnFn = (_b: string, _a: ReadonlyArray<string>, _o: SpawnOptions): ChildProcess =>
      stub as unknown as ChildProcess;
    const { supervisor } = makeSupervisor({
      spawnFn,
      sigtermToSigkillMs: 10,
      sigkillTimeoutMs: 20,
    });
    await supervisor.start(baseStartParams);
    // Let the optimistic setImmediate flip the handle to `ready`.
    await wait(5);
    const stopRes = await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
    // Safety net resolved without a real exit — handle stays draining.
    expect(stopRes).toEqual({ mode: 'hard', stopped: false, exit_code: null });
    const res = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [5],
        kind: 'message',
        text: 'during drain',
        deliveries: [
          { delivery_id: 5, seq: 5, presentation: testDeliveryPresentation('during drain') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(res).toEqual({ disposition: 'refused', reason: 'draining' });
  });

  it('REFUSE arm: live oneShot handle → refused startup_pending SYNC, never admitted→driver_refused (#task:1417 Stage 2b, Cindy review #2)', async () => {
    // A live oneShot (cursor) can never take a stdin delivery — it drains
    // its inbox only at spawn time. Before the fix, a ready oneShot fell
    // through to the WRITE arm (its `encodeStdinMessage` returns null and it
    // has no `readyToEncode`, so `driverReadyToEncode` defaults true),
    // returning `admitted` then emitting an async `driver_refused` — a code
    // reserved for a genuine driver programming error. The fix makes the
    // daemon refuse SYNC on `handle.driver.oneShot`, so the backend rolls
    // the rows back to pending and the oneShot drains on its next
    // exit→respawn.
    const oneShotDriver: Driver = {
      kind: 'cursor',
      binary: FIXTURE,
      kickoffMode: 'self_inits',
      releaseProfile: 'one-shot',
      admissionPolicy: 'write-success',
      oneShot: true,
      supportsNativeStandingPrompt: false,
      initLivenessSilenceMs: 30_000,
      initPreProtocolCapMs: 5 * 60_000,
      initPostProtocolMs: 60_000,
      buildArgs: () => [FIXTURE, '--echo-stdin'],
      applyEnv: (env) => ({ ...env }),
      encodeStdinMessage: () => null,
      encodeInterruptControl: () => null,
      parseLine: () => [],
      classifyInitProgress: () => null,
    };
    const { supervisor, events } = makeSupervisorWithDriver(oneShotDriver);
    // Bare start (no batch) → the handle comes up live and reaches ready.
    await supervisor.start(baseStartParams);
    await waitFor(() =>
      events.some((e) => e.event === 'runtime_output' && e.chunk.includes('READY')),
    );

    const res = await supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [77],
        kind: 'message',
        text: 'to a live oneShot',
        deliveries: [
          { delivery_id: 77, seq: 77, presentation: testDeliveryPresentation('to a live oneShot') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );

    expect(res).toEqual({ disposition: 'refused', reason: 'startup_pending' });
    // SYNC refuse — no admit, no delivery ack, and crucially no async
    // driver_refused failure emitted for this delivery.
    await wait(20);
    expect(
      events.some((e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(77)),
    ).toBe(false);
    expect(
      events.some(
        (e) =>
          e.event === 'runtime_delivery_failed' &&
          e.delivery_ids.includes(77) &&
          e.reason === 'driver_refused',
      ),
    ).toBe(false);

    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });
});

// ── #task:1417 Stage 3 — daemon-side mid-turn deferred queue ──────────
//
// The mid-turn queue lives in the daemon now. These tests drive a fully
// controlled stub child so the deliver/defer/flush sequence is
// deterministic: we choose exactly what `parseLine` returns for each
// stdout line, and await each `deliver()` before emitting the next line.
//
// Gates (Cindy):
//   1. `deferred` covers only a READY handle that can't take the write now
//      (mid-turn `gated`, or a prior write in flight) — not startup/oneShot.
//   2. `deferred` is a custody transfer, NOT an ack: no consumed/failed at
//      defer time.
//   3. a `direct` flush is FIFO one-at-a-time and NEVER combines.
//   4. a `gated` flush drains the WHOLE queue and combines into ONE write /
//      ONE consumed.
//   5. owner-scoped cleanup discards only the dropped connection's entries.
describe('Supervisor Stage 3 — daemon mid-turn deferred queue', () => {
  /** A stub child whose stdin.write settles on setImmediate, so a
   *  write-in-flight window is observable, and whose stdout we drive by
   *  hand. `kill` resolves the exit promptly for cleanup. Written lines
   *  are captured; `parkWrites` holds a write's completion callback until
   *  `resolveParkedWrite` so a test can interleave parses inside the
   *  write await window (#task:1563 P1-2 falsifier). */
  class DriveStub extends EventEmitter {
    readonly pid = 55_555;
    readonly stdout = new EventEmitter();
    readonly stderr = new EventEmitter();
    readonly written: string[] = [];
    parkWrites = false;
    pendingWriteCb: ((err?: Error | null) => void) | null = null;
    readonly stdin = {
      write: (line: string, cb?: (err?: Error | null) => void): boolean => {
        this.written.push(line);
        if (this.parkWrites) {
          this.pendingWriteCb = cb ?? null;
          return true;
        }
        setImmediate(() => cb?.());
        return true;
      },
      end: () => {},
      destroyed: false,
      once: () => {},
    };
    resolveParkedWrite(): void {
      const cb = this.pendingWriteCb;
      this.pendingWriteCb = null;
      cb?.();
    }
    kill(_sig?: NodeJS.Signals | number): boolean {
      setImmediate(() => this.emit('exit', 0, null));
      return true;
    }
  }

  function makeDriven(
    mode: 'gated' | 'direct',
    profile?: Driver['releaseProfile'],
    logger?: DaemonLogger,
    opts: {
      reservationStallWarnMs?: number;
      parkFirstWrite?: boolean;
      startOptions?: StartOptions;
    } = {},
  ) {
    // Fresh stub per spawn: a respawn on the SAME emitter would leave the
    // dead handle's stdout listener attached and double-consume nextParse.
    let stub = new DriveStub();
    if (opts.parkFirstWrite) stub.parkWrites = true;
    const base = makeProgrammableDriver(
      [],
      mode === 'gated' ? 'requires_first_message' : 'self_inits',
    ).driver;
    let nextParse: ParsedEvent[] = [];
    const driver: Driver = {
      ...base,
      releaseProfile:
        profile ??
        // Default legacy mapping: the old boundary-gated queue behavior
        // (defer mid-turn, drain-all at turn_end) lives on
        // next-turn-batch in the final tree.
        (mode === 'gated' ? 'next-turn-batch' : 'native-steer'),
      parseLine: () => {
        const out = nextParse;
        nextParse = [];
        return out;
      },
    };
    const spy = makeEmitSpy();
    let firstSpawn = true;
    const supervisor = new Supervisor({
      emit: spy.emit,
      logger: logger ?? silentLogger(),
      driverFor: () => driver,
      resolveRuntime: async (_kind, fallback) => ({ binary: fallback, version: null }),
      spawnFn: () => {
        if (!firstSpawn) stub = new DriveStub();
        firstSpawn = false;
        return stub as unknown as ChildProcess;
      },
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
      }),
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
      sigtermToSigkillMs: 50,
      sigkillTimeoutMs: 200,
      ...(opts.reservationStallWarnMs !== undefined
        ? { reservationStallWarnMs: opts.reservationStallWarnMs }
        : {}),
    });
    /** Emit one stdout line whose parse yields `events`. */
    const drive = (events: ParsedEvent[]) => {
      nextParse = events;
      stub.stdout.emit('data', 'line\n');
    };
    const deliver = (ids: number[], owner = 'owner-A') =>
      supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          delivery_ids: ids,
          kind: 'message',
          text: `msg-${ids.join('_')}`,
          deliveries: ids.map((id) => ({
            delivery_id: id,
            seq: id,
            presentation: testDeliveryPresentation(`msg-${ids.join('_')}`),
          })),
          spawn_options: opts.startOptions ?? baseStartParams.options,
        },
        { owner },
      );
    const consumedFor = (id: number) =>
      spy.events.filter(
        (e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(id),
      );
    return {
      get stub() {
        return stub;
      },
      supervisor,
      events: spy.events,
      drive,
      deliver,
      consumedFor,
      driver,
    };
  }

  it.each(['direct', 'gated'] as const)(
    '%s: a structured multi-id deferred batch preserves one canonical payload and one atomic consumed group',
    async (mode) => {
      const h = makeDriven(mode);
      const presentations = [
        {
          body: '[msg_id=mA sender=system type=system target=#conv:supervisorConv01 time=2026-01-01T00:00:00+00:00]\nA',
          instruction: 'system' as const,
        },
        {
          body: '[msg_id=mB sender=system type=system target=#conv:supervisorConv01 time=2026-01-01T00:00:01+00:00]\nB',
          instruction: 'system' as const,
        },
      ];
      const canonical = composeRuntimeDeliveryPresentation(
        presentations,
        'silverhand message send',
      );
      const structured = (ids: readonly number[]) => ({
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [...ids],
        kind: 'message' as const,
        text: canonical,
        deliveries: ids.map((id, index) => ({
          delivery_id: id,
          seq: id,
          presentation: presentations[index]!,
        })),
        spawn_options: baseStartParams.options,
      });
      const decoded = (line: string): string =>
        (JSON.parse(line) as { message: { content: string } }).message.content;

      try {
        const boot = await h.deliver([1]);
        expect(boot.disposition).toBe('spawning');
        if (mode === 'gated') {
          h.drive([{ kind: 'session_init', sessionId: 's1' }]);
        }
        await waitForDeliveryConsumed(h.events, 1);
        if (mode === 'gated') {
          h.drive([{ kind: 'turn_end' }]);
          await waitFor(() =>
            h.events.some(
              (event) => event.event === 'runtime_parsed_event' && event.parsed.kind === 'turn_end',
            ),
          );
        }

        // Immediate admission establishes the model-visible baseline.
        const beforeImmediate = h.stub.written.length;
        expect(
          (await h.supervisor.deliver(structured([72, 73]), { owner: 'structured-owner' }))
            .disposition,
        ).toBe('admitted');
        await waitFor(() => h.stub.written.length === beforeImmediate + 1);
        expect(decoded(h.stub.written.at(-1)!)).toBe(canonical);
        await waitForDeliveryConsumed(h.events, 72);
        const immediateSettlements = h.events.filter(
          (event) =>
            event.event === 'runtime_delivery_consumed' &&
            event.delivery_ids.some((id) => id === 72 || id === 73),
        );
        expect(immediateSettlements).toHaveLength(1);
        expect(immediateSettlements[0]).toMatchObject({
          delivery_ids: [72, 73],
        });

        let beforeDeferred: number;
        if (mode === 'direct') {
          h.stub.parkWrites = true;
          expect((await h.deliver([9])).disposition).toBe('admitted');
          beforeDeferred = h.stub.written.length;
        } else {
          h.drive([{ kind: 'tool_call', name: 'Bash', input: {} }]);
          await waitFor(() =>
            h.events.some(
              (event) =>
                event.event === 'runtime_parsed_event' && event.parsed.kind === 'tool_call',
            ),
          );
          beforeDeferred = h.stub.written.length;
        }

        expect(
          (await h.supervisor.deliver(structured([70, 71]), { owner: 'structured-owner' }))
            .disposition,
        ).toBe('deferred');
        expect(h.stub.written).toHaveLength(beforeDeferred);

        if (mode === 'direct') {
          h.stub.parkWrites = false;
          h.stub.resolveParkedWrite();
          await waitForDeliveryConsumed(h.events, 9);
        } else {
          h.drive([{ kind: 'turn_end' }]);
        }
        await waitFor(() => h.stub.written.length === beforeDeferred + 1);
        expect(decoded(h.stub.written.at(-1)!)).toBe(canonical);
        await waitForDeliveryConsumed(h.events, 70);
        const deferredSettlements = h.events.filter(
          (event) =>
            event.event === 'runtime_delivery_consumed' &&
            event.delivery_ids.some((id) => id === 70 || id === 71),
        );
        expect(deferredSettlements).toHaveLength(1);
        expect(deferredSettlements[0]).toMatchObject({
          delivery_ids: [70, 71],
        });
      } finally {
        await h.supervisor
          .stop({
            runtime_scope: {
              kind: 'conversation',
              conversation_id: 'Conversation0001',
              agent_id: 'a1',
            },
            runtime_generation: 1,
            mode: 'hard',
          })
          .catch(() => undefined);
      }
    },
  );

  it('#task:2106 strict-subset rebind is now admitted (per-id metadata always present): owner cleanup splits the group — rebound id delivers under the new owner, the dropped owner’s remaining id is discarded for re-drive, nothing is orphaned or double-consumed', async () => {
    const h = makeDriven('direct');
    try {
      expect((await h.deliver([1])).disposition).toBe('spawning');
      await waitForDeliveryConsumed(h.events, 1);

      h.stub.parkWrites = true;
      expect((await h.deliver([90], 'owner-1')).disposition).toBe('admitted');
      expect((await h.deliver([100, 101], 'owner-1')).disposition).toBe('deferred');
      const writesBeforeRebind = h.stub.written.length;

      // #task:2106 deleted the "opaque presentation group" concept: every
      // deferred entry now carries per-delivery metadata, so a strict subset of
      // a deferred group CAN be rebound to a new owner (id 100 moves to owner-2
      // while 101 stays with owner-1). No throw, no write — the group is still
      // deferred, only its per-id ownership changed.
      expect((await h.deliver([100], 'owner-2')).disposition).toBe('deferred');
      expect(h.stub.written).toHaveLength(writesBeforeRebind);

      // owner-1 drops: cleanup SPLITS the group by id (the per-id metadata makes
      // the subset takeable). owner-1's still-owned id (101) is discarded — it
      // rolls back to backend-pending for a fresh re-drive; the rebound id (100)
      // is retained in daemon custody under owner-2.
      expect(() => h.supervisor.cleanupDeferredForOwner('owner-1')).not.toThrow();

      h.stub.parkWrites = false;
      h.stub.resolveParkedWrite();
      await waitForDeliveryConsumed(h.events, 90);
      // The retained subset flushes under owner-2 exactly once, as its own
      // atomic group [100] — not fused back with the discarded 101.
      await waitForDeliveryConsumed(h.events, 100);
      await wait(30);

      expect(h.consumedFor(90)).toHaveLength(1);
      expect(h.consumedFor(100)).toHaveLength(1);
      // 101 belonged to the dropped owner-1 and was discarded on its cleanup —
      // it is never consumed here (the backend re-drives it as a fresh row).
      expect(h.consumedFor(101)).toHaveLength(0);
    } finally {
      await h.supervisor
        .stop({
          runtime_scope: {
            kind: 'conversation',
            conversation_id: 'Conversation0001',
            agent_id: 'a1',
          },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => undefined);
    }
  });

  it('AgentSlot structured owner cleanup preserves the explicit target and absolute workdir on the surviving subset', async () => {
    const startOptions = testStartOptions({
      agentId: 'a1',
      sessionScope: 'agent',
    });
    const h = makeDriven('gated', undefined, undefined, { startOptions });
    const presentations = [10, 11].map((id) => ({
      body: `msg-${id}`,
      instruction: 'other' as const,
    }));
    const batch = (ids: readonly number[]) => ({
      runtime_generation: 1,
      conversation_id: 'Conversation0001',
      target: {
        conversation_id: 'Conversation0001',
        workspace: { id: 'ws1', name: 'test-workspace' },
      },
      agent_id: 'a1',
      delivery_ids: [...ids],
      kind: 'message' as const,
      text: composeRuntimeDeliveryPresentation(
        ids.map((id) => presentations[id === 10 ? 0 : 1]!),
        'silverhand message send',
      ),
      deliveries: ids.map((id) => ({
        delivery_id: id,
        seq: id,
        presentation: presentations[id === 10 ? 0 : 1]!,
      })),
      spawn_options: startOptions,
    });
    const decoded = (line: string): string =>
      (JSON.parse(line) as { message: { content: string } }).message.content;
    try {
      expect((await h.deliver([1], 'owner-A')).disposition).toBe('spawning');
      h.drive([{ kind: 'session_init', sessionId: 's1' }]);
      await waitForDeliveryConsumed(h.events, 1);
      h.drive([{ kind: 'tool_call', name: 'Bash', input: {} }]);
      await waitFor(() =>
        h.events.some(
          (event) => event.event === 'runtime_parsed_event' && event.parsed.kind === 'tool_call',
        ),
      );

      expect((await h.supervisor.deliver(batch([10, 11]), { owner: 'owner-A' })).disposition).toBe(
        'deferred',
      );
      expect((await h.supervisor.deliver(batch([10]), { owner: 'owner-B' })).disposition).toBe(
        'deferred',
      );
      h.supervisor.cleanupDeferredForOwner('owner-A');

      const before = h.stub.written.length;
      h.drive([{ kind: 'turn_end' }]);
      await waitFor(() => h.stub.written.length === before + 1);
      const text = decoded(h.stub.written.at(-1)!);
      expect(text).toContain('msg-10');
      expect(text).not.toContain('msg-11');
      expect(text).not.toMatch(/runtime_scope|workdir=/u);
      expect(text).not.toContain(join(SUPERVISOR_AGENTS_ROOT, 'a1', 'workspaces', 'ws1'));
      await waitForDeliveryConsumed(h.events, 10);
      expect(h.consumedFor(11)).toHaveLength(0);
    } finally {
      await h.supervisor
        .stop({
          runtime_scope: { kind: 'agent', agent_id: 'a1' },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => undefined);
    }
  });

  it('AgentSlot keeps one process across origins without exposing runtime scope or directory coordinates', async () => {
    ensureSupervisorBundle('a1', 'ws2');
    const startOptions = testStartOptions({
      agentId: 'a1',
      sessionScope: 'agent',
    });
    const h = makeDriven('direct', undefined, undefined, { startOptions });
    const decoded = (line: string): string =>
      (JSON.parse(line) as { message: { content: string } }).message.content;
    const deliver = (
      conversationId: string,
      workspace: { id: string; name: string } | null,
      id: number,
    ) =>
      h.supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: conversationId,
          target: { conversation_id: conversationId, workspace },
          agent_id: 'a1',
          delivery_ids: [id],
          kind: 'message',
          text: `origin-${conversationId}`,
          deliveries: [
            {
              delivery_id: id,
              seq: id,
              presentation: testDeliveryPresentation(`origin-${conversationId}`),
            },
          ],
          spawn_options: startOptions,
        },
        { owner: 'owner-A' },
      );
    try {
      const fromA = await deliver('Conversation0001', { id: 'ws1', name: 'one' }, 1);
      expect(fromA.disposition).toBe('spawning');
      if (fromA.disposition === 'refused') throw new Error('A was refused');
      await waitForDeliveryConsumed(h.events, 1);
      h.drive([{ kind: 'session_init', sessionId: 'agent-slot-vendor-session' }]);
      await waitFor(() =>
        h.events.some(
          (event) => event.event === 'runtime_parsed_event' && event.parsed.kind === 'session_init',
        ),
      );
      h.drive([{ kind: 'tool_call', name: 'Bash', input: {} }]);
      await waitFor(() =>
        h.events.some(
          (event) =>
            event.event === 'runtime_state' &&
            event.conversation_id === 'Conversation0001' &&
            event.state.conversationId === 'Conversation0001',
        ),
      );
      const focusBoundary = h.events.length;
      const fromB = await deliver('Conversation0002', { id: 'ws2', name: 'two' }, 2);
      expect(fromB.disposition).toBe('admitted');
      if (fromB.disposition === 'refused') throw new Error('B was refused');
      await waitForDeliveryConsumed(h.events, 2);
      h.drive([{ kind: 'text', text: 'visible from B' }, { kind: 'turn_end' }]);
      await waitFor(() =>
        h.events
          .slice(focusBoundary)
          .some(
            (event) =>
              event.event === 'runtime_state' &&
              event.state.status === 'idle' &&
              event.state.terminalReason === 'turn_ended',
          ),
      );
      const bStates = h.events
        .slice(focusBoundary)
        .filter(
          (event): event is Extract<EventFrame, { event: 'runtime_state' }> =>
            event.event === 'runtime_state',
        );
      expect(bStates.length).toBeGreaterThan(0);
      for (const frame of bStates) {
        expect(frame.conversation_id).toBe('Conversation0002');
        expect(frame.state.conversationId).toBe('Conversation0002');
      }
      const fromInbox = await deliver('Conversation0003', null, 3);
      expect(fromInbox.disposition).toBe('admitted');
      if (fromInbox.disposition === 'refused') throw new Error('inbox was refused');
      await waitForDeliveryConsumed(h.events, 3);

      // Return to A after a workspace-less direct-message origin. The semantic
      // `dm:@handle` spelling is covered by the real CLI subprocess matrix;
      // here the host proves that A → B → DM → A remains one vendor process.
      const backToA = await deliver('Conversation0001', { id: 'ws1', name: 'one' }, 4);
      expect(backToA.disposition).toBe('admitted');
      if (backToA.disposition === 'refused') throw new Error('return to A was refused');
      await waitForDeliveryConsumed(h.events, 4);

      expect(fromA.pid).toBe(fromB.pid);
      expect(fromB.pid).toBe(fromInbox.pid);
      expect(fromInbox.pid).toBe(backToA.pid);
      expect(h.supervisor.status({}).runtimes).toHaveLength(1);
      expect(
        h.events.filter(
          (event) => event.event === 'runtime_parsed_event' && event.parsed.kind === 'session_init',
        ),
      ).toHaveLength(1);
      const visible = h.stub.written.map(decoded);
      for (const text of visible) expect(text).not.toContain('target=');
      for (const text of visible) {
        expect(text).not.toMatch(/runtime_scope|workdir=/u);
        expect(text).not.toContain(SUPERVISOR_AGENTS_ROOT);
      }
    } finally {
      await h.supervisor
        .stop({
          runtime_scope: { kind: 'agent', agent_id: 'a1' },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => undefined);
    }
  });

  it('F18: one pendingStaleEvent per runtime — a second 409 supersedes the first; the two outcomes (superseded-with-live-id vs true no_pending) are pinned on both surfaces', async () => {
    const startOptions = testStartOptions({
      agentId: 'a1',
      sessionScope: 'agent',
    });
    const h = makeDriven('direct', undefined, undefined, { startOptions });
    const posted = new Map<string, Record<string, unknown>[]>();
    const responses = new Map<string, 'stale' | 'ok'>([
      ['Conversation0001', 'stale'],
      ['Conversation0002', 'stale'],
    ]);
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
        const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
        const conversationId =
          typeof body['conversation_id'] === 'string'
            ? body['conversation_id']
            : (/^#conv:([^/]+)/.exec(String(body['target']))?.[1] ?? 'Conversation0001');
        const url = String(input);
        const bodies = posted.get(conversationId) ?? [];
        bodies.push(body);
        posted.set(conversationId, bodies);
        if (url.endsWith('/stale-message-resolution')) {
          return new Response(JSON.stringify({ resolved: true }), {
            status: 200,
            headers: { 'content-type': 'application/json' },
          });
        }
        if (responses.get(conversationId) === 'stale') {
          return new Response(
            JSON.stringify({
              error: {
                code: 'conflict',
                message: 'message stale',
                details: {
                  subkind: 'message_stale',
                  stale_event_id: `stale-${conversationId}`,
                  target_conversation_id: conversationId,
                },
              },
            }),
            {
              status: 409,
              headers: { 'content-type': 'application/json' },
            },
          );
        }
        return new Response(
          JSON.stringify({
            message_id: `sent-${conversationId}`,
            message_seq: 3,
            conversation_id: conversationId,
            thread_root_id: null,
          }),
          {
            status: 200,
            headers: { 'content-type': 'application/json' },
          },
        );
      }),
    );
    const send = (conversationId: string, content: string) =>
      h.supervisor.agentMessageSend({
        runtime_scope: { kind: 'agent' },
        target: `#conv:${conversationId}`,
        agent_id: 'a1',
        content,
      });
    const receive = (staleEventId: string, owner: string) =>
      h.supervisor.agentStaleReceive(
        {
          runtime_scope: { kind: 'agent' },
          agent_id: 'a1',
          stale_event_id: staleEventId,
          wait_ms: 0,
        },
        { owner, supportsResponseHandoff: true },
      );
    try {
      expect((await h.deliver([1])).disposition).toBe('spawning');
      await waitForDeliveryConsumed(h.events, 1);

      // Arm A, then B. There is exactly ONE pendingStaleEvent per runtime, so
      // B's 409 OVERWRITES A's — A's stale_event_id is no longer live. (The
      // retired model kept a per-origin stale event; F18 collapses it.)
      expect(await send('Conversation0001', 'arm A')).toMatchObject({ ok: false, status: 409 });
      expect(await send('Conversation0002', 'arm B')).toMatchObject({ ok: false, status: 409 });

      // ── stale-receive surface ──
      // A never-armed id AND the just-overwritten A id resolve the SAME way:
      // `stale_event_superseded` carrying the LIVE id (the only thing the agent
      // can act on) — NOT a false `no_pending_stale`. That both collapse to the
      // live id is the proof A was superseded, not independently retained.
      expect(await receive('stale-missing', 'control:missing')).toMatchObject({
        ok: true,
        outcome: 'stale_event_superseded',
        current_stale_event_id: 'stale-Conversation0002',
      });
      expect(await receive('stale-Conversation0001', 'control:A-gone')).toMatchObject({
        ok: true,
        outcome: 'stale_event_superseded',
        current_stale_event_id: 'stale-Conversation0002',
      });

      // ── stale-resolve surface (symmetric) ──
      // A mismatched id supersedes here too, echoing the live id.
      expect(
        await h.supervisor.agentStaleResolve({
          runtime_scope: { kind: 'agent' },
          agent_id: 'a1',
          stale_event_id: 'stale-Conversation0001',
          resolution_reason: 'obsolete',
        }),
      ).toEqual({
        ok: true,
        resolved: false,
        reason: 'stale_event_superseded',
        current_stale_event_id: 'stale-Conversation0002',
      });

      // Resolve the LIVE id → resolved, POSTing the backend-resolved target
      // conversation VERBATIM as the authz subject (F17), then clearing the
      // single held event.
      expect(
        await h.supervisor.agentStaleResolve({
          runtime_scope: { kind: 'agent' },
          agent_id: 'a1',
          stale_event_id: 'stale-Conversation0002',
          resolution_reason: 'obsolete',
        }),
      ).toEqual({ ok: true, resolved: true });
      expect(posted.get('Conversation0002')?.at(-1)).toMatchObject({
        conversation_id: 'Conversation0002',
        stale_event_id: 'stale-Conversation0002',
        resolution_reason: 'obsolete',
      });

      // ── the OTHER outcome: pendingStaleEvent === null → true absence ──
      // receive reports `no_pending_stale`; resolve reports the (distinctly
      // named) `no_pending_stale_event`. Neither carries a live id.
      expect((await receive('stale-Conversation0002', 'control:cleared')).outcome).toBe(
        'no_pending_stale',
      );
      expect(
        await h.supervisor.agentStaleResolve({
          runtime_scope: { kind: 'agent' },
          agent_id: 'a1',
          stale_event_id: 'stale-anything',
          resolution_reason: 'obsolete',
        }),
      ).toEqual({ ok: true, resolved: false, reason: 'no_pending_stale_event' });
    } finally {
      vi.unstubAllGlobals();
      await h.supervisor
        .stop({
          runtime_scope: { kind: 'agent', agent_id: 'a1' },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => undefined);
    }
  });

  // #task:2099: `runtime_message_stale` is the ONE authoritative "a send of mine
  // was refused as stale" fact. It rides the SAME gate as arming
  // (`extractStaleConflict`), so the emit arms and the fail-closed arms are the
  // same partition — that is what makes it a fact axis rather than a second,
  // looser classifier. The renderer used to guess this from the agent's command
  // line; these arms are why that guess can now be deleted.
  it('#task:2099: exactly one runtime_message_stale per stale refusal — success, non-stale 409, and an unactionable "stale" 409 emit none', async () => {
    const startOptions = testStartOptions({
      agentId: 'a1',
      sessionScope: 'agent',
    });
    const h = makeDriven('direct', undefined, undefined, { startOptions });
    let arm: 'stale' | 'ok' | 'conflict_other' | 'stale_unactionable' | 'server_error' = 'stale';
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => {
        const conflict = (details: Record<string, unknown> | null) =>
          new Response(
            JSON.stringify({
              error: { code: 'conflict', message: 'nope', ...(details ? { details } : {}) },
            }),
            { status: 409, headers: { 'content-type': 'application/json' } },
          );
        switch (arm) {
          case 'stale':
            return conflict({
              subkind: 'message_stale',
              stale_event_id: 'stale-1',
              target_conversation_id: 'Conversation0001',
            });
          case 'stale_unactionable':
            // Right subkind, but no actionable anchor — `extractStaleConflict`
            // fails CLOSED (no armed event), so there is nothing to report as a
            // stale refusal either.
            return conflict({ subkind: 'message_stale' });
          case 'conflict_other':
            return conflict({ subkind: 'something_else' });
          case 'server_error':
            return new Response('boom', { status: 500 });
          case 'ok':
            return new Response(
              JSON.stringify({
                message_id: 'sent-1',
                message_seq: 3,
                conversation_id: 'Conversation0001',
                thread_root_id: null,
              }),
              { status: 200, headers: { 'content-type': 'application/json' } },
            );
        }
      }),
    );
    const staleFrames = () =>
      h.events.filter(
        (event): event is Extract<EventFrame, { event: 'runtime_message_stale' }> =>
          event.event === 'runtime_message_stale',
      );
    const send = () =>
      h.supervisor.agentMessageSend({
        runtime_scope: { kind: 'agent' },
        target: '#conv:Conversation0001',
        agent_id: 'a1',
        content: 'hello',
      });
    try {
      expect((await h.deliver([1])).disposition).toBe('spawning');
      await waitForDeliveryConsumed(h.events, 1);

      // ── the ONE emitting arm ──
      expect(await send()).toMatchObject({ ok: false, status: 409 });
      expect(staleFrames()).toHaveLength(1);
      expect(staleFrames()[0]).toEqual({
        event: 'runtime_message_stale',
        runtime_scope: { kind: 'agent', agent_id: 'a1' },
        conversation_id: 'Conversation0001',
        agent_id: 'a1',
        runtime_generation: 1,
        ts: expect.any(Number),
      });

      // A REPLACEMENT 409 is a second refusal, so it gets its own row — the
      // single-`pendingStaleEvent` collapse (F18) is about what the agent can
      // still act on, not about how many refusals happened.
      expect(await send()).toMatchObject({ ok: false, status: 409 });
      expect(staleFrames()).toHaveLength(2);

      // ── the silent arms ──
      arm = 'ok';
      expect(await send()).toMatchObject({ ok: true });
      arm = 'conflict_other';
      expect(await send()).toMatchObject({ ok: false, status: 409 });
      arm = 'stale_unactionable';
      expect(await send()).toMatchObject({ ok: false, status: 409 });
      arm = 'server_error';
      expect(await send()).toMatchObject({ ok: false, status: 500 });
      expect(staleFrames()).toHaveLength(2);
    } finally {
      vi.unstubAllGlobals();
      await h.supervisor
        .stop({
          runtime_scope: { kind: 'agent', agent_id: 'a1' },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => undefined);
    }
  });

  // #task:2099 (Cindy review point 1): the activity coordinate is SNAPSHOTTED
  // before the HTTP await. `handle.conversationId` is the agent-scoped session's
  // mutable focus and `agentMessageSend` holds no lifecycle lock, so a delivery
  // that lands while the send is in flight rewrites it. Park the backend response
  // mid-flight, flip focus A→B, then release: the refusal must still file under
  // A, the conversation the command was issued from.
  it('#task:2099: a focus flip during the in-flight send does not move the stale row off the issuing conversation', async () => {
    ensureSupervisorBundle('a1', 'ws2');
    const startOptions = testStartOptions({
      agentId: 'a1',
      sessionScope: 'agent',
    });
    const h = makeDriven('direct', undefined, undefined, { startOptions });
    // Held in a container, not a `let`: TS narrows a `let` assigned only inside a
    // callback back to `null` at the call site.
    const parked: { release: (() => void) | null } = { release: null };
    const sendReached = new Promise<void>((resolveReached) => {
      vi.stubGlobal(
        'fetch',
        vi.fn(async () => {
          await new Promise<void>((release) => {
            parked.release = release;
            resolveReached();
          });
          return new Response(
            JSON.stringify({
              error: {
                code: 'conflict',
                message: 'message stale',
                details: {
                  subkind: 'message_stale',
                  stale_event_id: 'stale-1',
                  target_conversation_id: 'Conversation0001',
                },
              },
            }),
            { status: 409, headers: { 'content-type': 'application/json' } },
          );
        }),
      );
    });
    const deliverTo = (conversationId: string, workspace: string, id: number) =>
      h.supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: conversationId,
          target: {
            conversation_id: conversationId,
            workspace: { id: workspace, name: workspace },
          },
          agent_id: 'a1',
          delivery_ids: [id],
          kind: 'message',
          text: `origin-${conversationId}`,
          deliveries: [
            {
              delivery_id: id,
              seq: id,
              presentation: testDeliveryPresentation(`origin-${conversationId}`),
            },
          ],
          spawn_options: startOptions,
        },
        { owner: 'owner-A' },
      );
    try {
      expect((await deliverTo('Conversation0001', 'ws1', 1)).disposition).toBe('spawning');
      await waitForDeliveryConsumed(h.events, 1);

      // Issue the send from A and park it inside the backend round-trip.
      const inFlight = h.supervisor.agentMessageSend({
        runtime_scope: { kind: 'agent' },
        target: '#conv:Conversation0001',
        agent_id: 'a1',
        content: 'sent while focused on A',
      });
      await sendReached;

      // Focus moves to B while the send is still awaiting its 409.
      // `bindRuntimeFocus` runs at the head of the consume path, so B's own
      // consumed ack carrying `Conversation0002` IS the proof the handle's focus
      // had already been rewritten before the 409 came back — without it this
      // test could pass on a focus that never moved.
      expect((await deliverTo('Conversation0002', 'ws2', 2)).disposition).toBe('admitted');
      await waitForDeliveryConsumed(h.events, 2);
      expect(
        h.events.find(
          (event) => event.event === 'runtime_delivery_consumed' && event.delivery_ids.includes(2),
        ),
      ).toMatchObject({ conversation_id: 'Conversation0002' });

      parked.release?.();
      expect(await inFlight).toMatchObject({ ok: false, status: 409 });

      const staleFrames = h.events.filter(
        (event): event is Extract<EventFrame, { event: 'runtime_message_stale' }> =>
          event.event === 'runtime_message_stale',
      );
      expect(staleFrames).toHaveLength(1);
      expect(staleFrames[0]?.conversation_id).toBe('Conversation0001');
    } finally {
      vi.unstubAllGlobals();
      await h.supervisor
        .stop({
          runtime_scope: { kind: 'agent', agent_id: 'a1' },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => undefined);
    }
  });

  it('one Supervisor-owned fence blocks late start, deliver, and internal retry respawn', async () => {
    const { supervisor } = makeSupervisor();
    const coordinator = supervisor.agentHomeMutationCoordinator();
    await coordinator.runPurge('a1', async () => ({
      ok: true,
      purged_count: 0,
      quiesced_count: 0,
    }));

    await expect(supervisor.start(baseStartParams)).rejects.toMatchObject({
      code: 'agent_deleting',
    });
    await expect(
      supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          delivery_ids: [1],
          kind: 'message',
          text: 'late',
          deliveries: [{ delivery_id: 1, seq: 1, presentation: testDeliveryPresentation('late') }],
          spawn_options: baseStartParams.options,
        },
        { owner: 'owner-A' },
      ),
    ).rejects.toMatchObject({ code: 'agent_deleting' });

    await (
      supervisor as unknown as {
        internalRespawnForRetry(params: typeof baseStartParams, attempt: number): Promise<void>;
      }
    ).internalRespawnForRetry(baseStartParams, 1);
    expect(supervisor.status({ agent_id: 'a1' })).toEqual({
      runtimes: [],
    });
  });

  it('gated: mid-turn deliveries DEFER (no ack), then flush combined into ONE consumed', async () => {
    const h = makeDriven('gated');
    // Spawn-with-batch [1]; a requires_first_message driver parks the write
    // until session_init, which then drives the handle to ready.
    const spawn0 = await h.deliver([1]);
    expect(spawn0.disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await waitForDeliveryConsumed(h.events, 1);

    // Enter a turn: a tool_call closes the safe boundary.
    h.drive([{ kind: 'tool_call', name: 'Bash', input: {} }]);
    await waitFor(() =>
      h.events.some((e) => e.event === 'runtime_parsed_event' && e.parsed.kind === 'tool_call'),
    );

    // Two mid-turn deliveries must DEFER (gate 1) and NOT ack (gate 2).
    const d2 = await h.deliver([2]);
    const d3 = await h.deliver([3]);
    expect(d2.disposition).toBe('deferred');
    expect(d3.disposition).toBe('deferred');
    await wait(30);
    expect(h.consumedFor(2)).toHaveLength(0);
    expect(h.consumedFor(3)).toHaveLength(0);
    expect(h.events.some((e) => e.event === 'runtime_delivery_failed')).toBe(false);

    // turn_end reopens the boundary → gated flush drains the WHOLE queue and
    // combines into ONE consumed covering both ids (gate 4).
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 2);
    const combined = h.events.filter(
      (e) =>
        e.event === 'runtime_delivery_consumed' &&
        (e.delivery_ids.includes(2) || e.delivery_ids.includes(3)),
    );
    expect(combined).toHaveLength(1);
    expect(
      combined[0]!.event === 'runtime_delivery_consumed' && [...combined[0]!.delivery_ids],
    ).toEqual([2, 3]);

    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('P-A falsifier (#task:1558): N mid-turn inputs to a next-turn-batch runtime form exactly ONE follow-up batch, even with the transport-safe boundary open', async () => {
    const h = makeDriven('gated', 'next-turn-batch');
    // Spawn-with-batch [1]; write #1 arms the reservation AT THE WRITE
    // (§3.4 step 2) — before any runtime activity frame exists.
    const spawn0 = await h.deliver([1]);
    expect(spawn0.disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await waitForDeliveryConsumed(h.events, 1);

    // The P-A trap: NO tool_call is in flight, so the legacy transport-
    // safe predicate is open the whole time. Under the old unified gate
    // these three would be written immediately, queuing three future
    // turns inside the runtime (#1556). They must DEFER instead.
    for (const id of [2, 3, 4]) {
      const d = await h.deliver([id]);
      expect(d.disposition).toBe('deferred');
    }
    // Mid-turn activity (thinking = provider/model working, still no
    // tool) must not trigger a flush either.
    h.drive([{ kind: 'thinking', text: 'working' }]);
    await wait(30);
    expect(h.consumedFor(2)).toHaveLength(0);
    expect(h.consumedFor(3)).toHaveLength(0);
    expect(h.consumedFor(4)).toHaveLength(0);

    // Terminal turn_end releases: the whole queue drains as ONE
    // combined write / ONE consumed covering [2,3,4].
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 2);
    const combined = h.events.filter(
      (e) =>
        e.event === 'runtime_delivery_consumed' &&
        (e.delivery_ids.includes(2) || e.delivery_ids.includes(3) || e.delivery_ids.includes(4)),
    );
    expect(combined).toHaveLength(1);
    expect(
      combined[0]!.event === 'runtime_delivery_consumed' && [...combined[0]!.delivery_ids],
    ).toEqual([2, 3, 4]);

    // The flush write re-armed the reservation: a delivery right after
    // it still defers until the NEXT turn_end.
    const d5 = await h.deliver([5]);
    expect(d5.disposition).toBe('deferred');
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 5);

    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('reservation stall falsifier (#task:1563 §3.4/§8, Cindy P1-1): ONE deferred entry with NO further inflow gets exactly one timeout-driven warn — zero writes; late turn_end flushes and the re-arm resets the latch', async () => {
    const logs: Array<Record<string, unknown>> = [];
    const logger = {
      info: () => {},
      warn: (f: unknown) => {
        if (f && typeof f === 'object') logs.push(f as Record<string, unknown>);
      },
      error: () => {},
    } as unknown as DaemonLogger;
    const h = makeDriven('gated', 'next-turn-batch', logger, {
      reservationStallWarnMs: 80,
    });
    const stallLogs = () => logs.filter((l) => l['action'] === 'next_turn_reservation_stalled');

    // Spawn-with-batch [1]; the write arms the reservation.
    const spawn0 = await h.deliver([1]);
    expect(spawn0.disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await waitForDeliveryConsumed(h.events, 1);

    // ONE delivery defers; below the threshold the queue is silent.
    const d2 = await h.deliver([2]);
    expect(d2.disposition).toBe('deferred');
    expect(stallLogs()).toHaveLength(0);

    // Cindy P1-1: NO further inflow. The timeout alone must surface the
    // stall — the queued entry is waiting even though nothing new
    // arrives to re-trigger a check.
    await waitFor(() => stallLogs().length === 1);
    // Exactly one warn (one-shot latch), and STILL zero writes: the
    // diagnostic never forces a release (fail closed).
    await wait(200);
    expect(stallLogs()).toHaveLength(1);
    expect(h.consumedFor(2)).toHaveLength(0);
    expect(stallLogs()[0]!['deferred_queue_depth']).toBe(1);

    // A delivery arriving during the already-warned stall defers quietly.
    const d3 = await h.deliver([3]);
    expect(d3.disposition).toBe('deferred');
    expect(stallLogs()).toHaveLength(1);

    // Recovery: the terminal arrives late → normal combined flush of the
    // whole queue, ONE consumed covering [2,3].
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 2);
    const combined = h.events.filter(
      (e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(2),
    );
    expect(combined).toHaveLength(1);
    expect(
      combined[0]!.event === 'runtime_delivery_consumed' && [...combined[0]!.delivery_ids],
    ).toEqual([2, 3]);

    // The flush write re-armed the reservation AND reset the latch: a
    // new deferred entry under the new reservation stalls → second warn.
    const d5 = await h.deliver([5]);
    expect(d5.disposition).toBe('deferred');
    await waitFor(() => stallLogs().length === 2);
    expect(h.consumedFor(5)).toHaveLength(0);

    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('startup double-write falsifier (#task:1563 Cindy P1-2): session_init parsed inside the startup write await window must not re-enter the consume — stdin written ONCE, consumed ONCE', async () => {
    // The race: `dispatchParsedLine` runs synchronously from the stdout
    // data listener, OUTSIDE the lifecycle lock. A session_init landing
    // while `consumeStartupDelivery` is awaiting `writeStdinPayload`
    // used to re-enter the consume via `tryConsumeStartupDelivery`
    // (gate non-null, awaitingInit not yet set) → the SAME batch written
    // to stdin twice + double consumed. `startupConsumeInFlight` guards
    // exactly that seam.
    const h = makeDriven('gated', 'next-turn-batch', undefined, {
      parkFirstWrite: true,
    });
    const spawn0 = await h.deliver([1]);
    expect(spawn0.disposition).toBe('spawning');

    // The startup write is parked mid-flight.
    await waitFor(() => h.stub.pendingWriteCb !== null);
    expect(h.stub.written).toHaveLength(1);

    // session_init lands INSIDE the write await window.
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await wait(30);
    // No re-entry: the same batch must NOT have been written again.
    expect(h.stub.written).toHaveLength(1);

    // Release the parked write; the in-flight consume settles on its own
    // (initObserved is already true) — exactly one consumed.
    h.stub.parkWrites = false;
    h.stub.resolveParkedWrite();
    await waitForDeliveryConsumed(h.events, 1);
    await wait(30);
    expect(h.consumedFor(1)).toHaveLength(1);
    expect(h.stub.written).toHaveLength(1);

    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('crash/restart convergence (#task:1563 §8): reservation + deferred queue die with the runtime — per-entry child_exit settle, never consumed; respawn-with-batch admits and re-arms the reservation', async () => {
    const h = makeDriven('gated', 'next-turn-batch');
    const spawn0 = await h.deliver([1]);
    expect(spawn0.disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await waitForDeliveryConsumed(h.events, 1);

    // Reservation held → mid-turn deliveries defer into daemon custody.
    const d2 = await h.deliver([2]);
    const d3 = await h.deliver([3]);
    expect(d2.disposition).toBe('deferred');
    expect(d3.disposition).toBe('deferred');

    // Crash with the reservation still held. The in-memory reservation
    // and queue die with the handle; each deferred entry settles as
    // `child_exit` (custody back to backend → rows roll to pending),
    // and NO consumed is ever emitted for them.
    h.stub.emit('exit', 1, null);
    await waitFor(() =>
      h.events.some(
        (e) =>
          e.event === 'runtime_delivery_failed' &&
          e.delivery_ids.includes(3) &&
          e.reason === 'child_exit',
      ),
    );
    expect(
      h.events.some(
        (e) =>
          e.event === 'runtime_delivery_failed' &&
          e.delivery_ids.includes(2) &&
          e.reason === 'child_exit',
      ),
    ).toBe(true);
    expect(h.consumedFor(2)).toHaveLength(0);
    expect(h.consumedFor(3)).toHaveLength(0);

    // Backend re-drive after rollback: respawn-with-batch admits [2,3]
    // as the startup batch (one consumed covering both).
    const respawn = await h.deliver([2, 3]);
    expect(respawn.disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's2' }]);
    await waitForDeliveryConsumed(h.events, 2);
    const combined = h.events.filter(
      (e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(2),
    );
    expect(combined).toHaveLength(1);
    expect(
      combined[0]!.event === 'runtime_delivery_consumed' && [...combined[0]!.delivery_ids],
    ).toEqual([2, 3]);

    // The startup write re-armed the reservation on the NEW handle: the
    // next delivery defers until its turn_end — restart converges to the
    // exact same contract, no special casing.
    const d4 = await h.deliver([4]);
    expect(d4.disposition).toBe('deferred');
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 4);

    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it.each([
    { label: 'child exits', preventExit: false },
    { label: 'child survives SIGKILL timeout', preventExit: true },
  ])(
    'deletion quiesce settles every deferred entry exactly once when $label',
    async ({ preventExit }) => {
      const h = makeDriven('gated', 'next-turn-batch');
      expect((await h.deliver([1])).disposition).toBe('spawning');
      h.drive([{ kind: 'session_init', sessionId: 's1' }]);
      await waitForDeliveryConsumed(h.events, 1);
      expect((await h.deliver([2])).disposition).toBe('deferred');
      expect((await h.deliver([3])).disposition).toBe('deferred');
      if (preventExit) h.stub.kill = () => true;

      const coordinator = h.supervisor.agentHomeMutationCoordinator();
      const purge = coordinator.runPurge('a1', async () => {
        const quiesced = await h.supervisor.fenceAndQuiesceAgent('a1');
        return { ok: true, purged_count: 1, ...quiesced };
      });
      if (preventExit) {
        await expect(purge).rejects.toMatchObject({ code: 'quiesce_failed' });
      } else {
        await expect(purge).resolves.toMatchObject({ quiesced_count: 1 });
      }

      for (const id of [2, 3]) {
        expect(
          h.events.filter(
            (event) =>
              event.event === 'runtime_delivery_failed' &&
              event.delivery_ids.includes(id) &&
              event.reason === 'child_exit',
          ),
        ).toHaveLength(1);
        expect(h.consumedFor(id)).toHaveLength(0);
      }
    },
  );

  it('driver-initiated writes are refused once the runtime is draining (#task:3048)', async () => {
    const h = makeDriven('direct');
    let io: DriverIO | undefined;
    h.driver.bindIO = (bound) => {
      io = bound;
    };
    expect((await h.deliver([1])).disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await waitForDeliveryConsumed(h.events, 1);
    await expect(io!.writeLine('before-stop')).resolves.toBeUndefined();

    let stub = h.stub;
    stub.kill = () => true;
    const stopping = h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
    await wait(0);
    await expect(io!.writeLine('after-stop')).rejects.toThrow('runtime is draining');
    expect(stub.written.some((w) => w.includes('after-stop'))).toBe(false);
    stub.emit('exit', 0, null);
    await stopping;
  });

  it('prompt-boundary falsifier (#task:1565): deliveries during an active prompt DEFER; the prompt response reopens the boundary → ONE combined prompt, batch-boundary admission', async () => {
    const logs: Array<Record<string, unknown>> = [];
    const logger = {
      info: (f: unknown) => {
        if (f && typeof f === 'object') logs.push(f as Record<string, unknown>);
      },
      warn: (f: unknown) => {
        if (f && typeof f === 'object') logs.push(f as Record<string, unknown>);
      },
      error: () => {},
    } as unknown as DaemonLogger;
    const h = makeDriven('direct', 'prompt-boundary', logger);
    // Mirror AcpClient's single-in-flight prompt gate: encode closes the
    // boundary (activePromptRequestId set), the prompt RESPONSE — parsed
    // as turn_end — reopens it. This is a correctness invariant probed on
    // opencode 1.18.1: a second live prompt gets queued runtime-side and
    // BOTH responses are held to the end of the whole run, so per-prompt
    // boundaries only exist in the one-at-a-time regime.
    let promptActive = false;
    const baseEncode = h.driver.encodeStdinMessage.bind(h.driver);
    h.driver.encodeStdinMessage = (text, sess, opts) => {
      promptActive = true;
      return baseEncode(text, sess, opts);
    };
    const baseParse = h.driver.parseLine.bind(h.driver);
    h.driver.parseLine = (line) => {
      const events = baseParse(line);
      if (events.some((e) => e.kind === 'turn_end')) promptActive = false;
      return events;
    };
    h.driver.readyToEncode = () => !promptActive;

    const deliverSeq = (id: number, seq: number) =>
      h.supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          delivery_ids: [id],
          kind: 'message',
          text: `m${id}`,
          deliveries: [{ delivery_id: id, seq, presentation: testDeliveryPresentation(`m${id}`) }],
          spawn_options: baseStartParams.options,
        },
        { owner: 'test-owner' },
      );

    // Spawn-with-batch [1] (seq 10): the write IS the prompt → boundary
    // closes.
    const spawn0 = await deliverSeq(1, 10);
    expect(spawn0.disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await waitForDeliveryConsumed(h.events, 1);
    expect(promptActive).toBe(true);

    // Mid-prompt deliveries DEFER (boundary closed): no consumed.
    const d2 = await deliverSeq(2, 11);
    const d3 = await deliverSeq(3, 12);
    expect(d2.disposition).toBe('deferred');
    expect(d3.disposition).toBe('deferred');
    await wait(30);
    expect(h.consumedFor(2)).toHaveLength(0);
    expect(h.consumedFor(3)).toHaveLength(0);

    // The prompt RESPONSE (parsed turn_end) reopens the boundary: the
    // whole queue flushes as ONE combined prompt / ONE consumed, and the
    // batch admits at the boundary.
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 2);
    const combined = h.events.filter(
      (e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(2),
    );
    expect(combined).toHaveLength(1);
    // FIFO is part of the acceptance contract (no reorder): assert the
    // consumed id ORDER verbatim — no .sort() masking — and that the
    // combined payload writes m2 before m3 on the wire.
    expect(
      combined[0]!.event === 'runtime_delivery_consumed' && [...combined[0]!.delivery_ids],
    ).toEqual([2, 3]);
    const combinedWrite = h.stub.written.find((w) => w.includes('m2') && w.includes('m3'));
    expect(combinedWrite).toBeTruthy();
    expect(combinedWrite!.indexOf('m2')).toBeLessThan(combinedWrite!.indexOf('m3'));

    // The flush write opened the NEXT prompt: boundary closed again, a
    // new delivery defers until that prompt's response.
    expect(promptActive).toBe(true);
    const d4 = await deliverSeq(4, 13);
    expect(d4.disposition).toBe('deferred');
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 4);

    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it("B' (#task:1558): tool-boundary-steer writes immediately mid-turn (compaction is the only hold)", async () => {
    const logs: Array<Record<string, unknown>> = [];
    const logger = {
      info: (f: unknown) => {
        if (f && typeof f === 'object') logs.push(f as Record<string, unknown>);
      },
      warn: (f: unknown) => {
        if (f && typeof f === 'object') logs.push(f as Record<string, unknown>);
      },
      error: () => {},
    } as unknown as DaemonLogger;
    const h = makeDriven('gated', 'tool-boundary-steer', logger);
    const minted: Array<string | undefined> = [];
    const baseEncode = h.driver.encodeStdinMessage.bind(h.driver);
    h.driver.encodeStdinMessage = (text, sess, opts) => {
      minted.push(opts?.payloadUuid);
      return baseEncode(text, sess, opts);
    };

    // Spawn with seq so the turn becomes active (admission with seq).
    const spawn0 = await h.supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [1],
        kind: 'message',
        text: 'first',
        deliveries: [{ delivery_id: 1, seq: 1, presentation: testDeliveryPresentation('first') }],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(spawn0.disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await waitForDeliveryConsumed(h.events, 1);

    // Mid-turn with an OUTSTANDING tool call — the legacy predicate is
    // closed. tool-boundary-steer must still write immediately.
    h.drive([{ kind: 'tool_call', name: 'Bash', input: {} }]);
    await waitFor(() =>
      h.events.some((e) => e.event === 'runtime_parsed_event' && e.parsed.kind === 'tool_call'),
    );
    const d2 = await h.supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [2],
        kind: 'message',
        text: 'steer me in',
        deliveries: [
          { delivery_id: 2, seq: 2, presentation: testDeliveryPresentation('steer me in') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(d2.disposition).toBe('admitted');
    await waitForDeliveryConsumed(h.events, 2);
    const uuid2 = minted[minted.length - 1];
    expect(uuid2).toBeTruthy();

    // Compaction is the one migration-period hold: deliveries defer
    // while compacting and flush at compaction_finished.
    h.drive([{ kind: 'compaction_started' }]);
    await waitFor(() =>
      h.events.some(
        (e) => e.event === 'runtime_parsed_event' && e.parsed.kind === 'compaction_started',
      ),
    );
    const d3 = await h.supervisor.deliver(
      {
        runtime_generation: 1,
        conversation_id: 'Conversation0001',
        target: { conversation_id: 'Conversation0001', workspace: null },
        agent_id: 'a1',
        delivery_ids: [3],
        kind: 'message',
        text: 'wait for compaction',
        deliveries: [
          { delivery_id: 3, seq: 3, presentation: testDeliveryPresentation('wait for compaction') },
        ],
        spawn_options: baseStartParams.options,
      },
      { owner: 'test-owner' },
    );
    expect(d3.disposition).toBe('deferred');
    h.drive([{ kind: 'compaction_finished' }]);
    await waitForDeliveryConsumed(h.events, 3);

    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('P1-1 falsifier (#1558 final gate): a turn_end crossing the write await window must not be overwritten back into reserved', async () => {
    // Fast-runtime interleaving: the flush write's callback is parked;
    // the turn generated by that very write completes (turn_end parsed)
    // BEFORE the write callback resolves. The late completion must NOT
    // re-arm the reservation — otherwise the queue is held forever.
    class ParkedStub extends EventEmitter {
      readonly pid = 57_001;
      readonly stdout = new EventEmitter();
      readonly stderr = new EventEmitter();
      parkWrites = false;
      pendingWriteCb: ((err?: Error | null) => void) | null = null;
      readonly stdin = {
        write: (_l: string, cb?: (err?: Error | null) => void): boolean => {
          if (this.parkWrites) {
            this.pendingWriteCb = cb ?? null;
          } else setImmediate(() => cb?.());
          return true;
        },
        end: () => {},
        destroyed: false,
        once: () => {},
      };
      resolveParkedWrite(): void {
        const cb = this.pendingWriteCb;
        this.pendingWriteCb = null;
        cb?.();
      }
      kill(): boolean {
        setImmediate(() => this.emit('exit', 0, null));
        return true;
      }
    }
    const stub = new ParkedStub();
    const base = makeProgrammableDriver([], 'self_inits').driver;
    let nextParse: ParsedEvent[] = [];
    const driver: Driver = {
      ...base,
      releaseProfile: 'next-turn-batch',
      parseLine: () => {
        const out = nextParse;
        nextParse = [];
        return out;
      },
    };
    const spy = makeEmitSpy();
    const supervisor = new Supervisor({
      emit: spy.emit,
      logger: silentLogger(),
      driverFor: () => driver,
      resolveRuntime: async (_k, fb) => ({ binary: fb, version: null }),
      spawnFn: () => stub as unknown as ChildProcess,
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
      }),
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
      sigtermToSigkillMs: 50,
      sigkillTimeoutMs: 200,
    });
    const drive = (events: ParsedEvent[]) => {
      nextParse = events;
      stub.stdout.emit('data', 'line\n');
    };
    const deliver = (ids: number[]) =>
      supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          delivery_ids: ids,
          kind: 'message',
          text: `m-${ids.join('_')}`,
          deliveries: ids.map((id) => ({
            delivery_id: id,
            seq: id,
            presentation: testDeliveryPresentation(`m-${ids.join('_')}`),
          })),
          spawn_options: baseStartParams.options,
        },
        { owner: 'test-owner' },
      );

    // Spawn-with-batch [1]: park its write, complete its turn (turn_end)
    // while the write callback is parked, THEN resolve the write.
    stub.parkWrites = true;
    const spawn0 = await deliver([1]);
    expect(spawn0.disposition).toBe('spawning');
    await waitFor(() => stub.pendingWriteCb !== null, 1_000);
    drive([{ kind: 'turn_end' }]);
    await waitFor(() =>
      spy.events.some((e) => e.event === 'runtime_parsed_event' && e.parsed.kind === 'turn_end'),
    );
    stub.parkWrites = false;
    stub.resolveParkedWrite();
    await waitForDeliveryConsumed(spy.events, 1);

    // The reservation must NOT be armed by the late write completion:
    // a fresh delivery on the (idle, terminal-crossed) runtime must be
    // written immediately, not held forever.
    const d2 = await deliver([2]);
    expect(d2.disposition).toBe('admitted');
    await waitForDeliveryConsumed(spy.events, 2);
    await supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('P1-2 falsifier (#1558 final gate): a drain-all flush takes a bounded prefix and leaves the ordered remainder for the next boundary', async () => {
    const h = makeDriven('gated', 'next-turn-batch');
    const spawn0 = await h.deliver([1]);
    expect(spawn0.disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await waitForDeliveryConsumed(h.events, 1);

    // Queue more entries than DEFERRED_BATCH_MAX_ENTRIES (20) mid-turn.
    const ids = Array.from({ length: 23 }, (_, i) => 100 + i);
    for (const id of ids) {
      const d = await h.deliver([id]);
      expect(d.disposition).toBe('deferred');
    }

    // First turn_end: exactly the first 20 drain as ONE combined
    // consumed; 3 remain queued in order.
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 100);
    const firstBatch = h.events.filter(
      (e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(100),
    );
    expect(firstBatch).toHaveLength(1);
    expect(
      firstBatch[0]!.event === 'runtime_delivery_consumed' && [...firstBatch[0]!.delivery_ids],
    ).toEqual(ids.slice(0, 20));
    expect(h.consumedFor(120)).toHaveLength(0);

    // Next turn_end: the ordered remainder [120,121,122] drains.
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 120);
    const secondBatch = h.events.filter(
      (e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(120),
    );
    expect(
      secondBatch[0]!.event === 'runtime_delivery_consumed' && [...secondBatch[0]!.delivery_ids],
    ).toEqual([120, 121, 122]);
    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('P1-3 falsifier (#1558 final gate): an unknown releaseProfile fails closed on live input — deliver rejects, zero consumed', async () => {
    const h = makeDriven('direct', 'native-steer');
    // Runtime comes up healthy on a valid profile (spawn/kickoff is not
    // a live-release decision), THEN the profile goes bogus — as a
    // mis-wired future driver would be at live-delivery time.
    const spawn0 = await h.deliver([1]);
    expect(spawn0.disposition).toBe('spawning');
    await waitForDeliveryConsumed(h.events, 1);
    (h.driver as { releaseProfile: string }).releaseProfile = 'bogus-profile';

    // Live delivery must fail closed: the deliver command rejects
    // (custody stays with the backend, rows un-acked) and nothing is
    // written to the runtime.
    await expect(h.deliver([2])).rejects.toThrow(/unknown releaseProfile/);
    await wait(50);
    expect(h.consumedFor(2)).toHaveLength(0);

    (h.driver as { releaseProfile: string }).releaseProfile = 'native-steer';
    await h.supervisor
      .stop({
        runtime_scope: {
          kind: 'conversation',
          conversation_id: 'Conversation0001',
          agent_id: 'a1',
        },
        runtime_generation: 1,
        mode: 'hard',
      })
      .catch(() => undefined);
  });

  it('P1-2 byte-axis falsifier (#1558): the bytes cap splits the prefix and a single oversized entry still drains alone', async () => {
    const h = makeDriven('gated', 'next-turn-batch');
    const spawn0 = await h.deliver([1]);
    expect(spawn0.disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await waitForDeliveryConsumed(h.events, 1);

    const deliverText = (id: number, text: string) =>
      h.supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          delivery_ids: [id],
          kind: 'message',
          text,
          deliveries: [{ delivery_id: id, seq: id, presentation: testDeliveryPresentation(text) }],
          spawn_options: baseStartParams.options,
        },
        { owner: 'test-owner' },
      );

    // Five 60KiB entries: with the 512B per-entry wrapping budget the
    // 256KiB cap admits exactly four; the fifth waits.
    const big = 'x'.repeat(60 * 1024);
    for (const id of [201, 202, 203, 204, 205]) {
      const d = await deliverText(id, big);
      expect(d.disposition).toBe('deferred');
    }
    // A single entry larger than the whole cap must still drain alone
    // (never wedges the queue).
    const oversized = 'y'.repeat(300 * 1024);
    const d6 = await deliverText(206, oversized);
    expect(d6.disposition).toBe('deferred');

    // turn_end #1 → [201..204] as ONE consumed (bytes cap, not count cap).
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 201);
    const batch1 = h.events.filter(
      (e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(201),
    );
    expect(
      batch1[0]!.event === 'runtime_delivery_consumed' && [...batch1[0]!.delivery_ids],
    ).toEqual([201, 202, 203, 204]);
    expect(h.consumedFor(205)).toHaveLength(0);

    // turn_end #2 → [205] alone? No: 205 (60KiB) + 206 (300KiB) — 205
    // fits, adding 206 would blow the cap → [205] drains, 206 waits.
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 205);
    expect(h.consumedFor(206)).toHaveLength(0);

    // turn_end #3 → the single oversized 206 drains ALONE (at-least-one).
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 206);
    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('direct: a write-in-flight deferral flushes FIFO one-at-a-time, never combined', async () => {
    const h = makeDriven('direct');
    // Spawn-with-batch [1]; self_inits admits + consumes immediately → ready.
    const spawn0 = await h.deliver([1]);
    expect(spawn0.disposition).toBe('spawning');
    await waitForDeliveryConsumed(h.events, 1);

    // [2] ADMITs (ready, boundary open); its write is in flight (stub write
    // settles on setImmediate). [3] lands in that window → DEFER
    // (write-in-flight), NOT combined with [2].
    const d2 = await h.deliver([2]);
    const d3 = await h.deliver([3]);
    expect(d2.disposition).toBe('admitted');
    expect(d3.disposition).toBe('deferred');

    // Both settle as SEPARATE consumed events (FIFO), never a combined [2,3].
    await waitForDeliveryConsumed(h.events, 2);
    await waitForDeliveryConsumed(h.events, 3);
    const c2 = h.consumedFor(2);
    const c3 = h.consumedFor(3);
    expect(c2).toHaveLength(1);
    expect(c3).toHaveLength(1);
    // No single consumed carries both — direct never combines.
    expect(
      h.events.some(
        (e) =>
          e.event === 'runtime_delivery_consumed' &&
          e.delivery_ids.includes(2) &&
          e.delivery_ids.includes(3),
      ),
    ).toBe(false);

    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('owner-scoped cleanup discards only the dropped connection’s deferred entries', async () => {
    const h = makeDriven('gated');
    const spawn0 = await h.deliver([1], 'owner-A');
    expect(spawn0.disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await waitForDeliveryConsumed(h.events, 1);

    // Mid-turn: two owners each defer a delivery.
    h.drive([{ kind: 'tool_call', name: 'Bash', input: {} }]);
    await waitFor(() =>
      h.events.some((e) => e.event === 'runtime_parsed_event' && e.parsed.kind === 'tool_call'),
    );
    expect((await h.deliver([10], 'owner-A')).disposition).toBe('deferred');
    expect((await h.deliver([20], 'owner-B')).disposition).toBe('deferred');

    // owner-A drops: discard ONLY A's entry, emit nothing.
    h.supervisor.cleanupDeferredForOwner('owner-A');
    await wait(20);
    expect(
      h.events.some((e) =>
        e.event === 'runtime_delivery_consumed' ? e.delivery_ids.includes(10) : false,
      ),
    ).toBe(false);
    expect(h.events.some((e) => e.event === 'runtime_delivery_failed')).toBe(false);

    // turn_end: only owner-B's surviving [20] flushes; A's [10] is gone.
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 20);
    expect(h.consumedFor(10)).toHaveLength(0);

    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('reconnect/reclaim race: a same-id-set re-submit under a NEW owner rebinds the queued entry, so the OLD owner’s cleanup no longer drops it (Cindy P1)', async () => {
    // The orphan Cindy P1 flags: owner A defers [42] mid-turn; the backend
    // sees A drop, rolls the DB row back to pending, and re-drives it under
    // owner B; B's runtime_deliver [42] can reach the daemon BEFORE A's
    // cleanup. Owner-blind dedup would keep the entry under A, then A's
    // cleanup would discard custody the DB now attributes to B — orphan.
    // Owner-aware enqueue rebinds the entry to B, so A's cleanup misses it
    // and it flushes normally.
    const h = makeDriven('gated');
    const spawn0 = await h.deliver([1], 'owner-A');
    expect(spawn0.disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await waitForDeliveryConsumed(h.events, 1);

    h.drive([{ kind: 'tool_call', name: 'Bash', input: {} }]);
    await waitFor(() =>
      h.events.some((e) => e.event === 'runtime_parsed_event' && e.parsed.kind === 'tool_call'),
    );

    // A defers [42]; then B re-submits the SAME id-set before A's cleanup.
    expect((await h.deliver([42], 'owner-A')).disposition).toBe('deferred');
    expect((await h.deliver([42], 'owner-B')).disposition).toBe('deferred');

    // A's connection drops and its cleanup fires — must NOT drop [42] (now
    // owned by B).
    h.supervisor.cleanupDeferredForOwner('owner-A');

    // turn_end flushes the surviving entry → [42] is consumed exactly once.
    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 42);
    expect(h.consumedFor(42)).toHaveLength(1);

    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });

  it('same-owner re-submit of a queued id-set is idempotent (no double-queue, one consumed)', async () => {
    const h = makeDriven('gated');
    const spawn0 = await h.deliver([1], 'owner-A');
    expect(spawn0.disposition).toBe('spawning');
    h.drive([{ kind: 'session_init', sessionId: 's1' }]);
    await waitForDeliveryConsumed(h.events, 1);

    h.drive([{ kind: 'tool_call', name: 'Bash', input: {} }]);
    await waitFor(() =>
      h.events.some((e) => e.event === 'runtime_parsed_event' && e.parsed.kind === 'tool_call'),
    );

    // Same owner defers [7] twice — the second is deduped, not double-queued.
    expect((await h.deliver([7], 'owner-A')).disposition).toBe('deferred');
    expect((await h.deliver([7], 'owner-A')).disposition).toBe('deferred');

    h.drive([{ kind: 'turn_end' }]);
    await waitForDeliveryConsumed(h.events, 7);
    // Exactly one consumed carrying [7] — dedup held.
    expect(h.consumedFor(7)).toHaveLength(1);

    await h.supervisor.stop({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001', agent_id: 'a1' },
      runtime_generation: 1,
      mode: 'hard',
    });
  });
});

describe('Supervisor #2760 — Codex request rejection and thread isolation', () => {
  async function setup() {
    const child = new ControlledRuntime();
    const { supervisor, events } = make3cSupervisor({
      driverOverride: () => new CodexDriver(),
      spawnFn: () => child as unknown as ChildProcess,
    });
    const feed = (message: unknown) => child.stdout.emit('data', `${JSON.stringify(message)}\n`);
    await supervisor.start(baseStartParams);
    await waitFor(() => child.writes.length === 1);
    const initialize = JSON.parse(child.writes[0]!.line);
    child.writes[0]!.finish();
    feed({ id: initialize.id, result: {} });
    expect(child.writes).toHaveLength(3);
    child.writes[1]!.finish();
    child.writes[2]!.finish();
    const threadRequest = JSON.parse(child.writes[2]!.line);
    feed({ id: threadRequest.id, result: { thread: { id: 'main-thread' } } });
    const deliver = (id: number) =>
      supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          delivery_ids: [id],
          kind: 'message',
          text: `message ${id}`,
          deliveries: [
            { delivery_id: id, seq: id, presentation: testDeliveryPresentation(`message ${id}`) },
          ],
          spawn_options: baseStartParams.options,
        },
        { owner: 'test-owner' },
      );
    const inputs = () => child.writes.filter((w) => JSON.parse(w.line).method?.startsWith('turn/'));
    const started = (uuid: string, threadId = 'main-thread') =>
      feed({
        method: 'item/started',
        params: { threadId, item: { type: 'userMessage', clientId: uuid } },
      });
    const failed = (id: number) =>
      events.filter((e) => e.event === 'runtime_delivery_failed' && e.delivery_ids.includes(id));
    const consumed = (id: number) =>
      events.filter((e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(id));
    return { child, events, feed, deliver, inputs, started, failed, consumed };
  }

  it.each(['early', 'late', 'write-failure'] as const)(
    'a %s rejection settles once and releases the next batch',
    async (timing) => {
      const h = await setup();
      try {
        expect((await h.deliver(201)).disposition).toBe('admitted');
        await waitFor(() => h.inputs().length === 1);
        const first = h.inputs()[0]!;
        const request = JSON.parse(first.line);
        expect((await h.deliver(202)).disposition).toBe('deferred');
        if (timing === 'late') {
          first.finish();
          await wait(30);
        }
        h.feed({ id: request.id, error: { message: 'explicit rejection' } });
        if (timing !== 'late') {
          expect(h.failed(201)).toHaveLength(0);
          expect(h.inputs()).toHaveLength(1);
          first.finish(timing === 'write-failure' ? new Error('EPIPE') : undefined);
        }
        await waitFor(() => h.failed(201).length === 1);
        expect(h.failed(201)[0]).toMatchObject({
          reason: timing === 'write-failure' ? 'write_failed' : 'driver_refused',
        });
        expect(h.consumed(201)).toHaveLength(0);
        await waitFor(() => h.inputs().length === 2);
        h.feed({ id: request.id, error: { message: 'duplicate old rejection' } });
        h.started(request.params.clientUserMessageId);
        expect(h.failed(202)).toHaveLength(0);
        expect(h.consumed(202)).toHaveLength(0);
        const next = h.inputs()[1]!;
        const nextRequest = JSON.parse(next.line);
        h.started(nextRequest.params.clientUserMessageId);
        next.finish();
        await waitFor(() => h.consumed(202).length === 1);
        expect(h.failed(201)).toHaveLength(1);
        expect(h.failed(202)).toHaveLength(0);
        expect(h.inputs()).toHaveLength(2);
      } finally {
        h.child.dispose();
      }
    },
  );

  it('foreign lifecycle and success ACK never admit; a proven input cannot be rolled back', async () => {
    const h = await setup();
    try {
      h.feed({
        method: 'turn/started',
        params: { threadId: 'main-thread', turn: { id: 'main-turn' } },
      });
      h.feed({
        method: 'turn/started',
        params: { threadId: 'child-thread', turn: { id: 'child-turn' } },
      });
      expect((await h.deliver(211)).disposition).toBe('admitted');
      await waitFor(() => h.inputs().length === 1);
      const first = h.inputs()[0]!;
      const request = JSON.parse(first.line);
      expect(request.params).toMatchObject({
        threadId: 'main-thread',
        expectedTurnId: 'main-turn',
      });
      h.started(request.params.clientUserMessageId, 'child-thread');
      h.feed({ id: 9999, error: { message: 'unrelated rejection' } });
      first.finish();
      h.feed({ id: request.id, result: { turnId: 'main-turn' } });
      await wait(30);
      expect(h.failed(211)).toHaveLength(0);
      expect(h.consumed(211)).toHaveLength(0);
      expect((await h.deliver(212)).disposition).toBe('deferred');
      h.started(request.params.clientUserMessageId);
      await waitFor(() => h.consumed(211).length === 1);
      await waitFor(() => h.inputs().length === 2);
      h.feed({ id: request.id, error: { message: 'late contradictory rejection' } });
      expect(h.failed(211)).toHaveLength(0);
      expect(h.failed(212)).toHaveLength(0);
      const next = h.inputs()[1]!;
      h.started(JSON.parse(next.line).params.clientUserMessageId);
      next.finish();
      await waitFor(() => h.consumed(212).length === 1);
    } finally {
      h.child.dispose();
    }
  });

  it('admission observed during the write outranks a contradictory RPC error', async () => {
    const h = await setup();

    try {
      await h.deliver(221);
      await waitFor(() => h.inputs().length === 1);
      const first = h.inputs()[0]!;
      const request = JSON.parse(first.line);

      h.started(request.params.clientUserMessageId);
      h.feed({ id: request.id, error: { message: 'contradictory rejection' } });
      expect(h.consumed(221)).toHaveLength(0);
      expect(h.failed(221)).toHaveLength(0);

      first.finish();
      await waitFor(() => h.consumed(221).length === 1);
      expect(h.failed(221)).toHaveLength(0);
    } finally {
      h.child.dispose();
    }
  });

  it('post-exit rejection does not preempt a buffered admission proof', async () => {
    const h = await setup();

    try {
      await h.deliver(231);
      await waitFor(() => h.inputs().length === 1);
      const first = h.inputs()[0]!;
      const request = JSON.parse(first.line);
      first.finish();
      await wait(30);

      h.child.emit('exit', 0, null);
      h.feed({ id: request.id, error: { message: 'buffered rejection' } });
      expect(h.failed(231)).toHaveLength(0);

      h.started(request.params.clientUserMessageId);
      await waitFor(() => h.consumed(231).length === 1);
      h.child.stdout.emit('end');
      expect(h.failed(231)).toHaveLength(0);
    } finally {
      h.child.dispose();
    }
  });
});

describe('Supervisor #task:1486 — reasoning-effort is forwarded, not product-gated', () => {
  // Regression lock for Cindy's P1: the daemon is NOT a product-legality
  // authority. It does bounded structural wire validation and forwards the
  // effort token; product validity lives in the backend write boundary, and
  // Codex itself rejects an engine-unsupported effort. Re-introducing a static
  // product allowlist here would defeat the open-token contract (an older
  // daemon would `invalid_params` a future token a newer backend legitimately
  // sends), so any product-model check on the spawn path must fail this test.
  function codexStart(model: string, effort: string) {
    const options = testStartOptions({
      conversationId: 'Conversation0001',
      agentId: 'a1',
      agentName: 'tester',
      runtime: 'codex',
      model,
    });
    options.driver.reasoning_effort = effort;
    return {
      conversation_id: 'Conversation0001',
      agent_id: 'a1',
      runtime_generation: 1,
      options,
    };
  }

  it.each([
    ['my-proxy-model', 'high', 'model_reasoning_effort="high"'],
    ['gpt-5.6-sol', 'ultra', 'model_reasoning_effort="ultra"'],
    ['gpt-5.6-sol', 'giga', 'model_reasoning_effort="giga"'],
  ])(
    'forwards %s / %s to the spawned Codex argv without product gating',
    async (model, effort, expected) => {
      const child = new ControlledRuntime(true);
      let argv: readonly string[] = [];
      const { supervisor } = make3cSupervisor({
        driverOverride: () => new CodexDriver(),
        spawnFn: (_binary, args) => {
          argv = args;
          return child as unknown as ChildProcess;
        },
      });
      try {
        assertStarted(await supervisor.start(codexStart(model, effort)));
        expect(argv).toContain('-c');
        expect(argv[argv.indexOf('-c') + 1]).toBe(expected);
      } finally {
        // This argv test is not a failed-initialize scenario. Complete the
        // driver's queued handshake write before closing its transport.
        for (const write of child.writes) write.finish();
        child.dispose();
      }
    },
  );
});

describe('Supervisor #task:1614 — agent_stale_receive deferred pull (custody contract #task:1606)', () => {
  /** Same drivable stub the Stage 3 suite uses: hand-driven stdout,
   *  observable stdin writes, park-able write completion. */
  class PullDriveStub extends EventEmitter {
    readonly pid = 66_666;
    readonly stdout = new EventEmitter();
    readonly stderr = new EventEmitter();
    readonly written: string[] = [];
    parkWrites = false;
    pendingWriteCb: ((err?: Error | null) => void) | null = null;
    readonly stdin = {
      write: (line: string, cb?: (err?: Error | null) => void): boolean => {
        this.written.push(line);
        if (this.parkWrites) {
          this.pendingWriteCb = cb ?? null;
          return true;
        }
        setImmediate(() => cb?.());
        return true;
      },
      end: () => {},
      destroyed: false,
      once: () => {},
    };
    resolveParkedWrite(): void {
      const cb = this.pendingWriteCb;
      this.pendingWriteCb = null;
      cb?.();
    }
    kill(_sig?: NodeJS.Signals | number): boolean {
      setImmediate(() => this.emit('exit', 0, null));
      return true;
    }
  }

  /** Gated next-turn-batch (qoder-class) harness with per-delivery seq
   *  metadata — the deferred batch still carries per-delivery seqs for
   *  the combined turn's `delivered_up_to_seq` high-water. */
  function makePullHarness(
    opts: {
      deletionCustodySettleTimeoutMs?: number;
    } = {},
  ) {
    let stub = new PullDriveStub();
    let lastDeliveredSeqPath: string | undefined;
    const base = makeProgrammableDriver([], 'requires_first_message').driver;
    let nextParse: ParsedEvent[] = [];
    const driver: Driver = {
      ...base,
      releaseProfile: 'next-turn-batch',
      parseLine: () => {
        const out = nextParse;
        nextParse = [];
        return out;
      },
    };
    const spy = makeEmitSpy();
    let firstSpawn = true;
    const supervisor = new Supervisor({
      emit: spy.emit,
      logger: silentLogger(),
      driverFor: () => driver,
      resolveRuntime: async (_kind, fallback) => ({ binary: fallback, version: null }),
      spawnFn: (_binary, _args, options) => {
        lastDeliveredSeqPath = options.env?.['SILVERHAND_LAST_DELIVERED_SEQ_FILE'];
        if (!firstSpawn) stub = new PullDriveStub();
        firstSpawn = false;
        return stub as unknown as ChildProcess;
      },
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
      }),
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
      sigtermToSigkillMs: 50,
      sigkillTimeoutMs: 200,
      deletionCustodySettleTimeoutMs: opts.deletionCustodySettleTimeoutMs ?? 200,
    });
    const drive = (events: ParsedEvent[]) => {
      nextParse = events;
      stub.stdout.emit('data', 'line\n');
    };
    /** Deliver one message with structured per-delivery seq metadata. */
    const deliverSeq = (id: number, seq: number, owner = 'owner-A') =>
      supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          delivery_ids: [id],
          kind: 'message',
          text: `msg-${id}`,
          deliveries: [
            {
              delivery_id: id,
              seq,
              presentation: testDeliveryPresentation(`msg-${id}`),
            },
          ],
          spawn_options: baseStartParams.options,
        },
        { owner },
      );
    const consumedFor = (id: number) =>
      spy.events.filter(
        (e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(id),
      );
    /** #task:2095: freshness is now a backend RuntimeScope-wide EXISTS
     *  guard, not a daemon-stamped per-stream cursor. The daemon forwards
     *  NO `seen_up_to_seq`/`asserted_stream` — the only freshness field it
     *  can carry is the human `--anyway` bypass (`freshness_bypass`). So
     *  this stub answers every non-bypass send with a production-shaped
     *  409 (F17 detail: `target_conversation_id`, no seq), which arms the
     *  daemon's single `pendingStaleEvent`; an explicit `--anyway` send is
     *  the only one that lands. Coverage arithmetic is gone: whether a
     *  stale-receive drains the queue is a property of the daemon's
     *  deferred FIFO depth, decided host-side, never of a backend head. */
    const stubBackendFreshnessGuard = () => {
      const postedBodies: Record<string, unknown>[] = [];
      vi.stubGlobal(
        'fetch',
        vi.fn(async (_url: string, init?: RequestInit) => {
          const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
          postedBodies.push(body);
          if (body['freshness_bypass'] === true) {
            return new Response(
              JSON.stringify({
                message_id: 'm-ok',
                message_seq: 99,
                // The landed stream is derivable from the request's own
                // forwarded address.
                ...landedStream(body['target']),
              }),
              { status: 200, headers: { 'content-type': 'application/json' } },
            );
          }
          return new Response(
            JSON.stringify({
              error: {
                code: 'conflict',
                message: 'message stale',
                details: {
                  subkind: 'message_stale',
                  stale_event_id: 'stale-1',
                  target_conversation_id: 'Conversation0001',
                },
              },
            }),
            { status: 409, headers: { 'content-type': 'application/json' } },
          );
        }),
      );
      return postedBodies;
    };
    /** Ready a spawned gated handle: spawn-with-batch [1] seq 1, then
     *  session_init → ready; the spawn write settles delivery 1. */
    const boot = async () => {
      const spawn0 = await deliverSeq(1, 1);
      expect(spawn0.disposition).toBe('spawning');
      drive([{ kind: 'session_init', sessionId: 's1' }]);
      await waitForDeliveryConsumed(spy.events, 1);
    };
    /** The pull entry, with the server-side handoff emulated: returns the
     *  result plus a `settle` you invoke like the control socket would. */
    const pull = async (waitMs: number) => {
      const ctx: DeliveryOwnerContext = {
        owner: 'control:test',
        supportsResponseHandoff: true,
      };
      const result = await supervisor.agentStaleReceive(
        {
          runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001' },
          agent_id: 'a1',
          stale_event_id: 'stale-1',
          wait_ms: waitMs,
        },
        ctx,
      );
      return { result, settle: ctx.responseHandoff };
    };
    const stop = () =>
      supervisor
        .stop({
          runtime_scope: {
            kind: 'conversation',
            conversation_id: 'Conversation0001',
            agent_id: 'a1',
          },
          runtime_generation: 1,
          mode: 'hard',
        })
        .catch(() => undefined);
    return {
      readLastDeliveredSeq: () => {
        if (!lastDeliveredSeqPath) throw new Error('child did not receive its seq file path');
        return readFileSync(lastDeliveredSeqPath, 'utf8');
      },
      get stub() {
        return stub;
      },
      supervisor,
      events: spy.events,
      drive,
      deliverSeq,
      consumedFor,
      stubBackendFreshnessGuard,
      boot,
      pull,
      stop,
    };
  }

  const send = (
    supervisor: Supervisor,
    content: string,
    staleResolution?: 'unaffected' | 'revised',
    freshnessBypass?: boolean,
  ) =>
    supervisor.agentMessageSend({
      runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001' },
      target: '#conv:Conversation0001',
      agent_id: 'a1',
      content,
      ...(staleResolution ? { stale_resolution_reason: staleResolution } : {}),
      ...(freshnessBypass ? { freshness_bypass: true } : {}),
    });

  const runDeletionPurge = (supervisor: Supervisor, order: string[], beforeRemove = () => {}) =>
    supervisor.agentHomeMutationCoordinator().runPurge('a1', async () => {
      const quiesced = await supervisor.fenceAndQuiesceAgent('a1');
      beforeRemove();
      order.push('rm');
      return { ok: true as const, purged_count: 1, ...quiesced };
    });

  it('deletion fence closes stale-receive under the lifecycle lock before it can reserve custody', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    let releasePurge!: () => void;
    const holdPurge = new Promise<void>((resolve) => {
      releasePurge = resolve;
    });
    try {
      await h.boot();
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');

      const coordinator = h.supervisor.agentHomeMutationCoordinator();
      const purge = coordinator.runPurge('a1', async () => {
        await holdPurge;
        const quiesced = await h.supervisor.fenceAndQuiesceAgent('a1');
        return { ok: true as const, purged_count: 1, ...quiesced };
      });

      await expect(h.pull(1_000)).rejects.toMatchObject({
        code: 'agent_deleting',
      });
      expect(h.consumedFor(2)).toHaveLength(0);
      releasePurge();
      await expect(purge).resolves.toMatchObject({ quiesced_count: 1 });
      expect(
        h.events.filter(
          (event) => event.event === 'runtime_delivery_failed' && event.delivery_ids.includes(2),
        ),
      ).toHaveLength(1);
    } finally {
      releasePurge();
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('deletion waits for successful response handoff settlement after handle exit before rm, then consumes exactly once', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    const order: string[] = [];
    try {
      await h.boot();
      expect(h.readLastDeliveredSeq()).toBe('1\n');
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');
      const { result, settle } = await h.pull(1_000);
      expect(result.outcome).toBe('batch_queue_drained');
      // The reservation outlives the runtime handle. The deletion barrier
      // must find custody through the agent-level registry, not only the
      // current/living handle map.
      h.stub.emit('exit', 0, null);
      await waitFor(() => h.events.some((event) => event.event === 'runtime_exit'));

      const purge = runDeletionPurge(h.supervisor, order, () => {
        expect(h.readLastDeliveredSeq()).toBe('2\n');
        expect(h.consumedFor(2)).toHaveLength(1);
      });
      await wait(30);
      expect(order).not.toContain('rm');
      expect(h.consumedFor(2)).toHaveLength(0);

      settle!(true);
      await expect(purge).resolves.toMatchObject({ quiesced_count: 0 });
      expect(order).toEqual(['rm']);
      expect(h.consumedFor(2)).toHaveLength(1);
      expect(
        h.events.filter(
          (event) => event.event === 'runtime_delivery_failed' && event.delivery_ids.includes(2),
        ),
      ).toHaveLength(0);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('deletion waits for failed response handoff settlement, then fails restored custody exactly once before rm', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    const order: string[] = [];
    try {
      await h.boot();
      expect(h.readLastDeliveredSeq()).toBe('1\n');
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');
      const { result, settle } = await h.pull(1_000);
      expect(result.outcome).toBe('batch_queue_drained');

      const purge = runDeletionPurge(h.supervisor, order, () => {
        expect(h.readLastDeliveredSeq()).toBe('1\n');
        expect(
          h.events.filter(
            (e) => e.event === 'runtime_delivery_failed' && e.delivery_ids.includes(2),
          ),
        ).toHaveLength(1);
      });
      await wait(30);
      expect(order).not.toContain('rm');
      settle!(false);
      await expect(purge).resolves.toMatchObject({ quiesced_count: 1 });

      expect(h.readLastDeliveredSeq()).toBe('1\n');
      expect(order).toEqual(['rm']);
      expect(h.consumedFor(2)).toHaveLength(0);
      expect(
        h.events.filter(
          (event) =>
            event.event === 'runtime_delivery_failed' &&
            event.delivery_ids.includes(2) &&
            event.reason === 'child_exit',
        ),
      ).toHaveLength(1);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('deletion custody timeout returns retryable quiesce_failed without settlement or rm; handoff then retry converges', async () => {
    const h = makePullHarness({ deletionCustodySettleTimeoutMs: 25 });
    h.stubBackendFreshnessGuard();
    const order: string[] = [];
    try {
      await h.boot();
      expect(h.readLastDeliveredSeq()).toBe('1\n');
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');
      const { result, settle } = await h.pull(1_000);
      expect(result.outcome).toBe('batch_queue_drained');

      const timeoutError = await runDeletionPurge(h.supervisor, order).catch(
        (error) => error as Error & { code?: string },
      );
      if (!(timeoutError instanceof Error)) {
        throw new Error('expected deletion custody timeout to reject');
      }
      expect(timeoutError).toMatchObject({ code: 'quiesce_failed' });
      expect(timeoutError.message).toContain('stale-receive custody reservation');
      expect(timeoutError.message).toContain('response handoff settlement');
      expect(timeoutError.message).not.toContain('runtime child');
      expect(order).toEqual([]);
      expect(h.readLastDeliveredSeq()).toBe('1\n');
      expect(h.consumedFor(2)).toHaveLength(0);
      expect(
        h.events.filter(
          (event) => event.event === 'runtime_delivery_failed' && event.delivery_ids.includes(2),
        ),
      ).toHaveLength(0);
      await expect(h.deliverSeq(3, 3)).rejects.toMatchObject({
        code: 'agent_deleting',
      });

      settle!(true);
      await waitForDeliveryConsumed(h.events, 2);
      await expect(
        runDeletionPurge(h.supervisor, order, () => {
          expect(h.readLastDeliveredSeq()).toBe('2\n');
          expect(h.consumedFor(2)).toHaveLength(1);
        }),
      ).resolves.toMatchObject({
        quiesced_count: 1,
      });
      expect(order).toEqual(['rm']);
      expect(h.consumedFor(2)).toHaveLength(1);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('core falsifier: 409 → pull takes the deferred batch atomically → the reservation holds it off the stdin path until settlement → turn_end releases nothing twice', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      const writesAfterBoot = h.stub.written.length;

      // Send from inside the (reserved) turn → 409 arms the stale event.
      const stale = await send(h.supervisor, 'draft');
      expect(stale).toMatchObject({ ok: false, status: 409 });

      // The unseen window arrives mid-turn → deferred, never written.
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');
      expect((await h.deliverSeq(3, 3)).disposition).toBe('deferred');
      expect(h.consumedFor(2)).toHaveLength(0);

      // PULL: takes the whole bounded FIFO prefix; the splice drains the
      // deferred queue → batch_queue_drained.
      const { result, settle } = await h.pull(1_000);
      expect(result.outcome).toBe('batch_queue_drained');
      if (result.outcome !== 'batch_queue_drained') throw new Error('unreachable');
      expect([...result.delivery_ids]).toEqual([2, 3]);
      expect(result.text).toContain('msg-2');
      expect(result.text).toContain('msg-3');
      expect(settle).toBeTypeOf('function');

      // Reservation unresolved: a turn_end flush must NOT release the
      // remainder or anything else — zero new stdin writes.
      h.drive([{ kind: 'turn_end' }]);
      await wait(30);
      expect(h.stub.written.length).toBe(writesAfterBoot);
      expect(h.consumedFor(2)).toHaveLength(0);

      // Transport handoff proven → exactly one consumed covering [2,3].
      settle!(true);
      await waitForDeliveryConsumed(h.events, 2);
      const consumed = h.events.filter(
        (e) =>
          e.event === 'runtime_delivery_consumed' &&
          (e.delivery_ids.includes(2) || e.delivery_ids.includes(3)),
      );
      expect(consumed).toHaveLength(1);

      // turn_end after settlement: the pulled batch is GONE from daemon
      // custody — nothing re-flushes, zero duplicate exposure. (Freshness
      // itself is a backend RuntimeScope guard now, covered by the backend
      // suite; the daemon's proof point is this no-double-release.)
      h.drive([{ kind: 'turn_end' }]);
      await wait(30);
      expect(h.stub.written.length).toBe(writesAfterBoot);
      expect(
        h.events.filter(
          (e) => e.event === 'runtime_delivery_consumed' && e.delivery_ids.includes(2),
        ),
      ).toHaveLength(1);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('handoff failure before the proof point: FIFO restored in place, zero consumed; the runtime still holds undelivered input so a retry still 409s; the next flush releases it exactly once', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');

      const { result, settle } = await h.pull(1_000);
      expect(result.outcome).toBe('batch_queue_drained');

      // Pre-proof failure → restore, no settlement of any kind.
      settle!(false);
      await wait(30);
      expect(h.consumedFor(2)).toHaveLength(0);
      expect(h.events.some((e) => e.event === 'runtime_delivery_failed')).toBe(false);

      // The restored row is undelivered again, so the runtime's field of
      // view is still stale: a non-bypass retry honestly 409s (freshness
      // is the backend's EXISTS guard; the mock re-arms).
      const retry = await send(h.supervisor, 'retry', 'unaffected');
      expect(retry).toMatchObject({ ok: false, status: 409 });

      // The restored batch releases through the NORMAL path at turn_end —
      // exactly once.
      h.drive([{ kind: 'turn_end' }]);
      await waitForDeliveryConsumed(h.events, 2);
      expect(h.consumedFor(2)).toHaveLength(1);
      expect(h.stub.written.filter((w) => w.includes('msg-2'))).toHaveLength(1);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('push wins the interleave: after the auto-flush admitted the head, a later pull finds the deferred queue drained and times out, taking nothing', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');

      // turn_end BEFORE any pull: the normal flush wins, writes + admits.
      h.drive([{ kind: 'turn_end' }]);
      await waitForDeliveryConsumed(h.events, 2);

      // The stale event is still armed (nothing resolved it), but the
      // queue is empty and no release is underway → the pull has nothing
      // takeable and nothing provable, so a zero-wait pull times out.
      // (`already_admitted` DIED with the per-stream observed cursor:
      // freshness is no longer a seq the daemon can report as "already
      // covered".)
      const { result, settle } = await h.pull(0);
      expect(result.outcome).toBe('timeout');
      // No custody was taken → no handoff settlement was registered.
      expect(settle).toBeUndefined();
      expect(h.consumedFor(2)).toHaveLength(1);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('release-in-progress is never stolen: a covering write in flight yields normal_delivery_in_progress and the batch stays on the stdin path', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      // Close the boot turn so the next delivery takes the ADMIT arm —
      // that path sets `startupDelivery` and returns while the stdin
      // write is still in flight, the observable release-in-progress
      // window. (The gated FLUSH path performs its write inside the
      // lifecycle lock, so a pull can never observe that variant.)
      h.drive([{ kind: 'turn_end' }]);
      await wait(10);
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);

      h.stub.parkWrites = true;
      expect((await h.deliverSeq(2, 2)).disposition).toBe('admitted');
      await waitFor(() => h.stub.pendingWriteCb !== null);

      const { result, settle } = await h.pull(0);
      expect(result.outcome).toBe('normal_delivery_in_progress');
      expect(settle).toBeUndefined();
      expect(h.consumedFor(2)).toHaveLength(0);

      // The parked write settles through the NORMAL path — exactly once.
      h.stub.resolveParkedWrite();
      await waitForDeliveryConsumed(h.events, 2);
      expect(h.consumedFor(2)).toHaveLength(1);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('a bounded prefix that does not drain the queue reports batch_more_pending; the next pull drains the remainder → batch_queue_drained', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      // #task:2095 F12: "more pending" is now a property of the daemon's
      // deferred FIFO depth, not a backend head beyond some coverage. Defer
      // one more than the bounded-prefix cap (DEFERRED_BATCH_MAX_ENTRIES =
      // 20) so the pull's splice CANNOT drain the whole queue in one take.
      const ids = Array.from({ length: 21 }, (_, i) => i + 2); // 2..22
      for (const id of ids) {
        expect((await h.deliverSeq(id, id)).disposition).toBe('deferred');
      }

      // First pull takes the bounded FIFO prefix (20); one entry remains,
      // so the queue is NOT drained → batch_more_pending.
      const first = await h.pull(1_000);
      expect(first.result.outcome).toBe('batch_more_pending');
      if (first.result.outcome !== 'batch_more_pending') {
        throw new Error('unreachable');
      }
      expect([...first.result.delivery_ids]).toEqual(ids.slice(0, 20));
      first.settle!(true);
      await waitForDeliveryConsumed(h.events, 2);

      // The stale event is still armed (no send resolved it); a second pull
      // drains the lone remainder → batch_queue_drained.
      const second = await h.pull(1_000);
      expect(second.result.outcome).toBe('batch_queue_drained');
      if (second.result.outcome !== 'batch_queue_drained') {
        throw new Error('unreachable');
      }
      expect([...second.result.delivery_ids]).toEqual([22]);
      second.settle!(true);
      await waitForDeliveryConsumed(h.events, 22);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('no_pending_stale is a typed no-op; timeout changes neither custody nor the stale event', async () => {
    const h = makePullHarness();
    const postedBodies = h.stubBackendFreshnessGuard();
    try {
      await h.boot();

      // No stale armed → typed no-op, nothing registered.
      const noop = await h.pull(0);
      expect(noop.result).toEqual({ ok: true, outcome: 'no_pending_stale' });
      expect(noop.settle).toBeUndefined();

      // Arm the stale; the queue stays empty → bounded wait → timeout.
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      const before = h.events.length;
      const { result, settle } = await h.pull(50);
      expect(result.outcome).toBe('timeout');
      expect(settle).toBeUndefined();
      // Zero settlement events of any kind were emitted by the pull.
      expect(
        h.events
          .slice(before)
          .filter(
            (e) => e.event === 'runtime_delivery_consumed' || e.event === 'runtime_delivery_failed',
          ),
      ).toHaveLength(0);

      // The stale event survives the timeout: a bare retry still carries
      // the SAME stale_event_id (daemon state unchanged).
      const retry = await send(h.supervisor, 'retry');
      expect(retry).toMatchObject({ ok: false, status: 409 });
      expect(postedBodies.at(-1)?.['stale_event_id']).toBe('stale-1');
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('a waiting pull is woken by the arrival instead of polling: deliver mid-wait fulfills with the batch', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);

      // Start the pull FIRST (empty queue) — it must park as the waiter.
      const pending = h.pull(2_000);
      await wait(20);
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');

      const { result, settle } = await pending;
      expect(result.outcome).toBe('batch_queue_drained');
      settle!(true);
      await waitForDeliveryConsumed(h.events, 2);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it("one pull per handle: a rejected second call cannot clear the first call's waiter — the first fulfills on ARRIVAL, far before its deadline (P1-3)", async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);

      // Window is 10s — far beyond this test's budget, so a pass can only
      // come from the arrival wake, never from the timeout-fallback
      // re-check that made the pre-fix version falsely green.
      const first = h.pull(10_000);
      await wait(20); // first is parked as the waiter
      await expect(h.pull(0)).rejects.toMatchObject({
        code: 'pull_already_active',
      });

      // The first call is unaffected: the rejected call's cleanup ran,
      // and the ARRIVAL must still fulfill promptly (call-token keyed
      // waiter — the second call cannot have cleared it).
      const deliveredAt = Date.now();
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');
      const { result, settle } = await first;
      expect(Date.now() - deliveredAt).toBeLessThan(1_500);
      expect(result.outcome).toBe('batch_queue_drained');
      settle!(true);
      await waitForDeliveryConsumed(h.events, 2);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('an ingress that cannot prove the response handoff is refused fail-closed before any custody decision', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');

      await expect(
        h.supervisor.agentStaleReceive(
          {
            runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001' },
            agent_id: 'a1',
            stale_event_id: 'stale-1',
            wait_ms: 0,
          },
          { owner: 'outbound:1' },
        ),
      ).rejects.toMatchObject({ code: 'handoff_unsupported' });

      // Custody untouched: the batch still flushes normally.
      h.drive([{ kind: 'turn_end' }]);
      await waitForDeliveryConsumed(h.events, 2);
      expect(h.consumedFor(2)).toHaveLength(1);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('a delivery arriving while the reservation is unresolved defers behind it (no later release), and flushes only after settlement', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      const writesAfterBoot = h.stub.written.length;
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');

      const { result, settle } = await h.pull(1_000);
      expect(result.outcome).toBe('batch_queue_drained');

      // Arrives during the reservation window — and, adversarially, with
      // the boundary OPEN (turn_end already parsed): must still defer.
      h.drive([{ kind: 'turn_end' }]);
      await wait(10);
      expect((await h.deliverSeq(4, 4)).disposition).toBe('deferred');
      await wait(20);
      expect(h.stub.written.length).toBe(writesAfterBoot);

      // Settlement re-drives the flush: seq 4 now releases via stdin.
      settle!(true);
      await waitForDeliveryConsumed(h.events, 4);
      expect(h.consumedFor(4)).toHaveLength(1);
      expect(h.stub.written.filter((w) => w.includes('msg-4'))).toHaveLength(1);
      // The pulled batch itself was consumed exactly once and never
      // written to stdin.
      expect(h.consumedFor(2)).toHaveLength(1);
      expect(h.stub.written.filter((w) => w.includes('msg-2'))).toHaveLength(0);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('P1-1 owner disconnect: a re-driven copy of a RESERVED id-set never enqueues — one consumed, zero stdin exposure of the pulled bodies', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      const writesAfterBoot = h.stub.written.length;
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2, 'owner-A')).disposition).toBe('deferred');

      const { result, settle } = await h.pull(1_000);
      expect(result.outcome).toBe('batch_queue_drained');

      // Old owner's connection drops mid-reservation: backend rolls the
      // rows back and re-drives the SAME ids under a new connection. The
      // cleanup must not touch the reservation; the re-drive must dedup
      // against it instead of queuing a second copy.
      h.supervisor.cleanupDeferredForOwner('owner-A');
      expect((await h.deliverSeq(2, 2, 'owner-B')).disposition).toBe('deferred');

      settle!(true);
      await waitForDeliveryConsumed(h.events, 2);
      expect(h.consumedFor(2)).toHaveLength(1);

      // turn_end after settlement: the re-driven copy must NOT release
      // via stdin — the bodies were already exposed once as the tool
      // result. Zero new writes, no second consumed.
      h.drive([{ kind: 'turn_end' }]);
      await wait(30);
      expect(h.stub.written.length).toBe(writesAfterBoot);
      expect(h.consumedFor(2)).toHaveLength(1);
      expect(h.stub.written.filter((w) => w.includes('msg-2'))).toHaveLength(0);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('response-loss ownership union: reservation partial overlap rebinds held id and queues only novel FIFO remainder', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      const writesAfterBoot = h.stub.written.length;
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2, 'owner-A')).disposition).toBe('deferred');
      const { result, settle } = await h.pull(1_000);
      expect(result.outcome).toBe('batch_queue_drained');

      h.supervisor.cleanupDeferredForOwner('owner-A');
      const presentations = [2, 3].map((id) => ({
        body: `msg-${id}`,
        instruction: 'other' as const,
      }));
      const retry = await h.supervisor.deliver(
        {
          runtime_generation: 1,
          conversation_id: 'Conversation0001',
          target: { conversation_id: 'Conversation0001', workspace: null },
          agent_id: 'a1',
          delivery_ids: [2, 3],
          kind: 'message',
          text: composeRuntimeDeliveryPresentation(presentations, 'silverhand message send'),
          deliveries: [2, 3].map((id, index) => ({
            delivery_id: id,
            seq: id,
            presentation: presentations[index]!,
          })),
          spawn_options: baseStartParams.options,
        },
        { owner: 'owner-B' },
      );
      expect(retry.disposition).toBe('deferred');

      // Reservation handoff owns/consumes 2 exactly once and never writes it.
      settle!(true);
      await waitForDeliveryConsumed(h.events, 2);
      expect(h.consumedFor(2)).toHaveLength(1);
      expect(h.stub.written.filter((write) => write.includes('msg-2'))).toHaveLength(0);

      // Novel 3 remains behind the reservation, then releases once at the
      // next proven boundary. FIFO is reservation 2 → deferred 3.
      h.drive([{ kind: 'turn_end' }]);
      await waitForDeliveryConsumed(h.events, 3);
      expect(h.consumedFor(3)).toHaveLength(1);
      expect(h.stub.written.length).toBe(writesAfterBoot + 1);
      expect(h.stub.written.filter((write) => write.includes('msg-3'))).toHaveLength(1);
      expect(h.stub.written.filter((write) => write.includes('msg-2'))).toHaveLength(0);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('P1-1 restore variant: after an owner rebind, a pre-proof failure restores the prefix under the NEW owner and it releases exactly once via stdin', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2, 'owner-A')).disposition).toBe('deferred');

      const { result, settle } = await h.pull(1_000);
      expect(result.outcome).toBe('batch_queue_drained');

      h.supervisor.cleanupDeferredForOwner('owner-A');
      expect((await h.deliverSeq(2, 2, 'owner-B')).disposition).toBe('deferred');

      // Handoff fails before the proof point → restore in place. The
      // restored entry now belongs to owner-B (the DB's current
      // in-flight owner), so a LATER owner-A cleanup cannot discard it.
      settle!(false);
      await wait(10);
      h.supervisor.cleanupDeferredForOwner('owner-A');

      h.drive([{ kind: 'turn_end' }]);
      await waitForDeliveryConsumed(h.events, 2);
      expect(h.consumedFor(2)).toHaveLength(1);
      expect(h.stub.written.filter((w) => w.includes('msg-2'))).toHaveLength(1);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it("P1-2 settlement-before-send: with the response in the peer's hands and the server write callback NOT yet executed, a resend BLOCKS on the settlement barrier, then lands", async () => {
    const h = makePullHarness();
    const postedBodies = h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');
      expect((await h.deliverSeq(3, 3)).disposition).toBe('deferred');

      // `result` returned = the response bytes are (as far as this
      // process can know) in the peer's hands. The write callback has
      // deliberately NOT run — this is exactly the cross-process
      // interleave the kernel permits: the CLI can react before the
      // daemon's event loop executes the callback. Lock-chain position
      // proves nothing in this state.
      const { result, settle } = await h.pull(1_000);
      expect(result.outcome).toBe('batch_queue_drained');

      // The resend must BLOCK on the reservation's `settled` barrier —
      // not race the un-queued settlement, and not sneak through on Node
      // scheduling habits. (An `--anyway` resend so it lands once the
      // barrier releases; the barrier itself is unconditional on the
      // pending reservation, freshness aside.)
      let retryResolved: Awaited<ReturnType<typeof send>> | null = null;
      const retry = send(h.supervisor, 'forced draft', undefined, true).then((r) => {
        retryResolved = r;
        return r;
      });
      await wait(100);
      expect(retryResolved).toBeNull();

      // The callback finally runs (flush/error/destroy always fires it);
      // settlement completes and releases the barrier.
      settle!(true);
      const retryResult = await retry;
      expect(retryResult).toMatchObject({ ok: true, message_id: 'm-ok' });
      // The resend paired itself with the armed conflict and carried the
      // human bypass — the only freshness field the daemon forwards now.
      expect(postedBodies.at(-1)?.['stale_event_id']).toBe('stale-1');
      expect(postedBodies.at(-1)?.['freshness_bypass']).toBe(true);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('P1-2 companion: a failed handoff blocks resend until settlement, then forwards the backend rejection', async () => {
    const h = makePullHarness();
    const postedBodies = h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');

      const { result, settle } = await h.pull(1_000);
      expect(result.outcome).toBe('batch_queue_drained');

      const retry = send(h.supervisor, 'retry', 'unaffected');
      await wait(50);
      expect(postedBodies).toHaveLength(1);
      settle!(false); // handoff failed: content never reached the peer
      const retryResult = await retry;
      expect(retryResult).toMatchObject({ ok: false, status: 409 });
      expect(postedBodies).toHaveLength(2);
      expect(postedBodies[1]?.['stale_event_id']).toBe('stale-1');
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('P1-4 cancel leg: closing the issuing control connection unregisters the waiter promptly (zero custody) and frees the slot for the next pull', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);

      // Waiter parked with a 10s window under the control connection's
      // owner token; the connection then closes (Ctrl-C / CLI exit).
      const cancelledAt = Date.now();
      const parked = h.pull(10_000);
      await wait(20);
      h.supervisor.cleanupDeferredForOwner('control:test');
      const { result, settle } = await parked;
      // Aborts with the timeout arm's exact statement — nothing obtained,
      // custody and the stale event unchanged — well before the window.
      expect(result.outcome).toBe('timeout');
      expect(settle).toBeUndefined();
      expect(Date.now() - cancelledAt).toBeLessThan(1_500);

      // The waiter slot is free: an immediate retry registers instead of
      // failing pull_already_active, and fulfills on arrival.
      const next = h.pull(5_000);
      await wait(20);
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');
      const fulfilled = await next;
      expect(fulfilled.result.outcome).toBe('batch_queue_drained');
      fulfilled.settle!(true);
      await waitForDeliveryConsumed(h.events, 2);
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('P1-4 pre-registration cancel (round-3): a pull still QUEUED behind the lifecycle lock when its connection closes never registers a waiter and never squats the slot', async () => {
    const h = makePullHarness();
    h.stubBackendFreshnessGuard();
    try {
      await h.boot();
      expect((await send(h.supervisor, 'draft')).ok).toBe(false);

      // A real deferred flush owns the lifecycle lock while its stdin
      // callback is pending. No private key or lock implementation is used.
      expect((await h.deliverSeq(2, 2)).disposition).toBe('deferred');
      h.stub.parkWrites = true;
      h.drive([{ kind: 'turn_end' }]);
      await waitFor(() => h.stub.pendingWriteCb !== null);
      expect(h.consumedFor(2)).toHaveLength(0);

      const cancelledAt = Date.now();
      const queued = h.pull(10_000); // queued behind the parked lock
      await wait(20);
      // Connection closes while the call is still pre-registration: a
      // waiter scan alone sees nothing to cancel here.
      h.supervisor.cleanupDeferredForOwner('control:test');
      h.stub.parkWrites = false;
      h.stub.resolveParkedWrite();
      await waitForDeliveryConsumed(h.events, 2);

      const { result, settle } = await queued;
      expect(result.outcome).toBe('timeout');
      expect(settle).toBeUndefined();
      expect(Date.now() - cancelledAt).toBeLessThan(1_500);

      // The dead call registered nothing: a fresh pull takes the slot
      // immediately and fulfills on arrival.
      const next = h.pull(5_000);
      await wait(20);
      expect((await h.deliverSeq(3, 3)).disposition).toBe('deferred');
      const fulfilled = await next;
      expect(fulfilled.result.outcome).toBe('batch_queue_drained');
      fulfilled.settle!(true);
      await waitForDeliveryConsumed(h.events, 3);
    } finally {
      h.stub.parkWrites = false;
      h.stub.resolveParkedWrite();
      vi.unstubAllGlobals();
      await h.stop();
    }
  });

  it('验收⑥ F17: a name-form send that 409s arms the backend-resolved target; stale-resolve POSTs THAT conversation id verbatim as its authz subject', async () => {
    const h = makePullHarness();
    // The backend resolves the name-form target to a conversation id ONLY it
    // knows. The daemon holds no address resolver, so without the 409 carrying
    // `target_conversation_id` (F17) the follow-up stale-resolve would have no
    // authorization subject to POST — the silent breakage this test pins.
    const RESOLVED = 'BackendResolved99';
    const resolvePosts: Record<string, unknown>[] = [];
    vi.stubGlobal(
      'fetch',
      vi.fn(async (url: string, init?: RequestInit) => {
        const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
        if (String(url).endsWith('/stale-message-resolution')) {
          resolvePosts.push(body);
          return new Response(JSON.stringify({ resolved: true }), {
            status: 200,
            headers: { 'content-type': 'application/json' },
          });
        }
        // send-message: a name-form target 409s, and only the backend knows
        // which conversation the name denoted.
        return new Response(
          JSON.stringify({
            error: {
              code: 'conflict',
              message: 'message stale',
              details: {
                subkind: 'message_stale',
                stale_event_id: 'stale-9',
                target_conversation_id: RESOLVED,
              },
            },
          }),
          { status: 409, headers: { 'content-type': 'application/json' } },
        );
      }),
    );
    try {
      await h.boot();
      // A name-form target the daemon forwards verbatim and never pre-resolves.
      const stale = await h.supervisor.agentMessageSend({
        runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001' },
        target: '#some-channel-by-name',
        agent_id: 'a1',
        content: 'draft',
      });
      expect(stale).toMatchObject({ ok: false, status: 409 });

      const resolved = await h.supervisor.agentStaleResolve({
        runtime_scope: { kind: 'conversation', conversation_id: 'Conversation0001' },
        agent_id: 'a1',
        stale_event_id: 'stale-9',
        resolution_reason: 'obsolete',
      });
      expect(resolved).toEqual({ ok: true, resolved: true });

      // F17: the resolve POST's authz subject is the 409's
      // `target_conversation_id`, fed VERBATIM — the daemon had no other
      // source for it.
      expect(resolvePosts).toHaveLength(1);
      expect(resolvePosts[0]).toMatchObject({
        conversation_id: RESOLVED,
        stale_event_id: 'stale-9',
        resolution_reason: 'obsolete',
      });
    } finally {
      vi.unstubAllGlobals();
      await h.stop();
    }
  });
});

// ===========================================================================
// CONTRACT (Do Not Weaken) — the spawn path is the ONLY version source,
// #task:1909
//
// WHY THIS EXISTS: shiki's constraint is explicit — no spawn-time capability
// parsing and no added startup latency. The version therefore has to come
// from the runtime detection the spawn path ALREADY awaits to find the
// binary (`resolveRuntime`, formerly `resolveBinary`, which used to throw the
// probed version away). If a future change re-introduces a per-spawn probe,
// these tests are what makes that visible.
//
// INVARIANTS:
//  1. Exactly ONE runtime resolution per spawn, and no extra child process is
//     launched to ask for a version (`--version` never appears in a spawn).
//  2. The probed version reaches the driver via `bindRuntimeVersion` BEFORE
//     anything reads the driver's capabilities — `buildArgs` is the first
//     driver call the supervisor makes after resolution, so it is the
//     observation point.
//  3. The binding is per-spawn (driver instances are per-spawn), i.e. the
//     capability is pinned to the process generation. Never re-read live
//     detection at a later boundary decision.
// ===========================================================================
describe('Supervisor — runtime version binding on the spawn path (#task:1909)', () => {
  it('resolves the runtime once and binds the probed version before buildArgs', async () => {
    let resolveCalls = 0;
    const boundVersions: Array<string | null> = [];
    const versionAtBuildArgs: Array<string | null | undefined> = [];
    const spawnedArgs: string[][] = [];
    let seen: string | null | undefined;

    const base = makeFakeDriver(['--echo-stdin']);
    const driver: Driver = {
      ...base,
      bindRuntimeVersion: (version) => {
        boundVersions.push(version);
        seen = version;
      },
      buildArgs: (ctx) => {
        versionAtBuildArgs.push(seen);
        return base.buildArgs(ctx);
      },
    };

    const spy = makeEmitSpy();
    const supervisor = new Supervisor({
      emit: spy.emit,
      logger: silentLogger(),
      driverFor: () => driver,
      resolveRuntime: async (_kind, fallback) => {
        resolveCalls += 1;
        return { binary: fallback, version: '1.1.17' };
      },
      spawnFn: (bin, args, options) => {
        spawnedArgs.push([...args]);
        return spawn(execPath, [bin, ...args.slice(1)], options);
      },
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
      }),
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
      sigtermToSigkillMs: 50,
      sigkillTimeoutMs: 200,
    });

    try {
      const res = await supervisor.start(baseStartParams);
      expect(res.started).toBe(true);

      // One resolution, one binding — and the version was already bound by
      // the time the supervisor asked the driver for spawn args.
      expect(resolveCalls).toBe(1);
      expect(boundVersions).toEqual(['1.1.17']);
      expect(versionAtBuildArgs).toEqual(['1.1.17']);
      // No side probe: nothing on the spawn path shells out for a version.
      expect(spawnedArgs.flat()).not.toContain('--version');
    } finally {
      await supervisor.stop({
        runtime_scope: {
          kind: 'conversation',
          conversation_id: 'Conversation0001',
          agent_id: 'a1',
        },
        runtime_generation: 1,
        mode: 'hard',
      });
    }
  });

  it('binds null when detection could not read a version (drivers fail closed)', async () => {
    const boundVersions: Array<string | null> = [];
    const base = makeFakeDriver();
    const driver: Driver = {
      ...base,
      bindRuntimeVersion: (version) => boundVersions.push(version),
    };

    const spy = makeEmitSpy();
    const supervisor = new Supervisor({
      emit: spy.emit,
      logger: silentLogger(),
      driverFor: () => driver,
      resolveRuntime: async (_kind, fallback) => ({
        binary: fallback,
        version: null,
      }),
      spawnFn: (bin, args, options) => spawn(execPath, [bin, ...args.slice(1)], options),
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
      }),
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
      sigtermToSigkillMs: 50,
      sigkillTimeoutMs: 200,
    });

    try {
      expect((await supervisor.start(baseStartParams)).started).toBe(true);
      expect(boundVersions).toEqual([null]);
    } finally {
      await supervisor.stop({
        runtime_scope: {
          kind: 'conversation',
          conversation_id: 'Conversation0001',
          agent_id: 'a1',
        },
        runtime_generation: 1,
        mode: 'hard',
      });
    }
  });

  it('pins the resolved profile per spawn: a qoder respawn re-resolves from its own version', async () => {
    // Real QoderDriver, one instance per spawn exactly like production
    // `getDriver`. The version changes between generations, so this is the
    // mid-session CLI upgrade: the already-resolved generation must keep its
    // profile, and only the NEW child may steer.
    const versions = ['1.1.16', '1.1.17'];
    const instances: QoderDriver[] = [];
    let generation = 0;

    const spy = makeEmitSpy();
    const supervisor = new Supervisor({
      emit: spy.emit,
      logger: silentLogger(),
      driverFor: () => {
        const qoder = new QoderDriver();
        instances.push(qoder);
        return qoder;
      },
      // Point at the echo fixture instead of `qodercli`; the driver's own
      // args are real (buildArgs reads the materialized prompt) but the
      // fixture ignores them.
      resolveRuntime: async () => ({
        binary: FIXTURE,
        version: versions[generation] ?? null,
      }),
      spawnFn: (bin, _args, options) => spawn(execPath, [bin], options),
      runtimeStartResolveDeps: testRuntimeStartResolveDeps({
        agentHomeRootDir: SUPERVISOR_AGENTS_ROOT,
      }),
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
      sigtermToSigkillMs: 50,
      sigkillTimeoutMs: 200,
    });

    const qoderStartParams = {
      conversation_id: 'Conversation0001',
      agent_id: 'a1',
      runtime_generation: 1,
      options: testStartOptions({
        conversationId: 'Conversation0001',
        agentId: 'a1',
        agentName: 'tester',
        runtime: 'qoder',
      }),
    };

    try {
      expect((await supervisor.start(qoderStartParams)).started).toBe(true);
      expect(instances).toHaveLength(1);
      expect(instances[0]!.releaseProfile).toBe('next-turn-batch');

      await supervisor.stop({
        runtime_scope: {
          kind: 'conversation',
          conversation_id: 'Conversation0001',
          agent_id: 'a1',
        },
        runtime_generation: 1,
        mode: 'hard',
      });
      generation = 1;
      expect((await supervisor.start(qoderStartParams)).started).toBe(true);
      expect(instances).toHaveLength(2);
      expect(instances[1]!.releaseProfile).toBe('tool-boundary-steer');
      // The retired generation is untouched — capability is pinned to the
      // process it was resolved for, never re-read.
      expect(instances[0]!.releaseProfile).toBe('next-turn-batch');
    } finally {
      await supervisor.stop({
        runtime_scope: {
          kind: 'conversation',
          conversation_id: 'Conversation0001',
          agent_id: 'a1',
        },
        runtime_generation: 1,
        mode: 'hard',
      });
    }
  });
});
