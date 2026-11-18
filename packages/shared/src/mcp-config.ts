/**
 * Profile MCP — canonical config model + runtime-agnostic translation.
 *
 * Source of truth: `docs/agent-mcp-config-v1.md`. This module owns the
 * **canonical raw shape** a subject declares (the standard MCP `mcpServers`
 * map), how it parses/validates (fail-loud), the domain-scoped runtime name
 * prefix, and the pure per-runtime translations (canonical → Claude
 * `--mcp-config` JSON; canonical → Codex `-c mcp_servers.*` overrides). It is
 * host-agnostic and node-free: writing temp files / pushing argv lives in the
 * host drivers; this module only produces the values.
 *
 * Locked invariants (do not relax without updating the doc):
 *  - Additive only. We never emit `--strict-mcp-config`, never replace the
 *    user's own MCP, never touch the user's runtime config files.
 *  - The raw server name is the user-visible truth; the name handed to a
 *    runtime is private and ALWAYS domain-prefixed (`silverhand_<scope>__`)
 *    from v1 so a future workspace layer can't shadow it or force a rename
 *    (which a runtime would read as a brand-new server, resetting OAuth/cache).
 */
import { z } from 'zod';
import type { RuntimeKind } from './db/schema.js';

/**
 * MCP config scope domain. `profile` = the executing subject's global/L1
 * config (v1). `workspace` is reserved for the future workspace layer; it is
 * part of the name-prefix contract now so both layers can coexist without a
 * later rename.
 */
export type McpScope = 'profile' | 'workspace';

/**
 * Canonical filename of the raw profile MCP config under a subject's L1 home
 * (`<agent L1 dir>/mcp.json`; the daemon is the sole authority for that dir's
 * host-local coordinate). One constant so the daemon
 * read/write handler, the spawn loader, and the UI all agree.
 */
export const MCP_CONFIG_FILENAME = 'mcp.json';

/**
 * Server name charset. The raw name is embedded into a runtime server name
 * AND, for codex, into a TOML bare key (`mcp_servers.<prefixed>`), so it must
 * be bare-key safe. Restricting it up front (fail-loud) is cheaper than
 * quoting dotted TOML keys and keeps Claude/Codex names identical.
 */
const SERVER_NAME_RE = /^[A-Za-z0-9_-]{1,64}$/;

const StdioServerSchema = z
  .object({
    type: z.literal('stdio').optional(),
    command: z.string().min(1),
    args: z.array(z.string()).optional(),
    env: z.record(z.string()).optional(),
  })
  .strict();

const HttpServerSchema = z
  .object({
    type: z.enum(['http', 'sse']),
    url: z.string().url(),
    headers: z.record(z.string()).optional(),
  })
  .strict();

/**
 * One MCP server. Either a stdio launcher (`command` [+ `args`/`env`], the
 * default when `type` is omitted) or a remote endpoint (`type: http|sse` +
 * `url` [+ `headers`]). `.strict()` on both arms rejects unknown keys so a
 * typo or unsupported field fails loud at write/validate time.
 */
export const McpServerConfigSchema = z.union([StdioServerSchema, HttpServerSchema]);
export type McpServerConfig = z.infer<typeof McpServerConfigSchema>;

/**
 * The canonical raw file: the standard `{ "mcpServers": { <name>: {...} } }`
 * map, so it is copy-paste compatible with the ecosystem `.mcp.json` /
 * Claude's `--mcp-config`. Server names are validated against
 * {@link SERVER_NAME_RE}; `mcpServers` defaults to empty.
 */
export const McpConfigSchema = z
  .object({
    mcpServers: z
      .record(McpServerConfigSchema)
      .default({})
      .superRefine((servers, ctx) => {
        for (const name of Object.keys(servers)) {
          if (!SERVER_NAME_RE.test(name)) {
            ctx.addIssue({
              code: z.ZodIssueCode.custom,
              message: `invalid MCP server name "${name}": use 1-64 chars of [A-Za-z0-9_-]`,
              path: ['mcpServers', name],
            });
          }
        }
      }),
  })
  .strict();
