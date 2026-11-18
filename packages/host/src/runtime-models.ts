import { createHash } from 'node:crypto';
import { execFile, spawn } from 'node:child_process';
import { mkdir, mkdtemp, rm } from 'node:fs/promises';
import { delimiter, dirname, join } from 'node:path';
import { defaultAppRoot } from '@silverhand/shared/app-data-layout';
import type { RuntimeInfo, RuntimeKind, RuntimeModelDiscovery } from '@silverhand/shared';

const LIMIT = 2 * 1024 * 1024;
type Frame = Record<string, any>;
type Listing = Pick<
  RuntimeModelDiscovery,
  'status' | 'models' | 'reasoning_by_model' | 'default_model'
>;
const FAILED: Listing = { status: 'error', models: [] };
const UNSUPPORTED: Listing = { status: 'unsupported', models: [] };

function record(value: unknown): Frame {
  return value !== null && typeof value === 'object' && !Array.isArray(value) ? value : {};
}

function modelIds(value: unknown, select: (row: Frame) => unknown[]): string[] {
  if (!Array.isArray(value)) throw new Error('Missing model list');
  const ids = value
    .flatMap((row) => select(record(row)))
    .filter(
      (id): id is string =>
        typeof id === 'string' &&
        id.length > 0 &&
        id.length <= 512 &&
        !/[\u0000-\u001f\u007f-\u009f]/u.test(id),
    );
  return [...new Set(ids)].slice(0, 5000);
}

