/**
 * D4.d D2: `.materialized` sentinel I/O for the
 * `agent_materialize` daemon handler.
 *
 * The sentinel is a small JSON file at `<agent_dir>/.materialized`
 * written atomically (tmp + rename) as the LAST step of every
 * successful materialize. Its presence + content drives §4.6's
 * idempotency decision tree:
 *
 *   - file does not exist → caller decides based on dir state
 *     (empty → seed; foreign content → `agent_dir_collision`)
 *   - file exists but is not valid JSON / has unknown
 *     `version` → `agent_dir_collision` (don't trust an
 *     unrecognizable sentinel and silently rewrite over it)
 *   - file exists, valid, and both
 *     `(agent_id, template_choice)` match the incoming
 *     request → idempotent OK (handler returns ok:true without
 *     rewriting any file)
 *   - file exists, valid, but either identity field mismatches
 *     → `agent_dir_collision` (previously materialized for a
 *     different agent / template; refuse to overwrite,
 *     this is an operator-visible state needing investigation).
 *
 * Runtime remains recorded as materialization-time provenance, but it is not
 * directory identity: an agent may switch execution runtime without
 * rematerializing its L1 home. The minimum identity set of
 * `(agent_id, template_choice)` still prevents a foreign sentinel from
 * masquerading as this agent; a bare-existence check would still be unsafe.
 */

import { linkSync, mkdirSync, readFileSync, unlinkSync, writeFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { basename, dirname, join } from 'node:path';
import { z } from 'zod';
import { AGENT_HOME_SENTINEL_FILENAME, AGENT_HOME_TEMPLATE_VERSION } from './template.js';

export const AgentHomeSentinelSchema = z.object({
  /** Layout-format version. See `AGENT_HOME_TEMPLATE_VERSION`. */
  version: z.number().int().positive(),
  /** Server-allocated agent id this dir was materialized for. */
  agent_id: z.string().min(1),
  /** `template_choice` from the originating materialize call. */
  template_choice: z.string().min(1),
  /** Driver runtime kind at materialize time. */
  runtime: z.string().min(1),
  /** Daemon-local epoch ms when the sentinel was written. */
  materialized_at: z.number().int().positive(),
});
export type AgentHomeSentinel = z.infer<typeof AgentHomeSentinelSchema>;

export function sentinelPath(agentDir: string): string {
  return join(agentDir, AGENT_HOME_SENTINEL_FILENAME);
}

/**
 * Read the sentinel at `<agentDir>/.materialized`. Returns:
 *   - `{ kind: 'missing' }` if the file does not exist (any other
 *     I/O error rethrows so the handler can classify it).
 *   - `{ kind: 'invalid' }` if the file exists but cannot be
 *     JSON-parsed, fails schema validation, or carries an
 *     unrecognized `version`. The handler treats this as a
 *     `agent_dir_collision`.
 *   - `{ kind: 'ok', sentinel }` on a well-formed sentinel.
 */
export type ReadSentinelResult =
  | { kind: 'missing' }
  | { kind: 'invalid'; reason: string }
  | { kind: 'ok'; sentinel: AgentHomeSentinel };

export function readSentinel(agentDir: string): ReadSentinelResult {
  let raw: string;
  try {
    raw = readFileSync(sentinelPath(agentDir), 'utf8');
  } catch (err) {
    if (isErrno(err) && err.code === 'ENOENT') {
      return { kind: 'missing' };
    }
    throw err;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (err) {
    return {
      kind: 'invalid',
      reason: `sentinel is not valid JSON: ${err instanceof Error ? err.message : String(err)}`,
    };
  }
  const validated = AgentHomeSentinelSchema.safeParse(parsed);
  if (!validated.success) {
    return {
      kind: 'invalid',
      reason: `sentinel does not match schema: ${validated.error.errors
        .map((e) => `${e.path.join('.')}: ${e.message}`)
        .join('; ')}`,
    };
  }
  if (validated.data.version !== AGENT_HOME_TEMPLATE_VERSION) {
    return {
      kind: 'invalid',
      reason: `sentinel version ${validated.data.version} is not supported (handler expects ${AGENT_HOME_TEMPLATE_VERSION})`,
    };
  }
  return { kind: 'ok', sentinel: validated.data };
}

/**
 * Decide whether a present, well-formed sentinel matches an
 * incoming materialize request. Runtime is intentionally excluded: it records
 * materialization-time provenance and may differ after a runtime transition.
 */
export interface SentinelMatchInput {
  agent_id: string;
  template_choice: string;
}

export type SentinelMatchResult = { kind: 'match' } | { kind: 'mismatch'; reason: string };

export function compareSentinel(
  sentinel: AgentHomeSentinel,
  input: SentinelMatchInput,
): SentinelMatchResult {
  if (sentinel.agent_id !== input.agent_id) {
    return {
      kind: 'mismatch',
      reason: `sentinel.agent_id=${sentinel.agent_id} but request.agent_id=${input.agent_id}`,
    };
  }
  if (sentinel.template_choice !== input.template_choice) {
    return {
      kind: 'mismatch',
      reason: `sentinel.template_choice=${sentinel.template_choice} but request.template_choice=${input.template_choice}`,
    };
  }
  return { kind: 'match' };
}

/**
 * Atomically write the sentinel as the LAST step of seeding.
 * Order matters: if the process crashes mid-seed, the absence of
 * the sentinel ensures the next call retries the seed rather
 * than treating a half-written dir as up to date.
 *
 * Pattern (Cindy D2 P1-3 — fail-closed against TOCTOU): write
 * to a tmp file in the same dir, then `linkSync(tmp, target)`
 * for atomic AND exclusive creation. POSIX `link()` FAILS with
 * EEXIST if `target` already exists, so a concurrent writer that
 * races us to the same sentinel path cannot silently overwrite
 * the other process's bytes. Compare against `renameSync` —
 * which overwrites — and the consequence is that under
 * concurrent materialize attempts one process wins, the other
 * sees EEXIST → `agent_dir_collision`, and the operator can
 * retry the loser; the next attempt finds a valid sentinel and
 * returns idempotent ok. Mode 0o600 keeps the sentinel readable
 * only by the daemon's UID (it carries the agent_id, which is
 * not secret but is also not interesting to other users).
 */
export function writeSentinelAtomic(agentDir: string, sentinel: AgentHomeSentinel): void {
  const filePath = sentinelPath(agentDir);
  mkdirSync(dirname(filePath), { recursive: true });
  const tmpPath = join(
    dirname(filePath),
    `.${basename(filePath)}.${process.pid}.${randomUUID()}.tmp`,
  );
  try {
    writeFileSync(tmpPath, `${JSON.stringify(sentinel)}\n`, { mode: 0o600 });
    linkSync(tmpPath, filePath);
  } finally {
    // Best-effort cleanup; ignore errors so we surface the
    // original write failure (or the linkSync EEXIST) to the
    // caller.
    try {
      unlinkSync(tmpPath);
    } catch {
      // ignore
    }
  }
}

function isErrno(err: unknown): err is NodeJS.ErrnoException {
  return (
    typeof err === 'object' &&
    err !== null &&
    'code' in err &&
    typeof (err as { code: unknown }).code === 'string'
  );
}
