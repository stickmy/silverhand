import { execFile, spawn, type ChildProcess } from 'node:child_process';
import { closeSync, mkdirSync, openSync, readFileSync, readdirSync, unlinkSync } from 'node:fs';
import { Socket } from 'node:net';
import { hostname } from 'node:os';
import { basename, dirname, join, resolve, sep } from 'node:path';
import { promisify } from 'node:util';
import {
  apiPaths,
  CONNECT_ARG_CONNECT_TOKEN,
  CONNECT_ARG_CONNECT_TOKEN_ENV,
  CONNECT_ARG_DATA_ROOT,
  CONNECT_ARG_DATA_ROOT_ORIGIN,
  CONNECT_ARG_FOREGROUND,
  CONNECT_ARG_NO_DAEMON,
  CONNECT_ARG_QUIET,
  CONNECT_ARG_SERVER_URL,
  CONNECT_ARG_STDERR,
  CONNECT_SUBCOMMAND,
  STOP_SUBCOMMAND,
  readAllowInsecureRemoteFlag,
  shellQuoteConnectArg,
  type DaemonHostConfig,
} from '@silverhand/shared';
import {
  resolveComputerDataPaths,
  resolveConnectProfilePaths,
  computersRootDir,
} from '@silverhand/shared/app-data-layout';
import { runDaemon } from './run.js';
import { Supervisor } from './supervisor/index.js';
import type { DaemonIdentity } from './identity.js';
import { hostConnectWebSocketUrl } from './outbound-client.js';
import { defaultConnectDataRoot, defaultSilverhandStateRoot } from './computer-profile.js';
import { logDaemon } from './daemon-logging.js';

const execFileAsync = promisify(execFile);
const CONNECT_READY_TIMEOUT_MS = 10_000;
const STOP_GRACE_MS = 5_000;
const CONNECT_LAUNCHER_LOG = 'connect-launcher.log';
const CONNECT_TOKEN_ENV = 'SILVERHAND_DAEMON_CONNECT_TOKEN';
const DEFAULT_DAEMON_NPM_SPEC = '@silverhand-ai/daemon@latest';

export interface ConnectCommandOptions {
  serverUrl: string;
  connectToken: string;
  dataRoot: string | null;
  /** #task:1608 R2-3 — the data-root origin FACT. 'default' only when the
   *  detached child inherits the parent's default profile via the internal
   *  `--data-root-origin` flag; a user-supplied `--data-root` is always
   *  'explicit'. Never derived from the path shape. */
  dataRootOrigin: 'default' | 'explicit';
  alsoStderr: boolean;
  daemonMode: boolean;
}

export type ConnectParseResult =
  | { ok: true; options: ConnectCommandOptions }
  | { ok: false; exitCode: number; message: string };