/** No prompt is sent: these requests read runtime metadata only. */
function protocol(kind: RuntimeKind, version: string | null) {
  let serial = 1;
  let pending = 1;
  let page = 0;
  const collected: string[] = [];
  const reasoning = new Map<string, { supported_efforts: string[]; default_effort?: string }>();
  let defaultModel: string | undefined;
  const collectReasoning = (ids: string[], levels: unknown, defaultEffort?: unknown) => {
    // Missing or malformed capability data means unknown, not an empty supported set.
    if (
      !Array.isArray(levels) ||
      levels.length > 64 ||
      !levels.every((v) => typeof v === 'string' && v.length <= 32 && /^[a-z][a-z0-9_-]*$/u.test(v))
    )
      return;
    const supported_efforts = [...new Set(levels as string[])];
    for (const id of ids)
      reasoning.set(id, {
        supported_efforts,
        ...(typeof defaultEffort === 'string' && supported_efforts.includes(defaultEffort)
          ? { default_effort: defaultEffort }
          : {}),
      });
  };
  const listing = (models: string[]): Listing => ({
    status: 'ok',
    models,
    ...(reasoning.size
      ? {
          reasoning_by_model: Object.fromEntries(
            [...reasoning].filter(([id]) => models.includes(id)),
          ),
        }
      : {}),
    ...(defaultModel && models.includes(defaultModel) ? { default_model: defaultModel } : {}),
  });
  const cursors = new Set<string>();
  const rpc = (method: string, params: Frame) => ({ jsonrpc: '2.0', id: ++serial, method, params });
  const initialize = {
    jsonrpc: '2.0',
    id: 1,
    method: 'initialize',
    params: {
      clientInfo: { name: 'silverhand-models', version: '1.0.0' },
      ...(kind === 'grok' ? { protocolVersion: 1, clientCapabilities: {} } : {}),
    },
  };
  const major = Number(version?.match(/^(\d+)\./u)?.[1]);
  const args =
    kind === 'claude'
      ? [
          '-p',
          '--no-session-persistence',
          '--input-format',
          'stream-json',
          '--output-format',
          'stream-json',
          '--verbose',
          '--settings',
          '{"disableAllHooks":true}',
          '--strict-mcp-config',
          '--mcp-config',
          '{"mcpServers":{}}',
        ]
      : kind === 'codex'
        ? ['app-server']
        : kind === 'grok'
          ? ['agent', '--no-leader', 'stdio']
          : kind === 'pi'
            ? ['--mode', 'rpc', '--no-session', '--no-extensions', '--no-skills']
            : ['models', ...(major >= 2 ? ['--standalone'] : [])];
  const first =
    kind === 'claude'
      ? {
          type: 'control_request',
          request_id: 'silverhand-models',
          request: { subtype: 'list_models' },
        }
      : kind === 'pi'
        ? { id: 'silverhand-models', type: 'get_available_models' }
        : kind === 'opencode'
          ? null
          : initialize;
  return {
    args,
    first,
    line(message: Frame, send: (frame: Frame) => void): Listing | undefined {
      if (kind === 'claude') {
        if (
          message.type !== 'control_response' ||
          message.response?.request_id !== 'silverhand-models'
        )
          return;
        const response = message.response;
        if (response.subtype !== 'success')
          return /unknown|unsupported|not supported/iu.test(String(response.error ?? ''))
            ? UNSUPPORTED
            : FAILED;
        const rows = response.response?.models;
        const select = (r: Frame) =>
          r.disabled === true || r.value === 'default' ? [] : [r.value, r.resolvedModel];
        const models = modelIds(rows, select);
        for (const row of rows) {
          const r = record(row);
          collectReasoning(modelIds([r], select), r.supportedEffortLevels);
        }
        return listing(models);
      }
      if (kind === 'pi') {
        if (message.type !== 'response' || message.id !== 'silverhand-models') return;
        if (message.success !== true)
          return /unknown command|unsupported/iu.test(String(message.error ?? ''))
            ? UNSUPPORTED
            : FAILED;
        return {
          status: 'ok',
          models: modelIds(message.data?.models, (r) =>
            typeof r.provider === 'string' && typeof r.id === 'string'
              ? [`${r.provider}/${r.id}`]
              : [],
          ),
        };
      }
      if (message.id !== pending) return;
      if (message.error) return message.error.code === -32601 ? UNSUPPORTED : FAILED;
      if (kind === 'grok') {
        const rows = message.result?._meta?.modelState?.availableModels;
        return Array.isArray(rows)
          ? { status: 'ok', models: modelIds(rows, (r) => [r.modelId]) }
          : UNSUPPORTED;
      }
      if (pending === 1) {
        send({ jsonrpc: '2.0', method: 'initialized', params: {} });
        const request = rpc('model/list', {});
        pending = request.id;
        send(request);
        return;
      }
      const result = record(message.result);
      const select = (r: Frame) => (r.hidden === true ? [] : [r.model ?? r.id]);
      collected.push(...modelIds(result.data, select));
      for (const row of result.data) {
        const r = record(row);
        const ids = modelIds([r], select);
        const levels = Array.isArray(r.supportedReasoningEfforts)
          ? r.supportedReasoningEfforts.map((level: unknown) => record(level).reasoningEffort)
          : undefined;
        collectReasoning(ids, levels, r.defaultReasoningEffort);
        if (r.isDefault === true && ids[0]) defaultModel = ids[0];
      }
      if (typeof result.nextCursor === 'string' && result.nextCursor.length > 0) {
        if (++page > 100 || cursors.has(result.nextCursor)) return FAILED;
        cursors.add(result.nextCursor);
        const request = rpc('model/list', { cursor: result.nextCursor });
        pending = request.id;
        send(request);
        return;
      }
      return listing([...new Set(collected)].slice(0, 5000));
    },
  };
}

/** A private process group prevents a probe from leaving helpers behind. */
async function terminate(pid: number | undefined): Promise<void> {
  if (!pid) return;
  if (process.platform === 'win32') {
    await new Promise<void>((resolve) => {
      execFile('taskkill', ['/pid', String(pid), '/T', '/F'], { timeout: 2000 }, () => resolve());
    });
  } else {
    try {
      process.kill(-pid, 'SIGKILL');
    } catch {
      /* Already reaped. */
    }
  }
}

