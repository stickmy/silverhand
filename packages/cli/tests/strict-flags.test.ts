import { describe, expect, it } from 'vitest';
import { assertKnownFlags, GLOBAL_NOOP_FLAGS, GLOBAL_REMOVED_FLAGS } from '../src/strict-flags.js';
import { CliError } from '../src/errors.js';

describe('strict flag validation', () => {
  it('accepts only declared flags (booleans + value-bearing)', () => {
    expect(() =>
      assertKnownFlags(
        { 'task-number': '191', mine: true },
        {
          noun: 'task',
          verb: 'list',
          flags: ['task-number'],
          booleanFlags: ['mine'],
        },
      ),
    ).not.toThrow();
  });

  it('rejects an unknown flag with validation_error and exit-code 1 mapping', () => {
    let caught: unknown;
    try {
      assertKnownFlags(
        { 'task-numbers': '191' }, // common typo
        {
          noun: 'task',
          verb: 'list',
          flags: ['task-number'],
        },
      );
    } catch (err) {
      caught = err;
    }
    expect(caught).toBeInstanceOf(CliError);
    const e = caught as CliError;
    expect(e.code).toBe('validation_error');
    expect(e.retriable).toBe(false);
    expect(e.message).toContain('unknown flag --task-numbers');
    expect(e.message).toContain('did you mean --task-number?');
  });

  it('lists additional unknown flags after the first one in the message', () => {
    let caught: unknown;
    try {
      assertKnownFlags(
        { foo: 'a', bar: 'b', baz: 'c' },
        { noun: 'task', verb: 'list', flags: ['something-else'] },
      );
    } catch (err) {
      caught = err;
    }
    expect(caught).toBeInstanceOf(CliError);
    const msg = (caught as CliError).message;
    // First unknown is the leading one — the rest come in a parenthesized
    // tail so the agent sees every typo at once and doesn't keep retrying.
    expect(msg).toMatch(/unknown flag --foo/);
    expect(msg).toMatch(/also unrecognized: --bar, --baz/);
  });

  it('suggests a unique flag that extends the input by a dash segment', () => {
    let caught: unknown;
    try {
      assertKnownFlags(
        { 'source-message': 'x' },
        { noun: 'task', verb: 'create', flags: ['title', 'source-message-id', 'target'] },
      );
    } catch (err) {
      caught = err;
    }
    expect((caught as CliError).message).toContain('did you mean --source-message-id?');
  });

  it('falls back to a generic help hint when no candidate is close enough', () => {
    let caught: unknown;
    try {
      assertKnownFlags(
        { 'completely-different-flag': 'x' },
        { noun: 'task', verb: 'list', flags: ['title', 'body'] },
      );
    } catch (err) {
      caught = err;
    }
    const msg = (caught as CliError).message;
    expect(msg).toContain('run `silverhand task list --help`');
    expect(msg).not.toContain('did you mean');
  });

  it('matches across flags and booleanFlags when suggesting', () => {
    // `--mines` -> closest is the boolean flag `--mine` (distance 1).
    let caught: unknown;
    try {
      assertKnownFlags(
        { mines: true },
        { noun: 'task', verb: 'list', flags: ['title'], booleanFlags: ['mine'] },
      );
    } catch (err) {
      caught = err;
    }
    expect((caught as CliError).message).toContain('did you mean --mine?');
  });

  it('treats commands with empty flag lists as accepting nothing', () => {
    expect(() =>
      assertKnownFlags({ foo: 'a' }, { noun: 'workspace', verb: 'list', flags: [] }),
    ).toThrow(/unknown flag --foo/);
  });

  it('accepts global no-op flags on every command without explicit declaration', () => {
    // `--json` is the documented default output mode — agents must be
    // able to pass it on any subcommand (or none) without tripping
    // strict validation. Verified directly + via a command that
    // declares NO flags at all.
    expect(GLOBAL_NOOP_FLAGS.has('json')).toBe(true);
    expect(() =>
      assertKnownFlags({ json: true }, { noun: 'task', verb: 'list', flags: ['task-number'] }),
    ).not.toThrow();
    expect(() =>
      assertKnownFlags({ json: true }, { noun: 'workspace', verb: 'list', flags: [] }),
    ).not.toThrow();
  });
});