export function parseConnectArgv(
  argv: readonly string[],
  env: NodeJS.ProcessEnv = process.env,
): ConnectParseResult {
  let serverUrl: string | null = null;
  let connectToken: string | null = null;
  let connectTokenEnv: string | null = null;
  let dataRoot: string | null = null;
  let dataRootOrigin: 'default' | 'explicit' | null = null;
  let alsoStderr = true;
  let daemonMode = true;

  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    switch (arg) {
      case '-h':
      case '--help':
        return { ok: false, exitCode: 0, message: connectUsage() };
      case CONNECT_ARG_SERVER_URL:
        serverUrl = argv[++i] ?? null;
        break;
      case CONNECT_ARG_CONNECT_TOKEN:
        connectToken = argv[++i] ?? null;
        break;
      case CONNECT_ARG_CONNECT_TOKEN_ENV:
        connectTokenEnv = argv[++i] ?? null;
        break;
      case CONNECT_ARG_DATA_ROOT:
        dataRoot = argv[++i] ?? null;
        break;
      case CONNECT_ARG_DATA_ROOT_ORIGIN: {
        const value = argv[++i] ?? null;
        if (value !== 'default' && value !== 'explicit') {
          return {
            ok: false,
            exitCode: 1,
            message: `${CONNECT_ARG_DATA_ROOT_ORIGIN} must be 'default' or 'explicit'`,
          };
        }
        dataRootOrigin = value;
        break;
      }
      case CONNECT_ARG_FOREGROUND:
      case CONNECT_ARG_NO_DAEMON:
        daemonMode = false;
        break;
      case CONNECT_ARG_QUIET:
        alsoStderr = false;
        break;
      case CONNECT_ARG_STDERR:
        alsoStderr = true;
        break;
      default:
        return {
          ok: false,
          exitCode: 1,
          message: `unknown connect argument: ${arg}\n\n${connectUsage()}`,
        };
    }
  }

  if (!serverUrl) {
    return { ok: false, exitCode: 1, message: `--server-url is required\n\n${connectUsage()}` };
  }
  if (!connectToken && connectTokenEnv) {
    connectToken = env[connectTokenEnv] ?? null;
    if (!connectToken) {
      return {
        ok: false,
        exitCode: 1,
        message: `${CONNECT_ARG_CONNECT_TOKEN_ENV} ${connectTokenEnv} is not set`,
      };
    }
  }
  if (!connectToken) {
    return {
      ok: false,
      exitCode: 1,
      message: `--connect-token is required\n\n${connectUsage()}`,
    };
  }
  if (dataRoot !== null && !dataRoot.startsWith('/')) {
    return { ok: false, exitCode: 1, message: '--data-root must be absolute' };
  }
  if (dataRootOrigin !== null && dataRoot === null) {
    return {
      ok: false,
      exitCode: 1,
      message: `${CONNECT_ARG_DATA_ROOT_ORIGIN} requires ${CONNECT_ARG_DATA_ROOT}`,
    };
  }

  return {
    ok: true,
    options: {
      serverUrl,
      connectToken,
      dataRoot,
      dataRootOrigin: dataRootOrigin ?? 'explicit',
      alsoStderr,
      daemonMode,
    },
  };
}

