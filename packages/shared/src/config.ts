import { z } from 'zod';
import { BACKEND_ADMISSION } from './backend-admission.generated.js';

export const ServerModeSchema = z.enum(['embedded', 'remote']);
export type ServerMode = z.infer<typeof ServerModeSchema>;

export const ChildSessionLimitsSchema = z.object({
  max_concurrent_per_parent: z.number().int().positive().nullable().default(null),
  spawn_cooldown_seconds: z.number().int().nonnegative().nullable().default(null),
  // max_depth is hard-coded to 1 in code; field reserved for future
  max_depth: z.literal(1).default(1),
});
export type ChildSessionLimits = z.infer<typeof ChildSessionLimitsSchema>;

export const ServerConfigSchema = z.object({
  mode: ServerModeSchema,
  url: z.string().url(),
  auth_token: z.string().min(16).optional(),
  /**
   * Browser/Electron `Origin` values allowed to open the authenticated event
   * WebSocket — a first-class SECURITY allowlist, not a topology hint. A legit
   * desktop sends `Origin` = the origin it reaches this backend at (the main
   * process pins it via `installBackendEventWsOriginOverride`); a malicious page
   * sends its own domain (browsers forbid JS from spoofing `Origin`) and is
   * rejected. The ws-server trusts ONLY this list — it never derives trust from
   * `url`. Standalone populates it from `SILVERHAND_EVENT_WS_ALLOWED_ORIGINS`;
   * embedded seeds it with its finalized loopback origin at bind time. See
   * `docs/server-env-config.md` and `docs/orchestration-boundary-v1.md §4.3`.
   */
  event_ws_allowed_origins: z.array(z.string().url()).optional(),
  refresh_token: z.string().nullable().default(null),
  child_sessions: ChildSessionLimitsSchema.default({
    max_concurrent_per_parent: null,
    spawn_cooldown_seconds: null,
    max_depth: 1,
  }),
});

export type ServerConfig = z.infer<typeof ServerConfigSchema>;

export const SERVER_CONFIG_FILENAME = 'server-config.json';
export const INSTALLATION_ID_FILENAME = 'installation_id';
export const DAEMON_HOST_CONFIG_FILENAME = 'daemon-config.json';
export const EMBEDDED_BOOTSTRAP_FILENAME = 'embedded-bootstrap';
export const EVENT_WS_SESSION_COOKIE = BACKEND_ADMISSION.eventWebSocketSessionCookie;
export const AUTH_TOKEN_HEADER = 'authorization';

export const DaemonHostConfigSchema = z.object({
  server_url: z.string().url(),
  host_id: z.string().min(1),
  host_token: z.string().min(1),
  /**
   * Local daemon policy bit allowing non-loopback http:// remote
   * outbound connections for this client. This is derived from the
   * desktop package/env opt-in and written only into daemon-config;
   * it is intentionally not part of the server connection profile and
   * never crosses the daemon↔server protocol.
   */
  allow_insecure_remote: z.boolean().optional(),
  /**
   * Local config consistency anchor. When the desktop main
   * process derives daemon-config from the server connection profile
   * it stamps the profile's monotonic `version` here; the daemon
   * reads it solely to:
   *
   *   1. Include in `daemonHostConfigFingerprint` so a profile commit
   *      that bumps the version (even with the same `server_url` /
   *      `host_id` / `host_token`) is NOT treated as a no-op reload —
   *      otherwise desktop's generation tracking diverges from
   *      daemon's after a metadata-only commit.
   *   2. Be visible in daemon-side debug / status output for support.
   *
   * **Strict R4 boundary**: `profile_version` is local config only —
   * NEVER serialized into any outbound WSS frame, auth header, or
   * backend HTTP request. Future R4 unification (D5+) keeps this
   * field local; the daemon-server protocol stays unaware of it.
   *
   * Optional in the schema for backward compat — daemon-configs
   * written by D3.x desktops parse without the field — but new
   * desktop writes (5c phase-2 derive + retro-fit on rotate / claim)
   * MUST populate it once they have a profile to derive from.
   * Embedded-mode bootstrap (no profile present) intentionally leaves
   * the field undefined; that is also a stable state the daemon
   * fingerprint accommodates.
   */
  profile_version: z.number().int().nonnegative().optional(),
});
export type DaemonHostConfig = z.infer<typeof DaemonHostConfigSchema>;

/**
 * Environment variable names the `silverhand` CLI reads at every
 * invocation. Spawned agents inherit these from the host daemon child
 * spawn env. The CLI never accepts them as
 * arguments — agents must not override them, and the per-(cid, aid)
 * auth token is the integrity boundary that prevents impersonation
 * via a manual env override.
 *
 * These constants are exported here so both the spawn-side
 * (`@silverhand/host`) and the read-side (`packages/cli`) reference the
 * same string and cannot drift.
 */
