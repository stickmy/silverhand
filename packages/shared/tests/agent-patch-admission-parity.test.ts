import { describe, expect, it } from 'vitest';
import { UpdateAgentInputSchema } from '../src/index.js';
import corpus from './fixtures/agent-patch-admission.json' with { type: 'json' };

/**
 * TypeScript half of the agent-tuning PATCH admission corpus (anchor #2236).
 *
 * This side is the ORACLE: every `verdict` / `canonical` in the fixture is signed by
 * running the REAL `UpdateAgentInputSchema` here, never restated by hand. The Rust half
 * (an in-crate module in
 * `rust/crates/backend-server/src/identity_access/agent_patch.rs`) reads the same file and
 * holds its `AgentPatchRaw` decode + garde + `TryFrom<AgentTuningEdit>` to it.
 *
 * Scope is the SEMANTIC value plane only: whether a JSON object is admitted and, if so,
 * the normalized op-form it projects to. HTTP status classification (malformed-JSON 400
 * vs semantic 422 vs 415) is a Rust-only extractor matrix, not this corpus.
 */

type RepeatSpec = { $repeat: { unit: string; times: number } };
type FieldValue = unknown | RepeatSpec;
type Op = { op: 'leave' } | { op: 'clear' } | { op: 'set'; value: unknown };
interface Case {
  name: string;
  body: Record<string, FieldValue>;
  verdict: 'accept' | 'reject';
  canonical?: Record<string, Op>;
}

const isRepeat = (v: unknown): v is RepeatSpec =>
  typeof v === 'object' && v !== null && '$repeat' in v;

/** Expand a fixture value: a `$repeat` spec becomes `unit` repeated `times`; any other
 *  value passes through verbatim. BOTH language halves expand identically. */
function materialize(value: FieldValue): unknown {
  if (isRepeat(value)) {
    return value.$repeat.unit.repeat(value.$repeat.times);
  }
  return value;
}

function materializeBody(body: Record<string, FieldValue>): Record<string, unknown> {
  return Object.fromEntries(Object.entries(body).map(([key, value]) => [key, materialize(value)]));
}

function materializeOp(op: Op): Op {
  return op.op === 'set' ? { op: 'set', value: materialize(op.value) } : op;
}

function materializeCanonical(canonical: Record<string, Op>): Record<string, Op> {
  return Object.fromEntries(Object.entries(canonical).map(([key, op]) => [key, materializeOp(op)]));
}

/** Project the schema's parsed output to the shared normalized op-form. `display_name`
 *  is non-nullable (leave/set only); the other four are leave/clear/set. */
function project(parsed: Record<string, unknown>): Record<string, Op> {
  const has = (key: string) => Object.prototype.hasOwnProperty.call(parsed, key);
  const nullable = (key: string): Op =>
    !has(key)
      ? { op: 'leave' }
      : parsed[key] === null
        ? { op: 'clear' }
        : { op: 'set', value: parsed[key] };
  return {
    display_name: has('display_name') ? { op: 'set', value: parsed.display_name } : { op: 'leave' },
    description: nullable('description'),
    model: nullable('model'),
    reasoning_effort: nullable('reasoning_effort'),
    service_tier: nullable('service_tier'),
  };
}

describe('agent PATCH admission — TypeScript oracle signs the corpus', () => {
  const cases = corpus.cases as unknown as Case[];

  it('has cases to sign', () => {
    expect(cases.length).toBeGreaterThan(0);
  });

  for (const testCase of cases) {
    it(`${testCase.verdict}: ${testCase.name}`, () => {
      const body = materializeBody(testCase.body);
      const result = UpdateAgentInputSchema.safeParse(body);
      expect(result.success).toBe(testCase.verdict === 'accept');
      if (testCase.verdict === 'accept') {
        expect(testCase.canonical).toBeDefined();
        const projected = project(result.data as Record<string, unknown>);
        expect(projected).toEqual(materializeCanonical(testCase.canonical!));
      } else {
        expect(testCase.canonical).toBeUndefined();
      }
    });
  }
});