export async function runConnectCommand(
  argv: readonly string[],
  env: NodeJS.ProcessEnv = process.env,
): Promise<number | null> {
  const parsed = parseConnectArgv(argv, env);
  if (!parsed.ok) {
    const stream = parsed.exitCode === 0 ? process.stdout : process.stderr;
    stream.write(`silverhand-daemon connect: ${parsed.message}\n`);
    return parsed.exitCode;
  }

  const allowInsecureRemote = readAllowInsecureRemoteFlag(env);
  try {
    hostConnectWebSocketUrl(parsed.options.serverUrl, { allowInsecureRemote });
  } catch (err) {
    process.stderr.write(
      `silverhand-daemon connect: invalid --server-url: ${(err as Error).message}\n`,
    );
    return 1;
  }

  // #task:1608 R2-3 — the origin FACT decides the profile shape, never the
  // path shape. `--data-root` absent (or the internal origin flag saying
  // 'default') = default profile keyed by the host id; a user-supplied
  // explicit root is flat and self-contained. `<app-root>/computers/` is a
  // RESERVED namespace (its dir names are safeComputerDirId(host_id)
  // products): an explicit root inside it could collide with a managed
  // default profile, so it is rejected loudly — never silently redirected,
  // never silently run flat inside the managed namespace. Checked BEFORE
  // any network or filesystem side effect.
  const stateRoot = defaultSilverhandStateRoot();
  const useDefaultProfile =
    parsed.options.dataRoot === null || parsed.options.dataRootOrigin === 'default';
  if (!useDefaultProfile) {
    const computersRoot = resolve(computersRootDir(stateRoot));
    const requested = resolve(parsed.options.dataRoot!);
    if (requested === computersRoot || requested.startsWith(computersRoot + sep)) {
      process.stderr.write(
        `silverhand-daemon connect: --data-root may not point inside ${computersRoot} (reserved for daemon-managed default profiles); omit --data-root to use the default profile\n`,
      );
      return 1;
    }
  }

  let hostId: string;
  try {
    hostId = await resolveConnectHostId({
      serverUrl: parsed.options.serverUrl,
      connectToken: parsed.options.connectToken,
    });
  } catch (err) {
    process.stderr.write(`silverhand-daemon connect: ${(err as Error).message}\n`);
    return 1;
  }
  let dataRoot: string;
  let paths: ConnectRuntimePaths;
  if (useDefaultProfile) {
    dataRoot = defaultConnectDataRoot(hostId, stateRoot);
    if (
      parsed.options.dataRoot !== null &&
      resolve(parsed.options.dataRoot) !== resolve(dataRoot)
    ) {
      // The detached child inherited a default root minted by its parent;
      // it must re-derive to the SAME slot (same env, same token → same
      // host id). Divergence means the environment changed under us.
      process.stderr.write(
        `silverhand-daemon connect: inherited default data root ${parsed.options.dataRoot} does not match this host id's slot ${dataRoot}\n`,
      );
      return 1;
    }
    paths = defaultConnectRuntimePaths(hostId, stateRoot);
  } else {
    dataRoot = parsed.options.dataRoot!;
    paths = explicitConnectRuntimePaths(dataRoot);
  }
  mkdirSync(dataRoot, { recursive: true });
  mkdirSync(paths.runStateDir, { recursive: true });

  if (parsed.options.daemonMode) {
    return runConnectDaemonMode({
      options: parsed.options,
      dataRoot,
      defaultProfile: useDefaultProfile,
      paths,
      env,
    });
  }

  // Foreground = the actual daemon process. Refuse a double start before
  // opening any runtime surface for this profile.
  const alreadyRunning = await readExistingConnectDaemon(paths.pidFilePath, dataRoot);
  if (alreadyRunning.status !== 'none') {
    process.stderr.write(
      `silverhand-daemon connect: refusing to start; pid ${alreadyRunning.pid} already owns ${dataRoot}\n`,
    );
    return 1;
  }

  const hostConfig: DaemonHostConfig = {
    server_url: parsed.options.serverUrl,
    host_id: hostId,
    host_token: parsed.options.connectToken,
    ...(allowInsecureRemote ? { allow_insecure_remote: true } : {}),
  };
  const daemonIdentity: DaemonIdentity = {
    daemon_id: hostId,
    host_descriptor: {
      name: hostname() || 'Computer',
      kind: 'remote',
    },
  };
  const running = await runDaemon({
    socketPath: paths.socketPath,
    pidFilePath: paths.pidFilePath,
    logDir: paths.logDir,
    runtimePidRootDir: paths.runtimePidRootDir,
    agentHomeRootDir: paths.agentHomeRootDir,
    // The state root stays the GLOBAL home
    // root even for a custom data root: its live consumer is the per-computer
    // proxy-config coordinate, which is global by design.
    silverhandStateRootDir: defaultSilverhandStateRoot(),
    proxyConfigPath: paths.proxyConfigPath,
    hostConfig,
    daemonIdentity,
    daemonIdentityPath: null,
    installSignals: true,
    writeStdoutHandshake: false,
    alsoStderr: parsed.options.alsoStderr,
    supervisorFactory: (deps) => new Supervisor(deps),
  });
  logDaemon(running.logger, {
    action: 'connect_ready',
    pid: process.pid,
    host_id: hostId,
    data_root: dataRoot,
    log_dir: paths.logDir,
  });
  return null;
}

export async function resolveConnectHostId(input: {
  serverUrl: string;
  connectToken: string;
}): Promise<string> {
  const url = new URL(apiPaths.hostConnectTokenResolve, input.serverUrl);
  const response = await fetch(url, {
    method: 'GET',
    headers: {
      Authorization: `Bearer ${input.connectToken}`,
      Accept: 'application/json',
    },
  });
  if (!response.ok) {
    throw new Error(
      `server rejected connection command (${response.status} ${response.statusText})`,
    );
  }
  const body = (await response.json()) as unknown;
  if (
    !body ||
    typeof body !== 'object' ||
    typeof (body as { host_id?: unknown }).host_id !== 'string' ||
    !(body as { host_id: string }).host_id
  ) {
    throw new Error('server returned an invalid connect-token resolve response');
  }
  return (body as { host_id: string }).host_id;
}