export type McpConfig = z.infer<typeof McpConfigSchema>;

export const EMPTY_MCP_CONFIG: McpConfig = { mcpServers: {} };

export type ParseMcpResult = { ok: true; config: McpConfig } | { ok: false; error: string };

/**
 * Parse + validate raw JSON text into a {@link McpConfig}. Fail-loud: returns
 * a readable single-line error for bad JSON or schema violations and NEVER
 * throws. The write path (`profile mcp set` / `validate`) rejects on
 * `ok: false`; the spawn path drops the whole layer (loud, non-blocking) on
 * `ok: false`. An empty / whitespace-only body parses to the empty config.
 */
export function parseMcpConfig(raw: string): ParseMcpResult {
  const trimmed = raw.trim();
  if (trimmed.length === 0) return { ok: true, config: EMPTY_MCP_CONFIG };

  let json: unknown;
  try {
    json = JSON.parse(trimmed);
  } catch (err) {
    return {
      ok: false,
      error: `invalid JSON: ${err instanceof Error ? err.message : String(err)}`,
    };
  }

  const parsed = McpConfigSchema.safeParse(json);
  if (!parsed.success) {
    const first = parsed.error.issues[0];
    const where = first?.path.length ? ` at ${first.path.join('.')}` : '';
    return {
      ok: false,
      error: `invalid MCP config${where}: ${first?.message ?? 'schema mismatch'}`,
    };
  }
  return { ok: true, config: parsed.data };
}

/**
 * The private, domain-scoped runtime server name. ALWAYS prefixed so profile
 * and (future) workspace layers never collide at the runtime and a later
 * rename is never forced. Example: `silverhand_profile__github`.
 */
export function runtimeMcpServerName(scope: McpScope, rawName: string): string {
  return `silverhand_${scope}__${rawName}`;
}

/**
 * Canonical → Claude `--mcp-config` JSON object. Returns the `{ mcpServers }`
 * map with every server key rewritten to its domain-prefixed runtime name;
 * the per-server body is passed through unchanged (Claude reads the same
 * stdio / http shape). Returns `null` when there are no servers (the driver
 * then omits `--mcp-config` entirely).
 */
export function toClaudeMcpConfig(
  config: McpConfig,
  scope: McpScope = 'profile',
): { mcpServers: Record<string, McpServerConfig> } | null {
  const names = Object.keys(config.mcpServers);
  if (names.length === 0) return null;
  const mcpServers: Record<string, McpServerConfig> = {};
  for (const name of names) {
    mcpServers[runtimeMcpServerName(scope, name)] = config.mcpServers[name]!;
  }
  return { mcpServers };
}

/**
 * Canonical → Codex `-c` override VALUES (the `key=value` strings; the driver
 * supplies the `-c` argv token before each). ONE override per server
 * (`mcp_servers.<prefixed>=<inline-toml-table>`) so it MERGES with the user's
 * existing `[mcp_servers.*]` instead of replacing the whole table. The value
 * is a serialized TOML inline table (quoting handled by {@link serializeTomlValue}),
 * never a hand-built string. Returns `[]` when there are no servers.
 *
 * Each server is first projected through {@link toCodexServerConfig} into
 * Codex's NATIVE key names — Codex does not understand the canonical
 * `type`/`headers` keys (those are Claude's `--mcp-config` shape). Mismatched
 * keys would be silently ignored by Codex, leaving a remote server unusable
 * (Cindy #task:1135 review, Blocker 2). Projecting fail-loud here means a
 * server Codex cannot represent throws, which the spawn loader catches into a
 * loud, all-or-nothing layer drop.
 */
