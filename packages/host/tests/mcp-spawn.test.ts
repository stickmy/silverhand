import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { mkdtempSync, readFileSync, rmSync, writeFileSync, chmodSync, readdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { buildProfileMcpSpawnArgs } from '../src/daemon/mcp-spawn.js';
import type { DaemonLogger } from '../src/daemon/logger.js';

/**
 * Spawn-time profile-MCP adapter (#task:1135, docs/agent-mcp-config-v1.md §4).
 * Locks the per-runtime ADDITIVE translation + the fail-loud/non-blocking,
 * all-or-nothing failure semantics (§6):
 *   - absent file  → silent `[]` (the normal "no MCP" case)
 *   - empty config → `[]`
 *   - claude       → writes a per-spawn JSON, returns `--mcp-config <path>`
 *                    with names prefixed `silverhand_profile__*` (NEVER
 *                    `--strict-mcp-config`, which would REPLACE the user's MCP)
 *   - qoder        → canonical `profileMcp`; the driver writes `--mcp-config`
 *   - codex        → one `-c mcp_servers.<prefixed>=<toml>` per server
 *   - bad JSON     → loud, drop the WHOLE layer, still spawn (`[]`)
 *   - hard read err→ loud, `[]`
 *   - unknown rt   → loud, `[]`
 */

function captureLogger(): { logger: DaemonLogger; warns: unknown[][] } {
  const warns: unknown[][] = [];
  const logger = {
    debug: () => {},
    info: () => {},
    warn: (obj: unknown, msg?: string) => {
      warns.push([obj, msg]);
    },
    error: () => {},
  } as unknown as DaemonLogger;
  return { logger, warns };
}

const AGENT_ID = 'aid-mcp-spawn';
const STANDING_PROMPT = 'STANDING PROMPT — platform manual + memory';

let homeDir = '';
let spawnDir = '';
let systemPromptPath = '';
afterEach(() => {
  for (const d of [homeDir, spawnDir]) {
    try {
      if (d) rmSync(d, { recursive: true, force: true });
    } catch {
      /* best-effort */
    }
  }
});
beforeEach(() => {
  homeDir = mkdtempSync(join(tmpdir(), 'mcp-home-'));
  spawnDir = mkdtempSync(join(tmpdir(), 'mcp-spawn-'));
  // The materialized standing prompt the daemon writes per spawn. The argv
  // runtimes ignore it here; opencode reads it into OPENCODE_CONFIG_CONTENT.
  systemPromptPath = join(spawnDir, 'system-prompt.md');
  writeFileSync(systemPromptPath, STANDING_PROMPT);
});

describe('buildProfileMcpSpawnArgs', () => {
  it('Pi preserves profile MCP bytes without copying credentials or injecting configuration', async () => {
    const raw = JSON.stringify({
      mcpServers: { private: { command: 'fixture', env: { TOKEN: 'fixture-secret' } } },
    });
    const path = join(homeDir, 'mcp.json');
    writeFileSync(path, raw);
    const { logger, warns } = captureLogger();
    const before = readdirSync(spawnDir);
    const result = await buildProfileMcpSpawnArgs({
      runtimeKind: 'pi',
      agentHomeDir: homeDir,
      spawnDir,
      systemPromptPath,
      agentId: AGENT_ID,
      logger,
    });
    expect(result).toEqual({ args: [] });
    expect(readFileSync(path, 'utf8')).toBe(raw);
    expect(readdirSync(spawnDir)).toEqual(before);
    expect(JSON.stringify(warns)).not.toContain('fixture-secret');
  });
  it('returns [] silently when no mcp.json exists (the normal case)', async () => {
    const { logger, warns } = captureLogger();
    const r = await buildProfileMcpSpawnArgs({
      runtimeKind: 'claude',
      agentHomeDir: homeDir,
      spawnDir,
      systemPromptPath,
      agentId: AGENT_ID,
      logger,
    });
    expect(r.args).toEqual([]);
    expect(warns).toEqual([]); // absent is silent, NOT loud
  });

  it('returns [] for an empty server map', async () => {
    writeFileSync(join(homeDir, 'mcp.json'), JSON.stringify({ mcpServers: {} }));
    const { logger } = captureLogger();
    const r = await buildProfileMcpSpawnArgs({
      runtimeKind: 'codex',
      agentHomeDir: homeDir,
      spawnDir,
      systemPromptPath,
      agentId: AGENT_ID,
      logger,
    });
    expect(r.args).toEqual([]);
  });

  it('claude: writes a per-spawn config and passes --mcp-config (NEVER --strict-mcp-config)', async () => {
    writeFileSync(
      join(homeDir, 'mcp.json'),
      JSON.stringify({
        mcpServers: {
          fs: { command: 'npx', args: ['-y', 'server-fs'] },
          remote: { type: 'http', url: 'https://example.com/mcp' },
        },
      }),
    );
    const { logger } = captureLogger();
    const r = await buildProfileMcpSpawnArgs({
      runtimeKind: 'claude',
      agentHomeDir: homeDir,
      spawnDir,
      systemPromptPath,
      agentId: AGENT_ID,
      logger,
    });
    expect(r.args![0]).toBe('--mcp-config');
    expect(r.args).not.toContain('--strict-mcp-config');
    const outPath = r.args![1]!;
    expect(outPath).toBe(join(spawnDir, 'profile-mcp.json'));
    const written = JSON.parse(readFileSync(outPath, 'utf-8'));
    // Names are namespaced so they never shadow the user's own servers.
    expect(Object.keys(written.mcpServers).sort()).toEqual([
      'silverhand_profile__fs',
      'silverhand_profile__remote',
    ]);
    expect(written.mcpServers.silverhand_profile__fs.command).toBe('npx');
  });

  it('qoder: returns the canonical layer for the driver to place in its worker args', async () => {
    const config = { mcpServers: { fs: { command: 'npx', args: ['-y', 'server-fs'] } } };
    writeFileSync(join(homeDir, 'mcp.json'), JSON.stringify(config));
    const { logger } = captureLogger();
    const r = await buildProfileMcpSpawnArgs({
      runtimeKind: 'qoder',
      agentHomeDir: homeDir,
      spawnDir,
      systemPromptPath,
      agentId: AGENT_ID,
      logger,
    });
    expect(r.args).toBeUndefined();
    expect(r.profileMcp).toEqual(config);
  });

  it('codex: emits one -c mcp_servers.<prefixed> override per server', async () => {
    writeFileSync(
      join(homeDir, 'mcp.json'),
      JSON.stringify({
        mcpServers: {
          fs: { command: 'npx', args: ['-y', 'server-fs'] },
          remote: { type: 'http', url: 'https://example.com/mcp' },
        },
      }),
    );
    const { logger } = captureLogger();
    const r = await buildProfileMcpSpawnArgs({
      runtimeKind: 'codex',
      agentHomeDir: homeDir,
      spawnDir,
      systemPromptPath,
      agentId: AGENT_ID,
      logger,
    });
    // Two servers → two `-c <override>` pairs.
    const flags = r.args!.filter((a) => a === '-c');
    expect(flags).toHaveLength(2);
    expect(r.args).toHaveLength(4);
    const overrides = r.args!.filter((a) => a !== '-c');
    for (const o of overrides) {
      expect(o.startsWith('mcp_servers.silverhand_profile__')).toBe(true);
    }
    // The remote server is projected to Codex native keys (url, no
    // canonical type/headers leakage). Cindy Blocker 2.
    const remote = overrides.find((o) => o.includes('__remote'))!;
    expect(remote).toContain('url = "https://example.com/mcp"');
    expect(remote).not.toContain('type =');
    expect(remote).not.toContain('headers =');
  });

  it('bad JSON: drops the WHOLE layer, logs loud, still returns [] (agent spawns)', async () => {
    writeFileSync(join(homeDir, 'mcp.json'), '{ this is not json');
    const { logger, warns } = captureLogger();
    const r = await buildProfileMcpSpawnArgs({
      runtimeKind: 'claude',
      agentHomeDir: homeDir,
      spawnDir,
      systemPromptPath,
      agentId: AGENT_ID,
      logger,
    });
    expect(r.args).toEqual([]);
    expect(warns.length).toBeGreaterThan(0); // loud
  });

  it('unknown runtime: loud, returns [] (no silent ignore)', async () => {
    writeFileSync(
      join(homeDir, 'mcp.json'),
      JSON.stringify({ mcpServers: { fs: { command: 'x' } } }),
    );
    const { logger, warns } = captureLogger();
    const r = await buildProfileMcpSpawnArgs({
      runtimeKind: 'cursor',
      agentHomeDir: homeDir,
      spawnDir,
      systemPromptPath,
      agentId: AGENT_ID,
      logger,
    });
    expect(r.args).toEqual([]);
    expect(warns.length).toBeGreaterThan(0);
  });

  it.skipIf(process.getuid?.() === 0)(
    'hard read error (unreadable file): loud, returns []',
    async () => {
      const filePath = join(homeDir, 'mcp.json');
      writeFileSync(filePath, JSON.stringify({ mcpServers: {} }));
      chmodSync(filePath, 0o000);
      try {
        const { logger, warns } = captureLogger();
        const r = await buildProfileMcpSpawnArgs({
          runtimeKind: 'claude',
          agentHomeDir: homeDir,
          spawnDir,
          systemPromptPath,
          agentId: AGENT_ID,
          logger,
        });
        expect(r.args).toEqual([]);
        expect(warns.length).toBeGreaterThan(0);
      } finally {
        chmodSync(filePath, 0o644);
      }
    },
  );

  it('opencode: injects the standing prompt into OPENCODE_CONFIG_CONTENT even with NO mcp.json', async () => {
    const { logger, warns } = captureLogger();
    const r = await buildProfileMcpSpawnArgs({
      runtimeKind: 'opencode',
      agentHomeDir: homeDir,
      spawnDir,
      systemPromptPath,
      agentId: AGENT_ID,
      logger,
    });
    // No argv layer; the whole layer rides the config env.
    expect(r.args).toBeUndefined();
    const content = JSON.parse(r.env!.OPENCODE_CONFIG_CONTENT!);
    // Standing prompt injected as the custom agent's prompt (the ACP driver
    // then selects `mode = silverhand`).
    expect(content.agent.silverhand.prompt).toBe(STANDING_PROMPT);
    // No MCP configured → no `mcp` key (nothing to deep-merge over user config).
    expect(content.mcp).toBeUndefined();
    expect(warns).toEqual([]); // absent mcp is silent
  });

  it('opencode: deep-merge fragment carries BOTH the standing prompt and prefixed MCP servers', async () => {
    writeFileSync(
      join(homeDir, 'mcp.json'),
      JSON.stringify({
        mcpServers: {
          fs: { command: 'npx', args: ['-y', 'server-fs'], env: { TOK: 'v' } },
          remote: {
            type: 'http',
            url: 'https://example.com/mcp',
            headers: { Authorization: 'Bearer x' },
          },
        },
      }),
    );
    const { logger } = captureLogger();
    const r = await buildProfileMcpSpawnArgs({
      runtimeKind: 'opencode',
      agentHomeDir: homeDir,
      spawnDir,
      systemPromptPath,
      agentId: AGENT_ID,
      logger,
    });
    expect(r.args).toBeUndefined();
    const content = JSON.parse(r.env!.OPENCODE_CONFIG_CONTENT!);
    expect(content.agent.silverhand.prompt).toBe(STANDING_PROMPT);
    // Servers are domain-prefixed (never shadow the user's own) and projected
    // to opencode's native local/remote shape.
    expect(Object.keys(content.mcp).sort()).toEqual([
      'silverhand_profile__fs',
      'silverhand_profile__remote',
    ]);
    expect(content.mcp.silverhand_profile__fs).toEqual({
      type: 'local',
      command: ['npx', '-y', 'server-fs'],
      environment: { TOK: 'v' },
    });
    expect(content.mcp.silverhand_profile__remote).toEqual({
      type: 'remote',
      url: 'https://example.com/mcp',
      headers: { Authorization: 'Bearer x' },
    });
  });

  it('opencode: a missing standing-prompt file fails loud (spawn precondition, not an MCP drop)', async () => {
    const { logger } = captureLogger();
    await expect(
      buildProfileMcpSpawnArgs({
        runtimeKind: 'opencode',
        agentHomeDir: homeDir,
        spawnDir,
        systemPromptPath: join(spawnDir, 'does-not-exist.md'),
        agentId: AGENT_ID,
        logger,
      }),
    ).rejects.toThrow();
  });
});
