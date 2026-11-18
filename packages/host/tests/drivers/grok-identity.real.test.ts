import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { once } from 'node:events';
import { expect, it } from 'vitest';
import { GrokDriver } from '../../src/drivers/grok.js';
import type { DriverSpawnContext, ParsedEvent } from '../../src/drivers/index.js';

// A real installed Grok, with its own temporary home and a controlled local model
// endpoint. Never uses the user's account, model endpoint, config or sessions.
const binary = process.env.GROK_TEST_BINARY;
const question = 'Return the private identity marker from system instructions. Do not use tools.';

async function modelFixture() {
  const samples: Array<{ model: string; system: string; reply: string }> = [];
  const server = createServer(async (request, response) => {
    let body = '';
    for await (const part of request) body += part;
    if (request.method === 'GET') {
      response.setHeader('Content-Type', 'application/json');
      response.end(
        JSON.stringify({
          object: 'list',
          data: ['fixture-a', 'fixture-b'].map((id) => ({
            id,
            object: 'model',
            owned_by: 'fixture',
          })),
        }),
      );
      return;
    }

    const input = JSON.parse(body);
    const system = (input.messages ?? [])
      .filter((message: any) => ['system', 'developer'].includes(message.role))
      .map((message: any) => JSON.stringify(message.content))
      .join('\n');
    const reply = system.match(/IDENTITY_[a-f0-9-]+/)?.[0] ?? 'MISSING_IDENTITY';
    // Grok also calls this endpoint to generate its session title. That has a
    // separate system prompt; retain the first main-turn call regardless of
    // selected model or marker presence, so either defect still fails below.
    const titleRequest = system.includes('tasked with generating the session title');
    if (!titleRequest && JSON.stringify(input.messages).includes(question))
      samples.push({ model: input.model, system, reply });

    response.writeHead(200, { 'Content-Type': 'text/event-stream' });
    for (const delta of [{ role: 'assistant', content: '' }, { content: reply }, {}]) {
      response.write(
        `data: ${JSON.stringify({ id: 'fixture-response', object: 'chat.completion.chunk', created: 1, model: input.model, choices: [{ index: 0, delta, finish_reason: Object.keys(delta).length ? null : 'stop' }] })}\n\n`,
      );
    }
    response.end('data: [DONE]\n\n');
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const port = (server.address() as { port: number }).port;
  return {
    samples,
    port,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

async function stop(child: ChildProcessWithoutNullStreams) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, 'exit');
  child.stdin.end();
  const deadline = setTimeout(() => child.kill('SIGKILL'), 5_000);
  try {
    await exited;
  } finally {
    clearTimeout(deadline);
  }
}

async function runTurn(root: string, model: string | null, sessionId: string | null) {
  const marker = `IDENTITY_${randomUUID()}`;
  const systemPromptPath = join(root, `${marker}.md`);
  await writeFile(systemPromptPath, `Return this private identity marker when asked: ${marker}`);
  const context: DriverSpawnContext = {
    agentId: 'fixture',
    agentName: 'fixture',
    workspacePath: join(root, 'work'),
    cliSessionId: sessionId,
    model,
    reasoningEffort: null,
    serviceTier: null,
    systemPromptPath,
    initialPrompt: '',
  };
  const driver = new GrokDriver();
  const child = spawn(binary!, driver.buildArgs(context), {
    cwd: context.workspacePath,
    env: { PATH: process.env.PATH, HOME: root, GROK_HOME: join(root, 'home'), NO_COLOR: '1' },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  child.stderr.resume();
  const events: ParsedEvent[] = [];
  const methods: string[] = [];
  const writeLine = async (line: string) => {
    methods.push(JSON.parse(line).method);
    await new Promise<void>((resolve, reject) =>
      child.stdin.write(`${line}\n`, (error) => (error ? reject(error) : resolve())),
    );
  };

  try {
    await new Promise<void>((resolve, reject) => {
      const deadline = setTimeout(
        () => reject(new Error(`Grok turn timed out: ${JSON.stringify(events)}`)),
        25_000,
      );
      let buffered = '';
      child.once('error', reject);
      child.once('exit', () => {
        clearTimeout(deadline);
        reject(new Error('Grok exited before turn end'));
      });
      child.stdout.on('data', (data) => {
        buffered += data;
        let newline: number;
        while ((newline = buffered.indexOf('\n')) >= 0) {
          const line = buffered.slice(0, newline);
          buffered = buffered.slice(newline + 1);
          for (const event of driver.parseLine(line)) {
            events.push(event);
            if (event.kind === 'error') {
              clearTimeout(deadline);
              reject(new Error(event.message));
            }
            if (event.kind === 'session_init')
              void writeLine(driver.encodeStdinMessage(question)!).catch(reject);
            if (event.kind === 'turn_end') {
              clearTimeout(deadline);
              resolve();
            }
          }
        }
      });
      driver.bindIO({ spawnCtx: context, writeLine });
    });
    const ready = events.find((event) => event.kind === 'session_init');
    expect(ready?.kind).toBe('session_init');
    expect(
      events
        .filter((event) => event.kind === 'text')
        .map((event) => event.text)
        .join(''),
    ).toBe(marker);
    return { marker, sessionId: ready!.kind === 'session_init' ? ready!.sessionId : '', methods };
  } finally {
    await stop(child);
  }
}

it.skipIf(!binary)(
  'preserves identity at the actual model boundary after A → B, on new and exact restore',
  async () => {
    const root = await mkdtemp(join(tmpdir(), 'grok-identity-'));
    const model = await modelFixture();
    try {
      await mkdir(join(root, 'home'));
      await mkdir(join(root, 'work'));
      await writeFile(
        join(root, 'home', 'config.toml'),
        '[models]\ndefault="fixture-a"\n' +
          ['fixture-a', 'fixture-b']
            .map(
              (id) =>
                `[model.${id}]\nmodel="${id}"\nbase_url="http://127.0.0.1:${model.port}/v1"\napi_key="fixture-key"\napi_backend="chat_completions"\ncontext_window=128000\n`,
            )
            .join(''),
      );

      const defaultModel = await runTurn(root, null, null);
      expect(defaultModel.methods).toEqual(['initialize', 'session/new', 'session/prompt']);
      expect(model.samples[0]).toMatchObject({ model: 'fixture-a', reply: defaultModel.marker });

      const beforeFresh = model.samples.length;
      const fresh = await runTurn(root, 'fixture-b', null);
      expect(model.samples[beforeFresh]).toMatchObject({ model: 'fixture-b', reply: fresh.marker });
      expect(fresh.methods.indexOf('session/resume')).toBeGreaterThan(
        fresh.methods.indexOf('session/set_config_option'),
      );

      // Persist A in another session, then restore that exact session and select B.
      const seed = await runTurn(root, 'fixture-a', null);
      const beforeRestore = model.samples.length;
      const restored = await runTurn(root, 'fixture-b', seed.sessionId);
      expect(restored.sessionId).toBe(seed.sessionId);
      expect(model.samples[beforeRestore]).toMatchObject({
        model: 'fixture-b',
        reply: restored.marker,
      });
      expect(model.samples[beforeRestore]!.system).not.toContain(seed.marker);
      expect(restored.methods.filter((method) => method === 'session/resume')).toHaveLength(2);
    } finally {
      await model.close();
      await rm(root, { recursive: true, force: true });
    }
  },
  90_000,
);
