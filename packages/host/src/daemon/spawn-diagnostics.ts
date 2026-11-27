import { existsSync } from 'node:fs';
import { delimiter } from 'node:path';
import {
  SILVERHAND_AGENT_ID_ENV,
  SILVERHAND_AUTH_TOKEN_ENV,
  SILVERHAND_CONVERSATION_ID_ENV,
  SILVERHAND_SERVER_URL_ENV,
} from '@silverhand/shared';

/**
 * Secret-safe diagnostics for runtime child spawns.
 *
 * These helpers exist purely to make the *next* spawn failure legible
 * (#task:1420): when a runtime fails to exec, the logs must be able to
 * answer "what process environment did this daemon actually hand the
 * child, which binary did it exec, and what did the OS return?" — without
 * ever leaking a token, auth header, secret, or certificate body.
 *
 * They are observation only. Nothing here filters or rewrites the env the
 * daemon spawns with; that behavioral question (env sanitizer) is
 * deliberately deferred until this logging has produced real evidence.
 */

/**
 * PATH-entry substrings that mark an ephemeral / launch-context shim dir —
 * the kind of pollution a daemon inherits when it is started from a Codex
 * session, screen, or debug shell (the leading #1422 hypothesis). We only
 * *flag* these in the summary; we do not remove them.
 */
const SUSPICIOUS_PATH_MARKERS = [
  '/.codex/tmp',
  '/arg0',
  '.cache/codex-runtimes',
  '/tmp/',
  '/private/tmp/',
  '/var/folders/',
] as const;

/** Env values safe to log verbatim — identity / locale, never secret. */
const IDENTITY_ENV_KEYS = [
  'HOME',
  'USER',
  'LOGNAME',
  'SHELL',
  'LANG',
  'LANGUAGE',
  'TERM',
  'COLORTERM',
  'TMPDIR',
  'TZ',
] as const;

/**
 * Launch-context vars whose *values* are diagnostically load-bearing and
 * are not secrets: the loader-injection and parent-cwd vars that most
 * plausibly corrupt an exec. Logged with values so a failure can be tied
 * to a concrete injected library / option.
 */
const LAUNCH_CONTEXT_ENV_KEYS = [
  'PWD',
  'OLDPWD',
  'INIT_CWD',
  'ELECTRON_RUN_AS_NODE',
  'NODE_OPTIONS',
] as const;
const LAUNCH_CONTEXT_ENV_PREFIXES = ['DYLD_', 'LD_'] as const;

/** Cert vars: log the *path* and whether it exists — never the file body. */
const CERT_ENV_KEYS = [
  'NODE_EXTRA_CA_CERTS',
  'SSL_CERT_FILE',
  'SSL_CERT_DIR',
  'REQUESTS_CA_BUNDLE',
  'CURL_CA_BUNDLE',
  'GIT_SSL_CAINFO',
] as const;

/** Proxy vars: values can carry `user:pass@` credentials → mask userinfo. */
const PROXY_ENV_KEYS = [
  'HTTP_PROXY',
  'HTTPS_PROXY',
  'ALL_PROXY',
  'FTP_PROXY',
  'NO_PROXY',
  'http_proxy',
  'https_proxy',
  'all_proxy',
  'ftp_proxy',
  'no_proxy',
] as const;

/** Noisy-but-notable package-manager / editor context — names only. */
const NOTABLE_ENV_PREFIXES = ['npm_', 'YARN_', 'PNPM_', 'VSCODE_', 'TERM_PROGRAM', '__CF'] as const;

/**
 * Any key matching this is treated as sensitive: the value is NEVER logged,
 * only the key name is recorded (so pollution/skew is still observable).
 * Deliberately broad — false positives (a non-secret key redacted) are
 * harmless; a leaked secret is not.
 */
const SENSITIVE_ENV_PATTERN =
  /(TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|PRIVATE|SESSION|COOKIE|APIKEY|API_KEY|AUTH|_KEY$|_KEY_)/i;

export interface SpawnPathSummary {
  path_dir_count: number;
  path_dirs: string[];
  path_suspicious_dirs: string[];
  path_nonexistent_dirs: string[];
  path_relative_dirs: string[];
}

/**
 * Summarize a PATH string. PATH entries are directory names (not secrets),
 * so the full ordered list is logged — order and membership are exactly
 * what a resolver-picked-the-wrong-binary or ephemeral-shim investigation
 * needs. Suspicious / missing / relative entries are called out separately.
 */
