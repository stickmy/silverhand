import { EventEmitter } from 'node:events';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync, writeSync } from 'node:fs';
import { join } from 'node:path';
import { afterEach, describe, expect, it, vi } from 'vitest';

const { spawn } = vi.hoisted(() => ({ spawn: vi.fn() }));
vi.mock('node:child_process', async (original) => ({
  ...(await original<typeof import('node:child_process')>()),
  spawn,
}));

import {
  explicitConnectRuntimePaths,
  parseConnectArgv,
  runConnectCommand,
} from '../src/daemon/connect-command.js';

let dir: string | undefined;
afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  spawn.mockReset();
  if (dir) rmSync(dir, { recursive: true, force: true });
});

describe('detached connect logging', () => {
  it('disables duplicate stderr logs and retains only the latest launch fallback output', async () => {
    const root = process.env.SILVERHAND_DIR;
    if (!root) throw new Error('Tests require an isolated SILVERHAND_DIR');
    mkdirSync(root, { recursive: true });
    dir = mkdtempSync(join(root, 'connect-logs-'));
    const paths = explicitConnectRuntimePaths(dir);
    mkdirSync(paths.logDir, { recursive: true });
    writeFileSync(paths.launcherLogPath, 'old accumulated output\n');
    const dailyPath = join(paths.logDir, 'daemon.2026-10-28.log');
    writeFileSync(dailyPath, 'daily diagnostics\n');
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ host_id: 'fixture-host' }))),
    );
    const stderr = vi.spyOn(process.stderr, 'write').mockImplementation(() => true);

    for (const failure of ['first startup error', 'second startup error']) {
      spawn.mockImplementation(
        (_executable, args: string[], options: { stdio: [string, number, number] }) => {
          expect(args).toContain('--quiet');
          expect(args).not.toContain('--stderr');
          expect(options.stdio[1]).toBe(options.stdio[2]);
          writeSync(options.stdio[2], `${failure}\n`);
          const child = Object.assign(new EventEmitter(), { pid: 12345, unref() {} });
          // No real daemon/runtime or network is started by this fixture.
          setTimeout(() => child.emit('exit', 1, null), 0);
          return child;
        },
      );
      const result = await runConnectCommand([
        '--server-url',
        'https://example.invalid',
        '--connect-token',
        'fixture-token',
        '--data-root',
        dir,
        '--stderr',
      ]);
      expect(result).toBe(1);
      expect(readFileSync(paths.launcherLogPath, 'utf8')).toBe(`${failure}\n`);
      expect(readFileSync(dailyPath, 'utf8')).toBe('daily diagnostics\n');
    }
    expect(stderr.mock.calls.map(([value]) => String(value)).join('')).toContain(
      paths.launcherLogPath,
    );
  });

  it('preserves the foreground terminal mirror and quiet opt-out', () => {
    const args = [
      '--server-url',
      'https://example.invalid',
      '--connect-token',
      'fixture',
      '--foreground',
    ];
    expect(parseConnectArgv(args)).toMatchObject({
      ok: true,
      options: { daemonMode: false, alsoStderr: true },
    });
    expect(parseConnectArgv([...args, '--quiet'])).toMatchObject({
      ok: true,
      options: { alsoStderr: false },
    });
  });
});