export function toCodexMcpOverrides(config: McpConfig, scope: McpScope = 'profile'): string[] {
  const overrides: string[] = [];
  for (const [name, server] of Object.entries(config.mcpServers)) {
    const key = `mcp_servers.${runtimeMcpServerName(scope, name)}`;
    overrides.push(`${key}=${serializeTomlValue(toCodexServerConfig(server))}`);
  }
  return overrides;
}

/**
 * Project one canonical server into Codex's native `[mcp_servers.<name>]`
 * shape. Codex uses snake_case TOML keys and has NO `type` discriminator —
 * a remote server is identified by the presence of `url`:
 *  - stdio   → `{ command, args?, env? }` (the `type` marker is dropped).
 *  - http/sse→ `{ url, http_headers? }` (drop `type`; canonical literal
 *    `headers` → Codex `http_headers`). Codex's remote transport is
 *    url-based, so both `http` and `sse` canonical servers map to the same
 *    url form; whether the endpoint actually connects is the runtime's
 *    concern, not ours (we never pre-check MCP server connectivity).
 *
 * Verified against the bundled Codex binary (`codex-cli 0.140.0`,
 * `/Applications/Codex.app/Contents/Resources/codex` — the same native
 * bundle the daemon resolves, see `runtimes.ts` PLATFORM_CANDIDATES): a
 * remote server's TOML keys are `url` / `http_headers` (a table) /
 * `bearer_token_env_var` / `env_http_headers`, with the transport inferred
 * as `streamable_http` from `url` and NO `type` key; stdio uses
 * `command` / `args` / `env`. `codex mcp add ... --url ...` round-trips to
 * exactly this shape, and `codex app-server` accepts the same `-c
 * mcp_servers.<name>=<toml>` overrides the daemon appends to its argv.
 */
export function toCodexServerConfig(server: McpServerConfig): Record<string, unknown> {
  // The union isn't a TS-discriminated one (stdio's `type` is optional), so
  // narrow on the structural marker: a remote server is the one with `url`.
  if ('url' in server) {
    const out: Record<string, unknown> = { url: server.url };
    if (server.headers) out.http_headers = server.headers;
    return out;
  }
  // stdio (type omitted or 'stdio'): command is required by the schema.
  const out: Record<string, unknown> = { command: server.command };
  if (server.args) out.args = server.args;
  if (server.env) out.env = server.env;
  return out;
}

/**
 * One opencode `mcp.<name>` server. Mirrors opencode's config schema
 * (`McpLocalConfig` / `McpRemoteConfig`, verified against
 * https://opencode.ai/config.json for opencode 1.17.14): a `local` stdio
 * launcher folds command+args into a single `command` array and carries env
 * under `environment`; a `remote` server is a url endpoint with optional
 * `headers`. `additionalProperties:false` on both arms of the opencode schema,
 * so we emit ONLY these keys.
 */
export type OpencodeMcpServer =
  | { type: 'local'; command: string[]; environment?: Record<string, string> }
  | { type: 'remote'; url: string; headers?: Record<string, string> };

/**
 * Project one canonical server into opencode's native `mcp.<name>` shape:
 *  - stdio   → `{ type:'local', command:[command, ...args], environment? }`
 *    (opencode's `command` is a single argv array, and env is `environment`).
 *  - http/sse→ `{ type:'remote', url, headers? }` (both canonical remote
 *    variants map to the url-based `remote` form; the transport is opencode's
 *    concern, and we never pre-check connectivity — same rule as codex).
 */
export function toOpencodeServerConfig(server: McpServerConfig): OpencodeMcpServer {
  // The union isn't TS-discriminated (stdio's `type` is optional); narrow on
  // the structural `url` marker like {@link toCodexServerConfig}.
  if ('url' in server) {
    return {
      type: 'remote',
      url: server.url,
      ...(server.headers ? { headers: server.headers } : {}),
    };
  }
  return {
    type: 'local',
    command: server.args ? [server.command, ...server.args] : [server.command],
    ...(server.env ? { environment: server.env } : {}),
  };
}