export interface StopCommandOptions {
  dataRoot: string | null;
}

export type StopParseResult =
  | { ok: true; options: StopCommandOptions }
  | { ok: false; exitCode: number; message: string };

export function parseStopArgv(argv: readonly string[]): StopParseResult {
  let dataRoot: string | null = null;
  let sawDataRoot = false;

  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    switch (arg) {
      case '-h':
      case '--help':
        return { ok: false, exitCode: 0, message: stopUsage() };
      case CONNECT_ARG_DATA_ROOT:
        sawDataRoot = true;
        dataRoot = argv[++i] ?? null;
        break;
      default:
        return {
          ok: false,
          exitCode: 1,
          message: `unknown stop argument: ${arg}\n\n${stopUsage()}`,
        };
    }
  }

  if (sawDataRoot && dataRoot === null) {
    return { ok: false, exitCode: 1, message: '--data-root requires a value' };
  }
  if (dataRoot !== null && !dataRoot.startsWith('/')) {
    return { ok: false, exitCode: 1, message: '--data-root must be absolute' };
  }
  return { ok: true, options: { dataRoot } };
}

export interface StopCommandDeps {
  listCandidateDataRoots: () => string[];
  runPs: (pid: number) => Promise<string>;
  signalPid: (pid: number, signal: NodeJS.Signals | 0) => 'alive' | 'gone' | 'eperm';
  waitForExit: (pid: number, timeoutMs: number) => Promise<'gone' | 'still_alive'>;
}

export async function runStopCommand(
  argv: readonly string[],
  deps: Partial<StopCommandDeps> = {},
): Promise<number> {
  const parsed = parseStopArgv(argv);
  if (!parsed.ok) {
    const stream = parsed.exitCode === 0 ? process.stdout : process.stderr;
    stream.write(`silverhand-daemon stop: ${parsed.message}\n`);
    return parsed.exitCode;
  }
  const stopDeps = defaultStopCommandDeps(deps);
  let dataRoot = parsed.options.dataRoot;
  if (dataRoot === null) {
    const discovered = await discoverStopTarget(stopDeps);
    if (discovered.status === 'none') {
      process.stdout.write('No Silverhand daemon is running.\n');
      return 0;
    }
    if (discovered.status === 'ambiguous') {
      process.stderr.write(
        [
          'silverhand-daemon stop: multiple Silverhand daemons are running; pass --data-root.',
          ...discovered.targets.map((root) => `  ${formatStopCommand(root)}`),
          '',
        ].join('\n'),
      );
      return 1;
    }
    dataRoot = discovered.dataRoot;
  }
  const paths = stopRuntimePaths(dataRoot);
  const pid = readPidFile(paths.pidFilePath);
  if (pid === null) {
    process.stdout.write(`No Silverhand daemon is running for data root: ${dataRoot}\n`);
    return 0;
  }

  const live = stopDeps.signalPid(pid, 0);
  if (live === 'gone') {
    unlinkPidFileIfPresent(paths.pidFilePath);
    process.stdout.write(`No Silverhand daemon is running for data root: ${dataRoot}\n`);
    return 0;
  }
  if (live === 'eperm') {
    process.stderr.write(`silverhand-daemon stop: pid ${pid} exists but cannot be signaled\n`);
    return 1;
  }

  const cmdline = await stopDeps.runPs(pid);
  if (!isExpectedConnectDaemonCmdline(cmdline, dataRoot)) {
    process.stderr.write(
      `silverhand-daemon stop: refusing to stop pid ${pid}; pid file does not match a Silverhand daemon for ${dataRoot}\n`,
    );
    return 1;
  }

  const signaled = stopDeps.signalPid(pid, 'SIGTERM');
  if (signaled === 'gone') {
    unlinkPidFileIfPresent(paths.pidFilePath);
    process.stdout.write(`Silverhand daemon stopped (pid ${pid})\n`);
    return 0;
  }
  if (signaled === 'eperm') {
    process.stderr.write(`silverhand-daemon stop: pid ${pid} exists but cannot be signaled\n`);
    return 1;
  }

  const stopped = await stopDeps.waitForExit(pid, STOP_GRACE_MS);
  if (stopped === 'gone') {
    process.stdout.write(`Silverhand daemon stopped (pid ${pid})\n`);
    return 0;
  }
  process.stderr.write(
    `silverhand-daemon stop: pid ${pid} did not exit after SIGTERM; logs: ${paths.logDir}\n`,
  );
  return 1;
}

