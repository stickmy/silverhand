import { describe, expect, it } from 'vitest';
import { CreateAgentInputSchema, SwitchAgentRuntimeInputSchema } from '../src/index.js';

describe('SwitchAgentRuntimeInputSchema', () => {
  it('requires every tuning key while accepting explicit null defaults', () => {
    const explicitDefaults = {
      runtime: 'claude',
      model: null,
      reasoning_effort: null,
      service_tier: null,
    };
    expect(SwitchAgentRuntimeInputSchema.parse(explicitDefaults)).toEqual(explicitDefaults);
    for (const key of ['model', 'reasoning_effort', 'service_tier'] as const) {
      const { [key]: _omitted, ...input } = explicitDefaults;
      const result = SwitchAgentRuntimeInputSchema.safeParse(input);
      expect(result.success, key).toBe(false);
      if (!result.success) {
        expect(result.error.issues.map((issue) => issue.path)).toContainEqual([key]);
      }
    }
  });

  it('validates tuning against the target runtime', () => {
    expect(
      SwitchAgentRuntimeInputSchema.safeParse({
        runtime: 'codex',
        model: 'gpt-5-codex',
        reasoning_effort: 'high',
        service_tier: 'ultrafast',
      }).success,
    ).toBe(true);
    expect(
      SwitchAgentRuntimeInputSchema.safeParse({
        runtime: 'claude',
        model: null,
        reasoning_effort: 'high',
        service_tier: null,
      }).success,
    ).toBe(true);
    expect(
      SwitchAgentRuntimeInputSchema.safeParse({
        runtime: 'opencode',
        model: null,
        reasoning_effort: null,
        service_tier: 'default',
      }).success,
    ).toBe(false);
  });

  it('admits any well-formed effort and leaves model support to the CLI', () => {
    for (const runtime of ['claude', 'codex', 'qoder', 'pi', 'grok']) {
      for (const model of ['custom-model', 'gpt-5.6-luna', null]) {
        for (const effort of ['high', 'ultra', 'future-tier']) {
          expect(
            SwitchAgentRuntimeInputSchema.safeParse({
              runtime,
              model,
              reasoning_effort: effort,
              service_tier: null,
            }).success,
          ).toBe(true);
          expect(
            CreateAgentInputSchema.safeParse({
              name: 'tester',
              runtime,
              ...(model === null ? {} : { model }),
              reasoning_effort: effort,
            }).success,
          ).toBe(true);
        }
        for (const effort of ['', 'High', 'future tier', 'x'.repeat(33)]) {
          expect(
            SwitchAgentRuntimeInputSchema.safeParse({
              runtime,
              model,
              reasoning_effort: effort,
              service_tier: null,
            }).success,
          ).toBe(false);
          expect(
            CreateAgentInputSchema.safeParse({
              name: 'tester',
              runtime,
              ...(model === null ? {} : { model }),
              reasoning_effort: effort,
            }).success,
          ).toBe(false);
        }
      }
    }
  });

  it('distinguishes explicit driver default from an empty custom model', () => {
    expect(
      SwitchAgentRuntimeInputSchema.safeParse({
        runtime: 'codex',
        model: '',
        reasoning_effort: null,
        service_tier: null,
      }).success,
    ).toBe(false);
  });
});

describe('CreateAgentInputSchema runtime tuning', () => {
  it('requires a participant name to start with a letter', () => {
    expect(CreateAgentInputSchema.safeParse({ name: 'agent-2b', runtime: 'claude' }).success).toBe(
      true,
    );
    expect(CreateAgentInputSchema.safeParse({ name: '2b', runtime: 'claude' }).success).toBe(false);
  });

  it('rejects a service tier outside runtimes that support it', () => {
    expect(
      CreateAgentInputSchema.safeParse({
        name: 'claude-fast',
        runtime: 'claude',
        service_tier: 'ultrafast',
      }).success,
    ).toBe(false);
    expect(
      CreateAgentInputSchema.safeParse({
        name: 'codex-fast',
        runtime: 'codex',
        service_tier: 'ultrafast',
      }).success,
    ).toBe(true);
  });
});