/**
 * Canonical → opencode `{ mcp: { <prefixed>: {...} } }` config fragment, every
 * server key rewritten to its domain-prefixed runtime name. Returns `null`
 * when there are no servers. Unlike claude/codex (which pass argv), opencode
 * has no additive `--mcp-config` flag — this fragment is deep-merged into the
 * subject's config via `OPENCODE_CONFIG_CONTENT` (opencode merges it over the
 * user's on-disk config; the domain prefix keeps our servers from colliding
 * with the user's own).
 */
export function toOpencodeMcpConfig(
  config: McpConfig,
  scope: McpScope = 'profile',
): { mcp: Record<string, OpencodeMcpServer> } | null {
  const names = Object.keys(config.mcpServers);
  if (names.length === 0) return null;
  const mcp: Record<string, OpencodeMcpServer> = {};
  for (const name of names) {
    mcp[runtimeMcpServerName(scope, name)] = toOpencodeServerConfig(config.mcpServers[name]!);
  }
  return { mcp };
}

/**
 * Serialize a JSON-ish value to a TOML value literal (inline table / array /
 * string / number / boolean). Used to build codex `-c` override values
 * safely. Strings use TOML basic-string escaping; objects become inline
 * tables; arrays become inline arrays. `undefined` keys are skipped. Throws
 * on non-finite numbers / unsupported types so a bad mapping fails loud
 * rather than emitting malformed TOML.
 */
export function serializeTomlValue(value: unknown): string {
  if (typeof value === 'string') return serializeTomlString(value);
  if (typeof value === 'boolean') return value ? 'true' : 'false';
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) {
      throw new Error(`cannot serialize non-finite number to TOML: ${value}`);
    }
    return String(value);
  }
  if (Array.isArray(value)) {
    return `[${value.map(serializeTomlValue).join(', ')}]`;
  }
  if (value !== null && typeof value === 'object') {
    const parts: string[] = [];
    for (const [k, v] of Object.entries(value)) {
      if (v === undefined) continue;
      parts.push(`${serializeTomlKey(k)} = ${serializeTomlValue(v)}`);
    }
    return `{${parts.join(', ')}}`;
  }
  throw new Error(`cannot serialize value to TOML: ${String(value)}`);
}

// ── Live probe (#task:1137) — one-shot MCP connection diagnostic ─────────
//
// `probe` is the LIVE counterpart to the static `parseMcpConfig` check: it
// actually opens a single short-lived MCP connection to a declared server,
// runs the `initialize` handshake, and optionally `tools/list`, then closes
// immediately (the connection never outlives the call). It is a user-triggered
// diagnostic snapshot, NOT runtime MCP health state and NOT reused at agent
// spawn. Shapes live here (shared) so the daemon handler, the backend route,
// the CLI, and the renderer all agree; the SDK-driven connection itself lives
// in `packages/host` (node-only). Workspace-scoped MCP will reuse these shapes
// unchanged — only the host's source resolver + the route's permission gate
// differ by scope.

/**
 * The stages a probe walks: `parse` the config → find the named server
 * (`not_found` if absent) → `connect` the transport → MCP `initialize` →
 * (optional) `tools/list`. `phase` names the DEEPEST stage reached: on success
 * it is the last completed stage (`initialize` for a connection test, `tools`
 * when tools were listed); on failure it is the stage that failed.
 */
export const MCP_PROBE_PHASES = ['parse', 'not_found', 'connect', 'initialize', 'tools'] as const;
export const McpProbePhaseSchema = z.enum(MCP_PROBE_PHASES);
export type McpProbePhase = z.infer<typeof McpProbePhaseSchema>;

/** One tool reported by a server's `tools/list` (name + optional description).
 *  Tool metadata is not secret; header/env values never appear here. */
export const McpProbeToolSchema = z.object({
  name: z.string(),
  description: z.string().optional(),
});
export type McpProbeTool = z.infer<typeof McpProbeToolSchema>;

