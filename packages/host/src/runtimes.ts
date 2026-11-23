import { spawn } from 'node:child_process';
import { constants } from 'node:fs';
import { access, readdir, stat } from 'node:fs/promises';
import { delimiter, join } from 'node:path';
import { homedir } from 'node:os';
import {
  RUNTIME_CATALOG,
  type RuntimeCatalogResponse,
  type RuntimeInfo,
  type RuntimeKind,
} from '@silverhand/shared';

const DETECT_BINARIES: Record<RuntimeKind, string> = {
  claude: 'claude',
  codex: 'codex',
  cursor: 'cursor-agent',
  gemini: 'gemini',
  qoder: 'qodercli',
  opencode: 'opencode',
  pi: 'pi',
  grok: 'grok',
  antigravity: 'agy',
};

const DEFAULT_RUNTIME_DETECTION_CACHE_TTL_MS = 30_000;
const COMMAND_TIMEOUT_MS = 5_000;

/**
 * Per-kind, per-platform candidate paths probed BEFORE `which <bin>`.
 *
 * Motivation (codex / macOS): the npm-installed wrapper at
 * `~/.npm-global/bin/codex` is a Node script that re-spawns the real
 * `@openai/codex-darwin-arm64` binary after a fresh dyld + Node startup.
 * Cold-start latency for the wrapper is empirically 10–22s, which used
 * to push our 30s init watchdog past the line and SIGTERM Codex before
 * it could emit `session_init`. Slock-ai
 * resolves codex by preferring the native bundle binary inside the codex
 * app bundle on macOS, which skips the wrapper entirely and typically
 * cuts cold start in half.
 *
 * Bundle order (macOS): prefer ChatGPT.app's `codex-cli/bin/codex` launcher,
 * a small shell script that execs the bundled native CodexCLI.app binary
 * without the npm/Node wrapper. Older ChatGPT.app and standalone Codex.app
 * binaries remain fallbacks before PATH. Candidates are probed in order,
 * so the array order below IS the resolution priority.
 *
 * We deliberately do NOT version-match the bundle against the npm CLI:
 * the user has already accepted the bundle by installing the app, and
 * gating on version equality would silently disable the optimization
 * for anyone on a slightly different release. If alpha-bundle
 * incompatibility surfaces, fix the driver to handle both — don't fall
 * back to the slow wrapper.
 */
const PLATFORM_CANDIDATES: Partial<
  Record<RuntimeKind, Partial<Record<NodeJS.Platform, string[]>>>
> = {
  codex: {
    darwin: [
      '/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex',
      '/Applications/ChatGPT.app/Contents/Resources/codex',
      '/Applications/Codex.app/Contents/Resources/codex',
    ],
  },
};

/**
 * The platform-specific hard-preferred binary locations for `kind` (see
 * `PLATFORM_CANDIDATES`). Diagnostic use only: lets a caller classify a
 * resolved binary path as coming from a known app-bundle candidate vs a
 * plain PATH lookup, which is central to the #1422 "which codex did the
 * daemon actually exec" investigation. Does not affect resolution.
 */
export function platformBinaryCandidates(
  kind: RuntimeKind,
  platform: NodeJS.Platform = process.platform,
): readonly string[] {
  return PLATFORM_CANDIDATES[kind]?.[platform] ?? [];
}

