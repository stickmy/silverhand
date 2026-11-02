import { readFile, writeFile } from 'node:fs/promises';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import {
  MCP_CONFIG_FILENAME,
  buildOpencodeConfigContent,
  parseMcpConfig,
  runtimeMcpServerName,
  projectProfileMcpForRuntime,
  type McpConfig,
  type RuntimeKind,
} from '@silverhand/shared';
import type { DaemonLogger } from './logger.js';

/**
 * Spawn-time loader for the subject's **profile MCP** layer
 * (`docs/agent-mcp-config-v1.md`). Reads the L1 `mcp.json`, translates it to
 * the runtime's native ADDITIVE mechanism, and returns what to append to the
 * spawn: `args` for runtimes that take MCP via argv, `env` for runtimes that
 * take it via a config env. Keeping this in ONE module (keyed by `runtimeKind`),
 * rather than inside each driver's `buildArgs`, means: drivers stay pure/sync,
 * the file-write + parse live in one async place, and a runtime's mechanism can
 * be swapped without touching the drivers or the canonical model.
 *
 * Mechanisms (all additive — never replace the user's own MCP, never write
 * the user's runtime config, never call `<runtime> mcp add`):
 *  - **claude:** write a per-spawn temp JSON, pass `--mcp-config <temp>` (no
 *    `--strict-mcp-config`).
 *  - **qoder:** canonical config via `profileMcp`; the driver writes the same
 *    file into its worker args (SDK mode has no per-session argv).
 *  - **codex:** one `-c mcp_servers.<prefixed>=<inline-toml>` override per
 *    server (per-server so it MERGES with the user's `[mcp_servers.*]`).
 *  - **opencode:** no additive MCP flag exists; the profile MCP AND the standing
 *    prompt both ride a single deep-merged `OPENCODE_CONFIG_CONTENT` env
 *    ({@link buildOpencodeConfigContent}). Handled ahead of the argv runtimes
 *    because opencode needs a config env for its standing prompt even when
 *    there is no MCP layer at all.
 *
 *  - **grok:** pass canonical MCP to the driver for shared ACP projection
 *    in session/new or session/resume. No native config file is modified.
 *
 * Failure semantics (locked, §6): the WRITE path already validated, so a
 * read/parse failure here means a hand-edited / half-written file. We drop the
 * ENTIRE MCP layer (never a silent partial subset), log loud, and still spawn.
 * A missing file is the normal "no MCP" case and is silent. MCP *server*
 * connection failures are the runtime's problem, not ours — we never pre-check
 * them. The opencode standing prompt is NOT part of the MCP layer: it is a
 * spawn precondition (agent identity), so a failure to read it fails loud
 * rather than spawning an identity-less agent — matching qoder/codex, which
 * `readFileSync` the same file in their `buildArgs`.
 */
export interface ProfileMcpSpawnInput {
  runtimeKind: RuntimeKind;
  /** The subject's L1 home dir (daemon-resolved `agent.home_dir`). */
  agentHomeDir: string;
  /** The per-spawn temp dir to write any generated config into. */
  spawnDir: string;
  /**
   * The materialized standing-prompt file (`spawnPrep.systemPromptPath`).
   * Used by the opencode config env; ignored by the argv runtimes (they read
   * it themselves in `buildArgs` / append it as a flag).
   */
  systemPromptPath: string;
  agentId: string;
  logger: DaemonLogger;
}

/** Spawn inputs: argv, env, or canonical MCP for an ACP session request. */
export interface ProfileMcpSpawnResult {
  profileMcp?: McpConfig;
  args?: string[];
  env?: Record<string, string>;
}