export interface SummarizeSpawnPathOptions {
  /**
   * When user-configured env overrides `PATH`, keep only the host shim prefix
   * visible and redact the user-supplied remainder so custom env values never
   * leak into `runtime_spawn_attempt` logs.
   */
  redactEntriesAfterDir?: string;
}

export function summarizeSpawnPath(
  pathValue: string | undefined,
  options?: SummarizeSpawnPathOptions,
): SpawnPathSummary {
  const rawDirs = (pathValue ?? '').split(delimiter).filter(Boolean);
  const dirs = options?.redactEntriesAfterDir
    ? rawDirs.map((dir, index) =>
        index === 0 && dir === options.redactEntriesAfterDir ? dir : '<redacted-user-path>',
      )
    : rawDirs;
  const suspicious: string[] = [];
  const nonexistent: string[] = [];
  const relative: string[] = [];
  for (const dir of dirs) {
    if (dir === '<redacted-user-path>') continue;
    if (SUSPICIOUS_PATH_MARKERS.some((marker) => dir.includes(marker))) {
      suspicious.push(dir);
    }
    if (!dir.startsWith('/')) {
      relative.push(dir);
    } else if (!existsSync(dir)) {
      nonexistent.push(dir);
    }
  }
  return {
    path_dir_count: dirs.length,
    path_dirs: dirs,
    path_suspicious_dirs: suspicious,
    path_nonexistent_dirs: nonexistent,
    path_relative_dirs: relative,
  };
}

export interface SpawnEnvSummary {
  total_keys: number;
  identity: Record<string, string>;
  /** DYLD_ / LD_ / NODE_OPTIONS / PWD / ... — loader & parent-cwd vars, with values. */
  launch_context: Record<string, string>;
  cert_files: Record<string, { path: string; exists: boolean }>;
  /** Proxy vars with any `user:pass@` userinfo masked out. */
  proxy: Record<string, string>;
  /** Names of noisy package-manager / editor vars present (values omitted). */
  notable_keys: string[];
  /** Names of keys whose values were withheld as sensitive (values NEVER logged). */
  sensitive_keys_redacted: string[];
  /** Names of every other key present, values omitted — full key-set visibility. */
  other_keys: string[];
}

const REQUIRED_SILVERHAND_ENV_KEYS = [
  SILVERHAND_CONVERSATION_ID_ENV,
  SILVERHAND_AGENT_ID_ENV,
  SILVERHAND_SERVER_URL_ENV,
  SILVERHAND_AUTH_TOKEN_ENV,
] as const;

export interface SilverhandEnvPresenceSummary {
  all_present: boolean;
  present_keys: string[];
  missing_keys: string[];
}

/**
 * Report only whether the runtime identity/control-plane env exists. Values
 * are deliberately never returned: in particular, AUTH_TOKEN and SERVER_URL
 * must not become log payloads merely to prove that wiring is present.
 */
export function summarizeSilverhandEnvPresence(
  env: Readonly<Record<string, string | undefined>>,
  options: { conversationIdRequired?: boolean } = {},
): SilverhandEnvPresenceSummary {
  const presentKeys: string[] = [];
  const missingKeys: string[] = [];
  for (const key of REQUIRED_SILVERHAND_ENV_KEYS) {
    if (key === SILVERHAND_CONVERSATION_ID_ENV && options.conversationIdRequired === false)
      continue;
    const value = env[key];
    if (typeof value === 'string' && value.length > 0) {
      presentKeys.push(key);
    } else {
      missingKeys.push(key);
    }
  }
  return {
    all_present: missingKeys.length === 0,
    present_keys: presentKeys,
    missing_keys: missingKeys,
  };
}

function maskProxyValue(value: string): string {
  try {
    const url = new URL(value);
    if (url.username || url.password) {
      url.username = '';
      url.password = '';
      return url.toString();
    }
    return value;
  } catch {
    // Not a URL (e.g. NO_PROXY host list). Only worry if it smells like creds.
    return value.includes('@') ? '<redacted-proxy>' : value;
  }
}

/**
 * Build a secret-safe summary of the env a runtime child would be spawned
 * with. The invariant that matters: no value that could be a credential is
 * ever emitted. Identity/locale, loader-context, proxy (masked), and cert
 * *paths* carry values; everything else contributes its key name only.
 */