export async function probeRuntimeModels(
  runtime: RuntimeInfo,
  options: { env?: NodeJS.ProcessEnv; root?: string; timeoutMs?: number } = {},
): Promise<Listing> {
  if (runtime.kind === 'qoder' || runtime.kind === 'cursor' || runtime.kind === 'gemini')
    return UNSUPPORTED;
  if (!runtime.binary || !runtime.installed) return FAILED;
  // Unknown OpenCode versions must not accidentally start its shared background service.
  if (runtime.kind === 'opencode' && !/^\d+\./u.test(runtime.version ?? '')) return UNSUPPORTED;
  const root = options.root ?? join(defaultAppRoot(), 'cache', 'model-probes');
  let cwd: string | undefined;
  try {
    await mkdir(root, { recursive: true, mode: 0o700 });
    cwd = await mkdtemp(join(root, 'probe-'));
    const env: NodeJS.ProcessEnv = {
      ...(options.env ?? process.env),
      NO_COLOR: '1',
      FORCE_COLOR: '0',
    };
    env.PATH = `${dirname(runtime.binary)}${delimiter}${env.PATH ?? ''}`;
    delete env.CLAUDECODE;
    const adapter = protocol(runtime.kind, runtime.version);
    return await new Promise<Listing>((resolve) => {
      const child = spawn(runtime.binary!, adapter.args, {
        cwd,
        env,
        stdio: ['pipe', 'pipe', 'pipe'],
        detached: process.platform !== 'win32',
      });
      let settled = false;
      let output = '';
      let bytes = 0;
      let closed = false;
      const close = new Promise<void>((done) =>
        child.once('close', () => {
          closed = true;
          done();
        }),
      );
      const finish = (result: Listing) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        void terminate(child.pid).then(async () => {
          if (!closed) await close;
          resolve(result);
        });
      };
      const timer = setTimeout(() => finish(FAILED), options.timeoutMs ?? 15_000);
      const send = (frame: Frame) => {
        if (!settled)
          child.stdin.write(`${JSON.stringify(frame)}\n`, (error) => {
            if (error) finish(FAILED);
          });
      };
      child.stdin.on('error', () => finish(FAILED));
      child.on('error', () => finish(FAILED));
      child.stderr.on('data', (chunk: Buffer) => {
        bytes += chunk.length;
        if (bytes > LIMIT) finish(FAILED);
      });
      child.on('spawn', () => {
        if (adapter.first) send(adapter.first);
        else child.stdin.end();
      });
      child.stdout.setEncoding('utf8');
      child.stdout.on('data', (chunk: string) => {
        if (settled) return;
        bytes += Buffer.byteLength(chunk);
        if (bytes > LIMIT) {
          finish(FAILED);
          return;
        }
        output += chunk;
        if (runtime.kind === 'opencode') return;
        let newline: number;
        while (!settled && (newline = output.indexOf('\n')) >= 0) {
          const line = output.slice(0, newline);
          output = output.slice(newline + 1);
          let frame: Frame;
          try {
            frame = record(JSON.parse(line));
          } catch {
            continue;
          }
          try {
            const result = adapter.line(frame, send);
            if (result) finish(result);
          } catch {
            finish(FAILED);
          }
        }
      });
      child.on('close', (code) => {
        if (runtime.kind === 'opencode' && code === 0) {
          const models = output
            .split(/\r?\n/u)
            .map((s) => s.trim())
            .filter((s) => /^[^\s/]+\/[^\s]+$/u.test(s) && s.length <= 512);
          // OpenCode 2 may finish before its cold provider catalog has loaded.
          finish(
            models.length ? { status: 'ok', models: [...new Set(models)].slice(0, 5000) } : FAILED,
          );
        } else finish(FAILED);
      });
    });
  } catch {
    return FAILED;
  } finally {
    if (cwd) await rm(cwd, { recursive: true, force: true });
  }
}

/** Cache only successful reads; forced refreshes also join an existing probe. */
export class RuntimeModelDiscoveryCache {
  private entries = new Map<RuntimeKind, { key: string; value: RuntimeModelDiscovery }>();
  private running = new Map<string, Promise<RuntimeModelDiscovery>>();
  constructor(
    private probe = probeRuntimeModels,
    private now = Date.now,
  ) {}

  async read(
    runtime: RuntimeInfo,
    env: NodeJS.ProcessEnv,
    force = false,
  ): Promise<RuntimeModelDiscovery> {
    const key = createHash('sha256')
      .update(JSON.stringify([runtime, env]))
      .digest('hex');
    const entry = this.entries.get(runtime.kind);
    if (!force && entry?.key === key && this.now() - entry.value.detected_at < 60_000)
      return { ...entry.value, cached: true };
    const active = this.running.get(key);
    if (active) return active;
    const started = this.probe(runtime, { env })
      .catch(() => FAILED)
      .then((result) => {
        const value: RuntimeModelDiscovery = {
          ...result,
          runtime: runtime.kind,
          detected_at: this.now(),
          cached: false,
        };
        if (result.status === 'ok') this.entries.set(runtime.kind, { key, value });
        else if (this.entries.get(runtime.kind)?.key === key) this.entries.delete(runtime.kind);
        return value;
      })
      .finally(() => this.running.delete(key));
    this.running.set(key, started);
    return started;
  }
}