interface ConnectRuntimePaths {
  socketPath: string;
  pidFilePath: string;
  logDir: string;
  launcherLogPath: string;
  /** Where transient run files live; created before the daemon starts. */
  runStateDir: string;
  runtimePidRootDir: string;
  agentHomeRootDir: string;
  proxyConfigPath: string;
}

/**
 * #task:1608 R2-3 — the DEFAULT connect profile shape, keyed by the origin
 * fact (no user `--data-root`), never by path sniffing. The slot keeps
 * only durable state; transient run files and logs are app-scoped
 * (`resolveConnectProfilePaths`).
 */
export function defaultConnectRuntimePaths(
  computerId: string,
  silverhandStateRoot = defaultSilverhandStateRoot(),
): ConnectRuntimePaths {
  const profile = resolveConnectProfilePaths(silverhandStateRoot, computerId);
  return {
    socketPath: profile.run.socket,
    pidFilePath: profile.run.pidFile,
    logDir: profile.logs,
    launcherLogPath: join(profile.logs, CONNECT_LAUNCHER_LOG),
    runStateDir: profile.run.root,
    runtimePidRootDir: profile.run.runtimePids,
    agentHomeRootDir: profile.slot.agents,
    proxyConfigPath: profile.slot.proxyConfig,
  };
}

/** An EXPLICIT `--data-root`: the historical flat self-contained shape
 *  (never redirected, never writing back to ~/.silverhand — #task:1596). */
export function explicitConnectRuntimePaths(dataRoot: string): ConnectRuntimePaths {
  const computer = resolveComputerDataPaths(dataRoot);
  return {
    socketPath: computer.run.socket,
    pidFilePath: computer.run.pidFile,
    logDir: computer.logs,
    launcherLogPath: join(computer.logs, CONNECT_LAUNCHER_LOG),
    runStateDir: computer.run.root,
    runtimePidRootDir: computer.run.runtimePids,
    agentHomeRootDir: computer.agents,
    proxyConfigPath: computer.proxyConfig,
  };
}

/**
 * stop/discovery mapping for a data root. `<app-root>/computers/` is a
 * RESERVED namespace — `connect` REJECTS an explicit `--data-root` inside
 * it — so a data root that is a direct child of the computers root can
 * only be a daemon-managed default profile. Membership here is an
 * ENFORCED INVARIANT consumed at stop time, not a shape guess; everything
 * else is an explicit root with the flat shape.
 */
export function stopRuntimePaths(
  dataRoot: string,
  silverhandStateRoot = defaultSilverhandStateRoot(),
): ConnectRuntimePaths {
  const resolved = resolve(dataRoot);
  return dirname(resolved) === resolve(computersRootDir(silverhandStateRoot))
    ? defaultConnectRuntimePaths(basename(resolved), silverhandStateRoot)
    : explicitConnectRuntimePaths(dataRoot);
}

