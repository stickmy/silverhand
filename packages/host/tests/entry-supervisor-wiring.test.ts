/**
 * Daemon-mode boot smoke — verifies `packages/host/src/daemon/entry.ts`
 * actually wires a real supervisor into `runDaemon`.
 *
 * Backstory: Cindy `MlwnvOfs-L5Nv_x2` diagnosed the daemon-mode boot
 * blocker shiki hit running `SILVERHAND_RUNTIME_HOST=daemon pnpm dev`:
 * `entry.ts` was calling `runDaemon(...)` WITHOUT `supervisorFactory`,
 * so the IntentDispatcher booted with `supervisor: undefined` and
 * every `runtime_*` intent returned
 * `internal_error: runtime_* intent reached dispatcher with no
 * supervisor wired`. The fix is a one-liner — pass
 * `supervisorFactory: deps => new Supervisor(deps)` — but the
 * regression is sneaky (`pnpm dev` would still boot, agents would
 * just silently stop responding), so this test is the tripwire that
 * catches any future PR that drops the wire.
 *
 * Test shape (Cindy expanded review criteria `UBMs0leRXGvSPp7B`):
 *
 *   1. Spawn the REAL `entry.ts` as a subprocess via tsx — same path
 *      `spawnDaemonChild` exercises in production. In-process
 *      `runDaemon` would let us forget to test entry.ts itself.
 *   2. `runtime_status` returns `runtimes: []` instead of
 *      `internal_error`. That alone proves a supervisor is wired
 *      (the dispatcher's "no supervisor" throw fires for ANY
 *      `runtime_*` intent including status).
 *   3. The daemon log written during this boot contains NO payload
 *      / env / stdout / stderr field. Daemon log contract (Cindy
 *      `vhLGdqko0gaHrD_3` + `UOr7eZ8yA7COsyw9`) is structured-only;
 *      this regression guard makes sure neither Supervisor
 *      wiring nor entry.ts itself leaked a body field.
 *
 * Scope NOT covered here: a full `runtime_start → runtime_state`
 * roundtrip. That requires injecting a fake `driverFor` / `spawnFn`
 * into the supervisor, which entry.ts deliberately does not expose
 * (production callers must never get those seams). The behavioral
 * coverage of Supervisor under controlled spawn lives in
 * `supervisor.test.ts`; this file's job is the wire itself.
 */
import { afterEach, describe, expect, it } from 'vitest';
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { dirname, join, resolve } from 'node:path';
import { spawnDaemonChild, type DaemonHandle } from '../src/spawn.js';
import { HostClient } from '../src/client/index.js';

const HERE = dirname(fileURLToPath(import.meta.url));
const PACKAGE_ROOT = resolve(HERE, '..');
const TSX_BIN = join(PACKAGE_ROOT, 'node_modules', '.bin', 'tsx');
const DAEMON_ENTRY = resolve(HERE, '../src/daemon/entry.ts');

/** Field names that MUST NOT appear in any daemon JSONL line.
 *  Mirrors the daemon log contract: no payload, no env, no stdout /
 *  stderr chunk content, no text (Option A v2 —
 *  Cindy `UMGu36zoTDVuKWXi`: `runtime_wake.text` /
 *  `runtime_start.startup_delivery.text`
 *  carry raw user / agent message bytes the daemon will encode via
 *  its in-process driver; the daemon must never log the text). We
 *  grep on the JSON-key form (`"<name>":`) so unrelated occurrences
 *  in human-readable `msg` strings don't false-positive. */
const FORBIDDEN_LOG_KEYS = [
  '"payload":',
  '"env":',
  '"stdout":',
  '"stderr":',
  '"chunk":',
  '"soft_directive":',
  '"text":',
];

function readDaemonLogs(logDir: string): string {
  if (!existsSync(logDir)) return '';
  const files = readdirSync(logDir).filter((f) => /^daemon\.\d{4}-\d{2}-\d{2}\.log$/.test(f));
  return files.map((f) => readFileSync(join(logDir, f), 'utf-8')).join('\n');
}

