import { describe, expect, it } from 'vitest';
import { CreateAgentInputSchema, SwitchAgentRuntimeInputSchema } from '../src/index.js';

describe('SwitchAgentRuntimeInputSchema', () => {
  it('requires every tuning key while accepting explicit null defaults', () => {
    const explicitDefaults = {
      runtime: 'claude',
      model: null,
      reasoning_effort: null,
      fast_mode: null,
    };
    expect(SwitchAgentRuntimeInputSchema.parse(explicitDefaults)).toEqual(explicitDefaults);
    for (const key of ['model', 'reasoning_effort', 'fast_mode'] as const) {
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
        fast_mode: true,
      }).success,
    ).toBe(true);
    expect(
      SwitchAgentRuntimeInputSchema.safeParse({
        runtime: 'claude',
        model: null,
        reasoning_effort: 'high',
        fast_mode: null,
      }).success,
    ).toBe(true);
    expect(
      SwitchAgentRuntimeInputSchema.safeParse({
        runtime: 'opencode',
        model: null,
        reasoning_effort: null,
        fast_mode: false,
      }).success,
    ).toBe(false);
  });

  it('delegates Claude/Codex capability checks to the native CLI, but validates token shape', () => {
    for (const runtime of ['claude', 'codex']) {
      for (const model of ['custom-model', 'gpt-5.6-luna', null]) {
        for (const effort of ['high', 'ultra', 'future-tier']) {
          expect(
            SwitchAgentRuntimeInputSchema.safeParse({
              runtime,
              model,
              reasoning_effort: effort,
              fast_mode: null,
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
              fast_mode: null,
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
        fast_mode: null,
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

  it('rejects fast mode outside runtimes that support it', () => {
    expect(
      CreateAgentInputSchema.safeParse({
        name: 'claude-fast',
        runtime: 'claude',
        fast_mode: true,
      }).success,
    ).toBe(false);
    expect(
      CreateAgentInputSchema.safeParse({
        name: 'codex-fast',
        runtime: 'codex',
        fast_mode: true,
      }).success,
    ).toBe(true);
  });
});