export async function buildProfileMcpSpawnArgs(
  input: ProfileMcpSpawnInput,
): Promise<ProfileMcpSpawnResult> {
  const { runtimeKind, spawnDir, systemPromptPath, agentId, logger } = input;

  // Read + validate the profile MCP layer once. `null` = absent (silent) /
  // unreadable / invalid / empty (loud where a written file went bad).
  const mcpConfig = await loadProfileMcp(input);

  // opencode: standing prompt + profile MCP both ride ONE deep-merged
  // OPENCODE_CONFIG_CONTENT. The standing prompt is always injected (spawn
  // precondition); MCP only when present. Ahead of the `!mcpConfig` shortcut
  // below because opencode needs a config env even with no MCP layer.
  if (runtimeKind === 'opencode') {
    const standingPrompt = readFileSync(systemPromptPath, 'utf-8');
    const content = buildOpencodeConfigContent({
      standingPrompt,
      mcpConfig: mcpConfig ? opencodeProfileProjection(mcpConfig) : null,
    });
    return content ? { env: { OPENCODE_CONFIG_CONTENT: content } } : { args: [] };
  }

  if (!mcpConfig) return { args: [] };

  try {
    const projection = projectProfileMcpForRuntime(mcpConfig, runtimeKind);
    switch (projection.kind) {
      case 'claude': {
        if (runtimeKind === 'qoder') return { profileMcp: mcpConfig };
        const claudeConfig = projection.config;
        if (!claudeConfig) return { args: [] };
        const outPath = join(spawnDir, 'profile-mcp.json');
        await writeFile(outPath, JSON.stringify(claudeConfig, null, 2), 'utf-8');
        return { args: ['--mcp-config', outPath] };
      }
      case 'codex': {
        const overrides = projection.overrides;
        return { args: overrides.flatMap((o) => ['-c', o]) };
      }
      case 'acp':
        return { profileMcp: mcpConfig };
      default:
        // cursor / others: no profile MCP support in v1. Loud so a user who
        // configured MCP for an unsupported runtime isn't silently ignored.
        logger.warn(
          {
            agentId,
            runtimeKind,
            servers: Object.keys(mcpConfig.mcpServers).length,
          },
          'profile MCP: runtime has no MCP adapter; skipping the layer',
        );
        return { args: [] };
    }
  } catch (err) {
    // Translation/serialization failure (should be caught at write-time, but
    // be safe). Loud, all-or-nothing, non-blocking.
    logger.warn(
      { agentId, runtimeKind, err: String(err) },
      'profile MCP: failed to build runtime config; skipping the entire MCP layer (agent still spawns)',
    );
    return { args: [] };
  }
}

/**
 * Read + validate the subject's raw profile MCP config into a {@link McpConfig},
 * or `null` when there is nothing to apply (absent / unreadable / invalid /
 * empty). Absent is silent (the normal "no MCP" case); a read or validation
 * failure is loud (a hand-edited / half-written file) but still yields `null`
 * so the caller can spawn without the layer.
 */
async function loadProfileMcp(input: ProfileMcpSpawnInput): Promise<McpConfig | null> {
  const { runtimeKind, agentHomeDir, agentId, logger } = input;
  const filePath = join(agentHomeDir, MCP_CONFIG_FILENAME);

  let raw: string;
  try {
    raw = await readFile(filePath, 'utf-8');
  } catch (err) {
    if ((err as NodeJS.ErrnoException)?.code === 'ENOENT') {
      // No profile MCP configured — the normal case. Silent.
      return null;
    }
    // Hard read failure (perm/IO). Loud, drop the layer, still spawn.
    logger.warn(
      { agentId, runtimeKind, filePath, err: String(err) },
      'profile MCP: failed to read mcp.json; skipping the entire MCP layer (agent still spawns)',
    );
    return null;
  }

  const parsed = parseMcpConfig(raw);
  if (!parsed.ok) {
    // The write path validates, so a bad file is hand-edit/half-write. Loud,
    // all-or-nothing (never a partial subset), non-blocking.
    logger.warn(
      { agentId, runtimeKind, filePath, error: parsed.error },
      'profile MCP: mcp.json failed validation; skipping the entire MCP layer (agent still spawns)',
    );
    return null;
  }

  const serverNames = Object.keys(parsed.config.mcpServers);
  if (serverNames.length === 0) return null;

  for (const rawName of serverNames) {
    logger.debug(
      {
        agentId,
        runtimeKind,
        mapping: `profile/${rawName} -> ${runtimeMcpServerName('profile', rawName)}`,
      },
      'profile MCP: loading server',
    );
  }

  return parsed.config;
}

function opencodeProfileProjection(config: McpConfig) {
  const projection = projectProfileMcpForRuntime(config, 'opencode');
  if (projection.kind !== 'opencode') throw new Error('Unexpected OpenCode MCP projection.');
  return projection.config;
}
