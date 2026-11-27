import { EventEmitter } from 'node:events';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { PassThrough } from 'node:stream';
import type { ChildProcess } from 'node:child_process';
import { afterEach, describe, expect, it } from 'vitest';
import { SILVERHAND_ACP_AUTO_COMPACT_ENV } from '@silverhand/shared';
import { Supervisor } from '../src/daemon/supervisor/index.js';
import { AntigravityDriver } from '../src/drivers/antigravity.js';
import { createDaemonLogControl } from '../src/daemon/logger.js';
import { testRuntimeStartResolveDeps, testStartOptions } from './_helpers/runtime-start-options.js';

const tempDirs: string[] = [];
afterEach(() => {
  for (const dir of tempDirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

function createFakeChild(pid: number) {
  const stdin = new PassThrough();
  const stdout = new PassThrough();
  const stderr = new PassThrough();
  const child = Object.assign(new EventEmitter(), {
    pid,
    stdin,
    stdout,
    stderr,
    killed: false,
    exitCode: null as number | null,
    kill() {
      child.killed = true;
      child.exitCode = 0;
      child.emit('exit', 0, null);
      child.emit('close', 0, null);
      return true;
    },
  });

  const written: any[] = [];
  let buf = '';
  stdin.on('data', (chunk) => {
    buf += chunk.toString();
    let idx: number;
    while ((idx = buf.indexOf('\n')) !== -1) {
      const line = buf.slice(0, idx).trim();
      buf = buf.slice(idx + 1);
      if (line) written.push(JSON.parse(line));
    }
  });

  const sendStdout = (payload: unknown) => {
    stdout.write(`${JSON.stringify(payload)}\n`);
  };

  return { child: child as unknown as ChildProcess, written, sendStdout };
}

async function waitUntil(predicate: () => boolean, timeoutMs = 2000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) {
      throw new Error('waitUntil timed out');
    }
    await new Promise((r) => setTimeout(r, 10));
  }
}

describe('Supervisor: agent user_env isolation, spawn-log redaction, and ACP auto-compact queuing', () => {
  it('isolates user_env to target agent, redacts marker from spawn logs, and queues messages delivered during /compact', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'silverhand-sup-env-'));
    tempDirs.push(dir);
    const resolveDeps = testRuntimeStartResolveDeps({
      agentHomeRootDir: join(dir, 'agents'),
    });

    const spawnedEnvs = new Map<string, NodeJS.ProcessEnv>();
    const children = new Map<string, ReturnType<typeof createFakeChild>>();
    const infoLogs: Array<Record<string, unknown>> = [];
    let nextPid = 4100;

    const supervisor = new Supervisor({
      emit: () => {},
      logger: {
        filePath: '/dev/null',
        logControl: createDaemonLogControl('info'),
        debug: () => {},
        info: (fields) => infoLogs.push(fields as Record<string, unknown>),
        warn: () => {},
        error: () => {},
        close: async () => {},
      },
      driverFor: () => new AntigravityDriver(),
      resolveRuntime: async () => ({ binary: '/usr/local/bin/agy', version: '1.5.18' }),
      runtimeStartResolveDeps: resolveDeps,
      controlPlaneServerUrl: () => 'http://127.0.0.1:8765',
      localSilverhandCliLauncher: { command: process.execPath, args: ['-e', ''] },
      spawnFn: (_binary, _args, options) => {
        const env = (options.env ?? {}) as NodeJS.ProcessEnv;
        const agentId = String(env.SILVERHAND_AGENT_ID ?? 'unknown');
        spawnedEnvs.set(agentId, env);
        const fake = createFakeChild(nextPid++);
        children.set(agentId, fake);
        return fake.child;
      },
    });

    const marker = 'benign-marker-only-target-3148';
    const targetOptions = testStartOptions({
      runtime: 'antigravity',
      agentId: 'agent-target',
      agentName: 'target',
      conversationId: 'conv-1',
    });
    targetOptions.env_policy = {
      silverhand: { auth_token: 'tok-target' },
      user_env: [
        { name: SILVERHAND_ACP_AUTO_COMPACT_ENV, value: '200000' },
        { name: 'CUSTOM_MARKER', value: marker },
        { name: 'NODE_OPTIONS', value: `--title=${marker}` },
        { name: 'PATH', value: `/opt/${marker}/bin:/usr/bin` },
      ],
    };

    const siblingOptions = testStartOptions({
      runtime: 'antigravity',
      agentId: 'agent-sibling',
      agentName: 'sibling',
      conversationId: 'conv-1',
    });

    await supervisor.start({
      conversation_id: 'conv-1',
      agent_id: 'agent-target',
      runtime_generation: 1,
      options: targetOptions,
    });
    await supervisor.start({
      conversation_id: 'conv-1',
      agent_id: 'agent-sibling',
      runtime_generation: 1,
      options: siblingOptions,
    });

    // Target child receives user_env; sibling child on the same host does not.
    const targetEnv = spawnedEnvs.get('agent-target')!;
    const siblingEnv = spawnedEnvs.get('agent-sibling')!;
    expect(targetEnv.CUSTOM_MARKER).toBe(marker);
    expect(targetEnv[SILVERHAND_ACP_AUTO_COMPACT_ENV]).toBe('200000');
    expect(siblingEnv.CUSTOM_MARKER).toBeUndefined();
    expect(siblingEnv[SILVERHAND_ACP_AUTO_COMPACT_ENV]).toBeUndefined();

    // Pre-spawn diagnostics never leak the custom env marker (not even via NODE_OPTIONS or PATH).
    const spawnAttemptLogs = infoLogs.filter((entry) => entry.action === 'runtime_spawn_attempt');
    expect(spawnAttemptLogs).toHaveLength(2);
    expect(JSON.stringify(spawnAttemptLogs)).not.toContain(marker);

    // Drive Antigravity handshake on target child.
    const fake = children.get('agent-target')!;
    await waitUntil(() => fake.written.length >= 1);
    fake.sendStdout({
      jsonrpc: '2.0',
      id: fake.written[0].id,
      result: {
        protocolVersion: 1,
        agentCapabilities: {
          loadSession: true,
          sessionCapabilities: { resume: {}, load: {} },
          _meta: { antigravity: { systemPrompt: { append: true } } },
        },
      },
    });
    await waitUntil(() => fake.written.length >= 2);
    fake.sendStdout({
      jsonrpc: '2.0',
      id: fake.written[1].id,
      result: { sessionId: 'sess-1' },
    });

    // Deliver turn 1, cross threshold mid-turn, then finish turn 1 -> triggers /compact.
    const d1 = await supervisor.deliver(
      {
        conversation_id: 'conv-1',
        agent_id: 'agent-target',
        runtime_generation: 1,
        target: { conversation_id: 'conv-1', workspace: null },
        delivery_ids: [1],
        kind: 'message',
        deliveries: [
          { delivery_id: 1, seq: 1, presentation: { body: 'first turn', instruction: 'other' } },
        ],
        text: 'first turn',
        spawn_options: targetOptions,
      },
      { owner: 'test-owner' },
    );
    expect(d1.disposition).toBe('admitted');
    await waitUntil(() => fake.written.some((m) => m.params?.prompt?.[0]?.text === 'first turn'));
    const turn1 = fake.written.find((m) => m.params?.prompt?.[0]?.text === 'first turn');

    fake.sendStdout({
      jsonrpc: '2.0',
      method: 'session/update',
      params: {
        sessionId: 'sess-1',
        update: { sessionUpdate: 'usage_update', used: 205_000, size: 1_048_576 },
      },
    });
    fake.sendStdout({ jsonrpc: '2.0', id: turn1.id, result: { stopReason: 'end_turn' } });
    await waitUntil(() => fake.written.some((m) => m.params?.prompt?.[0]?.text === '/compact'));
    const compactMsg = fake.written.find((m) => m.params?.prompt?.[0]?.text === '/compact');

    // Deliver two user messages while /compact is running: both must defer in FIFO order.
    const d2 = await supervisor.deliver(
      {
        conversation_id: 'conv-1',
        agent_id: 'agent-target',
        runtime_generation: 1,
        target: { conversation_id: 'conv-1', workspace: null },
        delivery_ids: [2],
        kind: 'message',
        deliveries: [
          {
            delivery_id: 2,
            seq: 2,
            presentation: { body: 'during compact 1', instruction: 'other' },
          },
        ],
        text: 'during compact 1',
        spawn_options: targetOptions,
      },
      { owner: 'test-owner' },
    );
    const d3 = await supervisor.deliver(
      {
        conversation_id: 'conv-1',
        agent_id: 'agent-target',
        runtime_generation: 1,
        target: { conversation_id: 'conv-1', workspace: null },
        delivery_ids: [3],
        kind: 'message',
        deliveries: [
          {
            delivery_id: 3,
            seq: 3,
            presentation: { body: 'during compact 2', instruction: 'other' },
          },
        ],
        text: 'during compact 2',
        spawn_options: targetOptions,
      },
      { owner: 'test-owner' },
    );
    expect(d2.disposition).toBe('deferred');
    expect(d3.disposition).toBe('deferred');
    expect(fake.written.some((m) => m.params?.prompt?.[0]?.text === 'during compact 1')).toBe(
      false,
    );

    // Finish /compact -> queued messages flush in order!
    fake.sendStdout({ jsonrpc: '2.0', id: compactMsg.id, result: { stopReason: 'end_turn' } });
    await waitUntil(() =>
      fake.written.some((m) => m.params?.prompt?.[0]?.text === 'during compact 2'),
    );
    const promptTexts = fake.written
      .filter((m) => m.method === 'session/prompt')
      .map((m) => m.params.prompt[0].text);
    expect(promptTexts).toEqual(['first turn', '/compact', 'during compact 1', 'during compact 2']);

    await supervisor.shutdown();
  });
});
