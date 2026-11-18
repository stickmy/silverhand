import { describe, expect, it } from 'vitest';

import {
  RUNTIME_CATALOG,
  RUNTIME_ORDER_LIST,
  RuntimeKindSchema,
  ReasoningEffortTokenSchema,
  isBaselineReasoningEffort,
  reasoningEffortLabel,
  reasoningEffortOptionsForRuntime,
  reasoningEffortValidForRuntime,
  runtimeDefaultModel,
  runtimeCatalogEntry,
  runtimeCliLabel,
  runtimeLabel,
  runtimeSupportsReasoningEffort,
  runtimeSupportsFastMode,
} from '../src/index.js';

describe('runtime catalog (shared source of truth)', () => {
  it('covers every RuntimeKind exactly once, in order', () => {
    expect([...RUNTIME_ORDER_LIST].sort()).toEqual([...RuntimeKindSchema.options].sort());
    expect(RUNTIME_CATALOG.map((e) => e.kind)).toEqual([...RUNTIME_ORDER_LIST]);
  });

  it('exposes short + CLI labels for every kind', () => {
    for (const kind of RuntimeKindSchema.options) {
      const entry = runtimeCatalogEntry(kind);
      expect(entry.kind).toBe(kind);
      expect(runtimeLabel(kind)).toBe(entry.label);
      expect(runtimeCliLabel(kind)).toBe(entry.cliLabel);
      expect(entry.label.length).toBeGreaterThan(0);
      expect(entry.cliLabel.length).toBeGreaterThan(0);
    }
  });

  it('only offers spawnable runtimes: selectable is a strict subset per product curation', () => {
    // The implemented runtimes are offered; cursor/gemini remain
    // roadmap. The host-side test asserts selectable ⇒ hasDriver.
    const selectable = RUNTIME_CATALOG.filter((e) => e.selectable).map((e) => e.kind);
    expect(selectable).toEqual(['claude', 'codex', 'qoder', 'opencode', 'pi', 'grok']);
  });

  it('every catalog reasoning option is a structurally-valid bounded token', () => {
    // Static suggestions must remain well-formed tokens.
    // The only global constraint is that each option is a well-formed token
    // (lowercase, bounded) — legality is per-runtime, proven elsewhere.
    for (const entry of RUNTIME_CATALOG) {
      for (const opt of entry.reasoningEffortOptions) {
        expect(ReasoningEffortTokenSchema.safeParse(opt).success).toBe(true);
      }
      for (const options of Object.values(entry.reasoningEffortOptionsByModel ?? {})) {
        for (const opt of options) {
          expect(ReasoningEffortTokenSchema.safeParse(opt).success).toBe(true);
        }
      }
      // Claude obtains its options from native metadata.
      expect(runtimeSupportsReasoningEffort(entry.kind)).toBe(
        entry.kind === 'claude' || entry.reasoningEffortOptions.length > 0,
      );
    }
  });

  it('keeps codex and qoder reasoning sets distinct (no forced abstraction)', () => {
    expect(runtimeCatalogEntry('codex').reasoningEffortOptions).toEqual([
      'low',
      'medium',
      'high',
      'xhigh',
    ]);
    expect(runtimeCatalogEntry('qoder').reasoningEffortOptions).toEqual([
      'none',
      'low',
      'medium',
      'high',
      'xhigh',
      'max',
    ]);
    // Claude options come from native metadata.
    expect(runtimeCatalogEntry('claude').reasoningEffortOptions).toEqual([]);
  });

  it('offers Codex Max and Ultra on gpt-6.1-sol and gpt-6.1-terra', () => {
    for (const model of ['gpt-6.1-sol', 'gpt-6.1-terra']) {
      expect(reasoningEffortOptionsForRuntime('codex', model)).toEqual([
        'low',
        'medium',
        'high',
        'xhigh',
        'max',
        'ultra',
      ]);
    }
  });

  it('offers Codex Max on all verified 5.6 models and Ultra only on sol', () => {
    expect(reasoningEffortOptionsForRuntime('codex', 'gpt-5.6-sol')).toEqual([
      'low',
      'medium',
      'high',
      'xhigh',
      'max',
      'ultra',
    ]);
    for (const model of ['gpt-5.6-terra', 'gpt-5.6-luna']) {
      expect(reasoningEffortOptionsForRuntime('codex', model)).toEqual([
        'low',
        'medium',
        'high',
        'xhigh',
        'max',
      ]);
    }
    // Unlisted models outside approved families and the driver default
    // stay on the conservative baseline.
    for (const model of ['gpt-5.5', 'future-model', null]) {
      expect(reasoningEffortOptionsForRuntime('codex', model)).toEqual([
        'low',
        'medium',
        'high',
        'xhigh',
      ]);
    }
  });

  it('models qoder tiers as suggestions plus an explicit Ultimate create default', () => {
    expect(runtimeCatalogEntry('qoder').modelSuggestions).toEqual([
      'Auto',
      'Ultimate',
      'Performance',
      'Efficient',
      'Lite',
    ]);
    expect(runtimeDefaultModel('qoder')).toBe('Ultimate');
    expect(runtimeCatalogEntry('qoder').modelSuggestions).toContain(runtimeDefaultModel('qoder'));

    for (const kind of ['claude', 'codex', 'opencode', 'cursor', 'gemini'] as const) {
      expect(runtimeDefaultModel(kind)).toBeNull();
    }
  });

  it('exposes codex model suggestions as quick-picks (free text still accepted, no forced default)', () => {
    expect(runtimeCatalogEntry('codex').modelSuggestions).toEqual([
      'gpt-6.1-sol',
      'gpt-6.1-terra',
      'gpt-6-astra',
      'gpt-5.6-sol',
      'gpt-5.6-terra',
      'gpt-5.6-luna',
    ]);
    // Suggestions only — codex must not force a create-time default.
    expect(runtimeDefaultModel('codex')).toBeNull();
  });

  it('enables profile MCP for runtimes with spawn adapters', () => {
    expect(RUNTIME_CATALOG.filter((e) => e.supportsProfileMcp).map((e) => e.kind)).toEqual([
      'claude',
      'codex',
      'qoder',
      'opencode',
      'grok',
    ]);
  });

  it('exposes fast mode only for codex', () => {
    expect(RUNTIME_CATALOG.filter((e) => e.supportsFastMode).map((e) => e.kind)).toEqual(['codex']);
    for (const entry of RUNTIME_CATALOG) {
      expect(runtimeSupportsFastMode(entry.kind)).toBe(entry.supportsFastMode);
    }
  });

  it('title-cases reasoning labels for display while values stay lowercase', () => {
    expect(reasoningEffortLabel('xhigh')).toBe('Xhigh');
    expect(reasoningEffortLabel('none')).toBe('None');
    expect(reasoningEffortLabel('max')).toBe('Max');
    expect(reasoningEffortLabel('ultra')).toBe('Ultra');
  });

  it('validates Claude/Codex token shape independently of model capabilities', () => {
    for (const runtime of ['claude', 'codex'] as const) {
      for (const model of [null, 'custom-model', 'gpt-5.6-terra']) {
        for (const effort of [null, undefined, 'ultra', 'future-tier'])
          expect(reasoningEffortValidForRuntime(runtime, effort, model)).toBe(true);
        for (const effort of ['', 'High', 'high;exec', 'x'.repeat(33)])
          expect(reasoningEffortValidForRuntime(runtime, effort, model)).toBe(false);
      }
    }
    expect(reasoningEffortValidForRuntime('qoder', 'max')).toBe(true);
    expect(reasoningEffortValidForRuntime('qoder', 'ultra')).toBe(false);
    expect(reasoningEffortValidForRuntime('opencode', 'high')).toBe(false);
  });

  it('keeps static fallback membership separate from admission', () => {
    expect(isBaselineReasoningEffort('codex', 'high')).toBe(true);
    expect(isBaselineReasoningEffort('codex', 'ultra')).toBe(false);
    expect(reasoningEffortValidForRuntime('codex', 'ultra')).toBe(true);
  });
});

