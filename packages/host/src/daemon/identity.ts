import {
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
import { dirname } from 'node:path';
import { defaultAppRoot, resolveStandaloneDaemonPaths } from '@silverhand/shared/app-data-layout';

import { HelloResultSchema, type HelloResult } from '../protocol.js';

export type DaemonIdentity = Pick<HelloResult, 'daemon_id' | 'host_descriptor'>;

const DaemonIdentitySchema = HelloResultSchema.pick({
  daemon_id: true,
  host_descriptor: true,
});

/**
 * Current single-daemon bootstrap identity. Multi-daemon installs will
 * get a launcher-provided daemon home later; until then `local` is the
 * stable daemon_id that future registry migrations can backfill against.
 */
export const DEFAULT_DAEMON_IDENTITY: DaemonIdentity = Object.freeze({
  daemon_id: 'local',
  host_descriptor: Object.freeze({ name: 'local', kind: 'local' }),
});

/**
 * Bare-daemon fallback only (#task:1598 round 4): embedders pass the
 * identity coordinate explicitly (`daemonIdentityPath` — desktop forwards
 * the resolver's `localComputer.run.identity` via `--daemon-identity-path`;
 * `connect` passes `null` and supplies the identity value directly). The
 * historical sibling derivation from `runtimePidRootDir` was a parent
 * inversion and is gone. Env-AWARE since the v2 cutover: every default
 * root consults SILVERHAND_DIR (contract §8; no released artifact ever
 * shipped the env-independent bare-daemon defaults).
 */
export function defaultDaemonIdentityPath(): string {
  return resolveStandaloneDaemonPaths(defaultAppRoot()).identity;
}

export function readOrCreateDaemonIdentity(filePath: string): DaemonIdentity {
  if (!existsSync(filePath)) {
    mkdirSync(dirname(filePath), { recursive: true });
    // Atomic create: write a sibling tmp file first, then rename. A
    // crash mid-write must never leave a half-written `identity.json`
    // behind because future boots will JSON.parse-fail on it and
    // throw before we even reach `claimPidFile`. The pidfile root next
    // door (`runtime-pids/`) is written with the same tmp+rename
    // discipline (see `runtime-pidfiles.ts writeRuntimePidfile`) —
    // identity should match.
    const tmp = `${filePath}.tmp-${process.pid}`;
    try {
      writeFileSync(tmp, `${JSON.stringify(DEFAULT_DAEMON_IDENTITY, null, 2)}\n`, {
        mode: 0o600,
      });
      renameSync(tmp, filePath);
    } catch (err) {
      try {
        unlinkSync(tmp);
      } catch {
        // best-effort tmp cleanup; ignore if it never landed
      }
      throw err;
    }
    return DEFAULT_DAEMON_IDENTITY;
  }
  const parsed = JSON.parse(readFileSync(filePath, 'utf-8')) as unknown;
  return DaemonIdentitySchema.parse(parsed);
}

export function parseDaemonIdentityForTest(value: unknown): DaemonIdentity {
  return DaemonIdentitySchema.parse(value);
}
