import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';

import { ClaudeDriver } from '../../src/drivers/claude.js';
import type { DriverSpawnContext } from '../../src/drivers/index.js';

function makeSpawnCtx(overrides: Partial<DriverSpawnContext> = {}): {
  ctx: DriverSpawnContext;
  cleanup: () => void;
} {
  const tmp = mkdtempSync(join(tmpdir(), 'claude-driver-test-'));
  const systemPromptPath = join(tmp, 'system-prompt.md');
  writeFileSync(systemPromptPath, 'STANDING PROMPT\n', 'utf-8');

  const ctx: DriverSpawnContext = {
    agentId: 'agent-1',
    agentName: 'tester',
    workspacePath: tmp,
    cliSessionId: null,
    model: null,
    reasoningEffort: null,
    fastMode: null,
    systemPromptPath,
    initialPrompt: '',
    ...overrides,
  };

  return {
    ctx,
    cleanup: () => {
      rmSync(tmp, { recursive: true, force: true });
    },
  };
}

describe('ClaudeDriver.buildArgs', () => {
  let cleanup: (() => void) | null = null;
  afterEach(() => {
    cleanup?.();
    cleanup = null;
  });

  it('defaults to opus when no model is configured', () => {
    const built = makeSpawnCtx();
    cleanup = built.cleanup;

    const args = new ClaudeDriver().buildArgs(built.ctx);
    const idx = args.indexOf('--model');

    expect(idx).toBeGreaterThanOrEqual(0);
    expect(args[idx + 1]).toBe('opus');
  });

  it('forwards explicit effort verbatim and leaves an unset effort to the CLI', () => {
    const built = makeSpawnCtx({ model: 'custom-model', reasoningEffort: 'future-tier' });
    cleanup = built.cleanup;
    const driver = new ClaudeDriver();
    const args = driver.buildArgs(built.ctx);
    expect(args[args.indexOf('--effort') + 1]).toBe('future-tier');
    expect(driver.buildArgs({ ...built.ctx, reasoningEffort: null })).not.toContain('--effort');
  });

  it('uses an explicitly configured model', () => {
    const built = makeSpawnCtx({ model: 'sonnet' });
    cleanup = built.cleanup;

    const args = new ClaudeDriver().buildArgs(built.ctx);
    const idx = args.indexOf('--model');

    expect(idx).toBeGreaterThanOrEqual(0);
    expect(args[idx + 1]).toBe('sonnet');
  });

  it('does not pass an MCP config after the CLI cutover', () => {
    const built = makeSpawnCtx();
    cleanup = built.cleanup;

    const args = new ClaudeDriver().buildArgs(built.ctx);

    expect(args).not.toContain('--mcp-config');
    expect(args).not.toContain('--strict-mcp-config');
    expect(args).toContain('--append-system-prompt-file');
    expect(args).toContain(built.ctx.systemPromptPath);
  });
});

describe('ClaudeDriver.classifyInitProgress', () => {
  it('treats any complete stream-json frame as protocol-ready', () => {
    const d = new ClaudeDriver();
    expect(
      d.classifyInitProgress(
        JSON.stringify({ type: 'system', subtype: 'status', status: 'warming' }),
      ),
    ).toBe('protocol_ready');
  });

  it('treats system init as session-ready', () => {
    const d = new ClaudeDriver();
    expect(
      d.classifyInitProgress(
        JSON.stringify({ type: 'system', subtype: 'init', session_id: 'cli-1' }),
      ),
    ).toBe('session_ready');
  });

  it('ignores non-json lines', () => {
    expect(new ClaudeDriver().classifyInitProgress('not json')).toBeNull();
  });
});

describe('ClaudeDriver.parseLine', () => {
  it('projects Claude system api_error as a safe non-fatal error event', () => {
    const events = new ClaudeDriver().parseLine(
      JSON.stringify({
        type: 'system',
        subtype: 'api_error',
        level: 'error',
        error: {
          type: null,
          cause: {
            code: 'ConnectionRefused',
            path: 'http://127.0.0.1:9090/v1/messages?beta=true',
            errno: 0,
          },
        },
        retryAttempt: 3,
      }),
    );

    expect(events).toEqual([
      {
        kind: 'error',
        code: 'api_connection_refused',
        message: 'Claude API gateway unreachable; retrying (attempt 3)',
      },
    ]);
    expect(JSON.stringify(events)).not.toContain('127.0.0.1');
    expect(JSON.stringify(events)).not.toContain('/v1/messages');
  });

  it('classifies stale --resume result errors as fatal', () => {
    const events = new ClaudeDriver().parseLine(
      JSON.stringify({
        type: 'result',
        subtype: 'error_during_execution',
        is_error: true,
        stop_reason: null,
        session_id: '46af7b63-3663-4614-a4e5-a1e93f3a0230',
        errors: ['No conversation found with session ID: 11111111-1111-4111-8111-111111111111'],
      }),
    );

    expect(events[0]).toEqual({
      kind: 'error',
      code: 'session_not_found',
      fatal: true,
      message: 'No conversation found with session ID: 11111111-1111-4111-8111-111111111111',
    });
    expect(events[1]).toEqual({
      kind: 'turn_end',
      sessionId: '46af7b63-3663-4614-a4e5-a1e93f3a0230',
    });
  });
});

describe('ClaudeDriver — input payload uuid + command_lifecycle (#task:1557)', () => {
  it('stamps payloadUuid and explicit priority:"next" onto the stdin user frame', () => {
    const encoded = new ClaudeDriver().encodeStdinMessage('hello', 'sess-1', {
      payloadUuid: 'uuid-42',
    });
    const frame = JSON.parse(encoded!);
    expect(frame.uuid).toBe('uuid-42');
    expect(frame.session_id).toBe('sess-1');
    expect(frame.message.content[0].text).toBe('hello');
    // Product decision (thread 2026-07-20): explicit steer intent on
    // every frame, guarded against future default-behavior changes.
    expect(frame.priority).toBe('next');
  });

  it('omits the uuid field entirely when no payloadUuid is given (priority still present)', () => {
    const encoded = new ClaudeDriver().encodeStdinMessage('hello', null);
    const frame = JSON.parse(encoded!);
    expect('uuid' in frame).toBe(false);
    expect(frame.priority).toBe('next');
  });

  it.each(['queued', 'started', 'completed'] as const)(
    'parses command_lifecycle %s into input_lifecycle',
    (state) => {
      const events = new ClaudeDriver().parseLine(
        JSON.stringify({
          type: 'command_lifecycle',
          command_uuid: 'uuid-42',
          state,
          uuid: 'frame-own-id',
        }),
      );
      expect(events).toEqual([{ kind: 'input_lifecycle', commandUuid: 'uuid-42', state }]);
    },
  );

  it('drops command_lifecycle frames with unknown state or missing uuid', () => {
    const driver = new ClaudeDriver();
    expect(
      driver.parseLine(
        JSON.stringify({ type: 'command_lifecycle', command_uuid: 'u', state: 'x' }),
      ),
    ).toEqual([]);
    expect(
      driver.parseLine(JSON.stringify({ type: 'command_lifecycle', state: 'started' })),
    ).toEqual([]);
  });
});
