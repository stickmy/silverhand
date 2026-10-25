/**
 * P0-g (#task:2018) — the `--id` global-identity selector grammar on the
 * task verbs. Locks the CLI-side contract:
 *  - `--id` is the ONLY spelling of the global selector (the positional
 *    operand stays a board-local number; token shape is never guessed);
 *  - `--id` pairs with NOTHING: positional numbers and workspace selectors
 *    are rejected pre-flight (two coordinate systems, one operand);
 *  - claim batches are single-family: repeated `--id` XOR positional
 *    numbers;
 *  - wire mapping sends `task_id` / `task_ids` and NO workspace fields.
 */
import { describe, expect, it } from 'vitest';
import { findCommand } from '../src/commands/index.js';
import { CliError } from '../src/errors.js';
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

function capturing(method: string): {
  client: Record<string, (input: Record<string, unknown>) => Promise<unknown>>;
  capture: () => Record<string, unknown> | null;
} {
  let last: Record<string, unknown> | null = null;
  return {
    capture: () => last,
    client: {
      [method]: async (input: Record<string, unknown>) => {
        last = input;
        return { ok: true };
      },
    },
  };
}

async function expectCliError(run: Promise<unknown>, subkind: string): Promise<void> {
  try {
    await run;
  } catch (err) {
    expect(err).toBeInstanceOf(CliError);
    expect((err as CliError).subkind).toBe(subkind);
    return;
  }
  throw new Error(`expected CliError ${subkind}, but the run succeeded`);
}

describe('task --id selector grammar (P0-g)', () => {
  const detail = findCommand('task', 'detail')!;
  const status = findCommand('task', 'status')!;
  const unclaim = findCommand('task', 'unclaim')!;
  const edit = findCommand('task', 'edit')!;
  const claim = findCommand('task', 'claim')!;

  it('detail --id maps to {task_id} and nothing else', async () => {
    const { client, capture } = capturing('getTaskDetail');
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    await detail.run({ env, client: client as any, args: parsed([], { id: 'vls9VUa02jTqVVXV' }) });
    expect(capture()).toEqual({ task_id: 'vls9VUa02jTqVVXV' });
  });

  it('detail keeps the positional number lane byte-identical (task_number + selector)', async () => {
    const { client, capture } = capturing('getTaskDetail');
    await detail.run({
      env,
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      client: client as any,
      args: parsed(['42'], { 'workspace-id': 'w1' }),
    });
    expect(capture()).toEqual({ task_number: 42, workspace_id: 'w1' });
  });

  it('rejects --id + positional (two coordinate systems, one operand)', async () => {
    await expectCliError(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      detail.run({ env, client: {} as any, args: parsed(['42'], { id: 'abc' }) }),
      'mutually_exclusive',
    );
  });

  it('rejects --id + workspace selector on every single-task verb', async () => {
    for (const cmd of [detail, status, unclaim, edit]) {
      await expectCliError(
        cmd.run({
          env,
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          client: {} as any,
          args: parsed([], { id: 'abc', 'workspace-id': 'w1', status: 'done', title: 'x' }),
        }),
        'mutually_exclusive',
      );
    }
  });

  it('rejects a value-less or empty --id pre-flight', async () => {
    await expectCliError(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      detail.run({ env, client: {} as any, args: parsed([], { id: true }) }),
      'missing_flag_value',
    );
    await expectCliError(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      detail.run({ env, client: {} as any, args: parsed([], { id: '' }) }),
      'missing_flag_value',
    );
  });

  it('rejects a repeated --id on single-task verbs (batch is claim-only)', async () => {
    await expectCliError(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      detail.run({ env, client: {} as any, args: parsed([], { id: ['a', 'b'] }) }),
      'unexpected_arguments',
    );
  });

  it('status --id maps to {task_id, status}', async () => {
    const { client, capture } = capturing('updateTaskStatus');
    await status.run({
      env,
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      client: client as any,
      args: parsed([], { id: 'abc', status: 'in_review' }),
    });
    expect(capture()).toEqual({ task_id: 'abc', status: 'in_review' });
  });

  it('unclaim --id maps to {task_id}', async () => {
    const { client, capture } = capturing('unclaimTask');
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    await unclaim.run({ env, client: client as any, args: parsed([], { id: 'abc' }) });
    expect(capture()).toEqual({ task_id: 'abc' });
  });

  it('edit --id --title maps to {task_id, title}', async () => {
    const { client, capture } = capturing('editTask');
    await edit.run({
      env,
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      client: client as any,
      args: parsed([], { id: 'abc', title: 'renamed' }),
    });
    expect(capture()).toEqual({ task_id: 'abc', title: 'renamed' });
  });

  it('claim: repeated --id maps to a pure {task_ids} batch', async () => {
    const { client, capture } = capturing('claimTasks');
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    await claim.run({ env, client: client as any, args: parsed([], { id: ['a', 'b'] }) });
    expect(capture()).toEqual({ task_ids: ['a', 'b'] });
    // Single --id is a one-entry batch.
    const single = capturing('claimTasks');
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    await claim.run({ env, client: single.client as any, args: parsed([], { id: 'a' }) });
    expect(single.capture()).toEqual({ task_ids: ['a'] });
  });

  it('claim: mixed batch and id+selector are rejected pre-flight', async () => {
    await expectCliError(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      claim.run({ env, client: {} as any, args: parsed(['42'], { id: 'a' }) }),
      'mutually_exclusive',
    );
    await expectCliError(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      claim.run({ env, client: {} as any, args: parsed([], { id: 'a', workspace: 'eng' }) }),
      'mutually_exclusive',
    );
    // Number batches keep their existing lane untouched.
    const { client, capture } = capturing('claimTasks');
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    await claim.run({ env, client: client as any, args: parsed(['42', '43'], {}) });
    expect(capture()).toEqual({ task_numbers: [42, 43] });
  });
});

