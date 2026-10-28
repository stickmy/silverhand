#!/usr/bin/env node
/**
 * `silverhand-daemon` process entry. Thinnest possible argv shim
 * around `runDaemon`. Spawned by Electron main (and `dev:headless`)
 * before the backend boots; can also be invoked directly by an
 * operator for diagnostics.
 *
 * Argv contract:
 *
 *   --socket-path <abs>   (required) where to bind the Unix socket
 *   --pid-file   <abs>    (required) where to write the pid lock file
 *   --log-dir    <abs>    (required) where to write daemon.YYYY-MM-DD.log
 *   --host-config <abs>   optional daemon outbound WS config
 *   --local-outbound-socket-path <abs>
 *                         optional; where to bind the local outbound
 *                         frame server (embedded backend dials it for
 *                         the D5.a R4-unified outbound envelope). When
 *                         omitted the daemon does not start the local
 *                         outbound server.
 *   --runtime-pid-root-dir <abs>
 *                         optional; root for runtime child pidfiles.
 *                         Desktop passes a SILVERHAND_DIR-scoped path
 *                         so two app instances cannot reap each other's
 *                         runtime children.
 *   --computers-root-dir <abs>
 *                         optional (#task:1608); the per-computer durable
 *                         slots root (`<app-root>/computers`). The daemon
 *                         freezes the `computers/<computer_id>` slot per
 *                         accepted binding generation.
 *   --proxy-config-path <abs>
 *                         optional; daemon-local runtime proxy config.
 *                         Proxy is host-local truth, never server state.
 *   --control-plane-server-url-path <abs>
 *                         optional; host-local file whose content is the
 *                         control-plane origin injected into runtime CLI env.
 *                         Desktop uses this for embedded loopback boots.
 *   --daemon-identity-path <abs>
 *                         optional; persistent daemon identity file.
 *                         Desktop passes the resolver's
 *                         `localComputer.run.identity`; omitted → the
 *                         bare-daemon default under `~/.silverhand/daemon`.
 *   --stderr              optional; mirror log lines to stderr too
 *
 * Stdout:
 *   normal daemon: one NDJSON line on successful bind:
 *     {"event":"ready","socket_path":"...","daemon_pid":...,...}
 *   followed by nothing — stdout is reserved for the handshake.
 *
 * Stderr:
 *   parse errors before bind; nothing else unless `--stderr` is set.
 *
 * Exit codes:
 *   0  clean shutdown via SIGINT/SIGTERM
 *   1  argv parse failure / pre-bind error
 *   2  another daemon already owns the pid file
 *
 * Why all paths are required and absolute: the spawn helper builds
 * them deterministically and tests inject them. We don't apply
 * defaults in the entry — that would let a misconfigured operator
 * silently write the pid file under `process.cwd()` and rebuild every
 * dev run.
 */
import { runDaemon } from './run.js';
import { Supervisor } from './supervisor/index.js';
import { runConnectCommand, runStopCommand } from './connect-command.js';
import { enableNodeSystemCa } from '../node-system-ca.js';

enableNodeSystemCa();

interface ParsedArgv {
  socketPath: string;
  pidFilePath: string;
  logDir: string;
  hostConfigPath: string | null;
  localOutboundSocketPath: string | null;
  runtimePidRootDir: string | null;
  computersRootDir: string | null;
  silverhandStateRootDir: string | null;
  daemonIdentityPath: string | null;
  proxyConfigPath: string | null;
  controlPlaneServerUrlPath: string | null;
  alsoStderr: boolean;
}

