import { describe, expect, it } from 'vitest';
import {
  CliError,
  ERROR_CODES,
  exitCodeFor,
  fromNetworkError,
  fromUnknown,
  mapHttpError,
  writeError,
} from '../src/errors.js';

describe('CliError contract', () => {
  it('exposes the contract codes for stable downstream branching', () => {
    expect(ERROR_CODES).toEqual([
      'not_found',
      'forbidden',
      'unauthorized',
      'validation_error',
      'conflict',
      'rate_limited',
      'upstream_unavailable',
      'internal',
    ]);
  });

  it('maps exit codes per the agent-facing contract', () => {
    expect(exitCodeFor('validation_error')).toBe(1);
    expect(exitCodeFor('not_found')).toBe(1);
    expect(exitCodeFor('conflict')).toBe(1);
    expect(exitCodeFor('rate_limited')).toBe(2);
    expect(exitCodeFor('upstream_unavailable')).toBe(2);
    expect(exitCodeFor('unauthorized')).toBe(3);
    expect(exitCodeFor('forbidden')).toBe(3);
    expect(exitCodeFor('internal')).toBe(1);
  });
});

describe('mapHttpError', () => {
  it('infers from status when body is empty', () => {
    expect(mapHttpError(404, '').code).toBe('not_found');
    expect(mapHttpError(401, '').code).toBe('unauthorized');
    expect(mapHttpError(403, '').code).toBe('forbidden');
    expect(mapHttpError(409, '').code).toBe('conflict');
    expect(mapHttpError(429, '').code).toBe('rate_limited');
    expect(mapHttpError(503, '').code).toBe('upstream_unavailable');
    expect(mapHttpError(418, '').code).toBe('validation_error');
  });

  it('marks 5xx and 429 as retriable, others as not', () => {
    expect(mapHttpError(503, '').retriable).toBe(true);
    expect(mapHttpError(429, '').retriable).toBe(true);
    expect(mapHttpError(404, '').retriable).toBe(false);
    expect(mapHttpError(401, '').retriable).toBe(false);
  });

  it('prefers a backend-supplied stable code over status inference', () => {
    const err = mapHttpError(
      400,
      JSON.stringify({ error: { code: 'conflict', message: 'task already done' } }),
    );
    expect(err.code).toBe('conflict');
    expect(err.message).toBe('task already done');
  });

  it('ignores backend-supplied codes that are not in our enum', () => {
    const err = mapHttpError(
      400,
      JSON.stringify({ error: { code: 'not_a_real_code', message: 'oops' } }),
    );
    expect(err.code).toBe('validation_error');
    // We still surface the friendly message even though we rejected the code.
    expect(err.message).toBe('oops');
  });

  it('accepts flat {code, message} bodies (no nesting)', () => {
    const err = mapHttpError(429, JSON.stringify({ code: 'rate_limited', message: 'slow down' }));
    expect(err.code).toBe('rate_limited');
    expect(err.message).toBe('slow down');
  });

  it('survives non-JSON bodies (proxy HTML, plain text)', () => {
    const err = mapHttpError(502, '<html>Bad Gateway</html>');
    expect(err.code).toBe('upstream_unavailable');
    expect(err.message).toMatch(/HTTP 502/);
  });
});

describe('fromNetworkError', () => {
  it('wraps fetch failures as transient/upstream_unavailable', () => {
    const err = fromNetworkError(new Error('connect ECONNREFUSED'));
    expect(err.code).toBe('upstream_unavailable');
    expect(err.retriable).toBe(true);
    expect(err.message).toMatch(/ECONNREFUSED/);
  });
});

describe('fromUnknown', () => {
  it('passes CliError through unchanged', () => {
    const orig = new CliError({ code: 'conflict', message: 'x', retriable: false });
    expect(fromUnknown(orig)).toBe(orig);
  });

  it('maps zod-style errors to validation_error', () => {
    const zodLike = {
      name: 'ZodError',
      errors: [{ path: ['title'], message: 'Required' }],
    };
    const err = fromUnknown(zodLike);
    expect(err.code).toBe('validation_error');
    expect(err.message).toMatch(/title/);
  });

  it('falls back to internal for unknown throws (no stack leak)', () => {
    const err = fromUnknown(new Error('nope'));
    expect(err.code).toBe('internal');
    expect(err.retriable).toBe(false);
    expect(err.message).not.toContain('at ');
  });
});