export interface SummarizeSpawnEnvOptions {
  /** Extra keys whose values must never be logged (e.g. user-configured env). */
  redactedKeys?: ReadonlySet<string>;
}

export function summarizeSpawnEnv(
  env: NodeJS.ProcessEnv,
  options?: SummarizeSpawnEnvOptions,
): SpawnEnvSummary {
  const identity: Record<string, string> = {};
  const launchContext: Record<string, string> = {};
  const certFiles: Record<string, { path: string; exists: boolean }> = {};
  const proxy: Record<string, string> = {};
  const notableKeys: string[] = [];
  const sensitiveKeys: string[] = [];
  const otherKeys: string[] = [];
  let totalKeys = 0;

  for (const [key, value] of Object.entries(env)) {
    if (value === undefined) continue;
    totalKeys += 1;

    // Sensitivity (and any user-configured env key) is checked first and wins
    // over every value-bearing bucket so custom values never reach spawn logs.
    if (SENSITIVE_ENV_PATTERN.test(key) || options?.redactedKeys?.has(key)) {
      sensitiveKeys.push(key);
      continue;
    }
    if ((IDENTITY_ENV_KEYS as readonly string[]).includes(key)) {
      identity[key] = value;
      continue;
    }
    if (
      (LAUNCH_CONTEXT_ENV_KEYS as readonly string[]).includes(key) ||
      LAUNCH_CONTEXT_ENV_PREFIXES.some((prefix) => key.startsWith(prefix))
    ) {
      launchContext[key] = value;
      continue;
    }
    if ((CERT_ENV_KEYS as readonly string[]).includes(key)) {
      certFiles[key] = { path: value, exists: existsSync(value) };
      continue;
    }
    if ((PROXY_ENV_KEYS as readonly string[]).includes(key)) {
      proxy[key] = maskProxyValue(value);
      continue;
    }
    if (NOTABLE_ENV_PREFIXES.some((prefix) => key.startsWith(prefix))) {
      notableKeys.push(key);
      continue;
    }
    otherKeys.push(key);
  }

  return {
    total_keys: totalKeys,
    identity,
    launch_context: launchContext,
    cert_files: certFiles,
    proxy,
    notable_keys: notableKeys.sort(),
    sensitive_keys_redacted: sensitiveKeys.sort(),
    other_keys: otherKeys.sort(),
  };
}

/**
 * Argv is NOT safe to log raw: drivers inline sensitive material into it.
 * Codex passes profile-MCP config as `-c <inline toml>` whose body can carry
 * remote `http_headers` (Authorization) or stdio `env` secrets; Qoder passes
 * the whole standing prompt as `--append-system-prompt <prompt>`. So both the
 * pre-spawn argv and any `spawnargs` on a spawn error must go through this
 * redactor, which preserves shape (order, count, flag names) while withholding
 * value bodies.
 */

/** Any arg (or `--flag=value` name/value, or config key) matching this has
 *  its value withheld. Broad on purpose — over-redaction is harmless. */
const SENSITIVE_ARG_PATTERN =
  /(token|secret|password|passwd|credential|api[-_]?key|apikey|auth|bearer|cookie|headers?|mcp_servers)/i;

/** Flags whose *following* argument is an inline value that must be redacted
 *  (inline config / inline prompt). Path-valued flags like
 *  `--append-system-prompt-file` are deliberately absent — a path is not a secret. */
const REDACT_VALUE_AFTER_FLAG = new Set([
  '-c',
  '--config',
  '--append-system-prompt',
  '--system-prompt',
]);

function redactedPlaceholder(value: string): string {
  return `<redacted:len=${value.length}>`;
}

/** Redact the value handed to an inline-config/prompt flag while keeping any
 *  leading `key=` config path (a config key is not a secret, its value is). */
function redactFlagValue(flag: string, value: string): string {
  if (flag === '-c' || flag === '--config') {
    const eq = value.indexOf('=');
    if (eq > 0 && eq <= 64 && !SENSITIVE_ARG_PATTERN.test(value.slice(0, eq))) {
      return `${value.slice(0, eq)}=${redactedPlaceholder(value.slice(eq + 1))}`;
    }
  }
  return redactedPlaceholder(value);
}

/**
 * Produce a log-safe copy of a spawn argv. Never mutates or affects the real
 * args passed to the OS — this is purely for diagnostics.
 */
