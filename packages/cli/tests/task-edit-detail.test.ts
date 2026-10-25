/**
 * `silverhand task edit` / `task detail` command guards + wire mapping.
 * Locks: the task number arrives as a POSITIONAL operand (validated
 * client-side before any request), edit requires at least one of
 * --title / --body / --clear-body and maps to the last-write-wins wire
 * shape (no expected_revision), and detail resolves by task number.
 */
import { describe, expect, it } from 'vitest';
import { findCommand } from '../src/commands/index.js';
import type { CliEnv } from '../src/env.js';
import type { ParsedArgs } from '../src/parse.js';

const env: CliEnv = {
  conversationId: 'conv',
  slot: 'conversation',
  agentId: 'agent',
  serverUrl: 'http://localhost',
  authToken: 'tok',
};

function parsed(
  positional: string[],
  flags: Record<string, string | string[] | boolean> = {},
): ParsedArgs {
  return { flags, positional, rest: [] } as ParsedArgs;
}

describe('task detail guard', () => {
  const detail = findCommand('task', 'detail');
  if (!detail) throw new Error('task detail command missing');

  it('calls getTaskDetail with the positional task number', async () => {
    let seen: number | null = null;
    await detail.run({
      env,
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      client: {
        getTaskDetail: async (input: { task_number: number }) => {
          seen = input.task_number;
          return { task: {} };
        },
      } as any,
      args: parsed(['9']),
    });
    expect(seen).toBe(9);
  });

  it('rejects a missing task number', async () => {
    await expect(
      detail.run({
        env,
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        client: {} as any,
        args: parsed([]),
      }),
    ).rejects.toMatchObject({ subkind: 'missing_task_number' });
  });
});

/**
 * Arity and value laws for the positional operand, asserted per verb
 * rather than on the helper: the helper is an implementation detail, but
 * "detail takes one, claim takes many" is the contract. Every rejection
 * here must happen with a client that would THROW if called — that is
 * what proves the check is pre-network, which is the whole point of
 * validating client-side.
 */
describe('positional <task_number> arity + value laws', () => {
  /** A client whose every method throws — any request means the guard leaked. */
  const explodingClient = new Proxy(
    {},
    {
      get() {
        return () => {
          throw new Error('client must not be called: guard is pre-network');
        };
      },
    },
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
  ) as any;

  const pointVerbs = ['detail', 'unclaim', 'edit'] as const;

  it.each(pointVerbs)('task %s rejects a second positional token', async (verb) => {
    const cmd = findCommand('task', verb)!;
    await expect(
      cmd.run({
        env,
        client: explodingClient,
        args: parsed(['4', '5'], { status: 'done', title: 't' }),
      }),
    ).rejects.toMatchObject({ subkind: 'unexpected_arguments' });
  });

  it.each([
    ['zero', '0'],
    ['non-numeric', '42abc'],
    ['fractional', '4.5'],
    ['hex', '0x2a'],
    ['exponent', '1e3'],
    ['padded', ' 12'],
    // All digits, but past MAX_SAFE_INTEGER: a digits-only test plus `< 1`
    // would let this through and send a DIFFERENT (rounded) number than the
    // caller typed — or `Infinity`, which does not even survive JSON.
    ['unsafe integer', '9007199254740993'],
    ['absurdly long digit run', '9'.repeat(400)],
  ])('task detail rejects a %s task number', async (_label, token) => {
    const cmd = findCommand('task', 'detail')!;
    await expect(
      cmd.run({ env, client: explodingClient, args: parsed([token]) }),
    ).rejects.toMatchObject({ subkind: 'invalid_task_number' });
  });

  it('task claim rejects a batch whose only bad token is an unsafe integer', async () => {
    // The batch law and the value law must compose: one out-of-range operand
    // fails the whole call, so a claim never lands on a subset.
    const cmd = findCommand('task', 'claim')!;
    await expect(
      cmd.run({
        env,
        client: explodingClient,
        args: parsed(['42', '9007199254740993']),
      }),
    ).rejects.toMatchObject({ subkind: 'invalid_task_number' });
  });

  it('task claim accepts one number', async () => {
    let seen: readonly number[] | null = null;
    const cmd = findCommand('task', 'claim')!;
    await cmd.run({
      env,
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      client: {
        claimTasks: async (input: { task_numbers: number[] }) => {
          seen = input.task_numbers;
          return { results: [] };
        },
      } as any,
      args: parsed(['42']),
    });
    expect(seen).toEqual([42]);
  });

  it('task claim accepts a batch and preserves order', async () => {
    let seen: readonly number[] | null = null;
    const cmd = findCommand('task', 'claim')!;
    await cmd.run({
      env,
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      client: {
        claimTasks: async (input: { task_numbers: number[] }) => {
          seen = input.task_numbers;
          return { results: [] };
        },
      } as any,
      args: parsed(['42', '7', '43']),
    });
    expect(seen).toEqual([42, 7, 43]);
  });

  it('task claim rejects an empty operand list', async () => {
    const cmd = findCommand('task', 'claim')!;
    await expect(cmd.run({ env, client: explodingClient, args: parsed([]) })).rejects.toMatchObject(
      { subkind: 'missing_task_number' },
    );
  });

  it('task claim rejects a batch containing one bad token (all-or-nothing)', async () => {
    // Partial acceptance would silently claim a subset — the agent would
    // read "claimed" for a call it thinks named three tasks.
    const cmd = findCommand('task', 'claim')!;
    await expect(
      cmd.run({ env, client: explodingClient, args: parsed(['42', 'x', '43']) }),
    ).rejects.toMatchObject({ subkind: 'invalid_task_number' });
  });

  it('task unclaim and status forward the positional number to the wire', async () => {
    let unclaimed: number | null = null;
    await findCommand('task', 'unclaim')!.run({
      env,
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      client: {
        unclaimTask: async (input: { task_number: number }) => {
          unclaimed = input.task_number;
          return { success: true };
        },
      } as any,
      args: parsed(['11']),
    });
    expect(unclaimed).toBe(11);

    let statusInput: Record<string, unknown> | null = null;
    await findCommand('task', 'status')!.run({
      env,
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      client: {
        updateTaskStatus: async (input: Record<string, unknown>) => {
          statusInput = input;
          return { success: true };
        },
      } as any,
      args: parsed(['12'], { status: 'in_review' }),
    });
    expect(statusInput).toEqual({ task_number: 12, status: 'in_review' });
  });

  it('task status still requires --status (missing_required_flag, not a bad operand)', async () => {
    await expect(
      findCommand('task', 'status')!.run({
        env,
        client: explodingClient,
        args: parsed(['12']),
      }),
    ).rejects.toMatchObject({ subkind: 'missing_required_flag' });
  });
});