describe('daemon entry — production supervisor wiring (Cindy `MlwnvOfs-L5Nv_x2`)', () => {
  const cleanup: Array<() => Promise<void> | void> = [];

  afterEach(async () => {
    while (cleanup.length) {
      const fn = cleanup.pop();
      try {
        await fn?.();
      } catch {
        /* ignore */
      }
    }
  });

  it('entry.ts wires a real supervisor — runtime_status returns success, not "no supervisor wired"', async () => {
    expect(
      existsSync(TSX_BIN),
      'Install the declared tsx dev dependency before running the daemon entry smoke',
    ).toBe(true);
    const dir = mkdtempSync(join(tmpdir(), 'silverhand-host-entry-wire-'));
    cleanup.push(() => rmSync(dir, { recursive: true, force: true }));
    const socketPath = join(dir, 'daemon.sock');
    const pidFilePath = join(dir, 'daemon.pid');
    const logDir = join(dir, 'logs');
    const homeDir = join(dir, 'home');
    const computersRootDir = join(dir, 'computers');
    const silverhandStateRootDir = join(dir, 'state');
    const launcher = {
      command: TSX_BIN,
      args: [DAEMON_ENTRY],
      env: { HOME: homeDir },
    };
    let handle: DaemonHandle | null = null;
    handle = await spawnDaemonChild({
      launcher,
      socketPath,
      pidFilePath,
      logDir,
      // #task:1598 round 4 — end-to-end proof that the explicit identity
      // coordinate survives spawn → argv → entry → runDaemon (no sibling
      // derivation left to fall back on).
      daemonIdentityPath: join(dir, 'run', 'identity.json'),
      computersRootDir,
      silverhandStateRootDir,
      alsoStderr: false,
      maxRestarts: 0,
      restartWindowMs: 1_000,
      readyTimeoutMs: 10_000,
    });
    cleanup.push(() => handle!.kill());
    expect(handle.pid).toBeGreaterThan(0);
    expect(handle.socketPath).toBe(socketPath);
    expect(existsSync(pidFilePath)).toBe(true);

    const client = new HostClient({ socketPath });
    cleanup.push(() => client.close());
    await client.connect();

    // The actual proof: runtime_status MUST succeed. Without the
    // supervisor wired the dispatcher throws
    // `runtime_* intent reached dispatcher with no supervisor wired`
    // and the call rejects with `internal_error`. With it wired,
    // Supervisor's status() returns `{runtimes: []}` because no
    // (conv, agent) has been started yet.
    const status = await client.runtimeStatus({});
    expect(status).toEqual({ runtimes: [] });

    // Sanity check that `ping` still works after a runtime_* call
    // (regression guard against accidentally breaking the PR-1
    // surface while wiring the supervisor).
    const pong = await client.ping();
    expect(pong.pong).toBe(true);
    expect(pong.daemon_pid).toBe(handle.pid);

    // The identity file materialized at the EXPLICIT forwarded path —
    // not beside the runtime-pid root, not under the sandbox home.
    expect(existsSync(join(dir, 'run', 'identity.json'))).toBe(true);
    expect(existsSync(join(homeDir, '.silverhand', 'daemon', 'identity.json'))).toBe(false);

    await client.close();
    await handle.kill({ gracefulMs: 2_000 });
    expect(existsSync(pidFilePath)).toBe(false);

    // Log contract: no payload/env/stdout/stderr/chunk
    // keys anywhere in the daemon JSONL written during this boot.
    // We check post-shutdown so the close-out / shutting_down lines
    // are also covered.
    const logs = readDaemonLogs(logDir);
    expect(logs).not.toBe(''); // sanity: we wrote SOMETHING
    for (const forbidden of FORBIDDEN_LOG_KEYS) {
      expect(
        logs.includes(forbidden),
        `daemon log contains forbidden key ${forbidden} — daemon log contract violation (Cindy \`vhLGdqko0gaHrD_3\`)`,
      ).toBe(false);
    }
    // Also belt-and-suspenders: the very error string from the
    // dispatcher's "no supervisor" throw must not appear. If it does,
    // entry.ts dropped the supervisor wire AND we somehow still
    // booted — fail loudly with the exact diagnostic Cindy used.
    expect(logs.includes('no supervisor wired')).toBe(false);
  }, 20_000);
});