export function redactSpawnArgs(args: readonly string[]): string[] {
  const out: string[] = [];
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === undefined) continue;
    // `--flag=value` single token.
    const eq = arg.startsWith('-') ? arg.indexOf('=') : -1;
    if (eq > 0) {
      const name = arg.slice(0, eq);
      const value = arg.slice(eq + 1);
      if (
        REDACT_VALUE_AFTER_FLAG.has(name) ||
        SENSITIVE_ARG_PATTERN.test(name) ||
        SENSITIVE_ARG_PATTERN.test(value)
      ) {
        out.push(`${name}=${redactedPlaceholder(value)}`);
      } else {
        out.push(arg);
      }
      continue;
    }
    // `--flag value` two tokens: either an inline-config/prompt flag, or any
    // flag whose *name* looks sensitive (--api-key, --token, --authorization,
    // ...). Keep the flag name (a name is diagnostic, not a secret) and redact
    // the following value token. Guard on `!startsWith('-')` so a boolean flag
    // doesn't swallow the next flag as if it were a value.
    const isFlag = arg.startsWith('-');
    if (REDACT_VALUE_AFTER_FLAG.has(arg) || (isFlag && SENSITIVE_ARG_PATTERN.test(arg))) {
      out.push(arg);
      const next = args[i + 1];
      if (next !== undefined && !next.startsWith('-')) {
        i += 1;
        out.push(redactFlagValue(arg, next));
      }
      continue;
    }
    // A standalone (non-flag) token that itself smells like a credential.
    if (!isFlag && SENSITIVE_ARG_PATTERN.test(arg)) {
      out.push(redactedPlaceholder(arg));
      continue;
    }
    out.push(arg);
  }
  return out;
}

export interface SpawnErrorInfo {
  message: string;
  code?: string;
  errno?: number;
  syscall?: string;
  path?: string;
  spawnargs?: string[];
  /** Human-readable interpretation of the errno / code. */
  readable: string;
}

/**
 * Extract the structured fields Node hangs off a spawn `error` (code, errno,
 * syscall, path, spawnargs) and attach a readable interpretation — most
 * importantly mapping the opaque `spawn Unknown system error -8` (ENOEXEC,
 * which libuv has no UV_* code for) to something an operator can act on.
 */
export function describeSpawnError(err: unknown): SpawnErrorInfo {
  const e = err as (NodeJS.ErrnoException & { spawnargs?: unknown }) | null | undefined;
  const info: SpawnErrorInfo = {
    message: e?.message ?? String(err),
    readable: '',
  };
  if (typeof e?.code === 'string') info.code = e.code;
  if (typeof e?.errno === 'number') info.errno = e.errno;
  if (typeof e?.syscall === 'string') info.syscall = e.syscall;
  if (typeof e?.path === 'string') info.path = e.path;
  if (Array.isArray(e?.spawnargs)) {
    info.spawnargs = redactSpawnArgs(e.spawnargs.map((arg) => String(arg)));
  }
  info.readable = explainSpawnFailure(info);
  return info;
}

function explainSpawnFailure(info: SpawnErrorInfo): string {
  const { code, errno, message } = info;
  if (code === 'ENOEXEC' || errno === -8 || /Unknown system error -8/.test(message)) {
    return (
      'ENOEXEC (errno 8): the OS refused to exec the target as a valid ' +
      'executable image. Causes: zero-length / half-written binary, missing ' +
      'or invalid shebang, or an exec-time loader failure (e.g. DYLD_* ' +
      'injection or a picked-up wrapper script). libuv has no UV_ code for ' +
      'ENOEXEC, so Node surfaces it as "spawn Unknown system error -8".'
    );
  }
  if (code === 'EBADARCH' || errno === -86) {
    return (
      'EBADARCH (errno 86): CPU architecture mismatch — the binary is built ' +
      'for a different arch than this daemon host.'
    );
  }
  if (code === 'EACCES' || errno === -13) {
    return (
      'EACCES (errno 13): permission denied — the binary lacks the execute ' +
      'bit, or a directory on the path to it is unreadable.'
    );
  }
  if (code === 'ENOENT' || errno === -2) {
    return (
      'ENOENT (errno 2): no such file — the resolved binary path does not ' + 'exist at exec time.'
    );
  }
  const parts: string[] = [];
  if (code) parts.push(`code ${code}`);
  if (typeof errno === 'number') parts.push(`errno ${errno}`);
  return `Unmapped spawn failure${parts.length ? ` (${parts.join(', ')})` : ''}.`;
}
