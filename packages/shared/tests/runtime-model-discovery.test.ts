import { expect, it } from 'vitest';
import { RuntimeCatalogResponseSchema, RuntimeModelDiscoverySchema } from '../src/api/runtime.js';

const base = {
  runtime: 'pi',
  status: 'ok',
  models: ['provider/custom-model'],
  detected_at: 1,
  cached: false,
};
it('keeps model discovery optional for old producers and accepts arbitrary native model IDs', () => {
  expect(
    RuntimeCatalogResponseSchema.parse({ runtimes: [], detected_at: 1 }).model_discovery,
  ).toBeUndefined();
  expect(RuntimeModelDiscoverySchema.parse(base).models).toEqual(['provider/custom-model']);
  expect(RuntimeModelDiscoverySchema.parse({ ...base, models: [] }).status).toBe('ok');
});
it('refuses malformed discovery metadata', () => {
  for (const override of [
    { runtime: 'unknown' },
    { status: 'unknown' },
    { models: [''] },
    { models: ['x\n'] },
    { models: ['x'.repeat(513)] },
    { status: 'error' },
    { status: 'unsupported' },
  ])
    expect(RuntimeModelDiscoverySchema.safeParse({ ...base, ...override }).success).toBe(false);
  expect(
    RuntimeModelDiscoverySchema.safeParse({ ...base, status: 'error', models: [] }).success,
  ).toBe(true);
});

it('round-trips native effort metadata and refuses dangling or malformed capabilities', () => {
  const valid = {
    ...base,
    runtime: 'codex',
    default_model: base.models[0],
    reasoning_by_model: {
      [base.models[0]!]: { supported_efforts: ['high', 'future-tier'], default_effort: 'high' },
    },
  };
  expect(RuntimeModelDiscoverySchema.parse(valid)).toEqual(valid);
  for (const override of [
    { default_model: 'unknown' },
    { reasoning_by_model: { unknown: { supported_efforts: [] } } },
    { reasoning_by_model: { [base.models[0]!]: { supported_efforts: ['BAD'] } } },
    {
      reasoning_by_model: {
        [base.models[0]!]: { supported_efforts: ['high'], default_effort: 'max' },
      },
    },
  ])
    expect(RuntimeModelDiscoverySchema.safeParse({ ...valid, ...override }).success).toBe(false);
});