async function runConnectDaemonMode(input: {
  options: ConnectCommandOptions;
  dataRoot: string;
  /** The parent's origin fact, forwarded to the child explicitly. */
  defaultProfile: boolean;
  paths: ConnectRuntimePaths;
  env: NodeJS.ProcessEnv;
}): Promise<number> {
  const existing = await readExistingConnectDaemon(input.paths.pidFilePath, input.dataRoot);
  switch (existing.status) {
    case 'matching':
      writeDaemonStartedHint({
        verb: 'already running',
        pid: existing.pid,
        dataRoot: input.dataRoot,
        logDir: input.paths.logDir,
      });
      return 0;
    case 'live_mismatch':
      process.stderr.write(
        `silverhand-daemon connect: refusing to start; ${input.paths.pidFilePath} points at live pid ${existing.pid} that is not this Silverhand daemon instance\n`,
      );
      return 1;
    case 'eperm':
      process.stderr.write(
        `silverhand-daemon connect: refusing to start; pid ${existing.pid} exists but cannot be inspected\n`,
      );
      return 1;
    case 'none':
      break;
  }

  const entryPath = process.argv[1];
  if (!entryPath) {
    process.stderr.write('silverhand-daemon connect: cannot locate daemon entrypoint\n');
    return 1;
  }

  mkdirSync(input.paths.logDir, { recursive: true });
  // This is the latest launch's fallback output, not a second daemon log archive.
  // The running daemon owns its daily files; retain raw startup/crash errors here.
  const logFd = openSync(input.paths.launcherLogPath, 'w', 0o600);
  let child: ChildProcess;
  try {
    child = spawn(
      process.execPath,
      [
        entryPath,
        CONNECT_SUBCOMMAND,
        CONNECT_ARG_FOREGROUND,
        CONNECT_ARG_SERVER_URL,
        input.options.serverUrl,
        CONNECT_ARG_CONNECT_TOKEN_ENV,
        CONNECT_TOKEN_ENV,
        CONNECT_ARG_DATA_ROOT,
        input.dataRoot,
        // #task:1608 R2-3 — the child inherits the origin FACT; it never
        // re-derives default/explicit from the path shape. The pinned
        // --data-root stays for stop's cmdline identification.
        ...(input.defaultProfile ? [CONNECT_ARG_DATA_ROOT_ORIGIN, 'default'] : []),
        CONNECT_ARG_QUIET,
      ],
      {
        detached: true,
        env: { ...input.env, [CONNECT_TOKEN_ENV]: input.options.connectToken },
        stdio: ['ignore', logFd, logFd],
      },
    );
  } catch (err) {
    closeSync(logFd);
    process.stderr.write(
      `silverhand-daemon connect: failed to start daemon: ${(err as Error).message}\n`,
    );
    return 1;
  }
  closeSync(logFd);
  child.unref();

  if (!child.pid) {
    process.stderr.write(
      'silverhand-daemon connect: failed to start daemon: child pid unavailable\n',
    );
    return 1;
  }

  const ready = await waitForDetachedDaemonReady(child, {
    pidFilePath: input.paths.pidFilePath,
    socketPath: input.paths.socketPath,
    timeoutMs: CONNECT_READY_TIMEOUT_MS,
  });
  if (ready.status === 'ready') {
    writeDaemonStartedHint({
      verb: 'started',
      pid: child.pid,
      dataRoot: input.dataRoot,
      logDir: input.paths.logDir,
    });
    return 0;
  }
  if (ready.status === 'exited') {
    process.stderr.write(
      `silverhand-daemon connect: daemon exited before it was ready (code=${ready.code ?? 'null'}, signal=${ready.signal ?? 'null'}). Logs: ${input.paths.launcherLogPath}\n`,
    );
    return 1;
  }
  process.stderr.write(
    `silverhand-daemon connect: daemon did not become ready within ${CONNECT_READY_TIMEOUT_MS}ms. Logs: ${input.paths.launcherLogPath}\n`,
  );
  return 1;
}

function writeDaemonStartedHint(input: {
  verb: 'started' | 'already running';
  pid: number;
  dataRoot: string;
  logDir: string;
}): void {
  process.stdout.write(`Silverhand daemon ${input.verb} (pid ${input.pid})\n`);
  process.stdout.write(`Logs: ${input.logDir}\n`);
  process.stdout.write(`Stop: ${formatStopCommand(input.dataRoot)}\n`);
}

