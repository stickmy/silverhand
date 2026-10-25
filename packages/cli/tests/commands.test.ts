import { describe, expect, it } from 'vitest';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { AVATAR_MAX_BYTES, parseDeliveryTarget } from '@silverhand/shared';
import {
  COMMAND_GROUPS,
  findCommand,
  findFacet,
  findFacetCommand,
  findGroup,
} from '../src/commands/index.js';
import type { BackendClient } from '../src/client.js';

/** Every leaf command, whether flat (`noun verb`) or faceted
 *  (`noun facet verb`), with a human label for assertion messages. */
function allCommands(): Array<{ label: string; cmd: import('../src/commands/index.js').Command }> {
  const out: Array<{ label: string; cmd: import('../src/commands/index.js').Command }> = [];
  for (const g of COMMAND_GROUPS) {
    for (const c of g.commands) out.push({ label: `${g.noun} ${c.verb}`, cmd: c });
    for (const f of g.facets ?? []) {
      for (const c of f.commands) out.push({ label: `${g.noun} ${f.facet} ${c.verb}`, cmd: c });
    }
  }
  return out;
}

describe('command catalogue', () => {
  it('every command has a non-empty help block', () => {
    for (const { label, cmd } of allCommands()) {
      expect(cmd.help, label).toMatch(/silverhand /);
      expect(cmd.summary.length, label).toBeGreaterThan(0);
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
    for (const { label, cmd } of allCommands()) {
      expect(cmd.flags, `${label} missing flags declaration`).toBeDefined();
    }
  });

  // ── Shape laws for the #task:1945 interface convergence ─────────────
  // These are contract guards, not snapshots: each one locks a rule that
  // typecheck cannot see, and each fails loudly when a future command
  // quietly re-introduces the shape the convergence removed.

  it('only the declared allowlist opts into positional arguments', () => {
    // `allowPositional` is a bare boolean on each command, so it has no
    // declaration surface a reviewer can diff — a new `true` anywhere
    // would silently reopen the "bash word-split tail landed in
    // positional" footgun that the dispatcher's A1 guard exists to
    // prevent. This asserts the EXACT set, so adding a consumer forces
    // an explicit change here (and therefore a review of its arity
    // parsing). It deliberately does NOT verify arity: that lives in
    // each command's own parser and its branch tests.
    const positional = allCommands()
      .filter(({ cmd }) => cmd.allowPositional === true)
      .map(({ label }) => label)
      .sort();
    expect(positional).toEqual([
      'file update', // <attachment_id> (alias of --attachment-id)
      'file upload', // <abs_path> (alias of --path)
      'file view', // <attachment_id> (alias of --attachment-id)
      'message search', // free-text query
      'task claim', // <task_number> ... (one-or-more)
      'task detail', // <task_number> (exactly-one)
      'task edit',
      'task status',
      'task unclaim',
    ]);
  });

  it('--task-number is a list-only filter; the by-number verbs teach the positional shape', () => {
    // One name, one role. On `task list` it is an OPTIONAL repeatable
    // FILTER; on the by-number verbs it used to be the REQUIRED operand,
    // which is now positional. Accepting both spellings there would
    // leave two ways to say the same required argument — the redundancy
    // this cut removes — so the flag must be `removedFlags` (an
    // actionable `flag_removed` hint), never an accepted alias.
    const byNumberVerbs = ['claim', 'unclaim', 'status', 'edit', 'detail'];
    for (const verb of byNumberVerbs) {
      const cmd = findCommand('task', verb)!;
      expect(cmd.flags, `task ${verb} still accepts --task-number`).not.toContain('task-number');
      expect(
        cmd.removedFlags?.['task-number'],
        `task ${verb} must teach the positional shape`,
      ).toBeDefined();
      expect(cmd.removedFlags!['task-number']!.hint).toContain('positional');
    }
    expect(findCommand('task', 'list')!.flags).toContain('task-number');
  });

  it('no command accepts --channel-id: conversation is the one name for the range', () => {
    // Channel/thread/conversation had three spellings on the wire and in
    // flags. `--conversation-id` is canonical; `message search` keeps
    // `--channel-id` only as a removed-flag migration hint.
    for (const { label, cmd } of allCommands()) {
      expect(cmd.flags ?? [], `${label} accepts --channel-id`).not.toContain('channel-id');
      expect(cmd.booleanFlags ?? [], `${label} accepts --channel-id`).not.toContain('channel-id');
    }
    const search = findCommand('message', 'search')!;
    expect(search.removedFlags?.['channel-id']?.replacement).toBe('--conversation-id');
    expect(search.flags).toContain('conversation-id');
  });

  it('message search is selector-only: no --scope, and the three selectors are declared', () => {
    const search = findCommand('message', 'search')!;
    expect(search.flags, 'search still takes a mode flag').not.toContain('scope');
    expect(search.removedFlags?.['scope']).toBeDefined();
    expect(search.flags).toContain('workspace-id');
    expect(search.flags).toContain('conversation-id');
    expect(search.booleanFlags).toContain('current-workspace');
  });

  it("an entity flag that carries that entity's id is spelled <entity>-id", () => {
    // The naming law, in the only form the real surface supports: for an
    // entity whose canonical flag IS the id (`--attachment-id`,
    // `--conversation-id`, `--schedule-id`), the bare noun must never come
    // back. Without the suffix an agent cannot tell from the flag name
    // whether to pass an id or a name, which is the guess that produced
    // repeated `--attachment <label>` failures.
    //
    // Deliberately NOT asserted, because the surface legitimately differs:
    // role-named flags carry ids under a positional name (`--root
    // <message_id>`, `--around <message_id>`, `--assignee <agent_id>`) —
    // they name a relation, not an entity; and a bare noun that takes free
    // TEXT is correct as-is (`schedule create --message <text>`). Widening
    // this list therefore needs a real `-id` sibling to point at.
    const bareEntityNouns = ['attachment', 'conversation', 'channel', 'schedule', 'event', 'task'];
    for (const { label, cmd } of allCommands()) {
      const declared = [...(cmd.flags ?? []), ...(cmd.booleanFlags ?? [])];
      for (const noun of bareEntityNouns) {
        expect(
          declared,
          `${label} declares --${noun}; an id-bearing entity flag must be --${noun}-id`,
        ).not.toContain(noun);
      }
    }
  });

  it('stale controls address the exact held draft object', () => {
    const resolve = findCommand('message', 'stale-resolve')!;
    const receive = findCommand('message', 'stale-receive')!;
    expect(resolve.flags).toEqual(['stale-event-id', 'resolution']);
    expect(receive.flags).toEqual(['stale-event-id', 'wait']);
    expect(resolve.removedFlags?.['event-id']?.replacement).toBe('stale-event-id');
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
    for (const { label, cmd } of allCommands()) {
      for (const re of forbidden) {
        expect(cmd.help, `${label} help leaks "${re}"`).not.toMatch(re);
      }
    }
  });
});

describe('profile facet (3-level: profile mcp <verb>)', () => {
  it('resolves a facet command by (noun, facet, verb)', () => {
    expect(findFacetCommand('profile', 'mcp', 'get')).toBeDefined();
    expect(findFacetCommand('profile', 'mcp', 'set')).toBeDefined();
    expect(findFacetCommand('profile', 'mcp', 'add')).toBeDefined();
    expect(findFacetCommand('profile', 'mcp', 'remove')).toBeDefined();
    expect(findFacetCommand('profile', 'mcp', 'clear')).toBeDefined();
    expect(findFacetCommand('profile', 'mcp', 'validate')).toBeDefined();
    expect(findFacetCommand('profile', 'mcp', 'probe')).toBeDefined();
    expect(findFacetCommand('profile', 'mcp', 'bogus')).toBeUndefined();
    expect(findFacet('profile', 'bogus')).toBeUndefined();
  });

  it('the profile noun carries BOTH flat verbs and the mcp facet (#task:1178)', () => {
    // Flat `profile get|update` coexist with the `mcp` facet — the
    // dispatcher tries the facet token first, then falls through to a
    // flat verb, so `profile mcp get` and `profile get` never collide.
    const flat = findGroup('profile')?.commands.map((c) => c.verb);
    expect(flat).toEqual(['get', 'update']);
    expect(findCommand('profile', 'get')).toBeDefined();
    expect(findCommand('profile', 'update')).toBeDefined();
    // The facet is unaffected.
    expect(findFacetCommand('profile', 'mcp', 'get')).toBeDefined();
  });

  it('validate is CLI-local: parses stdin and reports server_count, no backend call', async () => {
    const cmd = findFacetCommand('profile', 'mcp', 'validate')!;
    const raw = JSON.stringify({
      mcpServers: { a: { command: 'x' }, b: { type: 'http', url: 'https://e/' } },
    });
    const result = await runWithStdin(cmd, raw, {} as BackendClient);
    expect(result).toEqual({ ok: true, server_count: 2 });
  });

  it('validate rejects bad JSON with validation_error (no backend call)', async () => {
    const cmd = findFacetCommand('profile', 'mcp', 'validate')!;
    await expect(runWithStdin(cmd, '{ not json', {} as BackendClient)).rejects.toMatchObject({
      code: 'validation_error',
    });
  });

  it('set pipes raw stdin verbatim to the client', async () => {
    const cmd = findFacetCommand('profile', 'mcp', 'set')!;
    let captured: string | undefined;
    const client = {
      setProfileMcp: async (raw: string) => {
        captured = raw;
        return { ok: true, bytes_written: raw.length };
      },
    } as unknown as BackendClient;
    const raw = '{"mcpServers":{"a":{"command":"x"}}}';
    const result = await runWithStdin(cmd, raw, client);
    expect(captured).toBe(raw);
    expect(result).toMatchObject({ ok: true });
  });

  // add/remove/clear are CLI-orchestrated over get+set (the host stays an
  // opaque verbatim writer — #task:1135). Their merge / collision /
  // not-found logic lives ONLY here, so it is unit-tested here.

  /** Mock client whose getProfileMcp returns `existingRaw` and whose
   *  setProfileMcp records the written text. */
  function mcpClient(existingRaw: string | null) {
    const calls: { wrote?: string } = {};
    const client = {
      getProfileMcp: async () => ({ raw: existingRaw }),
      setProfileMcp: async (raw: string) => {
        calls.wrote = raw;
        return { ok: true, bytes_written: raw.length };
      },
    } as unknown as BackendClient;
    return { client, calls };
  }

  it('add merges a new server, preserving existing ones', async () => {
    const cmd = findFacetCommand('profile', 'mcp', 'add')!;
    const { client, calls } = mcpClient('{"mcpServers":{"a":{"command":"x"}}}');
    const result = await runWithStdin(
      cmd,
      '{"mcpServers":{"b":{"type":"http","url":"https://e/"}}}',
      client,
    );
    expect(result).toEqual({ ok: true, added: ['b'], total: 2 });
    expect(JSON.parse(calls.wrote!)).toEqual({
      mcpServers: {
        a: { command: 'x' },
        b: { type: 'http', url: 'https://e/' },
      },
    });
  });

  it('add fails with conflict on a same-named server and writes nothing', async () => {
    const cmd = findFacetCommand('profile', 'mcp', 'add')!;
    const { client, calls } = mcpClient('{"mcpServers":{"a":{"command":"x"}}}');
    await expect(
      runWithStdin(cmd, '{"mcpServers":{"a":{"command":"y"}}}', client),
    ).rejects.toMatchObject({ code: 'conflict' });
    expect(calls.wrote).toBeUndefined();
  });

  it('add --replace overwrites only the named server, keeping the rest', async () => {
    const cmd = findFacetCommand('profile', 'mcp', 'add')!;
    const { client, calls } = mcpClient(
      '{"mcpServers":{"a":{"command":"old"},"b":{"command":"keep"}}}',
    );
    const result = await runWithStdin(cmd, '{"mcpServers":{"a":{"command":"new"}}}', client, {
      replace: true,
    });
    expect(result).toEqual({ ok: true, added: ['a'], total: 2 });
    expect(JSON.parse(calls.wrote!)).toEqual({
      mcpServers: { a: { command: 'new' }, b: { command: 'keep' } },
    });
  });

  it('add onto an unset config writes just the new servers', async () => {
    const cmd = findFacetCommand('profile', 'mcp', 'add')!;
    const { client, calls } = mcpClient(null);
    const result = await runWithStdin(cmd, '{"mcpServers":{"a":{"command":"x"}}}', client);
    expect(result).toEqual({ ok: true, added: ['a'], total: 1 });
    expect(JSON.parse(calls.wrote!)).toEqual({ mcpServers: { a: { command: 'x' } } });
  });

  it('remove deletes the named server and keeps the others', async () => {
    const cmd = findFacetCommand('profile', 'mcp', 'remove')!;
    const { client, calls } = mcpClient('{"mcpServers":{"a":{"command":"x"},"b":{"command":"y"}}}');
    const result = await runWithStdin(cmd, '', client, { server: 'a' });
    expect(result).toEqual({ ok: true, removed: 'a', remaining: 1 });
    expect(JSON.parse(calls.wrote!)).toEqual({ mcpServers: { b: { command: 'y' } } });
  });

  it('remove fails with not_found for an absent server (no write)', async () => {
    const cmd = findFacetCommand('profile', 'mcp', 'remove')!;
    const { client, calls } = mcpClient('{"mcpServers":{"a":{"command":"x"}}}');
    await expect(runWithStdin(cmd, '', client, { server: 'zzz' })).rejects.toMatchObject({
      code: 'not_found',
    });
    expect(calls.wrote).toBeUndefined();
  });

  it('remove fails with not_found when no config is set', async () => {
    const cmd = findFacetCommand('profile', 'mcp', 'remove')!;
    const { client } = mcpClient(null);
    await expect(runWithStdin(cmd, '', client, { server: 'a' })).rejects.toMatchObject({
      code: 'not_found',
    });
  });

  it('clear writes an empty config', async () => {
    const cmd = findFacetCommand('profile', 'mcp', 'clear')!;
    const { client, calls } = mcpClient('{"mcpServers":{"a":{"command":"x"}}}');
    const result = await runWithStdin(cmd, '', client);
    expect(result).toEqual({ ok: true, cleared: true });
    expect(JSON.parse(calls.wrote!)).toEqual({ mcpServers: {} });
  });
});

/** Run a command whose `run` reads stdin, by feeding `input` through a
 *  faked `process.stdin` async-iterator (the same shape the command's
 *  `for await (const chunk of process.stdin)` consumes). */
async function runWithStdin(
  cmd: import('../src/commands/index.js').Command,
  input: string,
  client: BackendClient,
  flags: Record<string, unknown> = {},
): Promise<unknown> {
  const realStdin = Object.getOwnPropertyDescriptor(process, 'stdin');
  const fake = {
    isTTY: false,
    async *[Symbol.asyncIterator]() {
      yield Buffer.from(input, 'utf8');
    },
  };
  Object.defineProperty(process, 'stdin', { value: fake, configurable: true });
  try {
    return await cmd.run({
      env: {
        conversationId: 'conv',
        agentId: 'agent',
        serverUrl: 'http://127.0.0.1:4488',
        authToken: 'token',
      },
      client,
      args: { flags, positional: [], rest: [] },
    } as any);
  } finally {
    if (realStdin) Object.defineProperty(process, 'stdin', realStdin);
  }
}

describe('profile update behavior (#task:1178)', () => {
  // Magic-byte PNG header — enough for `sniffImageMime` to classify it as
  // image/png (it reads only the leading signature, not a full decode).
  const PNG_BYTES = Buffer.from([
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x00,
  ]);

  function profileUpdateCmd(): import('../src/commands/index.js').Command {
    const cmd = findCommand('profile', 'update');
    expect(cmd).toBeDefined();
    return cmd!;
  }

  /** A client that records every `updateProfile` call and returns `profile`
   *  (the command unwraps `{ profile }` and prints the bare DTO). */
  function captureUpdateClient(
    profile: Record<string, unknown> = {
      id: 'agent',
      name: 'a',
      display_name: 'A',
      description: null,
      avatar_url: null,
    },
  ) {
    const calls: { count: number; input?: any } = { count: 0 };
    const client = {
      updateProfile: async (input: unknown) => {
        calls.count++;
        calls.input = input;
        return { profile };
      },
    } as unknown as BackendClient;
    return { client, calls };
  }

  function runProfileUpdate(
    flags: Record<string, unknown>,
    client: BackendClient,
  ): Promise<unknown> {
    return profileUpdateCmd().run({
      env: {
        conversationId: 'conv',
        agentId: 'agent',
        serverUrl: 'http://127.0.0.1:4488',
        authToken: 'token',
      },
      client,
      args: { flags, positional: [], rest: [] },
    } as any);
  }

  it('rejects more than one avatar action and never calls the client', async () => {
    const { client, calls } = captureUpdateClient();
    await expect(
      runProfileUpdate({ 'avatar-file': '/abs/a.png', 'clear-avatar': true }, client),
    ).rejects.toMatchObject({ code: 'validation_error' });
    expect(calls.count).toBe(0);
  });

  it('rejects an empty update (no flags) without calling the client', async () => {
    const { client, calls } = captureUpdateClient();
    await expect(runProfileUpdate({}, client)).rejects.toMatchObject({
      code: 'validation_error',
    });
    expect(calls.count).toBe(0);
  });

  it('rejects a non-absolute --avatar-file before any client call', async () => {
    const { client, calls } = captureUpdateClient();
    await expect(runProfileUpdate({ 'avatar-file': 'relative.png' }, client)).rejects.toMatchObject(
      { code: 'validation_error' },
    );
    expect(calls.count).toBe(0);
  });

  it('--avatar-file sends text + sniffed avatar bytes in one update', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'silverhand-avatar-file-'));
    try {
      const p = join(dir, 'a.png');
      await writeFile(p, PNG_BYTES);
      const fresh = {
        id: 'agent',
        name: 'a',
        display_name: 'Alice',
        description: 'd',
        avatar_url: '/api/agents/agent/avatar?v=1',
      };
      const { client, calls } = captureUpdateClient(fresh);
      const result = await runProfileUpdate({ 'display-name': 'Alice', 'avatar-file': p }, client);
      expect(calls.count).toBe(1);
      expect(calls.input.display_name).toBe('Alice');
      expect(calls.input.avatar).toMatchObject({
        action: 'set',
        filename: 'a.png',
        contentType: 'image/png',
      });
      expect(Buffer.isBuffer(calls.input.avatar.buffer)).toBe(true);
      expect((calls.input.avatar.buffer as Buffer).equals(PNG_BYTES)).toBe(true);
      // The command unwraps `{ profile }` → returns the bare DTO.
      expect(result).toEqual(fresh);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it('--clear-avatar sends a clear action and no bytes', async () => {
    const { client, calls } = captureUpdateClient();
    await runProfileUpdate({ 'clear-avatar': true }, client);
    expect(calls.count).toBe(1);
    expect(calls.input.avatar).toEqual({ action: 'clear' });
  });

  it('--avatar-url downloads CLI-side and uploads the sniffed blob', async () => {
    const origFetch = globalThis.fetch;
    globalThis.fetch = (async () => new Response(PNG_BYTES, { status: 200 })) as typeof fetch;
    try {
      const { client, calls } = captureUpdateClient();
      await runProfileUpdate({ 'avatar-url': 'https://example.test/a.png' }, client);
      expect(calls.count).toBe(1);
      expect(calls.input.avatar).toMatchObject({
        action: 'set',
        contentType: 'image/png',
      });
      expect((calls.input.avatar.buffer as Buffer).equals(PNG_BYTES)).toBe(true);
    } finally {
      globalThis.fetch = origFetch;
    }
  });

  it('--avatar-url that 404s never calls the client', async () => {
    const origFetch = globalThis.fetch;
    globalThis.fetch = (async () => new Response(null, { status: 404 })) as typeof fetch;
    try {
      const { client, calls } = captureUpdateClient();
      await expect(
        runProfileUpdate({ 'avatar-url': 'https://example.test/x.png' }, client),
      ).rejects.toMatchObject({ code: 'validation_error' });
      expect(calls.count).toBe(0);
    } finally {
      globalThis.fetch = origFetch;
    }
  });

  it('--avatar-url that exceeds the redirect cap never calls the client', async () => {
    const origFetch = globalThis.fetch;
    globalThis.fetch = (async () =>
      new Response(null, {
        status: 302,
        headers: { location: 'https://example.test/next' },
      })) as typeof fetch;
    try {
      const { client, calls } = captureUpdateClient();
      await expect(
        runProfileUpdate({ 'avatar-url': 'https://example.test/a.png' }, client),
      ).rejects.toMatchObject({ code: 'validation_error' });
      expect(calls.count).toBe(0);
    } finally {
      globalThis.fetch = origFetch;
    }
  });

  it('--avatar-url over the size cap aborts the download, no client call', async () => {
    const big = new Uint8Array(AVATAR_MAX_BYTES + 1024);
    const origFetch = globalThis.fetch;
    globalThis.fetch = (async () => new Response(big, { status: 200 })) as typeof fetch;
    try {
      const { client, calls } = captureUpdateClient();
      await expect(
        runProfileUpdate({ 'avatar-url': 'https://example.test/big.png' }, client),
      ).rejects.toMatchObject({ code: 'validation_error' });
      expect(calls.count).toBe(0);
    } finally {
      globalThis.fetch = origFetch;
    }
  });
});

describe('thread legacy verbs', () => {
  /** Every address a hint TEACHES, resolved against the real codec.
   *
   *  A migration hint is the one place an agent is most likely to copy a
   *  spelling verbatim, and both of these hints shipped the retired
   *  `thread:<message_id>` / `conversation:<id>` grammars long after the codec
   *  stopped accepting them (#task:2021 P4-4). Asserting on a substring only
   *  pins whatever the hint happens to say today; substituting real ids into
   *  the placeholders and parsing pins the thing that matters — the hint
   *  teaches an address the CLI would actually accept. */
  function taughtAddresses(hint: string): string[] {
    return [...hint.matchAll(/`[^`]*--target '([^']+)'/g)].map((m) =>
      m[1]!
        .replace('<conversation_id>', 'convAAAAAAAAAA01')
        .replace('<root_message_id>', 'msgBBBBBBBBBBB02'),
    );
  }

  it('removes create/close and keeps explicit migration hints', () => {
    expect(findCommand('thread', 'create')).toBeUndefined();
    expect(findCommand('thread', 'close')).toBeUndefined();
    const group = findGroup('thread');
    expect(group?.removedVerbs?.create).toMatchObject({
      replacement: 'follow',
    });
    expect(group?.removedVerbs?.close).toMatchObject({
      replacement: 'unfollow',
    });
    expect(group?.removedVerbs?.close?.hint).toContain('thread unfollow');

    // `create` must send the agent to a reply cluster, `close` to a whole
    // conversation — so the two hints are not interchangeable, and each one's
    // address must both parse and name the right SHAPE.
    const created = taughtAddresses(group!.removedVerbs!.create!.hint);
    expect(created).toHaveLength(1);
    expect(parseDeliveryTarget(created[0]!)).toMatchObject({
      conversationId: 'convAAAAAAAAAA01',
      threadRootId: 'msgBBBBBBBBBBB02',
    });

    const closed = taughtAddresses(group!.removedVerbs!.close!.hint);
    expect(closed).toHaveLength(1);
    expect(parseDeliveryTarget(closed[0]!)).toMatchObject({
      conversationId: 'convAAAAAAAAAA01',
      threadRootId: null,
    });
  });
});