describe('removed-flag handling', () => {
  it('retires the process-wide route flag on every command with one migration error', () => {
    expect(GLOBAL_REMOVED_FLAGS['route-conversation-id']).toBeDefined();
    expect(() =>
      assertKnownFlags(
        { 'route-conversation-id': 'Conversation0001' },
        { noun: 'workspace', verb: 'list', flags: [] },
      ),
    ).toThrowError(
      expect.objectContaining({
        subkind: 'flag_removed',
        details: {
          flag: 'route-conversation-id',
          replacement: 'none',
        },
      }),
    );
  });

  it('throws flag_removed with replacement and hint when a removed flag is supplied', () => {
    let caught: unknown;
    try {
      assertKnownFlags(
        { content: 'hello' },
        {
          noun: 'message',
          verb: 'send',
          flags: ['target'],
          removedFlags: {
            content: {
              replacement: 'stdin',
              hint: 'pipe content via stdin',
            },
          },
        },
      );
    } catch (err) {
      caught = err;
    }
    expect(caught).toBeInstanceOf(CliError);
    const e = caught as CliError;
    expect(e.code).toBe('validation_error');
    expect(e.subkind).toBe('flag_removed');
    expect(e.retriable).toBe(false);
    expect(e.message).toContain('--content was removed');
    expect(e.message).toContain('pipe content via stdin');
    expect(e.details).toEqual({ flag: 'content', replacement: 'stdin' });
  });

  it('flag_removed wins over the unknown-flag check (priority guarantee)', () => {
    let caught: unknown;
    try {
      assertKnownFlags(
        { content: 'hello', bogus: 'x' },
        {
          noun: 'message',
          verb: 'send',
          flags: ['target'],
          removedFlags: {
            content: { replacement: 'stdin', hint: 'use stdin' },
          },
        },
      );
    } catch (err) {
      caught = err;
    }
    const e = caught as CliError;
    // `flag_removed` for the deprecated `--content` must win even
    // though `--bogus` is also unknown — that's the migration UX
    // contract Cindy and Alice locked in.
    expect(e.subkind).toBe('flag_removed');
    expect(e.message).toContain('--content was removed');
  });

  it('falls through to unknown-flag handling when no removed flag matched', () => {
    let caught: unknown;
    try {
      assertKnownFlags(
        { 'task-numbers': '191' },
        {
          noun: 'task',
          verb: 'list',
          flags: ['task-number'],
          removedFlags: {
            content: { replacement: 'stdin', hint: 'use stdin' },
          },
        },
      );
    } catch (err) {
      caught = err;
    }
    const e = caught as CliError;
    // Unknown-flag throws now carry `subkind: 'unknown_flag'` so trace
    // sinks can group typos by the structured reason rather than parsing
    // the message string.
    expect(e.subkind).toBe('unknown_flag');
    expect(e.message).toContain('unknown flag --task-numbers');
    expect(e.details).toMatchObject({
      flag: 'task-numbers',
      suggestion: 'task-number',
    });
  });
});

describe('unknown flag suggestions through validation', () => {
  it.each([
    ['insertion', 'cats', ['cat'], 'cat'],
    ['deletion', 'cat', ['cats'], 'cats'],
    ['substitution', 'bat', ['cat'], 'cat'],
    ['distance two', 'cxx', ['cat'], 'cat'],
    ['distance three', 'xxx', ['cat'], undefined],
    ['nearest candidate, not first match', 'attachmen', ['attachm', 'attachment'], 'attachment'],
  ] as const)('%s', (_label, input, flags, suggestion) => {
    expect(() =>
      assertKnownFlags({ [input]: true }, { noun: 'task', verb: 'list', flags }),
    ).toThrowError(
      expect.objectContaining({
        code: 'validation_error',
        subkind: 'unknown_flag',
        details: { flag: input, ...(suggestion ? { suggestion } : {}) },
      }),
    );
  });
});