export const SILVERHAND_CONVERSATION_ID_ENV = 'SILVERHAND_CONVERSATION_ID';
export const SILVERHAND_AGENT_ID_ENV = 'SILVERHAND_AGENT_ID';
export const SILVERHAND_SERVER_URL_ENV = 'SILVERHAND_SERVER_URL';
export const SILVERHAND_AUTH_TOKEN_ENV = 'SILVERHAND_AUTH_TOKEN';
export const SILVERHAND_LAST_DELIVERED_SEQ_FILE_ENV = 'SILVERHAND_LAST_DELIVERED_SEQ_FILE';
export const SILVERHAND_DAEMON_SOCKET_ENV = 'SILVERHAND_DAEMON_SOCKET';
export const SILVERHAND_ACP_AUTO_COMPACT_ENV = 'SILVERHAND_ACP_AUTO_COMPACT';

/**
 * D4.a Commit 1 — dev-only escape hatch that widens the server-URL
 * classifier to accept non-loopback http://... URLs (classified as
 * `{ kind: 'remote', insecure: true }`).
 *
 * Scope-locked invariant (Cindy review on slicing v2): this flag
 * ONLY affects acceptance of non-loopback http URLs. It does NOT:
 *   - relax TLS certificate verification on https:// URLs (those
 *     still go through Node's system-default CA pipeline),
 *   - widen the scheme allowlist (ws/wss/file/ftp still rejected),
 *   - affect the loopback http path (which never needed it).
 *
 * The flag value is intentionally NOT persisted to the server
 * connection profile (`docs/auth-and-host-model.md` §3.2 — Server
 * connection profile): if it
 * were, toggling the env would cause profile↔runtime drift. Profile
 * acceptance is re-evaluated on every read against current env.
 *
 * Read site (Commit 1): pure constant only. Wiring into the daemon /
 * Setup Wizard / Settings consumers lands in subsequent D4.a commits.
 */
export const SILVERHAND_ALLOW_INSECURE_REMOTE_ENV = 'SILVERHAND_ALLOW_INSECURE_REMOTE';

type EnvironmentSnapshot = Readonly<Record<string, string | undefined>>;

/**
 * Helper: read the dev-only insecure-remote env flag from a `process.env`
 * snapshot. Returns true iff the env value is exactly `'1'`.
 *
 * Strict equality (not truthiness) is deliberate: `'0'`, `'false'`,
 * `'off'`, and empty string all mean "off" — only the explicit `'1'`
 * activates the escape hatch. This matches the existing convention
 * already used by `SILVERHAND_DISABLE_SIGNUP` in
 * `packages/backend/src/auth/config.ts`.
 */
export function readAllowInsecureRemoteFlag(env: EnvironmentSnapshot): boolean {
  return env[SILVERHAND_ALLOW_INSECURE_REMOTE_ENV] === '1';
}

/**
 * D4.a Commit 3 — dev-only escape hatch that forces the desktop main
 * process to boot the EMBEDDED backend even when a server connection
 * profile (`docs/auth-and-host-model.md` §3.2 — Server connection
 * profile) is present and would
 * otherwise route to remote mode.
 *
 * Intended use cases (all dev-only):
 *   - A developer has a committed remote profile but wants to debug
 *     the embedded code path without clearing the profile.
 *   - The remote backend is temporarily unreachable and the developer
 *     wants the app to come up against embedded so the rest of the
 *     surface remains testable.
 *   - Regression tests for the embedded boot path that share a
 *     `~/.silverhand/` fixture with a remote profile.
 *
 * The flag does NOT:
 *   - clear or modify the profile file (the profile remains exactly
 *     as the user committed it; the next launch without the flag
 *     resumes remote mode);
 *   - widen any classifier acceptance (independent of
 *     `SILVERHAND_ALLOW_INSECURE_REMOTE`, which only affects URL
 *     classification);
 *   - leak into the daemon (daemon does NOT read the profile or this
 *     flag — Cindy R4 / §7.2 "Daemon never reads the profile").
 *
 * Strict-equality semantics matches the rest of our env contract:
 * only the literal `'1'` activates it.
 */
export const SILVERHAND_FORCE_EMBEDDED_ENV = 'SILVERHAND_FORCE_EMBEDDED';

export function readForceEmbeddedFlag(env: EnvironmentSnapshot): boolean {
  return env[SILVERHAND_FORCE_EMBEDDED_ENV] === '1';
}