export function formatStopCommand(dataRoot: string): string {
  return [
    'npx',
    DEFAULT_DAEMON_NPM_SPEC,
    STOP_SUBCOMMAND,
    CONNECT_ARG_DATA_ROOT,
    shellQuoteConnectArg(dataRoot),
  ].join(' ');
}

type ExistingConnectDaemon =
  | { status: 'none' }
  | { status: 'matching'; pid: number }
  | { status: 'live_mismatch'; pid: number }
  | { status: 'eperm'; pid: number };

async function readExistingConnectDaemon(
  pidFilePath: string,
  dataRoot: string,
): Promise<ExistingConnectDaemon> {
  const pid = readPidFile(pidFilePath);
  if (pid === null) return { status: 'none' };
  const state = signalPid(pid, 0);
  if (state === 'gone') return { status: 'none' };
  if (state === 'eperm') return { status: 'eperm', pid };
  const cmdline = await defaultRunPs(pid);
  if (!isExpectedConnectDaemonCmdline(cmdline, dataRoot)) {
    return { status: 'live_mismatch', pid };
  }
  return { status: 'matching', pid };
}

function readPidFile(pidFilePath: string): number | null {
  try {
    const raw = readFileSync(pidFilePath, 'utf8').trim();
    const pid = Number.parseInt(raw, 10);
    return Number.isInteger(pid) && pid > 0 ? pid : null;
  } catch {
    return null;
  }
}

function unlinkPidFileIfPresent(pidFilePath: string): void {
  try {
    unlinkSync(pidFilePath);
  } catch {
    /* missing/concurrent unlink is fine */
  }
}

function signalPid(pid: number, signal: NodeJS.Signals | 0): 'alive' | 'gone' | 'eperm' {
  try {
    process.kill(pid, signal);
    return 'alive';
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    if (code === 'EPERM') return 'eperm';
    return 'gone';
  }
}

async function defaultRunPs(pid: number): Promise<string> {
  try {
    const { stdout } = await execFileAsync('ps', ['-ww', '-o', 'command=', '-p', String(pid)]);
    return stdout.toString().trim();
  } catch {
    return '';
  }
}

function defaultStopCommandDeps(overrides: Partial<StopCommandDeps>): StopCommandDeps {
  return {
    listCandidateDataRoots: overrides.listCandidateDataRoots ?? listDefaultConnectDataRoots,
    runPs: overrides.runPs ?? defaultRunPs,
    signalPid: overrides.signalPid ?? signalPid,
    waitForExit: overrides.waitForExit ?? waitForExit,
  };
}

export type StopTargetDiscovery =
  | { status: 'none' }
  | { status: 'found'; dataRoot: string; pid: number }
  | { status: 'ambiguous'; targets: string[] };

export async function discoverStopTarget(deps: StopCommandDeps): Promise<StopTargetDiscovery> {
  const liveTargets: Array<{ dataRoot: string; pid: number }> = [];
  for (const dataRoot of deps.listCandidateDataRoots()) {
    const pidFilePath = stopRuntimePaths(dataRoot).pidFilePath;
    const pid = readPidFile(pidFilePath);
    if (pid === null) continue;
    const state = deps.signalPid(pid, 0);
    if (state === 'gone') {
      unlinkPidFileIfPresent(pidFilePath);
      continue;
    }
    if (state === 'eperm') continue;
    const cmdline = await deps.runPs(pid);
    if (!isExpectedConnectDaemonCmdline(cmdline, dataRoot)) continue;
    liveTargets.push({ dataRoot, pid });
  }
  if (liveTargets.length === 0) return { status: 'none' };
  if (liveTargets.length === 1) {
    return { status: 'found', dataRoot: liveTargets[0]!.dataRoot, pid: liveTargets[0]!.pid };
  }
  return { status: 'ambiguous', targets: liveTargets.map((t) => t.dataRoot) };
}

