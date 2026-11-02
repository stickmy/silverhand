import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { RUNTIME_CATALOG } from '@silverhand/shared';
import { describe, expect, it } from 'vitest';

import { getDriver, hasDriver } from '../src/drivers/index.js';
import type { DriverSpawnContext } from '../src/drivers/index.js';
import { buildProfileMcpSpawnArgs } from '../src/daemon/mcp-spawn.js';
import type { DaemonLogger } from '../src/daemon/logger.js';

const NOOP_LOGGER = {
  debug() {},
  info() {},
  warn() {},
  error() {},
} as unknown as DaemonLogger;

/**
 * These tests are the drift-closing backstop for the shared runtime
 * catalog (`packages/shared/src/runtime-catalog.ts`). The catalog is
 * the SoT for UI-facing per-runtime facts, but some of those facts
 * mirror host-side truth (driver existence, driver spawn args, the MCP
 * spawn loader). Here we assert the mirror holds, so adding a runtime
 * can't leave the catalog claiming something the host doesn't back.
 */

function makeCtx(overrides: Partial<DriverSpawnContext> = {}): {
  ctx: DriverSpawnContext;
  cleanup: () => void;
} {
  const tmp = mkdtempSync(join(tmpdir(), 'catalog-invariant-'));
  const systemPromptPath = join(tmp, 'system-prompt.md');
  writeFileSync(systemPromptPath, 'PROMPT\n', 'utf-8');
  return {
    ctx: {
      agentId: 'a1',
      agentName: 't',
      workspacePath: tmp,
      cliSessionId: null,
      model: null,
      reasoningEffort: null,
      fastMode: null,
      systemPromptPath,
      initialPrompt: '',
      ...overrides,
    },
    cleanup: () => rmSync(tmp, { recursive: true, force: true }),
  };
}

describe('runtime catalog ⟺ host invariants', () => {
  it('selectable ⇒ hasDriver (never offer a runtime we cannot spawn)', () => {
    for (const entry of RUNTIME_CATALOG) {
      if (entry.selectable) {
        expect(hasDriver(entry.kind)).toBe(true);
      }
    }
  });

  it('reasoningEffortOptions non-empty ⟺ the driver forwards the value', async () => {
    // Only exercise runtimes with a driver that accepts stdin-style
    // buildArgs (skip cursor's one-shot argv kickoff / gemini's absence).
    for (const entry of RUNTIME_CATALOG) {
      if (entry.kind === 'cursor' || entry.kind === 'gemini') continue;
      const built = makeCtx({ reasoningEffort: 'high' });
      try {
        const driver = getDriver(entry.kind);
        const args = driver.buildArgs(built.ctx);
        const requests: any[] = [];
        if (entry.kind === 'grok') {
          driver.bindIO!({
            spawnCtx: built.ctx,
            writeLine: async (line) => {
              requests.push(JSON.parse(line));
            },
          });
          await Promise.resolve();
          driver.parseLine(
            JSON.stringify({
              id: requests[0].id,
              result: {
                protocolVersion: 1,
                agentCapabilities: { sessionCapabilities: { resume: {} } },
              },
            }),
          );
          driver.parseLine(
            JSON.stringify({ id: requests[1].id, result: { sessionId: 'fixture' } }),
          );
        }
        const forwardsReasoning =
          args.some((a) => a.includes('high')) ||
          requests.some(
            (r) =>
              r.method === 'session/set_config_option' &&
              r.params.configId === 'reasoning_effort' &&
              r.params.value === 'high',
          );
        expect(forwardsReasoning).toBe(entry.reasoningEffortOptions.length > 0);
      } finally {
        built.cleanup();
      }
    }
  });

  it('supportsFastMode ⟺ the driver forwards the fast-mode flag', () => {
    for (const entry of RUNTIME_CATALOG) {
      if (entry.kind === 'cursor' || entry.kind === 'gemini') continue;
      const built = makeCtx({ fastMode: true });
      try {
        const args = getDriver(entry.kind).buildArgs(built.ctx);
        expect(args.includes('fast_mode')).toBe(entry.supportsFastMode);
      } finally {
        built.cleanup();
      }
    }
  });

  it('supportsProfileMcp ⟺ the MCP spawn loader has an adapter', async () => {
    for (const entry of RUNTIME_CATALOG) {
      const tmp = mkdtempSync(join(tmpdir(), 'catalog-mcp-'));
      const spawnDir = mkdtempSync(join(tmpdir(), 'catalog-mcp-spawn-'));
      const systemPromptPath = join(spawnDir, 'system-prompt.md');
      writeFileSync(systemPromptPath, 'PROMPT\n', 'utf-8');
      writeFileSync(
        join(tmp, 'mcp.json'),
        JSON.stringify({
          mcpServers: { demo: { command: 'echo', args: ['hi'] } },
        }),
        'utf-8',
      );
      try {
        const result = await buildProfileMcpSpawnArgs({
          runtimeKind: entry.kind,
          agentHomeDir: tmp,
          spawnDir,
          systemPromptPath,
          agentId: 'a1',
          logger: NOOP_LOGGER,
        });
        // The MCP layer is argv (claude/codex), `profileMcp` (grok/qoder) or the
        // opencode config env, where the standing prompt is ALWAYS present so
        // the MCP layer is specifically the presence of an `mcp` section.
        const hasMcpLayer =
          entry.kind === 'opencode'
            ? Boolean(JSON.parse(result.env?.OPENCODE_CONFIG_CONTENT ?? '{}').mcp)
            : entry.kind === 'grok' || entry.kind === 'qoder'
              ? Boolean(result.profileMcp)
              : (result.args?.length ?? 0) > 0;
        expect(hasMcpLayer).toBe(entry.supportsProfileMcp);
      } finally {
        rmSync(tmp, { recursive: true, force: true });
        rmSync(spawnDir, { recursive: true, force: true });
      }
    }
  });
});