/**
 * The result of a single live probe. `message` is REDACTED — it never contains
 * header/env values, URL query strings, or bearer tokens, only safe shape info
 * (present on failure, optional on success). `server_info` comes from the
 * `initialize` result (not a secret); `tools` is present only when
 * `include_tools` was requested and the probe reached the `tools` phase.
 */
export const McpProbeResultSchema = z.object({
  ok: z.boolean(),
  phase: McpProbePhaseSchema,
  message: z.string().optional(),
  server_info: z.object({ name: z.string().optional(), version: z.string().optional() }).optional(),
  tools: z.array(McpProbeToolSchema).optional(),
});
export type McpProbeResult = z.infer<typeof McpProbeResultSchema>;

/**
 * The public probe REQUEST body (CLI → backend route). The server is named by
 * its RAW name (the user-visible key, never the `silverhand_<scope>__` runtime
 * name — a probe connects to the real endpoint). The subject/scope is resolved
 * server-side from the caller's identity, so it is NOT part of this body.
 */
export const McpProbeRequestSchema = z.object({
  server_name: z.string().min(1),
  include_tools: z.boolean().optional(),
});
export type McpProbeRequest = z.infer<typeof McpProbeRequestSchema>;

/** TOML bare key if safe, else a quoted key. */
function serializeTomlKey(key: string): string {
  return /^[A-Za-z0-9_-]+$/.test(key) ? key : serializeTomlString(key);
}

/** TOML basic string with the standard escape set. */
function serializeTomlString(value: string): string {
  const escaped = value
    .replace(/\\/g, '\\\\')
    .replace(/"/g, '\\"')
    .replace(/\n/g, '\\n')
    .replace(/\r/g, '\\r')
    .replace(/\t/g, '\\t');
  return `"${escaped}"`;
}

/** Standard ACP session MCP wire projection; shares the profile namespace. */
export type AcpMcpServer =
  | { name: string; command: string; args: string[]; env: { name: string; value: string }[] }
  | { name: string; type: 'http' | 'sse'; url: string; headers: { name: string; value: string }[] };

export function toAcpMcpServers(config: McpConfig, scope: McpScope): AcpMcpServer[] {
  return Object.entries(config.mcpServers).map(([name, server]) => {
    const identity = { name: runtimeMcpServerName(scope, name) };
    if ('command' in server) {
      return {
        ...identity,
        command: server.command,
        args: server.args ?? [],
        env: Object.entries(server.env ?? {}).map(([name, value]) => ({ name, value })),
      };
    }
    return {
      ...identity,
      type: server.type,
      url: server.url,
      headers: Object.entries(server.headers ?? {}).map(([name, value]) => ({ name, value })),
    };
  });
}

export type ProfileMcpProjection =
  | { kind: 'claude'; config: ReturnType<typeof toClaudeMcpConfig> }
  | { kind: 'codex'; overrides: ReturnType<typeof toCodexMcpOverrides> }
  | { kind: 'opencode'; config: ReturnType<typeof toOpencodeMcpConfig> }
  | { kind: 'acp'; servers: AcpMcpServer[] };

/** Static projection only. Does not connect, authenticate, or prove server health. */
export function projectProfileMcpForRuntime(
  config: McpConfig,
  runtime: RuntimeKind,
): ProfileMcpProjection {
  switch (runtime) {
    case 'claude':
    case 'qoder':
      return { kind: 'claude', config: toClaudeMcpConfig(config, 'profile') };
    case 'codex':
      return { kind: 'codex', overrides: toCodexMcpOverrides(config, 'profile') };
    case 'opencode':
      return { kind: 'opencode', config: toOpencodeMcpConfig(config, 'profile') };
    case 'grok':
      return { kind: 'acp', servers: toAcpMcpServers(config, 'profile') };
    default:
      throw new Error(`Profile MCP is not loaded for ${runtime}.`);
  }
}