function listDefaultConnectDataRoots(): string[] {
  const computersRoot = computersRootDir(defaultSilverhandStateRoot());
  try {
    return readdirSync(computersRoot, { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .map((entry) => join(computersRoot, entry.name));
  } catch {
    return [];
  }
}

function isExpectedConnectDaemonCmdline(cmdline: string, dataRoot: string): boolean {
  return (
    cmdlineHasToken(cmdline, CONNECT_SUBCOMMAND) &&
    (cmdlineHasToken(cmdline, CONNECT_ARG_FOREGROUND) ||
      cmdlineHasToken(cmdline, CONNECT_ARG_NO_DAEMON)) &&
    cmdlineHasArgPair(cmdline, CONNECT_ARG_DATA_ROOT, dataRoot)
  );
}

function cmdlineHasToken(cmdline: string, token: string): boolean {
  return cmdline.split(/\s+/).includes(token);
}

function cmdlineHasArgPair(cmdline: string, flag: string, value: string): boolean {
  return (
    cmdline.includes(`${flag} ${value}`) ||
    cmdline.includes(`${flag} ${shellQuoteConnectArg(value)}`)
  );
}

type DetachedReadyResult =
  | { status: 'ready' }
  | { status: 'exited'; code: number | null; signal: NodeJS.Signals | null }
  | { status: 'timeout' };

async function waitForDetachedDaemonReady(
  child: ChildProcess,
  opts: { pidFilePath: string; socketPath: string; timeoutMs: number },
): Promise<DetachedReadyResult> {
  let exit: Extract<DetachedReadyResult, { status: 'exited' }> | null = null;
  child.once('exit', (code, signal) => {
    exit = { status: 'exited', code, signal };
  });

  const deadline = Date.now() + opts.timeoutMs;
  while (Date.now() < deadline) {
    if (exit) return exit;
    const pid = readPidFile(opts.pidFilePath);
    if (pid === child.pid && (await canConnectUnixSocket(opts.socketPath))) {
      return { status: 'ready' };
    }
    await sleep(100);
  }
  return exit ?? { status: 'timeout' };
}

async function canConnectUnixSocket(socketPath: string): Promise<boolean> {
  return new Promise((resolve) => {
    const sock = new Socket();
    let settled = false;
    const settle = (ok: boolean): void => {
      if (settled) return;
      settled = true;
      try {
        sock.destroy();
      } catch {
        /* already closed */
      }
      resolve(ok);
    };
    const timer = setTimeout(() => settle(false), 100);
    timer.unref?.();
    sock.once('connect', () => {
      clearTimeout(timer);
      settle(true);
    });
    sock.once('error', () => {
      clearTimeout(timer);
      settle(false);
    });
    sock.connect(socketPath);
  });
}

async function waitForExit(pid: number, timeoutMs: number): Promise<'gone' | 'still_alive'> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (signalPid(pid, 0) === 'gone') return 'gone';
    await sleep(100);
  }
  return signalPid(pid, 0) === 'gone' ? 'gone' : 'still_alive';
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function connectUsage(): string {
  return [
    'Usage:',
    '  silverhand-daemon connect --server-url <url> --connect-token <token> [--data-root <abs>] [--foreground]',
    '',
    'Options:',
    '  --server-url       Silverhand server URL.',
    '  --connect-token    Connection credential shown once by the app.',
    '  --data-root        Optional absolute root for daemon runtime files.',
    '  --foreground       Run in the current terminal instead of daemon mode.',
    '  --no-daemon        Alias for --foreground.',
    '  --quiet            Do not mirror foreground daemon logs to stderr.',
  ].join('\n');
}

function stopUsage(): string {
  return [
    'Usage:',
    '  silverhand-daemon stop [--data-root <abs>]',
    '',
    'Options:',
    '  --data-root        Absolute root for the daemon instance to stop. Omit only when one instance is running.',
  ].join('\n');
}
