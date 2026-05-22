import { describe, expect, it } from 'vitest';
import { COMMAND_GROUPS, findCommand, findGroup } from '../src/commands/index.js';
import type { BackendClient } from '../src/client.js';

describe('command catalogue', () => {
  it('exposes the seven nouns we promised in the prompt', () => {
    const nouns = COMMAND_GROUPS.map(g => g.noun);
    expect(nouns).toEqual([
      'message',
      'task',
      'thread',
      'channel',
      'workspace',
      'schedule',
      'file',
    ]);
  });

  it('matches every MCP tool with exactly one CLI verb', () => {
    // The 18 backend internal routes that MCP currently exposes.
    // If you add a route, add a CLI command, and add it here.
    const expected: Array<[string, string]> = [
      ['message', 'send'],
      ['message', 'history'],
      ['message', 'search'],
      ['task', 'list'],
      ['task', 'create'],
      ['task', 'claim'],
      ['task', 'unclaim'],
      ['task', 'status'],
      ['thread', 'create'],
      ['thread', 'close'],
      ['channel', 'create'],
      ['channel', 'list'],
      ['workspace', 'list'],
      ['schedule', 'create'],
      ['schedule', 'list'],
      ['schedule', 'cancel'],
      ['file', 'upload'],
      ['file', 'view'],
    ];
    for (const [noun, verb] of expected) {
      const cmd = findCommand(noun, verb);
      expect(cmd, `${noun} ${verb}`).toBeDefined();
    }
    const total = COMMAND_GROUPS.reduce((acc, g) => acc + g.commands.length, 0);
    expect(total).toBe(18);
  });

  it('every command has a non-empty help block', () => {
    for (const g of COMMAND_GROUPS) {
      for (const c of g.commands) {
        expect(c.help, `${g.noun} ${c.verb}`).toMatch(/silverhand /);
        expect(c.summary.length, `${g.noun} ${c.verb}`).toBeGreaterThan(0);
      }
    }
  });

  it('returns undefined for unknown noun/verb (caller surfaces validation_error)', () => {
    expect(findGroup('bogus')).toBeUndefined();
    expect(findCommand('message', 'bogus')).toBeUndefined();
  });

  it('every command declares its allowed flags so the dispatcher can reject typos', () => {
    // Strict-flag validation lives in cli.ts but it consumes the
    // `flags` + `booleanFlags` arrays declared on each command. If a
    // command doesn't declare them, the dispatcher would silently accept
    // any flag and re-introduce the "agent typed a typo, got the full
    // board" footgun. Workspace.list legitimately takes nothing — it
    // declares an empty `flags: []`.
    for (const g of COMMAND_GROUPS) {
      for (const c of g.commands) {
        expect(c.flags, `${g.noun} ${c.verb} missing flags declaration`).toBeDefined();
      }
    }
  });

  it('command help blocks do not leak transport/credential semantics to agents', () => {
    // The CLI is already configured for the current session by the
    // backend. Help text the agent reads must not mention SILVERHAND_*
    // env vars, AUTH_TOKEN, or "auth"-flavored exit-code descriptions —
    // the prompt explicitly keeps these layers transparent. We match
    // the four known env vars by name (rather than `SILVERHAND_` as a
    // bare prefix) so unrelated tokens like the heredoc delimiter
    // `SILVERHAND_EOF` recommended in `silverhand message send` help
    // are not falsely flagged.
    const forbidden = [
      /SILVERHAND_(?:AUTH_TOKEN|CONVERSATION_ID|AGENT_ID|SERVER_URL)\b/,
      /AUTH_TOKEN/,
      /\bauth failure\b/i,
    ];
    for (const g of COMMAND_GROUPS) {
      for (const c of g.commands) {
        for (const re of forbidden) {
          expect(
            c.help,
            `${g.noun} ${c.verb} help leaks "${re}"`,
          ).not.toMatch(re);
        }
      }
    }
  });
});

describe('thread close flag plumbing', () => {
  it('forwards --allow-open-tasks to the client as allow_open_tasks: true', async () => {
    const cmd = findCommand('thread', 'close');
    expect(cmd).toBeDefined();
    expect(cmd!.booleanFlags).toContain('allow-open-tasks');
    const calls: Array<Record<string, unknown>> = [];
    const fakeClient = {
      closeThread: async (input: Record<string, unknown>) => {
        calls.push(input);
        return { thread: { id: 't' } };
      },
    } as unknown as BackendClient;
    await cmd!.run({
      client: fakeClient,
      args: {
        flags: { summary: 'done', 'allow-open-tasks': true },
        positional: [],
        rest: [],
      },
    } as any);
    expect(calls).toHaveLength(1);
    expect(calls[0]).toEqual({ summary: 'done', allow_open_tasks: true });
  });

  it('omits allow_open_tasks when the flag is absent (back-compat default)', async () => {
    const cmd = findCommand('thread', 'close');
    expect(cmd).toBeDefined();
    const calls: Array<Record<string, unknown>> = [];
    const fakeClient = {
      closeThread: async (input: Record<string, unknown>) => {
        calls.push(input);
        return { thread: { id: 't' } };
      },
    } as unknown as BackendClient;
    await cmd!.run({
      client: fakeClient,
      args: {
        flags: { summary: 'done' },
        positional: [],
        rest: [],
      },
    } as any);
    expect(calls[0]).toEqual({ summary: 'done' });
    expect(calls[0]).not.toHaveProperty('allow_open_tasks');
  });
});