describe('ReasoningEffortTokenSchema — "open" is bounded, not arbitrary', () => {
  it('accepts well-formed lowercase tokens (known + never-seen)', () => {
    for (const token of [
      'none',
      'high',
      'xhigh',
      'max',
      'ultra',
      'giga',
      'a',
      'v2',
      'multi-agent',
      'a'.repeat(32),
    ]) {
      expect(ReasoningEffortTokenSchema.safeParse(token).success).toBe(true);
    }
  });

  it('rejects uppercase, whitespace, illegal chars, empty, and over-length', () => {
    for (const token of [
      'HIGH', // uppercase
      'High', // mixed case
      'a b', // whitespace
      'x!', // illegal char
      '9x', // leading digit
      '-x', // leading dash
      '_x', // leading underscore
      '', // empty (min 1)
      'a'.repeat(33), // over the 32-char bound
    ]) {
      expect(ReasoningEffortTokenSchema.safeParse(token).success).toBe(false);
    }
  });
});

describe('Grok runtime capabilities', () => {
  it('offers native MCP and model-specific effort without forcing a model', () => {
    expect(runtimeCatalogEntry('grok')).toMatchObject({
      selectable: true,
      supportsProfileMcp: true,
      supportsFastMode: false,
    });
    expect(reasoningEffortOptionsForRuntime('grok', 'grok-4.6')).toEqual([
      'low',
      'medium',
      'high',
      'xhigh',
    ]);
    expect(reasoningEffortOptionsForRuntime('grok', 'grok-4.5')).toEqual(['low', 'medium', 'high']);
    expect(reasoningEffortValidForRuntime('grok', 'xhigh', 'custom')).toBe(false);
    expect(runtimeDefaultModel('grok')).toBeNull();
  });
});

describe('Codex GPT-6 family reasoning', () => {
  it('offers and accepts Max/Ultra throughout the approved family', () => {
    for (const model of ['gpt-6', 'gpt-6-astra', 'gpt-6-future', ' gpt-6-astra ']) {
      expect(reasoningEffortOptionsForRuntime('codex', model)).toEqual([
        'low',
        'medium',
        'high',
        'xhigh',
        'max',
        'ultra',
      ]);
      expect(reasoningEffortValidForRuntime('codex', 'ultra', model)).toBe(true);
    }
  });

  it('does not use static family names to restrict Codex admission', () => {
    for (const model of [
      null,
      '',
      'gpt-6-',
      'gpt-60',
      'gpt-6astra',
      'proxy/gpt-6-astra',
      'gpt-5.6-future',
      'gpt-5.6-terra',
    ]) {
      expect(reasoningEffortValidForRuntime('codex', 'ultra', model)).toBe(true);
    }
    expect(reasoningEffortValidForRuntime('grok', 'ultra', 'gpt-6-astra')).toBe(false);
  });
});