describe('file view command', () => {
  it('returns the zero-copy local path when the loopback backend path is readable', async () => {
    const cmd = findCommand('file', 'view');
    expect(cmd).toBeDefined();
    const dir = await mkdtemp(join(tmpdir(), 'silverhand-file-view-local-'));
    try {
      const localPath = join(dir, 'shot.png');
      await writeFile(localPath, Buffer.from('local-bytes'));
      let downloaded = false;
      const fakeClient = {
        locateAttachment: async () => ({
          storage: 'local',
          local_path: localPath,
          filename: 'shot.png',
          content_type: 'image/png',
        }),
        downloadAttachment: async () => {
          downloaded = true;
          return { buffer: Buffer.from('remote-bytes'), contentType: 'image/png' };
        },
      } as unknown as BackendClient;

      const result = await cmd!.run({
        env: {
          conversationId: 'conv-local',
          agentId: 'agent',
          serverUrl: 'http://127.0.0.1:4488',
          authToken: 'token',
        },
        client: fakeClient,
        args: {
          flags: { 'attachment-id': 'att-local' },
          positional: [],
          rest: [],
        },
      } as any);

      expect(downloaded).toBe(false);
      expect(result).toEqual({
        storage: 'local',
        local_path: localPath,
        filename: 'shot.png',
        content_type: 'image/png',
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it('downloads remote-backend attachments into a local cache path', async () => {
    const cmd = findCommand('file', 'view');
    expect(cmd).toBeDefined();
    const silverhandDir = await mkdtemp(join(tmpdir(), 'silverhand-file-view-cache-'));
    const previousSilverhandDir = process.env.SILVERHAND_DIR;
    try {
      process.env.SILVERHAND_DIR = `  ${silverhandDir}  `;
      const result = (await cmd!.run({
        env: {
          slot: 'agent',
          conversationId: null,
          agentId: 'agent',
          serverUrl: 'https://remote.example.test',
          authToken: 'token',
        },
        client: {
          locateAttachment: async () => ({
            storage: 'local',
            local_path: '/remote/server/blobs/shot.png',
            filename: 'shot.png',
            content_type: 'image/png',
          }),
          downloadAttachment: async () => ({
            buffer: Buffer.from('remote-bytes'),
            contentType: 'image/png',
          }),
        } as unknown as BackendClient,
        args: {
          flags: { 'attachment-id': 'att-remote' },
          positional: [],
          rest: [],
        },
      } as any)) as {
        storage: 'local';
        local_path: string;
        filename: string;
        content_type: string;
      };

      expect(result.storage).toBe('local');
      expect(result.local_path).toBe(
        join(silverhandDir, 'attachments-cache', 'agent', 'att-remote', 'shot.png'),
      );
      expect(result.filename).toBe('shot.png');
      expect(result.content_type).toBe('image/png');
      expect(await readFile(result.local_path, 'utf8')).toBe('remote-bytes');
    } finally {
      if (previousSilverhandDir === undefined) delete process.env.SILVERHAND_DIR;
      else process.env.SILVERHAND_DIR = previousSilverhandDir;
      await rm(silverhandDir, { recursive: true, force: true });
    }
  });
});

describe('task create --target <address>', () => {
  // Locks the load-bearing contract of the collapsed door (#task:2021 P4-4): the
  // task's stream is ONE required address, it reaches the wire VERBATIM as
  // `target`, and it is never conditional — a create with no `target` field on
  // the wire would be the ambient "current conversation" default this shard
  // removed, and backend/schema/typecheck would all stay green if the CLI
  // silently dropped it.
  const CONV = 'convAAAAAAAAAA01';
  const ROOT = 'rootBBBBBBBBBB02';

  function captureCreateTasks(flags: Record<string, string>) {
    const cmd = findCommand('task', 'create');
    expect(cmd).toBeDefined();
    let captured: unknown;
    let calls = 0;
    const client = {
      createTasks: async (input: unknown) => {
        calls += 1;
        captured = input;
        return { tasks: [] };
      },
    } as unknown as BackendClient;
    return cmd!
      .run({
        env: {
          conversationId: CONV,
          slot: 'conversation',
          agentId: 'agent',
          serverUrl: 'http://127.0.0.1:4488',
          authToken: 'token',
        },
        client,
        args: { flags, positional: [], rest: [] },
      } as any)
      .then(() => ({
        input: captured as { tasks: unknown[]; target?: unknown; conversation_id?: unknown },
        calls,
      }));
  }

  /** The same door with a client that MUST NOT be reached. */
  async function runExpectingNoRequest(flags: Record<string, string>) {
    const cmd = findCommand('task', 'create');
    return cmd!.run({
      env: {
        conversationId: CONV,
        slot: 'conversation',
        agentId: 'agent',
        serverUrl: 'http://127.0.0.1:4488',
        authToken: 'token',
      },
      client: {
        createTasks: async () => {
          throw new Error('the door must refuse before any request');
        },
      } as unknown as BackendClient,
      args: { flags, positional: [], rest: [] },
    } as any);
  }

  it('forwards --target verbatim, and unconditionally', async () => {
    const { input, calls } = await captureCreateTasks({ title: 'T', target: `#conv:${CONV}` });
    expect(calls).toBe(1);
    expect(input.tasks).toEqual([{ title: 'T' }]);
    expect(input.target).toBe(`#conv:${CONV}`);
    // The retired place-naming fields are GONE from the wire, not merely unused.
    expect('conversation_id' in input).toBe(false);
    expect('reply_to_message_id' in input).toBe(false);
  });

  it('forwards a thread suffix as part of the ONE address, undecomposed', async () => {
    // The CLI does not split the address into conversation + root: the resolver
    // is the only thing that decides what the suffix denotes, so a client-side
    // split would be a second opinion that could disagree with it.
    const { input } = await captureCreateTasks({
      title: 'T',
      target: `#conv:${CONV}/thread:${ROOT}`,
    });
    expect(input.target).toBe(`#conv:${CONV}/thread:${ROOT}`);
  });

  it.each([
    ['the readable name form', '#silverhand/roadmap'],
    ['the direct-pair shorthand', 'dm:@norman'],
  ])(
    'forwards %s verbatim too — the CLI has no opinion on which spelling',
    async (_label, address) => {
      const { input } = await captureCreateTasks({ title: 'T', target: address });
      expect(input.target).toBe(address);
    },
  );

  it('refuses a missing --target as target_required, before any request', async () => {
    await expect(runExpectingNoRequest({ title: 'T' })).rejects.toMatchObject({
      code: 'validation_error',
      subkind: 'target_required',
      // The one correction: a ConversationSlot inhabits exactly one stream, and
      // the id form of it is always spellable.
      details: { flag: 'target', correct_target: `#conv:${CONV}` },
    });
  });

  it('refuses a --target that is not an address as target_malformed, before stdin is read', async () => {
    // `task create` reads its body from stdin, so a target that could never land
    // must fail before the caller's piped body is drained.
    await expect(runExpectingNoRequest({ title: 'T', target: 'channel' })).rejects.toMatchObject({
      code: 'validation_error',
      subkind: 'target_malformed',
    });
  });

  it('retires --conversation-id and --reply-to as removed flags that teach the address', () => {
    const cmd = findCommand('task', 'create')!;
    expect(cmd.flags).toContain('target');
    for (const flag of ['conversation-id', 'reply-to']) {
      const removed = cmd.removedFlags?.[flag];
      expect(removed, flag).toBeDefined();
      // An actionable `flag_removed` hint, never an accepted alias: both used to
      // name a PLACE, and both places are now part of the one address.
      expect(removed!.hint).toContain('--target');
    }
    expect(cmd.flags).not.toContain('conversation-id');
    expect(cmd.flags).not.toContain('reply-to');
  });

  // The route refuses cross-conversation + --claim with the typed
  // `cross_conversation_claim` op error and carries NO human-readable reason;
  // this mapper is the SINGLE source of that reason. Lock the classification
  // (code + subkind) so the CLI keys on the code, not the message text.
  it('maps a cross_conversation_claim failure envelope to conflict + subkind', async () => {
    const cmd = findCommand('task', 'create');
    const client = {
      createTasks: async () => ({ error: 'cross_conversation_claim' }),
    } as unknown as BackendClient;
    await expect(
      cmd!.run({
        env: {
          conversationId: CONV,
          slot: 'conversation',
          agentId: 'agent',
          serverUrl: 'http://127.0.0.1:4488',
          authToken: 'token',
        },
        client,
        args: { flags: { title: 'T', target: `#conv:${CONV}` }, positional: [], rest: [] },
      } as any),
    ).rejects.toMatchObject({ code: 'conflict', subkind: 'cross_conversation_claim' });
  });
});

describe('member add --target <address>', () => {
  // The same collapsed-door contract on a NON-message door (#task:2021 P4-4): a
  // roster change belongs to exactly one conversation, so the address is required
  // and reaches the wire verbatim. The credential's conversation is where the
  // token is valid, never a default destination.
  const CONV = 'convAAAAAAAAAA01';
  const ROOT = 'rootBBBBBBBBBB02';

  function run(flags: Record<string, string>, client: BackendClient) {
    const cmd = findCommand('member', 'add');
    expect(cmd).toBeDefined();
    return cmd!.run({
      env: {
        conversationId: CONV,
        slot: 'conversation',
        agentId: 'agent',
        serverUrl: 'http://127.0.0.1:4488',
        authToken: 'token',
      },
      client,
      args: { flags, positional: [], rest: [] },
    } as any);
  }

  function capture(flags: Record<string, string>) {
    let captured: unknown;
    let calls = 0;
    const client = {
      addConversationMember: async (input: unknown) => {
        calls += 1;
        captured = input;
        return { members: [] };
      },
    } as unknown as BackendClient;
    return run(flags, client).then(() => ({
      input: captured as { handle?: unknown; target?: unknown },
      calls,
    }));
  }

  const refusing = {
    addConversationMember: async () => {
      throw new Error('the door must refuse before any request');
    },
  } as unknown as BackendClient;

  it('forwards handle + --target verbatim, and unconditionally', async () => {
    const { input, calls } = await capture({ handle: '@bob', target: `#conv:${CONV}` });
    expect(calls).toBe(1);
    expect(input).toEqual({ handle: '@bob', target: `#conv:${CONV}` });
  });

  it("forwards a thread suffix undecomposed — the refusal is the server's to state", async () => {
    // The CLI does not strip the suffix to "help": which conversation a reply
    // cluster belongs to is the resolver's answer, and a client-side strip would
    // silently turn a refused request into a different, accepted one.
    const { input } = await capture({
      handle: '@bob',
      target: `#conv:${CONV}/thread:${ROOT}`,
    });
    expect(input.target).toBe(`#conv:${CONV}/thread:${ROOT}`);
  });

  it('refuses a missing --target as target_required, before any request', async () => {
    await expect(run({ handle: '@bob' }, refusing)).rejects.toMatchObject({
      code: 'validation_error',
      subkind: 'target_required',
      details: { flag: 'target', correct_target: `#conv:${CONV}` },
    });
  });

  it('does not second-guess the grammar — a malformed address goes to the resolver', async () => {
    // The local grammar check is bought by SIDE EFFECTS (`message send` and
    // `task create` drain stdin before they can send). This door has none, so it
    // pays for no second opinion: the resolver answers `target_malformed` once.
    const { input, calls } = await capture({ handle: '@bob', target: 'the planning channel' });
    expect(calls).toBe(1);
    expect(input.target).toBe('the planning channel');
  });

  it('still refuses a missing --handle, and names the flag', async () => {
    await expect(run({ target: `#conv:${CONV}` }, refusing)).rejects.toMatchObject({
      code: 'validation_error',
      subkind: 'missing_required_flag',
      details: { command: 'member add', flag: 'handle' },
    });
  });
});

describe('channel create --target <address>', () => {
  // The hardest door of #task:2021 P4-4, and the one that could not be deferred:
  // `channel create` has no object id of its own and posts nowhere, so its
  // `--target` is the FORK SOURCE — the conversation whose placement and roster the
  // new channel is made from. Until P4-4 both came from the runtime route
  // coordinate, which #task:2030 removes entirely.
  const CONV = 'convAAAAAAAAAA01';
  const ROOT = 'rootBBBBBBBBBB02';

  const REQUIRED = {
    title: 'release signing',
    'context-summary': 'the user asked to split this out',
    'initial-prompt': 'continue here',
  };

  function run(
    flags: Record<string, string>,
    client: BackendClient,
    env:
      | { conversationId: string; slot: 'conversation' }
      | {
          conversationId: null;
          slot: 'agent';
        } = {
      conversationId: CONV,
      slot: 'conversation',
    },
  ) {
    const cmd = findCommand('channel', 'create');
    expect(cmd).toBeDefined();
    return cmd!.run({
      env: {
        conversationId: env.conversationId,
        slot: env.slot,
        agentId: 'agent',
        serverUrl: 'http://127.0.0.1:4488',
        authToken: 'token',
      },
      client,
      args: { flags, positional: [], rest: [] },
    } as any);
  }

  function capture(
    flags: Record<string, string>,
    env?:
      | { conversationId: string; slot: 'conversation' }
      | {
          conversationId: null;
          slot: 'agent';
        },
  ) {
    let captured: unknown;
    let calls = 0;
    const client = {
      createChannel: async (input: unknown) => {
        calls += 1;
        captured = input;
        return { channel_id: 'newAAAAAAAAAA04' };
      },
    } as unknown as BackendClient;
    return run(flags, client, env).then(() => ({
      input: captured as Record<string, unknown>,
      calls,
    }));
  }

  const refusing = {
    createChannel: async () => {
      throw new Error('the door must refuse before any request');
    },
  } as unknown as BackendClient;

  it('forwards the fork source verbatim alongside the channel fields', async () => {
    const { input, calls } = await capture({ ...REQUIRED, target: `#conv:${CONV}` });
    expect(calls).toBe(1);
    expect(input).toEqual({
      target: `#conv:${CONV}`,
      title: 'release signing',
      context_summary: 'the user asked to split this out',
      initial_prompt: 'continue here',
    });
  });

  it('AGENT SLOT: the explicit target is the only fork-source coordinate', async () => {
    const { input } = await capture(
      { ...REQUIRED, target: `#conv:${CONV}` },
      { conversationId: null, slot: 'agent' },
    );
    expect(input.target).toBe(`#conv:${CONV}`);
  });

  it("forwards a thread suffix undecomposed — the refusal is the server's to state", async () => {
    // Same rule as `member add`: a reply cluster is refused
    // (`thread_suffix_not_supported`), and it is refused by the RESOLVER. Stripping
    // the suffix here would quietly fork the parent instead.
    const { input } = await capture({
      ...REQUIRED,
      target: `#conv:${CONV}/thread:${ROOT}`,
    });
    expect(input.target).toBe(`#conv:${CONV}/thread:${ROOT}`);
  });

  it('refuses a missing --target as target_required, before any request', async () => {
    await expect(run(REQUIRED, refusing)).rejects.toMatchObject({
      code: 'validation_error',
      subkind: 'target_required',
      details: { flag: 'target', correct_target: `#conv:${CONV}` },
    });
  });

  it('AGENT SLOT: the same refusal, with no correction invented', async () => {
    // An AgentSlot inhabits no stream, so there is no "the one you are in" to
    // suggest.
    const result = run(REQUIRED, refusing, { conversationId: null, slot: 'agent' });
    await expect(result).rejects.toMatchObject({
      code: 'validation_error',
      subkind: 'target_required',
      details: { flag: 'target' },
    });
    await expect(result).rejects.toHaveProperty('details', { flag: 'target' });
  });

  it('checks the address FIRST: a missing title does not mask a missing target', async () => {
    // Order matters for the report as much as for the actor: two independent
    // omissions must not collapse into whichever check happens to run first, and
    // the address is the one that changes WHERE the act lands.
    await expect(run({}, refusing)).rejects.toMatchObject({
      subkind: 'target_required',
    });
    await expect(run({ target: `#conv:${CONV}`, title: 'x' }, refusing)).rejects.toMatchObject({
      code: 'validation_error',
    });
  });
});

describe('file upload --target <address>', () => {
  // An attachment is owned by ONE conversation (#task:2021 P4-4). The bytes are
  // local, the destination is not: the credential's conversation is where the
  // token is valid, never a default place to leave a file.
  const CONV = 'convAAAAAAAAAA01';
  const ROOT = 'rootBBBBBBBBBB02';

  function run(flags: Record<string, string>, client: BackendClient) {
    const cmd = findCommand('file', 'upload');
    expect(cmd).toBeDefined();
    return cmd!.run({
      env: {
        conversationId: CONV,
        slot: 'conversation',
        agentId: 'agent',
        serverUrl: 'http://127.0.0.1:4488',
        authToken: 'token',
      },
      client,
      args: { flags, positional: [], rest: [] },
    } as any);
  }

  function capture(flags: Record<string, string>) {
    let captured: unknown;
    let calls = 0;
    const client = {
      uploadAttachment: async (input: unknown) => {
        calls += 1;
        captured = input;
        return { id: 'att_1' };
      },
    } as unknown as BackendClient;
    return run(flags, client).then(() => ({
      input: captured as Record<string, unknown>,
      calls,
    }));
  }

  const refusing = {
    uploadAttachment: async () => {
      throw new Error('the door must refuse before any request');
    },
  } as unknown as BackendClient;

  it('forwards --target verbatim beside the path, and unconditionally', async () => {
    const { input, calls } = await capture({
      path: '/tmp/shot.png',
      target: `#conv:${CONV}`,
    });
    expect(calls).toBe(1);
    expect(input).toEqual({ path: '/tmp/shot.png', target: `#conv:${CONV}` });
  });

  it("forwards a thread suffix undecomposed — the refusal is the server's to state", async () => {
    // Threads inherit their parent's files, so this address has no upload
    // destination; a client-side strip would turn a refusal into a different,
    // accepted request.
    const { input } = await capture({
      path: '/tmp/shot.png',
      target: `#conv:${CONV}/thread:${ROOT}`,
    });
    expect(input.target).toBe(`#conv:${CONV}/thread:${ROOT}`);
  });

  it('refuses a missing --target as target_required, before the file is read', async () => {
    await expect(run({ path: '/tmp/shot.png' }, refusing)).rejects.toMatchObject({
      code: 'validation_error',
      subkind: 'target_required',
      details: { flag: 'target', correct_target: `#conv:${CONV}` },
    });
  });

  it('asks for the destination before the bytes when both are missing', async () => {
    // Order is deliberate: the address is the fact the caller cannot recover
    // from the environment, so it is the first thing named.
    await expect(run({}, refusing)).rejects.toMatchObject({
      code: 'validation_error',
      subkind: 'target_required',
    });
  });

  it('still refuses a missing --path once the destination is named', async () => {
    await expect(run({ target: `#conv:${CONV}` }, refusing)).rejects.toMatchObject({
      code: 'validation_error',
    });
  });

  it('does not second-guess the grammar — a malformed address goes to the resolver', async () => {
    // No stdin to drain, so this door buys no local grammar check: the resolver
    // answers `target_malformed` exactly once.
    const { input, calls } = await capture({
      path: '/tmp/shot.png',
      target: 'the design channel',
    });
    expect(calls).toBe(1);
    expect(input.target).toBe('the design channel');
  });

  it('declares --target as a flag of the upload door', () => {
    expect(findCommand('file', 'upload')!.flags).toContain('target');
  });
});

describe('schedule create --target-conversation <address>', () => {
  // The one grammar on a door whose address is a business VALUE rather than the
  // operation's own place (#task:2021 P4-4). A schedule is owned by the AGENT —
  // `list` is a union over its whole set and `cancel` is addressed by the
  // schedule's id — so the door itself takes no `--target`. What needs an address
  // is the WAKE: which conversation's `(conversation, you)` runtime is started.
  // Hence the door's own flag name, and an otherwise identical refusal.
  const CONV = 'convAAAAAAAAAA01';
  const ROOT = 'rootBBBBBBBBBB02';

  function run(flags: Record<string, string>, client: BackendClient) {
    const cmd = findCommand('schedule', 'create');
    expect(cmd).toBeDefined();
    return cmd!.run({
      env: {
        conversationId: CONV,
        slot: 'conversation',
        agentId: 'agent',
        serverUrl: 'http://127.0.0.1:4488',
        authToken: 'token',
      },
      client,
      args: { flags, positional: [], rest: [] },
    } as any);
  }

  function capture(flags: Record<string, string>) {
    let captured: unknown;
    let calls = 0;
    const client = {
      createSchedule: async (input: unknown) => {
        calls += 1;
        captured = input;
        return { schedule: {} };
      },
    } as unknown as BackendClient;
    return run(flags, client).then(() => ({
      input: captured as Record<string, unknown>,
      calls,
    }));
  }

  const refusing = {
    createSchedule: async () => {
      throw new Error('the door must refuse before any request');
    },
  } as unknown as BackendClient;

  it('forwards the address verbatim as target_conversation, and never an id field', async () => {
    const { input, calls } = await capture({
      title: 'standup',
      message: 'post the standup',
      kind: 'once',
      'delay-seconds': '600',
      'target-conversation': `#conv:${CONV}`,
    });
    expect(calls).toBe(1);
    expect(input.target_conversation).toBe(`#conv:${CONV}`);
    // The retired field named a place as a bare id; it is gone from the wire, not
    // merely unset beside the address.
    expect('target_conversation_id' in input).toBe(false);
  });

  it.each([
    ['interval', { kind: 'interval', 'every-seconds': '3600' }],
    ['cron', { kind: 'cron', expression: '0 9 * * 1-5', timezone: 'Asia/Shanghai' }],
  ])('carries the destination on a %s spec too — one door, one address', async (_label, spec) => {
    // Each spec kind builds its own input literal, which is exactly where a
    // destination gets forgotten: the branch that repeats is the branch that drifts.
    const { input } = await capture({
      title: 'weekly',
      message: 'do the weekly thing',
      ...(spec as Record<string, string>),
      'target-conversation': `#conv:${CONV}`,
    });
    expect(input.target_conversation).toBe(`#conv:${CONV}`);
  });

  it('refuses a missing --target-conversation as target_required, before any request', async () => {
    await expect(
      run({ title: 'standup', message: 'post it', kind: 'once', 'delay-seconds': '600' }, refusing),
    ).rejects.toMatchObject({
      code: 'validation_error',
      // The SAME subkind as every other door: an agent that learned the rule on
      // `message send` already knows it here, even though the flag is named for
      // what it means rather than for where the call acts.
      subkind: 'target_required',
      details: { flag: 'target-conversation', correct_target: `#conv:${CONV}` },
    });
  });

  it("forwards a thread suffix undecomposed — the refusal is the server's to state", async () => {
    // A reply cluster shares its parent's runtime, so this address names nothing a
    // wake could act on. The CLI still does not strip it: stripping would silently
    // turn a refused request into a different, accepted one that wakes the parent.
    const { input } = await capture({
      title: 'standup',
      message: 'post it',
      kind: 'once',
      'delay-seconds': '600',
      'target-conversation': `#conv:${CONV}/thread:${ROOT}`,
    });
    expect(input.target_conversation).toBe(`#conv:${CONV}/thread:${ROOT}`);
  });

  it('does not second-guess the grammar — a malformed address goes to the resolver', async () => {
    // No stdin to drain and nothing uploaded, so this door buys no local grammar
    // check: the resolver answers `target_malformed` exactly once.
    const { input, calls } = await capture({
      title: 'standup',
      message: 'post it',
      kind: 'once',
      'delay-seconds': '600',
      'target-conversation': 'the standup channel',
    });
    expect(calls).toBe(1);
    expect(input.target_conversation).toBe('the standup channel');
  });

  it('retires --target-conversation-id, and the hint teaches both changes', () => {
    const cmd = findCommand('schedule', 'create')!;
    expect(cmd.flags).toContain('target-conversation');
    expect(cmd.flags).not.toContain('target-conversation-id');
    const removed = cmd.removedFlags?.['target-conversation-id'];
    expect(removed).toBeDefined();
    expect(removed!.replacement).toBe('--target-conversation');
    // A rename by one word is beyond `closestFlag`'s edit-distance reach, so
    // without this entry the old flag reads as an unknown one — and the hint has
    // to carry the second change too, since the flag also stopped being optional.
    expect(removed!.hint).toContain('#conv:');
    expect(removed!.hint).toContain('required');
  });

  it('leaves list and cancel address-free: an agent-owned set has no one stream', () => {
    for (const verb of ['list', 'cancel']) {
      const cmd = findCommand('schedule', verb)!;
      expect(cmd.flags, verb).not.toContain('target');
      expect(cmd.flags, verb).not.toContain('target-conversation');
    }
  });
});

describe('file verbs take their id/path as a positional operand', () => {
  const env = {
    conversationId: 'c',
    slot: 'conversation',
    agentId: 'a',
    serverUrl: 'http://127.0.0.1:1',
    authToken: 't',
  };

  it('file update <id> --path p maps the operand to the attachment id', async () => {
    let seen: unknown;
    const client = { updateAttachment: async (input: unknown) => ((seen = input), {}) };
    await findCommand('file', 'update')!.run({
      env,
      client,
      args: { flags: { path: '/tmp/x' }, positional: ['att-1'], rest: [] },
    } as any);
    expect(seen).toEqual({ attachmentId: 'att-1', path: '/tmp/x' });
  });

  it('file upload --target t <path> maps the operand to the path', async () => {
    let seen: unknown;
    const client = { uploadAttachment: async (input: unknown) => ((seen = input), {}) };
    await findCommand('file', 'upload')!.run({
      env,
      client,
      args: { flags: { target: '#conv:c' }, positional: ['/tmp/a.png'], rest: [] },
    } as any);
    expect(seen).toEqual({ path: '/tmp/a.png', target: '#conv:c' });
  });

  it('refuses the operand together with its flag, or two operands', async () => {
    const view = findCommand('file', 'view')!;
    for (const args of [
      { flags: { 'attachment-id': 'a' }, positional: ['b'], rest: [] },
      { flags: {}, positional: ['a', 'b'], rest: [] },
    ]) {
      await expect(view.run({ env, client: {}, args } as any)).rejects.toMatchObject({
        subkind: 'unexpected_arguments',
      });
    }
  });
});