describe('task list help states the scope-branched defaults (B3, #task:2018 review round 1)', () => {
  it('describes both defaults and never claims current-workspace as the unconditional default', () => {
    const list = findCommand('task', 'list')!;
    // Both branches present.
    expect(list.help).toContain('conversation-scoped');
    expect(list.help).toContain('agent-scoped');
    expect(list.help).toContain('UNION of every Board you can read');
    // Filters narrow, rows self-locate.
    expect(list.help).toContain('only NARROW');
    expect(list.help).toContain('do NOT');
    // The retired ambient narrative must not come back.
    expect(list.help.includes("the default is the actor's current workspace")).toBe(false);
    expect(list.help.includes('Returns aggregated tasks across the current workspace')).toBe(false);
  });
});

describe('task list scope flags', () => {
  const list = findCommand('task', 'list')!;

  it('passes --target and --workspace through as query fields', async () => {
    const { client, capture } = capturing('listTasks');
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    await list.run({ env, client: client as any, args: parsed([], { target: '#conv:abc' }) });
    expect(capture()).toEqual({ target: '#conv:abc' });
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    await list.run({ env, client: client as any, args: parsed([], { workspace: 'eng' }) });
    expect(capture()).toEqual({ workspace_name: 'eng' });
  });

  it('rejects --target with another scope flag', async () => {
    const others: Record<string, string>[] = [
      { workspace: 'eng' },
      { 'workspace-id': 'w' },
      { 'conversation-id': 'c' },
    ];
    for (const other of others) {
      await expectCliError(
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        list.run({ env, client: {} as any, args: parsed([], { target: '#conv:abc', ...other }) }),
        'mutually_exclusive',
      );
    }
  });
});

describe('task status accepts several task numbers', () => {
  const status = findCommand('task', 'status')!;

  it('keeps the single-number result shape', async () => {
    const { client, capture } = capturing('updateTaskStatus');
    const result = await status.run({
      env,
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      client: client as any,
      args: parsed(['7'], { status: 'done' }),
    });
    expect(capture()).toEqual({ task_number: 7, status: 'done' });
    expect(result).toEqual({ ok: true });
  });

  it('applies each number in order and reports per-task outcomes', async () => {
    const calls: number[] = [];
    const client = {
      updateTaskStatus: async (input: { task_number: number }) => {
        calls.push(input.task_number);
        if (input.task_number === 42)
          throw new CliError({ code: 'not_found', message: 'gone', retriable: false });
        return input.task_number === 43
          ? { success: false, code: 'task_not_found', reason: 'no task' }
          : { success: true };
      },
    };
    const result = await status.run({
      env,
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      client: client as any,
      args: parsed(['41', '42', '43'], { status: 'done', workspace: 'eng' }),
    });
    expect(calls).toEqual([41, 42, 43]);
    expect(result).toEqual({
      results: [
        { task_number: 41, success: true },
        { task_number: 42, success: false, code: 'not_found', reason: 'gone' },
        { task_number: 43, success: false, code: 'task_not_found', reason: 'no task' },
      ],
    });
  });
});
