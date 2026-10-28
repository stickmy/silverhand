import {
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from 'node:fs';
import { join } from 'node:path';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { createDaemonLogControl, createDaemonLogger } from '../src/daemon/logger.js';
import { SILVERHAND_LOG_LEVEL_ENV } from '@silverhand/shared';

const dirs: string[] = [];

function tempDir(): string {
  const root = process.env.SILVERHAND_DIR;
  if (!root) throw new Error('Tests require an isolated SILVERHAND_DIR');
  mkdirSync(root, { recursive: true });
  const dir = mkdtempSync(join(root, 'silverhand-daemon-logger-'));
  dirs.push(dir);
  return dir;
}

function readLines(filePath: string): Array<Record<string, unknown>> {
  const text = readFileSync(filePath, 'utf8').trim();
  if (!text) return [];
  return text.split('\n').map((line) => JSON.parse(line) as Record<string, unknown>);
}

describe('daemon file logger levels', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.useRealTimers();
    while (dirs.length) {
      rmSync(dirs.pop()!, { recursive: true, force: true });
    }
  });

  it('owns daily retention without desktop, appends on restart, and rolls while running', async () => {
    vi.useFakeTimers({ toFake: ['Date'] });
    vi.setSystemTime(new Date('2026-10-20T23:59:59Z'));
    const dir = tempDir();
    for (let day = 1; day < 20; day++) {
      writeFileSync(join(dir, `daemon.2026-10-${String(day).padStart(2, '0')}.log`), 'old\n');
    }
    const foreign = join(dir, 'backend.2026-10-01.log');
    writeFileSync(foreign, 'desktop\n');
    const first = createDaemonLogger({ dir });
    first.warn({}, 'first boot');
    await first.close();
    expect(readdirSync(dir).filter((name) => name.startsWith('daemon.'))).toHaveLength(14);
    const second = createDaemonLogger({ dir });
    second.warn({}, 'second boot');
    vi.setSystemTime(new Date('2026-10-21T00:00:00Z'));
    second.warn({}, 'next day');
    await second.close();
    await second.close();
    expect(readLines(join(dir, 'daemon.2026-10-20.log')).map((line) => line.msg)).toEqual([
      'first boot',
      'second boot',
    ]);
    expect(second.filePath).toBe(join(dir, 'daemon.2026-10-21.log'));
    expect(readLines(second.filePath).map((line) => line.msg)).toEqual(['next day']);
    expect(
      readdirSync(dir)
        .filter((name) => name.startsWith('daemon.'))
        .sort(),
    ).toEqual(
      Array.from({ length: 14 }, (_, i) => `daemon.2026-10-${String(i + 8).padStart(2, '0')}.log`),
    );
    expect(readFileSync(foreign, 'utf8')).toBe('desktop\n');
  });

  it('writes info by default so daemon lifecycle diagnostics land on disk', async () => {
    const previous = process.env[SILVERHAND_LOG_LEVEL_ENV];
    delete process.env[SILVERHAND_LOG_LEVEL_ENV];
    try {
      const logger = createDaemonLogger({ dir: tempDir() });

      logger.info({ action: 'ready' }, 'daemon ready');
      await logger.close();

      const [line] = readLines(logger.filePath);
      expect(line?.msg).toBe('daemon ready');
      expect(typeof line?.ts).toBe('string');
      expect(line?.ts).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3} [+-]\d{2}:\d{2}$/);
      expect(typeof line?.ts_unix_ms).toBe('number');
    } finally {
      if (previous === undefined) {
        delete process.env[SILVERHAND_LOG_LEVEL_ENV];
      } else {
        process.env[SILVERHAND_LOG_LEVEL_ENV] = previous;
      }
    }
  });

  it('expires legacy daemon and activity files in its own directory after 14 idle days', async () => {
    vi.useFakeTimers({ toFake: ['Date'] });
    vi.setSystemTime(new Date('2026-10-20T00:00:00Z'));
    const dir = tempDir();
    const names = [
      'daemon-2026-05-01T00-00-00-000Z.log',
      'agent-activity-2026-05-01T00-00-00-000Z.log',
    ];
    for (const name of names) {
      writeFileSync(join(dir, name), 'old\n');
      utimesSync(join(dir, name), new Date('2026-10-06Z'), new Date('2026-10-06Z'));
    }
    const log = createDaemonLogger({ dir });
    // Exactly 14 days is retained; the next daily rotation expires it.
    for (const name of names) expect(readdirSync(dir)).toContain(name);
    vi.setSystemTime(new Date('2026-10-21T00:00:00Z'));
    log.warn({}, 'roll');
    await log.close();
    for (const name of names) expect(readdirSync(dir)).not.toContain(name);
  });

  it('mirrors the exact JSONL line to stderr when requested', async () => {
    const writes: string[] = [];
    vi.spyOn(process.stderr, 'write').mockImplementation((chunk) => {
      writes.push(String(chunk));
      return true;
    });
    const logger = createDaemonLogger({
      dir: tempDir(),
      alsoStderr: true,
      logControl: createDaemonLogControl('info'),
    });

    logger.warn({ action: 'diagnostic' }, 'daemon diagnostic');
    await logger.close();

    const diskLine = readFileSync(logger.filePath, 'utf8').trim();
    expect(writes).toEqual([`${diskLine}\n`]);
    expect(writes[0]).not.toContain('[daemon warn]');
  });

  it('filters info at warn level and lifts a single conversation during debug window', async () => {
    const control = createDaemonLogControl('warn');
    const logger = createDaemonLogger({
      dir: tempDir(),
      logControl: control,
    });

    logger.info({ conversationId: 'c1' }, 'hidden info');
    control.enableConversationDebug('c1', Date.now() + 60_000);
    logger.debug({ conversationId: 'c1' }, 'visible debug');
    logger.info({ conversationId: 'other' }, 'hidden other');
    logger.warn({ conversationId: 'other' }, 'visible warn');
    await logger.close();

    expect(readLines(logger.filePath).map((line) => line.msg)).toEqual([
      'visible debug',
      'visible warn',
    ]);
  });

  it('redacts content fields on disk while preserving structural diagnostics', async () => {
    const logger = createDaemonLogger({
      dir: tempDir(),
      logControl: createDaemonLogControl('debug'),
    });

    logger.debug(
      {
        conversationId: 'c1',
        chunk: 'SECRET-CHUNK',
        text: 'SECRET-TEXT',
        stderr: 'SECRET-STDERR',
        env_TOKEN: 'SECRET-ENV',
        payload: 'SECRET-PAYLOAD',
        body: 'SECRET-BODY',
        prompt: 'SECRET-PROMPT',
        line: 'SECRET-LINE',
        env: 'SECRET-ENV-OBJECT',
        stdout: 'SECRET-STDOUT',
        nested: { soft_directive: 'SECRET-DIRECTIVE', keep: 'visible' },
        ok: true,
      },
      'redaction check',
    );
    await logger.close();

    const serialized = JSON.stringify(readLines(logger.filePath)[0]);
    expect(serialized).toContain('"ok":true');
    expect(serialized).toContain('"keep":"visible"');
    expect(serialized).toContain('"redactedFieldCount":11');
    expect(serialized).not.toContain('SECRET-');
    expect(serialized).not.toContain('SECRET-CHUNK');
    expect(serialized).not.toContain('SECRET-TEXT');
    expect(serialized).not.toContain('SECRET-STDERR');
    expect(serialized).not.toContain('SECRET-ENV');
    expect(serialized).not.toContain('SECRET-DIRECTIVE');
    expect(serialized).not.toContain('"chunk"');
    expect(serialized).not.toContain('"text"');
    expect(serialized).not.toContain('"stderr"');
    expect(serialized).not.toContain('"env_TOKEN"');
    expect(serialized).not.toContain('"soft_directive"');
  });
});