async function isExecutableFile(path: string): Promise<boolean> {
  try {
    const s = await stat(path);
    if (!s.isFile()) return false;
    await access(path, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

interface CommandResult {
  code: number | null;
  stdout: string;
  stderr: string;
}

async function runCommand(
  command: string,
  args: readonly string[],
  timeoutMs = COMMAND_TIMEOUT_MS,
): Promise<CommandResult | null> {
  return new Promise((resolve) => {
    const proc = spawn(command, [...args], {
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let out = '';
    let err = '';
    let settled = false;
    const finish = (result: CommandResult | null) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve(result);
    };
    const timer = setTimeout(() => {
      proc.kill();
      finish(null);
    }, timeoutMs);
    proc.stdout.on('data', (chunk: Buffer) => {
      out += chunk.toString();
    });
    proc.stderr.on('data', (chunk: Buffer) => {
      err += chunk.toString();
    });
    proc.on('error', () => finish(null));
    proc.on('close', (code) => {
      finish({ code, stdout: out, stderr: err });
    });
  });
}

function splitPath(value: string | undefined | null): string[] {
  if (!value) return [];
  return value
    .split(delimiter)
    .map((p) => p.trim())
    .filter(Boolean);
}

function unique(values: readonly string[]): string[] {
  return [...new Set(values)];
}

async function existingChildDirs(parent: string, childSuffix: string): Promise<string[]> {
  try {
    const entries = await readdir(parent, { withFileTypes: true });
    return entries.filter((e) => e.isDirectory()).map((e) => join(parent, e.name, childSuffix));
  } catch {
    return [];
  }
}

async function versionManagerCandidateDirs(home: string): Promise<string[]> {
  const dirs = [
    join(home, '.volta/bin'),
    join(home, '.asdf/shims'),
    join(home, '.local/share/mise/shims'),
    join(home, '.nvm/current/bin'),
    join(home, '.fnm/aliases/default/bin'),
  ];
  dirs.push(...(await existingChildDirs(join(home, '.nvm/versions/node'), 'bin')));
  dirs.push(...(await existingChildDirs(join(home, '.fnm/node-versions'), 'installation/bin')));
  return dirs;
}

function commonCandidateDirs(home: string): string[] {
  return [
    join(home, '.local/bin'),
    join(home, '.npm-global/bin'),
    join(home, '.npm/bin'),
    join(home, '.bun/bin'),
    join(home, 'Library/pnpm'),
    join(home, '.yarn/bin'),
    join(home, '.config/yarn/global/node_modules/.bin'),
    '/opt/homebrew/bin',
    '/opt/homebrew/sbin',
    '/usr/local/bin',
    '/usr/local/sbin',
    '/opt/local/bin',
    '/usr/bin',
    '/bin',
    '/usr/sbin',
    '/sbin',
  ];
}

async function pathHelperDirs(pathHelperPath: string | null): Promise<string[]> {
  if (!pathHelperPath) return [];
  const result = await runCommand(pathHelperPath, ['-s'], 2_000);
  if (!result || result.code !== 0) return [];
  const match = result.stdout.match(/PATH="([^"]+)"/);
  return splitPath(match?.[1]);
}

async function loginShellDirs(shellPath: string | null): Promise<string[]> {
  if (!shellPath) return [];
  const markerStart = '__silverhand_path_start__';
  const markerEnd = '__silverhand_path_end__';
  const script = [
    `printf '%s\\n' ${markerStart}`,
    `printf '%s\\n' "$PATH"`,
    `printf '%s\\n' ${markerEnd}`,
  ].join('; ');
  const result = await runCommand(shellPath, ['-ilc', script], 5_000);
  if (!result || result.code !== 0) return [];
  const start = result.stdout.indexOf(markerStart);
  const end = result.stdout.indexOf(markerEnd);
  if (start < 0 || end <= start) return [];
  const body = result.stdout.slice(start + markerStart.length, end).trim();
  return splitPath(body.split('\n').find(Boolean));
}

export interface RuntimeDetectionOptions {
  force?: boolean;
  detectKinds?: readonly RuntimeKind[];
  now?: () => number;
  cacheTtlMs?: number;
  pathEnv?: string | null;
  platform?: NodeJS.Platform;
  homeDir?: string;
  shellPath?: string | null;
  pathHelperPath?: string | null;
  extraCandidateDirs?: readonly string[];
  includeDefaultCandidateDirs?: boolean;
}

interface RuntimeDetectionCache {
  expiresAt: number;
  response: RuntimeCatalogResponse;
}

let runtimeDetectionCache: RuntimeDetectionCache | null = null;

function canUseRuntimeDetectionCache(options: RuntimeDetectionOptions): boolean {
  return (
    options.detectKinds === undefined &&
    options.pathEnv === undefined &&
    options.platform === undefined &&
    options.homeDir === undefined &&
    options.shellPath === undefined &&
    options.pathHelperPath === undefined &&
    options.extraCandidateDirs === undefined &&
    options.includeDefaultCandidateDirs === undefined
  );
}

async function collectPathDirs(options: RuntimeDetectionOptions): Promise<string[]> {
  const platform = options.platform ?? process.platform;
  const home = options.homeDir ?? homedir();
  const shellPath =
    options.shellPath === undefined
      ? platform === 'darwin'
        ? (process.env.SHELL ?? null)
        : null
      : options.shellPath;
  const pathHelperPath =
    options.pathHelperPath === undefined
      ? platform === 'darwin'
        ? '/usr/libexec/path_helper'
        : null
      : options.pathHelperPath;

  const dirs = [
    ...splitPath(options.pathEnv === undefined ? process.env.PATH : options.pathEnv),
    ...(await loginShellDirs(shellPath)),
    ...(await pathHelperDirs(pathHelperPath)),
    ...(options.extraCandidateDirs ?? []),
    ...(options.includeDefaultCandidateDirs === false
      ? []
      : [...commonCandidateDirs(home), ...(await versionManagerCandidateDirs(home))]),
  ];
  return unique(dirs);
}

/**
 * Resolve `kind` to an executable path. Tries platform-specific known
 * locations first (see `PLATFORM_CANDIDATES`), then falls back to a
 * PATH lookup via `which`.
 */
async function resolveBinary(
  kind: RuntimeKind,
  bin: string,
  pathDirs: readonly string[],
  platform: NodeJS.Platform,
): Promise<string | null> {
  const candidates = PLATFORM_CANDIDATES[kind]?.[platform] ?? [];
  for (const candidate of candidates) {
    if (await isExecutableFile(candidate)) {
      return candidate;
    }
  }
  for (const dir of pathDirs) {
    const candidate = join(dir, bin);
    if (await isExecutableFile(candidate)) {
      return candidate;
    }
  }
  return null;
}

/**
 * Run `<bin> --version` and parse a semver-ish token out of the first
 * line of output.
 *
 * Timeout note: bumped from 3s to 30s on 2026-05-08. The codex npm
 * wrapper takes 10–22s on a cold dyld (fnm Node start + `require.resolve`
 * + spawning the real binary), so a 3s probe was returning `null`
 * version even when the runtime was perfectly installed and would have
 * answered eventually. 30s leaves headroom for the slowest observed
 * wrapper start while still bounding the hang if a binary is genuinely
 * stuck. The codex cold-start incident that motivated this budget is
 * inlined above; full postmortem in git history.
 */
async function probeVersion(bin: string): Promise<string | null> {
  return new Promise((resolve) => {
    const proc = spawn(bin, ['--version'], { stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    let err = '';
    let settled = false;
    const finish = (value: string | null) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve(value);
    };
    proc.stdout.on('data', (chunk: Buffer) => {
      out += chunk.toString();
    });
    proc.stderr.on('data', (chunk: Buffer) => {
      err += chunk.toString();
    });
    proc.on('error', () => finish(null));
    const timer = setTimeout(() => {
      proc.kill();
      finish(null);
    }, 30_000);
    proc.on('close', () => {
      const text = (out || err).trim();
      const match = text.match(/\d+\.\d+(?:\.\d+)?/);
      const firstLine = text.split('\n')[0]?.trim();
      finish(match ? match[0] : firstLine || null);
    });
  });
}

function runtimeProbeKinds(): RuntimeKind[] {
  return RUNTIME_CATALOG.filter((entry) => entry.selectable).map((entry) => entry.kind);
}

async function detectRuntime(
  kind: RuntimeKind,
  pathDirs: readonly string[],
  platform: NodeJS.Platform,
): Promise<RuntimeInfo> {
  try {
    const bin = DETECT_BINARIES[kind];
    const path = await resolveBinary(kind, bin, pathDirs, platform);
    if (!path) {
      return {
        kind,
        state: 'absent',
        reason: 'not_found',
        installed: false,
        binary: null,
        version: null,
      } satisfies RuntimeInfo;
    }
    const version = await probeVersion(path);
    return {
      kind,
      state: version ? 'present' : 'present_unverified',
      reason: version ? 'resolved' : 'version_unavailable',
      installed: true,
      binary: path,
      version,
    } satisfies RuntimeInfo;
  } catch {
    return {
      kind,
      state: 'unknown',
      reason: 'probe_failed',
      installed: false,
      binary: null,
      version: null,
    } satisfies RuntimeInfo;
  }
}

async function buildRuntimeCatalog(
  options: RuntimeDetectionOptions = {},
): Promise<RuntimeCatalogResponse> {
  const now = options.now?.() ?? Date.now();
  const cacheable = canUseRuntimeDetectionCache(options);
  if (
    cacheable &&
    !options.force &&
    runtimeDetectionCache &&
    runtimeDetectionCache.expiresAt > now
  ) {
    return runtimeDetectionCache.response;
  }
  const platform = options.platform ?? process.platform;
  const kinds = [...(options.detectKinds ?? runtimeProbeKinds())];
  const pathDirs = await collectPathDirs(options);
  const results = await Promise.all(kinds.map((kind) => detectRuntime(kind, pathDirs, platform)));
  const response = { runtimes: results, detected_at: now } satisfies RuntimeCatalogResponse;
  if (cacheable) {
    runtimeDetectionCache = {
      response,
      expiresAt: now + (options.cacheTtlMs ?? DEFAULT_RUNTIME_DETECTION_CACHE_TTL_MS),
    };
  }
  return response;
}

export async function detectRuntimeCatalog(
  options: RuntimeDetectionOptions = {},
): Promise<RuntimeCatalogResponse> {
  return buildRuntimeCatalog(options);
}

export async function detectRuntimes(
  options: RuntimeDetectionOptions = {},
): Promise<RuntimeInfo[]> {
  return (await detectRuntimeCatalog(options)).runtimes;
}
