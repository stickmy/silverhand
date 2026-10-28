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
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createFileLogger } from '../src/node-file-logger.js';

let dir: string;
const day = (n: number) => `2026-10-${String(n).padStart(2, '0')}`;
const path = (n: number) => join(dir, `desktop.${day(n)}.log`);
const logger = () => createFileLogger({ dir, alsoConsole: false });

describe('daily file logger', () => {
  beforeEach(() => {
    const root = process.env.SILVERHAND_DIR;
    if (!root) throw new Error('Tests require an isolated SILVERHAND_DIR');
    mkdirSync(root, { recursive: true });
    dir = mkdtempSync(join(root, 'daily-logs-'));
    vi.useFakeTimers({ toFake: ['Date'] });
    vi.setSystemTime(new Date('2026-10-20T23:59:59.999Z'));
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.useRealTimers();
    rmSync(dir, { recursive: true, force: true });
  });

  it('keeps writing the old file if the next daily file cannot be opened', async () => {
    const log = logger();
    log.warn({}, 'before midnight');
    mkdirSync(path(21));
    const report = vi.spyOn(console, 'error').mockImplementation(() => {});
    vi.setSystemTime(new Date('2026-10-21T00:00:00Z'));
    log.warn({}, 'after failed rotation');
    log.warn({}, 'still writing');
    expect(log.filePath).toBe(path(20));
    expect(report).toHaveBeenCalledTimes(1);
    await log.close();
    expect(readFileSync(path(20), 'utf8')).toContain('after failed rotation');
    expect(readFileSync(path(20), 'utf8')).toContain('still writing');
  });

  it('appends across same-day restarts without losing earlier lines', async () => {
    for (const message of ['first boot', 'second boot']) {
      const log = logger();
      log.warn({}, message);
      await log.close();
    }
    expect(readdirSync(dir)).toEqual(['desktop.2026-10-20.log']);
    expect(
      readFileSync(path(20), 'utf8')
        .trim()
        .split('\n')
        .map((s) => JSON.parse(s).msg),
    ).toEqual(['first boot', 'second boot']);
  });

  it('expires old backend names by last write without touching Rust or audit logs', async () => {
    const old = ['backend-2026-05-01T00-00-00-000Z.log', 'backend.2026-09-01.log'];
    const protectedNames = [
      'rust-backend.2026-05-01.log',
      'cli-2026-05-01T00-00-00-000Z.log',
      'agent-audit-2026-05-01T00-00-00-000Z.log',
      'daemon-2026-05-01T00-00-00-000Z.log',
    ];
    for (const name of [...old, ...protectedNames]) {
      const file = join(dir, name);
      writeFileSync(file, 'old\n');
      utimesSync(file, new Date('2026-09-01Z'), new Date('2026-09-01Z'));
    }
    const recent = join(dir, 'backend-2026-05-02T00-00-00-000Z.log');
    writeFileSync(recent, 'old boot but recently written\n');
    utimesSync(recent, new Date(), new Date());
    const log = logger();
    log.warn({}, 'new desktop log');
    await log.close();
    for (const name of old) expect(readdirSync(dir)).not.toContain(name);
    for (const name of protectedNames) expect(readFileSync(join(dir, name), 'utf8')).toBe('old\n');
    expect(readFileSync(recent, 'utf8')).toContain('recently written');
    expect(readFileSync(path(20), 'utf8')).toContain('new desktop log');
  });

  it('rolls on the first persisted write after UTC midnight; children share the new sink', async () => {
    const log = logger();
    const child = log.child({ component: 'child' });
    log.warn({}, 'before midnight');
    vi.setSystemTime(new Date('2026-10-21T00:00:00Z'));
    log.debug({}, 'filtered');
    expect(log.filePath).toBe(path(20));
    child.error({ env_TOKEN: 'secret' }, 'after midnight');
    expect(log.filePath).toBe(path(21));
    expect(child.filePath).toBe(path(21));
    // Skip several idle days; no empty files should be made for them.
    vi.setSystemTime(new Date('2026-10-25T08:00:00Z'));
    log.warn({}, 'after idle');
    await child.close();
    await log.close();
    log.warn({}, 'late write');
    expect(readdirSync(dir).sort()).toEqual([20, 21, 25].map((n) => `desktop.${day(n)}.log`));
    expect(readFileSync(path(20), 'utf8')).toContain('before midnight');
    const next = JSON.parse(readFileSync(path(21), 'utf8'));
    expect(next.msg).toBe('after midnight');
    expect(next.component).toBe('child');
    expect(JSON.stringify(next)).not.toContain('secret');
    expect(readFileSync(path(25), 'utf8')).toContain('after idle');
    expect(readFileSync(path(25), 'utf8')).not.toContain('late write');
  });

  it('prunes only its daily files at startup and rollover, retaining 14 including today', async () => {
    for (let n = 1; n < 20; n++) writeFileSync(path(n), 'old\n');
    const untouched = [
      'daemon.2026-10-01.log',
      'rust-backend.2026-10-01.log',
      'cli-2026-10-01T00-00-00-000Z.log',
      'backend-2026-10-01T00-00-00-000Z.log',
      'desktop.notes.log',
    ];
    for (const name of untouched) writeFileSync(join(dir, name), 'keep\n');
    const log = logger();
    log.warn({}, 'today');
    // Startup cleanup counts the newly created file in its budget.
    expect(readdirSync(dir)).not.toContain('desktop.2026-10-06.log');
    expect(readFileSync(path(7), 'utf8')).toBe('old\n');
    vi.setSystemTime(new Date('2026-10-21T00:00:00Z'));
    log.warn({}, 'tomorrow');
    await log.close();
    expect(
      readdirSync(dir)
        .filter((name) => /^desktop\.\d{4}-\d{2}-\d{2}\.log$/.test(name))
        .sort(),
    ).toEqual(Array.from({ length: 14 }, (_, i) => `desktop.${day(i + 8)}.log`));
    for (const name of untouched) expect(readFileSync(join(dir, name), 'utf8')).toBe('keep\n');
  });
});