describe('writeError', () => {
  it('emits one-line JSON with the error payload in JSON mode', () => {
    const writes: string[] = [];
    const err = new CliError({ code: 'not_found', message: 'task #42 not found', retriable: false });
    writeError(err, { json: true, stderr: { write: s => writes.push(s) } });
    expect(writes.length).toBe(1);
    expect(writes[0]!.endsWith('\n')).toBe(true);
    const parsed = JSON.parse(writes[0]!);
    expect(parsed).toEqual({
      error: { code: 'not_found', message: 'task #42 not found', retriable: false },
    });
  });

  it('emits a short prefixed line in human mode', () => {
    const writes: string[] = [];
    const err = new CliError({ code: 'forbidden', message: 'no access', retriable: false });
    writeError(err, { json: false, stderr: { write: s => writes.push(s) } });
    expect(writes[0]).toBe('silverhand: forbidden: no access\n');
  });

  it('never writes a stack trace', () => {
    const writes: string[] = [];
    const inner = new Error('boom');
    const cli = fromUnknown(inner);
    writeError(cli, { json: true, stderr: { write: s => writes.push(s) } });
    expect(writes[0]).not.toContain(' at ');
    expect(writes[0]).not.toContain('\n    at ');
  });

  it('includes optional subkind and details in JSON mode when provided', () => {
    const writes: string[] = [];
    const err = new CliError({
      code: 'validation_error',
      subkind: 'unexpected_arguments',
      message: 'flags only',
      retriable: false,
      details: { positional: ['stray'], rest: [] },
    });
    writeError(err, { json: true, stderr: { write: s => writes.push(s) } });
    const parsed = JSON.parse(writes[0]!);
    expect(parsed).toEqual({
      error: {
        code: 'validation_error',
        subkind: 'unexpected_arguments',
        message: 'flags only',
        retriable: false,
        details: { positional: ['stray'], rest: [] },
      },
    });
  });

  it('omits subkind/details from JSON when not set (back-compat)', () => {
    const writes: string[] = [];
    const err = new CliError({
      code: 'validation_error',
      message: 'plain old',
      retriable: false,
    });
    writeError(err, { json: true, stderr: { write: s => writes.push(s) } });
    const parsed = JSON.parse(writes[0]!);
    expect(parsed.error).not.toHaveProperty('subkind');
    expect(parsed.error).not.toHaveProperty('details');
  });

  it('includes subkind in the human prefix as `code/subkind`', () => {
    const writes: string[] = [];
    const err = new CliError({
      code: 'validation_error',
      subkind: 'flag_removed',
      message: 'use stdin',
      retriable: false,
    });
    writeError(err, { json: false, stderr: { write: s => writes.push(s) } });
    expect(writes[0]).toBe(
      'silverhand: validation_error/flag_removed: use stdin\n',
    );
  });

  it('renders blocking_tasks details on indented lines in human mode', () => {
    const writes: string[] = [];
    const err = new CliError({
      code: 'conflict',
      message: 'cannot close thread: 2 open tasks',
      retriable: false,
      details: {
        reason: 'has_open_tasks',
        blocking_tasks: [
          {
            task_number: 521,
            status: 'in_review',
            title: 'protocol handshake',
            assignee_name: 'Cindy',
            assignee_agent_id: 'abc',
          },
          {
            task_number: 522,
            status: 'todo',
            title: 'orphan reconcile',
            assignee_name: null,
            assignee_agent_id: null,
          },
        ],
      },
    });
    writeError(err, { json: false, stderr: { write: s => writes.push(s) } });
    const joined = writes.join('');
    expect(joined).toContain(
      'silverhand: conflict: cannot close thread: 2 open tasks\n',
    );
    expect(joined).toContain(
      '  #task:521 (in_review) @Cindy — protocol handshake\n',
    );
    expect(joined).toContain('  #task:522 (todo) — orphan reconcile\n');
  });

  it('JSON mode keeps blocking_tasks inside the single-line payload', () => {
    const writes: string[] = [];
    const err = new CliError({
      code: 'conflict',
      message: 'cannot close thread: 1 open task',
      retriable: false,
      details: {
        reason: 'has_open_tasks',
        blocking_tasks: [
          {
            task_number: 99,
            status: 'in_progress',
            title: 't',
            assignee_name: 'A',
            assignee_agent_id: 'x',
          },
        ],
      },
    });
    writeError(err, { json: true, stderr: { write: s => writes.push(s) } });
    expect(writes.length).toBe(1);
    const parsed = JSON.parse(writes[0]!);
    expect(parsed.error.code).toBe('conflict');
    expect(parsed.error.retriable).toBe(false);
    expect(parsed.error.details.reason).toBe('has_open_tasks');
    expect(parsed.error.details.blocking_tasks).toHaveLength(1);
  });
});

describe('mapHttpError detail propagation', () => {
  it('lifts nested details.blocking_tasks into CliError.details', () => {
    const body = JSON.stringify({
      error: {
        code: 'conflict',
        message: 'cannot close thread: 1 open task',
        details: {
          reason: 'has_open_tasks',
          blocking_tasks: [
            {
              task_number: 7,
              status: 'in_review',
              title: 'x',
              assignee_name: 'Alice',
              assignee_agent_id: 'aid',
            },
          ],
        },
      },
    });
    const err = mapHttpError(409, body);
    expect(err.code).toBe('conflict');
    expect(err.retriable).toBe(false);
    const payload = err.toPayload();
    expect(payload.details).toBeDefined();
    expect((payload.details as any).reason).toBe('has_open_tasks');
    expect((payload.details as any).blocking_tasks).toHaveLength(1);
    expect((payload.details as any).blocking_tasks[0]).toMatchObject({
      task_number: 7,
      status: 'in_review',
      assignee_name: 'Alice',
    });
  });

  it('falls through cleanly when body has no details', () => {
    const err = mapHttpError(
      409,
      JSON.stringify({ error: { code: 'conflict', message: 'oops' } }),
    );
    expect(err.code).toBe('conflict');
    expect(err.toPayload().details).toBeUndefined();
  });
});