function parseArgv(argv: readonly string[]): ParsedArgv | { error: string } {
  let socketPath: string | null = null;
  let pidFilePath: string | null = null;
  let logDir: string | null = null;
  let hostConfigPath: string | null = null;
  let localOutboundSocketPath: string | null = null;
  let runtimePidRootDir: string | null = null;
  let computersRootDir: string | null = null;
  let silverhandStateRootDir: string | null = null;
  let daemonIdentityPath: string | null = null;
  let proxyConfigPath: string | null = null;
  let controlPlaneServerUrlPath: string | null = null;
  let alsoStderr = false;
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    switch (arg) {
      case '--socket-path':
        socketPath = argv[++i] ?? null;
        break;
      case '--pid-file':
        pidFilePath = argv[++i] ?? null;
        break;
      case '--log-dir':
        logDir = argv[++i] ?? null;
        break;
      case '--host-config':
        hostConfigPath = argv[++i] ?? null;
        break;
      case '--local-outbound-socket-path':
        localOutboundSocketPath = argv[++i] ?? null;
        break;
      case '--runtime-pid-root-dir':
        runtimePidRootDir = argv[++i] ?? null;
        break;
      case '--computers-root-dir':
        computersRootDir = argv[++i] ?? null;
        break;
      case '--silverhand-state-root-dir':
        silverhandStateRootDir = argv[++i] ?? null;
        break;
      case '--daemon-identity-path':
        daemonIdentityPath = argv[++i] ?? null;
        break;
      case '--proxy-config-path':
        proxyConfigPath = argv[++i] ?? null;
        break;
      case '--control-plane-server-url-path':
        controlPlaneServerUrlPath = argv[++i] ?? null;
        break;
      case '--stderr':
        alsoStderr = true;
        break;
      default:
        return { error: `unknown argument: ${arg}` };
    }
  }
  if (!socketPath) return { error: '--socket-path is required' };
  if (!pidFilePath) return { error: '--pid-file is required' };
  if (!logDir) return { error: '--log-dir is required' };
  if (!socketPath.startsWith('/')) return { error: '--socket-path must be absolute' };
  if (!pidFilePath.startsWith('/')) return { error: '--pid-file must be absolute' };
  if (!logDir.startsWith('/')) return { error: '--log-dir must be absolute' };
  if (hostConfigPath !== null && !hostConfigPath.startsWith('/')) {
    return { error: '--host-config must be absolute' };
  }
  if (localOutboundSocketPath !== null && !localOutboundSocketPath.startsWith('/')) {
    return { error: '--local-outbound-socket-path must be absolute' };
  }
  if (runtimePidRootDir !== null && !runtimePidRootDir.startsWith('/')) {
    return { error: '--runtime-pid-root-dir must be absolute' };
  }
  if (computersRootDir !== null && !computersRootDir.startsWith('/')) {
    return { error: '--computers-root-dir must be absolute' };
  }
  if (silverhandStateRootDir !== null && !silverhandStateRootDir.startsWith('/')) {
    return { error: '--silverhand-state-root-dir must be absolute' };
  }
  if (daemonIdentityPath !== null && !daemonIdentityPath.startsWith('/')) {
    return { error: '--daemon-identity-path must be absolute' };
  }
  if (proxyConfigPath !== null && !proxyConfigPath.startsWith('/')) {
    return { error: '--proxy-config-path must be absolute' };
  }
  if (controlPlaneServerUrlPath !== null && !controlPlaneServerUrlPath.startsWith('/')) {
    return { error: '--control-plane-server-url-path must be absolute' };
  }
  return {
    socketPath,
    pidFilePath,
    logDir,
    hostConfigPath,
    localOutboundSocketPath,
    runtimePidRootDir,
    computersRootDir,
    silverhandStateRootDir,
    daemonIdentityPath,
    proxyConfigPath,
    controlPlaneServerUrlPath,
    alsoStderr,
  };
}

async function main(): Promise<void> {
  const argv = process.argv.slice(2);
  if (argv[0] === 'connect') {
    const exitCode = await runConnectCommand(argv.slice(1));
    if (exitCode !== null) process.exit(exitCode);
    return;
  }
  if (argv[0] === 'stop') {
    const exitCode = await runStopCommand(argv.slice(1));
    process.exit(exitCode);
  }
  const parsed = parseArgv(argv);
  if ('error' in parsed) {
    process.stderr.write(`silverhand-daemon: ${parsed.error}\n`);
    process.exit(1);
  }
  try {
    await runDaemon({
      socketPath: parsed.socketPath,
      pidFilePath: parsed.pidFilePath,
      logDir: parsed.logDir,
      hostConfigPath: parsed.hostConfigPath,
      localOutboundSocketPath: parsed.localOutboundSocketPath,
      runtimePidRootDir: parsed.runtimePidRootDir ?? undefined,
      computersRootDir: parsed.computersRootDir ?? undefined,
      silverhandStateRootDir: parsed.silverhandStateRootDir ?? undefined,
      daemonIdentityPath: parsed.daemonIdentityPath ?? undefined,
      proxyConfigPath: parsed.proxyConfigPath ?? undefined,
      controlPlaneServerUrlPath: parsed.controlPlaneServerUrlPath ?? undefined,
      installSignals: true,
      writeStdoutHandshake: true,
      alsoStderr: parsed.alsoStderr,
      // Production wire: Supervisor owns spawn/deliver/stop and the
      // per-(conv, agent) AgentRunState lifecycle. Without this factory
      // `runDaemon` boots a PR-1 surface (ping/status/echo) only, and
      // every `runtime_*` intent returns `internal_error: runtime_*
      // intent reached dispatcher with no supervisor wired` — the
      // exact daemon-mode boot bug Cindy diagnosed in `MlwnvOfs-L5Nv_x2`.
      // The test helper supervisor validates protocol wiring without a
      // real CLI child; production wires Supervisor.
      supervisorFactory: (deps) => new Supervisor(deps),
    });
  } catch (err) {
    const e = err as NodeJS.ErrnoException;
    process.stderr.write(`silverhand-daemon: ${e.message}\n`);
    process.exit(e.code === 'EALREADY' ? 2 : 1);
  }
  // We deliberately do NOT call process.stdin.resume() here — the
  // shutdown handler installed by runDaemon takes care of keeping the
  // process alive (the listening socket has open handles); when it
  // tears down via process.exit() we want a clean exit, not an event-
  // loop wait.
}

void main();
