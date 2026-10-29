import type {
  LocalHostProxyConfig as DesktopHostProxyConfig,
  LocalHostProxyConfigResult as DesktopHostProxyConfigResult,
  LocalHostMutationResult as DesktopHostMutationResult,
  ProfileSaveWithTestResult,
  LocalHostPartialApplyRetryResult as PartialApplyRetryResult,
} from "@silverhand/client";
import type {
  ClientAuthState as DesktopAuthState,
  ClientAuthResult as DesktopAuthResult,
  PasswordRecoveryCall,
  PasswordRecoveryResult,
  BackendProxyRequest,
  BackendProxyResponse,
  BackendUploadRequest,
} from "@silverhand/client";
import { SessionGeneration } from "@silverhand/client";
import { classifyMeFetchOutcome, type SessionValidationResult } from "@silverhand/client";
import { desktopDaemonLauncher } from "./local-host/daemon-launcher";
import {
  app,
  BrowserWindow,
  Menu,
  session as electronSession,
  shell,
  type MenuItemConstructorOptions,
} from "electron";
import { execFileSync, spawn as spawnChildProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { rm as fsRm } from "node:fs/promises";
import { homedir } from "node:os";
import { delimiter, join, resolve } from "node:path";
import {
  AuthSessionResponseSchema,
  CLIENT_INFO_HEADER,
  EVENT_WS_SESSION_COOKIE,
  HOST_PROXY_CONFIG_OFFLINE_ERROR_CODE,
  HostClaimOrCreateResponseSchema,
  HostProxyConfigResponseSchema,
  HostProxyConfigSchema,
  HostRevokeResponseSchema,
  HostRotateTokenEmbeddedResponseSchema,
  X_SILVERHAND_LEGACY_USER_ID_HEADER,
  X_SILVERHAND_PRINCIPAL_HEADER,
  apiPaths,
  authPaths,
  classifyDesktopServerApiCompatibility,
  classifyServerUrl,
  type DesktopApiAdvertisement,
  DESKTOP_API_EPOCH_HEADER,
  DESKTOP_SERVER_API_EPOCH,
  MINIMUM_SERVER_API_EPOCH,
  readAllowInsecureRemoteFlag,
  readForceEmbeddedFlag,
  redactServerConnectionProfile,
  type AuthLoginInput,
  type AuthSignupInput,
  type HostDto,
  type HostProxyConfig,
  type ServerConnectionProfile,
  type ServerConnectionProfileView,
  type ServerInfo,
  type UserDto,
} from "@silverhand/shared";
import { createFileLogger, type FileLogger } from "@silverhand/shared/node-file-logger";
import { startDesktopEmbeddedBackend } from "./embedded/embedded-launcher.js";
import type {
  EmbeddedRustBackendHandle,
  EmbeddedRustBackendTerminalReason,
} from "./embedded/embedded-rust-backend.js";
import {
  persistEmbeddedBackend,
  invalidateEmbeddedRuntime,
} from "./embedded/embedded-backend-publication.js";
import { EmbeddedBackendOwner } from "./embedded/embedded-backend-owner.js";
const embeddedBackendOwner = new EmbeddedBackendOwner();
import {
  DESKTOP_ALLOW_INSECURE_REMOTE,
  DESKTOP_REMOTE_ONLY,
  PACKAGED_DEFAULT_SERVER_URL,
} from "../build-flags.js";
import type { LocalHostWsAttachment } from "@silverhand/client";
import { describeLocalHostWsAttachment } from "./local-host/local-host-ws-attachment.js";
import {
  enableNodeSystemCa,
  HostClient,
  OUTBOUND_PROTOCOL_VERSION,
  spawnDaemonChild,
  type DaemonHandle,
} from "@silverhand/host";
import { describeStartupFailure } from "./shell/startup-failure.js";
import { showStartupFailureWindow } from "./shell/startup-failure-window.js";
import {
  electronUserDataPath,
  shouldRedirectElectronUserData,
  loadOrCreateServerConfig,
  silverhandDir,
  writeServerConfig,
  invalidateEmbeddedServerUrl,
  desktopAppPaths,
} from "./shell/config.js";
import { resolveEffectiveBootProfile } from "./shell/desktop-build-config.js";
import { desktopClientInfoHeaderValue } from "./client/client-info.js";
import { createRuntimeUpgradeMonitor } from "./client/runtime-upgrade-monitor.js";
import { resolveRendererEntry } from "./shell/renderer-entry.js";
import { createGuardedIpcRegistrar } from "./ipc/guarded-ipc.js";
import { waitForDaemonGeneration } from "./local-host/daemon-generation-attach.js";
import { applyFatalExitToClaim } from "./local-host/local-host-failure.js";
import { executeHydrate, type HydrateOutcome } from "./client/auth-hydrate.js";
import {
  buildHostClaimInput,
  loadOrCreateInstallationId,
  readDaemonHostConfig,
  removeDaemonHostConfig,
  applyRotateSideEffects,
  applyRevokeSideEffects,
  assertApiProxyPath,
  writeDaemonHostConfig,
  type LocalHostClaimStatus,
} from "./local-host/local-host-claim.js";
import { type ExpectedAttachWindow } from "./local-host/outbound-attach.js";
import {
  performOutboundReload,
  bindAttachmentAccessor,
  type OutboundReloadOutcome,
} from "./local-host/outbound-reload.js";
import {
  applyServerProfile,
  derivePartialApplyState,
  reconcilePartialApplyAtBoot,
  type PartialApplyState,
  type ApplyServerProfileDeps,
  type ServerProfileApplyOutcome,
} from "./local-host/server-profile-apply.js";
import { gracefullyDetachOutbound } from "./local-host/outbound-detach.js";
import {
  clearServerConnectionProfile,
  readServerConnectionProfile,
  readServerConnectionProfileViewOrNull,
  writeServerConnectionProfile,
} from "./client/server-connection-profile.js";
import {
  clearPersistedDesktopSession,
  loadPersistedDesktopSessionToken,
  persistDesktopSessionToken,
} from "./client/desktop-session-storage.js";
import {
  extractViewerUserFromProxyResponse,
  shouldClearPasswordSession,
} from "./client/profile-session-sync.js";
import {
  decideBackendBootMode,
  executeBackendStartup,
  planBackendStartup,
  ProfileUnusableError,
  RemoteOnlyServerProfileRequiredError,
} from "./shell/backend-boot-mode.js";
import {
  buildRemoteBackendHandle,
  type RemoteBackendHandle,
} from "./client/remote-backend-handle.js";
import { probeServerInfo, type ProbeServerInfoResult } from "./client/server-connection-test.js";
import { fetchServerInfoWithRetry } from "./client/fetch-server-info.js";
import { registerEventWsDiagnosticIpcHandler } from "./ipc/event-ws-diagnostics.js";
import { registerSelectionExportIpcHandlers } from "./ipc/selection-export.js";
import { ContextualCloseCoordinator, type CloseTarget } from "./shell/contextual-close.js";
import {
  classifyOutboundProtocolCompatibility,
  type OutboundProtocolCompatibility,
} from "./local-host/outbound-protocol-compatibility.js";
import {
  configureRendererBackendNetwork,
  fetchBackendWithNetworkPolicy,
} from "./client/renderer-backend-network.js";

function backendFetch(input: string | URL, init?: RequestInit): Promise<Response> {
  const headers = new Headers(init?.headers);
  headers.set(DESKTOP_API_EPOCH_HEADER, String(DESKTOP_SERVER_API_EPOCH));
  // Observability input: fail-open telemetry; null just omits the header.
  if (!headers.has(CLIENT_INFO_HEADER)) {
    const value = desktopClientInfoHeaderValue(desktopAppPaths().installationId, logger);
    if (value) headers.set(CLIENT_INFO_HEADER, value);
  }
  return fetchBackendWithNetworkPolicy(fetch, input, { ...init, headers });
}
import { registerSurfaceIpcHandlers } from "./ipc/surface-ipc.js";
import { createAppUpdateController } from "./update/app-update-engine.js";
import {
  APP_UPDATE_SHOW_DIALOG_CHANNEL,
  APP_UPDATE_STATE_CHANNEL,
  registerAppUpdateIpcHandlers,
} from "./update/app-update-ipc.js";
import { nativeCopy, setNativeLocale } from "./shell/native-copy.js";

const nodeSystemCaActivation = enableNodeSystemCa();

type BackendHandle =
  | { readonly kind: "embedded"; readonly running: EmbeddedRustBackendHandle }
  | RemoteBackendHandle;

let backend: BackendHandle | null = null;
let daemon: DaemonHandle | null = null;
let hostClient: HostClient | null = null;

if (shouldRedirectElectronUserData()) {
  app.setPath("userData", electronUserDataPath());
}
let logger: FileLogger | null = null;
let mainWindow: BrowserWindow | null = null;
const clientSessionGeneration = new SessionGeneration();
let switchingServer = false;
let userSessionToken: string | null = null;
let userSessionUser: UserDto | null = null;

const rendererEntry = resolveRendererEntry({
  devServerUrl: process.env["ELECTRON_RENDERER_URL"],
  mainDirname: __dirname,
});

const guardedIpc = createGuardedIpcRegistrar({
  resolveRendererWebContents: () => mainWindow?.webContents ?? null,
  entry: rendererEntry,
  log: {
    warn: (obj, msg) => logger?.warn(obj, msg),
  },
});

const runtimeUpgradeMonitor = createRuntimeUpgradeMonitor({
  probe: async () => (await fetchServerInfo()).desktop_api,
  notify: () => {
    mainWindow?.webContents.send("epoch:update-required");
  },
  log: {
    info: (obj, msg) => logger?.info(obj, msg),
    warn: (obj, msg) => logger?.warn(obj, msg),
  },
});
let localHostClaim: LocalHostClaimStatus = { status: "unclaimed", hostId: null };
let rendererBackendResolvedProxy: string | null = null;

let partialApplyState: PartialApplyState = { kind: "fully_applied" };

// Null when this build has no update channel; every update entry then stays hidden.
const appUpdate = createAppUpdateController({
  info: (m) => logger?.info({ component: "app-update" }, String(m)),
  warn: (m) => logger?.warn({ component: "app-update" }, String(m)),
  error: (m) => logger?.error({ component: "app-update" }, String(m)),
});

/** Set `SILVERHAND_HEADLESS=1` to boot the backend without a window.
 *  Useful for curl-driven smoke tests and integration scripts — see
 *  `pnpm dev:headless`. The process keeps the backend alive until
 *  SIGINT / SIGTERM. */
const HEADLESS = process.env.SILVERHAND_HEADLESS === "1";

function desktopAllowsInsecureRemote(): boolean {
  return DESKTOP_ALLOW_INSECURE_REMOTE || readAllowInsecureRemoteFlag(process.env);
}

interface RemoteDebuggingConfig {
  enabled: boolean;
  port: number | null;
  reason?: string;
  envName?: string;
}

const remoteDebugging = configureRemoteDebugging();

/**
 * Expose Electron's renderer targets over local-only CDP.
 *
 * Dev mode keeps the historical default of 9222 when the renderer dev server
 * is active. Packaged builds are opt-in only: set
 * `SILVERHAND_REMOTE_DEBUG_PORT=<port>` to open the endpoint for profiling.
 */
function configureRemoteDebugging(): RemoteDebuggingConfig {
  const explicit = readRemoteDebugPortEnv();
  const hasRendererDevServer = Boolean(process.env["ELECTRON_RENDERER_URL"]);
  if (!explicit && app.isPackaged) {
    return { enabled: false, port: null, reason: "packaged_env_absent" };
  }
  if (!explicit && !hasRendererDevServer) {
    return { enabled: false, port: null, reason: "no_renderer_dev_server" };
  }

  const raw = explicit?.value ?? "9222";
  const value = raw.trim();
  if (/^(0|false|off|disabled)$/i.test(value)) {
    return {
      enabled: false,
      port: null,
      reason: "disabled_by_env",
      envName: explicit?.name,
    };
  }

  const port = Number(value);
  if (!Number.isInteger(port) || port < 1 || port > 65_535) {
    console.warn(
      `[main] invalid ${explicit?.name ?? "default CDP port"}=${JSON.stringify(raw)}; CDP remote debugging disabled`,
    );
    return {
      enabled: false,
      port: null,
      reason: "invalid_port",
      envName: explicit?.name,
    };
  }

  app.commandLine.appendSwitch("remote-debugging-address", "127.0.0.1");
  app.commandLine.appendSwitch("remote-debugging-port", String(port));
  process.stderr.write(`[silverhand] remote debugging on 127.0.0.1:${port}\n`);
  return { enabled: true, port, envName: explicit?.name };
}

function readRemoteDebugPortEnv(): { name: string; value: string } | null {
  const candidates = [
    "SILVERHAND_REMOTE_DEBUG_PORT",
    "SILVERHAND_CDP_PORT",
    "SILVERHAND_REMOTE_DEBUGGING_PORT",
  ] as const;
  for (const name of candidates) {
    const value = process.env[name];
    if (value !== undefined) return { name, value };
  }
  return null;
}

function appendExistingPathEntries(
  rawPath: string,
  candidates: readonly string[],
): { path: string; added: string[] } {
  const parts = rawPath.split(delimiter).filter(Boolean);
  const seen = new Set(parts);
  const added = candidates.filter((candidate) => {
    if (seen.has(candidate)) return false;
    if (!existsSync(candidate)) return false;
    seen.add(candidate);
    return true;
  });
  return { path: [...added, ...parts].join(delimiter), added };
}

/**
 * macOS: when launched from Finder/Dock, GUI apps inherit launchd's
 * minimal default PATH (`/usr/bin:/bin:/usr/sbin:/sbin`) — they do NOT
 * see the PATH from `~/.zshrc` / `config.fish`. That means binaries the
 * user installed via brew (`/opt/homebrew/bin`), nvm, or `npm i -g`
 * (`~/.npm-global/bin`) are invisible, and `detectRuntimes` reports
 * "claude / codex not installed" even though they're on the user's
 * shell PATH.
 *
 * Fix: ask the user's login shell what its PATH actually is, and copy
 * that into `process.env.PATH`. Same trick Slack / Cursor / VS Code use
 * (cf. the `fix-path` package — inlined here to avoid a new dep).
 *
 * Only runs when packaged + macOS; in `pnpm dev` we already inherit
 * the terminal's PATH so this would be wasted work (and could conflict
 * if the dev shell has a different PATH than the user's login shell).
 *
 * The login-shell probe is necessary but not sufficient: users can have
 * version-manager shims installed in common per-user locations without
 * exporting those directories from their shell rc files. We prepend only
 * existing fallback directories so packaged daemon children can find
 * runtime shims like `~/.local/bin/claude` without inventing PATH entries.
 */
function patchPathFromLoginShell(): { ok: boolean; detail: string } {
  if (!app.isPackaged || process.platform !== "darwin") {
    return { ok: false, detail: "skipped (not packaged darwin)" };
  }
  const userShell = process.env.SHELL || "/bin/zsh";
  try {
    // `-i -l -c`: interactive + login so rc/profile files run. Use
    // `printenv` (a real binary, not a shell builtin) instead of
    // `echo $PATH` so the probe stays shell-agnostic — bash/zsh use
    // `${PATH}` while fish needs `{$PATH}`, and we don't want to
    // branch per shell. Sentinel markers protect us from rc-file
    // banner output (oh-my-zsh, fish greetings, etc.).
    const out = execFileSync(
      userShell,
      ["-ilc", "echo __PATH_BEGIN__; printenv PATH; echo __PATH_END__"],
      { encoding: "utf-8", timeout: 5_000, stdio: ["ignore", "pipe", "pipe"] },
    );
    const m = out.match(/__PATH_BEGIN__\s*\n([\s\S]*?)\n\s*__PATH_END__/);
    if (!m || !m[1]) {
      return {
        ok: false,
        detail: `no marker match (shell=${userShell}, raw=${JSON.stringify(out.slice(0, 200))})`,
      };
    }
    const patched = appendExistingPathEntries(m[1].trim(), [
      join(homedir(), ".local", "bin"),
      join(homedir(), ".npm-global", "bin"),
      join(homedir(), ".bun", "bin"),
      "/opt/homebrew/bin",
      "/opt/homebrew/sbin",
      "/usr/local/bin",
    ]);
    process.env.PATH = patched.path;
    return {
      ok: true,
      detail: `shell=${userShell}, len=${process.env.PATH.length}, added=${patched.added.join(":") || "none"}`,
    };
  } catch (err) {
    return {
      ok: false,
      detail: `shell=${userShell}, threw=${(err as Error).message}`,
    };
  }
}

function resolveDaemonLauncher() {
  return desktopDaemonLauncher({
    isPackaged: app.isPackaged,
    appPath: app.getAppPath(),
    resourcesPath: process.resourcesPath,
    execPath: process.execPath,
  });
}

interface DaemonPaths {
  socketPath: string;
  pidFilePath: string;
  logDir: string;
  runtimePidRootDir: string;
  computersRootDir: string;
  silverhandStateRootDir: string;
  daemonIdentityPath: string;
  hostConfigPath: string;
  controlPlaneServerUrlPath: string;
}

function daemonPaths(): DaemonPaths {
  // Every path comes from the app-data resolver. run/ stays wholly disposable
  // and contains no durable agent data.
  const paths = desktopAppPaths();
  return {
    socketPath: paths.run.socket,
    pidFilePath: paths.run.pidFile,
    logDir: paths.logs,
    runtimePidRootDir: paths.run.runtimePids,
    computersRootDir: paths.computersRoot,
    silverhandStateRootDir: paths.appRoot,
    daemonIdentityPath: paths.run.identity,
    hostConfigPath: paths.desktop.computerBinding,
    controlPlaneServerUrlPath: paths.run.controlPlaneServerUrl,
  };
}

function embeddedBootstrapPath(): string {
  return desktopAppPaths().embeddedServer.bootstrap;
}

function requireBackendUrl(): string {
  if (!backend) throw new Error("backend not ready");
  return backend.kind === "embedded" ? backend.running.url : backend.url;
}

function requireEmbeddedBackend(reason: string): EmbeddedRustBackendHandle {
  if (!backend) {
    throw new Error(`backend not ready (caller: ${reason})`);
  }
  if (backend.kind !== "embedded") {
    throw new Error(`embedded backend required but desktop is in remote mode (caller: ${reason})`);
  }
  return backend.running;
}

function hasLocalRegistryObserver(): boolean {
  return false;
}

function backendUrl(path: string): string {
  return new URL(path, requireBackendUrl()).toString();
}

function eventWsCookieUrl(): string {
  return new URL(apiPaths.eventWebSocket, requireBackendUrl()).toString();
}

function backendOrigin(): string {
  return new URL(requireBackendUrl()).origin;
}

async function syncEventWsSessionCookie(token: string | null): Promise<void> {
  if (!backend) return;
  const url = eventWsCookieUrl();
  await electronSession.defaultSession.cookies.remove(url, EVENT_WS_SESSION_COOKIE).catch((err) => {
    logger?.warn({ err }, "failed to clear event websocket session cookie");
  });
  if (!token) return;
  await electronSession.defaultSession.cookies
    .set({
      url,
      name: EVENT_WS_SESSION_COOKIE,
      value: token,
      httpOnly: true,
      sameSite: "strict",
      secure: new URL(requireBackendUrl()).protocol === "https:",
      path: apiPaths.eventWebSocket,
    })
    .catch((err) => {
      logger?.warn({ err }, "failed to set event websocket session cookie");
    });
}

function installBackendEventWsOriginOverride(): void {
  if (!backend) return;

  const expectedOrigin = backendOrigin();
  const expectedUrl = new URL(apiPaths.eventWebSocket, requireBackendUrl());
  expectedUrl.protocol = expectedUrl.protocol === "https:" ? "wss:" : "ws:";

  const clientInfoHeader = desktopClientInfoHeaderValue(desktopAppPaths().installationId, logger);

  electronSession.defaultSession.webRequest.onBeforeSendHeaders(
    { urls: [`${expectedUrl.protocol}//${expectedUrl.host}${apiPaths.eventWebSocket}`] },
    (details, callback) => {
      const url = new URL(details.url);
      if (url.origin === expectedUrl.origin && url.pathname === apiPaths.eventWebSocket) {
        delete details.requestHeaders.origin;
        delete details.requestHeaders.cookie;
        details.requestHeaders.Origin = expectedOrigin;
        if (userSessionToken) {
          details.requestHeaders.Cookie = `${EVENT_WS_SESSION_COOKIE}=${encodeURIComponent(userSessionToken)}`;
        } else {
          delete details.requestHeaders.Cookie;
        }
        if (clientInfoHeader) {
          details.requestHeaders[CLIENT_INFO_HEADER] = clientInfoHeader;
        }
        details.requestHeaders[DESKTOP_API_EPOCH_HEADER] = String(DESKTOP_SERVER_API_EPOCH);
      }
      callback({ requestHeaders: details.requestHeaders });
    },
  );
  electronSession.defaultSession.webRequest.onErrorOccurred(
    { urls: [`${expectedUrl.protocol}//${expectedUrl.host}${apiPaths.eventWebSocket}`] },
    (details) => {
      logger?.warn(
        {
          category: "event_ws.network",
          error: details.error,
          resource_type: details.resourceType,
          backend_origin: expectedOrigin,
          resolved_proxy: rendererBackendResolvedProxy,
        },
        "event websocket network request failed",
      );
    },
  );
}

function clearPersistedUserSession(): void {
  clearPersistedDesktopSession({ sessionPath: desktopAppPaths().desktop.session, logger });
}

function loadPersistedUserSessionToken(): string | null {
  return loadPersistedDesktopSessionToken({
    sessionPath: desktopAppPaths().desktop.session,
    logger,
  });
}

function persistUserSessionToken(token: string): void {
  persistDesktopSessionToken({ sessionPath: desktopAppPaths().desktop.session, token, logger });
}

async function storeUserSession(token: string, user: UserDto): Promise<void> {
  userSessionToken = token;
  userSessionUser = user;
  persistUserSessionToken(token);
  await syncEventWsSessionCookie(token);
}

/**
 * Reseat the cached session user from any proxied response that carries an
 * authoritative viewer DTO — local profile writes and fresh `GET /api/me`
 * reads alike. `auth:getState`'s `user` is a session snapshot, not the
 * profile authority, so this is what keeps it from going stale after an
 * edit made on this machine OR on another one (#task:1702).
 */
function reseatCachedUserSessionFromProxy(
  request: { path: string; method?: string },
  response: BackendProxyResponse,
  requestSessionToken: string,
): void {
  // A response belongs to the session that issued it, not whichever session is
  // current when the network finishes (logout/account switch may have happened).
  if (!userSessionToken || !userSessionUser || userSessionToken !== requestSessionToken) return;
  const user = extractViewerUserFromProxyResponse(request, response);
  if (!user) return;
  if (userSessionUser && userSessionUser.id !== user.id) {
    logger?.warn(
      {
        cached_user_id: userSessionUser.id,
        response_user_id: user.id,
        path: request.path,
        method: request.method ?? "GET",
      },
      "viewer profile response returned a different user; keeping cached auth session user",
    );
    return;
  }
  userSessionUser = user;
}

// Human-session expiry does not revoke the independent daemon binding.
// Only an explicit user logout opts into removing the local binding file.
async function clearUserSession(preserveHostBinding = true): Promise<void> {
  clientSessionGeneration.invalidate();
  userSessionToken = null;
  userSessionUser = null;

  localHostClaim = { status: "unclaimed", hostId: null };

  activeAttachWatchdog?.abort();
  activeAttachWatchdog = null;

  clearPersistedUserSession();

  if (!preserveHostBinding) {
    removeDaemonHostConfig(desktopAppPaths().desktop.computerBinding);
  }
  await syncEventWsSessionCookie(null);
}

async function clearUserSessionAfterUnauthorized(source: string): Promise<void> {
  if (!userSessionToken && !userSessionUser) return;
  logger?.warn({ source }, "desktop auth session rejected; clearing local session");
  await clearUserSession();
}

function parseJson(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return null;
  }
}

async function fetchServerInfo(): Promise<ServerInfo> {
  return fetchServerInfoWithRetry({
    url: backendUrl(apiPaths.serverInfo),
  });
}

interface RemoteCompatibilitySnapshot {
  outbound: OutboundProtocolCompatibility;
  desktopApi: DesktopApiAdvertisement | undefined;
}

async function fetchRemoteCompatibilitySnapshot(): Promise<RemoteCompatibilitySnapshot> {
  try {
    const info = await fetchServerInfoWithRetry({
      url: backendUrl(apiPaths.serverInfo),
      fetcher: (input) => backendFetch(input),
      // BackendInfo is a periodic capability probe, not a boot gate. One
      // attempt keeps it cheap; a transient failure becomes `unknown` and the
      // next poll/focus refetch retries without blocking the renderer.
      retryDelaysMs: [],
    });
    return {
      outbound: classifyOutboundProtocolCompatibility({
        appProtocolVersion: OUTBOUND_PROTOCOL_VERSION,
        serverProtocolVersion: info.outbound_protocol_version,
      }),
      desktopApi: info.desktop_api,
    };
  } catch (err) {
    logger?.debug({ err }, "remote compatibility snapshot unavailable");
    return {
      outbound: classifyOutboundProtocolCompatibility({
        appProtocolVersion: OUTBOUND_PROTOCOL_VERSION,
        serverProtocolVersion: undefined,
      }),
      desktopApi: undefined,
    };
  }
}

async function validateUserSessionToken(token: string): Promise<SessionValidationResult> {
  let res: Response;
  try {
    res = await backendFetch(backendUrl(apiPaths.me), {
      headers: { Authorization: `Bearer ${token}` },
    });
  } catch (err) {
    return classifyMeFetchOutcome({ kind: "throw", error: err });
  }
  // Only attempt to read the body when the status is 2xx — the
  // classifier doesn't need a body for non-2xx arms (it routes
  // on status alone), and pulling bytes off a 5xx response wastes
  // time on the failure path.
  let body: unknown = null;
  if (res.ok) {
    body = await res.json().catch(() => null);
  }
  return classifyMeFetchOutcome({
    kind: "response",
    status: res.status,
    body,
  });
}

let hydrateInFlight: Promise<HydrateOutcome> | null = null;

async function runHydrate(): Promise<HydrateOutcome> {
  if (hydrateInFlight) return hydrateInFlight;
  const isCurrent = clientSessionGeneration.capture();
  const flight = executeHydrate({
    loadToken: loadPersistedUserSessionToken,
    validate: validateUserSessionToken,
    store: async (token, user) => {
      if (isCurrent()) await storeUserSession(token, user);
    },
    clear: async () => {
      if (isCurrent()) await clearUserSession();
    },
    claim: async () => {
      if (isCurrent()) await claimLocalHostForUserSession();
    },
    log: {
      info: (obj, msg) => logger?.info(obj, msg),
      warn: (obj, msg) => logger?.warn(obj, msg),
    },
  }).finally(() => {
    if (hydrateInFlight === flight) hydrateInFlight = null;
  });
  hydrateInFlight = flight;
  return flight;
}

async function hydrateDesktopSession(): Promise<boolean> {
  const outcome = await runHydrate();
  return outcome === "authenticated";
}

async function desktopAuthState(): Promise<DesktopAuthState> {
  if (switchingServer)
    return {
      status: "backend_unreachable",
      server_url: requireBackendUrl(),
      message: "Changing server connection",
    };
  let info: ServerInfo;
  try {
    info = await fetchServerInfo();
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    const url = backendUrl("");
    logger?.warn(
      { err, server_url: url },
      "desktopAuthState: backend unreachable (server_info fetch failed)",
    );
    return {
      status: "backend_unreachable",
      server_url: url,
      message,
    };
  }

  {
    const advertised = info.desktop_api;
    const serverEpoch = advertised?.current_epoch ?? 0;
    const compatibility = classifyDesktopServerApiCompatibility({
      clientEpoch: DESKTOP_SERVER_API_EPOCH,
      minimumClientEpoch: advertised?.minimum_client_epoch ?? 0,
      serverEpoch,
      minimumServerEpoch: MINIMUM_SERVER_API_EPOCH,
    });
    if (compatibility === "client_too_old") {
      return {
        status: "app_update_required",
        server_api_epoch: serverEpoch,
        minimum_client_epoch: advertised?.minimum_client_epoch ?? 0,
      };
    }
    if (compatibility === "server_too_old") {
      return {
        status: "server_update_required",
        server_api_epoch: serverEpoch,
        client_api_epoch: DESKTOP_SERVER_API_EPOCH,
      };
    }
    // Compatible pair observed: re-arm the runtime monitor (#task:1763) so a
    // later server bump can promote again (covers server rollback / app
    // update cycles).
    runtimeUpgradeMonitor.reset();
  }

  if ((!userSessionToken || !userSessionUser) && loadPersistedUserSessionToken()) {
    await runHydrate();
  }
  if (userSessionToken && userSessionUser) {
    return { status: "authenticated", user: userSessionUser };
  }

  if (loadPersistedUserSessionToken()) {
    const url = backendUrl("");
    return {
      status: "backend_unreachable",
      server_url: url,
      message: "session validation pending — /api/me did not return a verdict",
    };
  }
  return { status: "unauthenticated" };
}

/** Anonymous recovery call on the current server; a late answer after a server
 *  or session change is dropped. Never stores a session. */
async function passwordRecovery(input: PasswordRecoveryCall): Promise<PasswordRecoveryResult> {
  const changed = { status: 409, bodyText: '{"error":"session_changed"}' };
  if (switchingServer || backend?.kind !== "remote") return changed;
  const isCurrent = clientSessionGeneration.capture();
  const path =
    input.action === "request"
      ? authPaths.passwordRecoveryRequest
      : authPaths.passwordRecoveryReset;
  try {
    const res = await backendFetch(backendUrl(path), {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: input.body,
    });
    const bodyText = await res.text();
    return isCurrent() ? { status: res.status, bodyText } : changed;
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { status: 0, bodyText: JSON.stringify({ error: "backend_unreachable", message }) };
  }
}

async function postAuthSession(
  path: typeof authPaths.signup | typeof authPaths.login,
  input: AuthSignupInput | AuthLoginInput,
): Promise<DesktopAuthResult> {
  if (switchingServer)
    return {
      ok: false,
      status: 409,
      statusText: "Conflict",
      bodyText: "Server connection is changing",
    };
  const isCurrent = clientSessionGeneration.capture();
  let res: Response;
  try {
    res = await backendFetch(backendUrl(path), {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(input),
    });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return {
      ok: false,
      status: 0,
      statusText: "Network Error",
      bodyText: JSON.stringify({ error: "backend_unreachable", message }),
    };
  }
  const text = await res.text();
  if (!res.ok) {
    return {
      ok: false,
      status: res.status,
      statusText: res.statusText,
      bodyText: text,
    };
  }
  const parsed = AuthSessionResponseSchema.safeParse(parseJson(text));
  if (!parsed.success) {
    return {
      ok: false,
      status: 502,
      statusText: "Bad Gateway",
      bodyText: JSON.stringify({ error: "invalid auth response" }),
    };
  }
  if (!isCurrent())
    return { ok: false, status: 409, statusText: "Conflict", bodyText: "Session changed" };
  await storeUserSession(parsed.data.user_token, parsed.data.user);
  if (isCurrent()) await claimLocalHostForUserSession();
  if (!isCurrent()) {
    return { ok: false, status: 409, statusText: "Conflict", bodyText: "Session changed" };
  }
  return { ok: true, user: parsed.data.user };
}

async function bootstrapEmbeddedUserSession(): Promise<boolean> {
  if (backend?.kind !== "embedded") return false;

  const path = embeddedBootstrapPath();

  if (!existsSync(path)) return false;

  const token = readFileSync(path, "utf8").trim();
  if (!token) return false;

  const res = await backendFetch(backendUrl(authPaths.embeddedBootstrap), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ token }),
  });
  const text = await res.text();

  if (!res.ok) {
    logger?.warn(
      { status: res.status, statusText: res.statusText },
      "embedded bootstrap session request failed",
    );
    return false;
  }

  const parsed = AuthSessionResponseSchema.safeParse(parseJson(text));
  if (!parsed.success) {
    logger?.warn(
      { err: parsed.error.message },
      "embedded bootstrap returned invalid auth response",
    );
    return false;
  }
  await storeUserSession(parsed.data.user_token, parsed.data.user);
  await claimLocalHostForUserSession();

  return true;
}

async function claimLocalHostForUserSession(): Promise<void> {
  const isCurrent = clientSessionGeneration.capture();
  const token = userSessionToken;
  if (!token || !backend) {
    localHostClaim = { status: "unclaimed", hostId: null };
    return;
  }

  const backendUrlSnapshot = requireBackendUrl();
  const paths = desktopAppPaths();
  const existingConfig = readDaemonHostConfig(paths.desktop.computerBinding);
  try {
    const installationId = loadOrCreateInstallationId(paths.installationId);
    const claimInput = buildHostClaimInput({
      installationId,
      existingConfig,
    });
    const res = await backendFetch(backendUrl(apiPaths.hostClaimOrCreate), {
      method: "POST",
      headers: {
        Authorization: `Bearer ${token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify(claimInput),
    });
    const text = await res.text();
    if (!isCurrent()) return;

    if (res.status === 401) {
      await clearUserSessionAfterUnauthorized("local host claim");
      return;
    }
    if (res.status === 409) {
      localHostClaim = {
        status: "revoked",
        hostId: existingConfig?.host_id ?? null,
      };
      logger?.warn("local host claim failed: host revoked");
      return;
    }
    if (!res.ok) {
      throw new Error(`local host claim failed: ${res.status} ${res.statusText}`);
    }

    const parsed = HostClaimOrCreateResponseSchema.safeParse(parseJson(text));
    if (!parsed.success) {
      throw new Error(`invalid local host claim response: ${parsed.error.message}`);
    }

    const hostToken =
      parsed.data.host_token ??
      (existingConfig?.host_id === parsed.data.host_id ? existingConfig.host_token : null);
    if (!hostToken) {
      throw new Error("local host claim response omitted host_token without reusable config");
    }

    const localHostId = parsed.data.host_id;
    const attachWindow = captureAttachWindow(localHostId);

    const profileRead = readServerConnectionProfile({
      profilePath: paths.desktop.serverProfile,
      logger: logger ?? undefined,
    });
    const profileVersion = profileRead.ok ? profileRead.profile.version : undefined;
    const allowInsecureRemote = desktopAllowsInsecureRemote();

    writeDaemonHostConfig(paths.desktop.computerBinding, {
      server_url: backendUrlSnapshot,
      host_id: localHostId,
      host_token: hostToken,
      ...(allowInsecureRemote ? { allow_insecure_remote: true } : {}),
      ...(profileVersion !== undefined ? { profile_version: profileVersion } : {}),
    });
    localHostClaim = { status: "claimed", hostId: localHostId };

    // Fire-and-forget: the claim itself has succeeded; the outbound
    // attach is a parallel control-plane action. UI keeps showing
    // `connecting` until the watchdog observes a fresh attachment;
    // attach failures stay diagnostic and do NOT mutate the
    // user-facing `local_host_status` enum.
    void triggerLocalHostOutboundAttach({
      localHostId,
      configPath: paths.desktop.computerBinding,
      attachWindow,
    });
  } catch (err) {
    if (!isCurrent()) return;
    localHostClaim = {
      status: "error",
      hostId: existingConfig?.host_id ?? localHostClaim.hostId,
    };
    logger?.warn({ err }, "local host claim failed");
  }
}

async function logoutUserSession(): Promise<{ ok: true }> {
  const token = userSessionToken;
  const logoutUrl = token ? backendUrl(authPaths.logout) : null;
  await clearUserSession(false);
  if (token && logoutUrl) {
    await backendFetch(logoutUrl, {
      method: "POST",
      headers: { Authorization: `Bearer ${token}` },
    }).catch((err) => {
      logger?.warn({ err }, "desktop auth logout revoke failed");
    });
  }
  return { ok: true };
}

function snapshotLocalHostInfo(): {
  local_host_id: string | null;
  local_host_status: LocalHostClaimStatus["status"];
  local_host_ws_attachment: LocalHostWsAttachment;
  local_host_server_url: string | null;
} {
  return {
    local_host_id: localHostClaim.status === "claimed" ? localHostClaim.hostId : null,
    local_host_status: localHostClaim.status,
    local_host_ws_attachment: describeLocalHostWsAttachment({
      hasLocalRegistryObserver: hasLocalRegistryObserver(),
      attachedAt: null,
    }),
    local_host_server_url:
      readDaemonHostConfig(desktopAppPaths().desktop.computerBinding)?.server_url ?? null,
  };
}

function captureAttachWindow(localHostId: string): ExpectedAttachWindow {
  return {
    expectedHostId: localHostId,
    previousConnectionId: null,
    expectAttachSince: Date.now(),
  };
}

/** Aborts the active attach watchdog, if any, so logout / app quit
 *  doesn't leak a polling timer past its useful life. */
let activeAttachWatchdog: { abort(): void } | null = null;

async function triggerLocalHostOutboundAttach(input: {
  localHostId: string;
  configPath: string;
  attachWindow: ExpectedAttachWindow;
}): Promise<void> {
  const client = hostClient;
  const isCurrent = clientSessionGeneration.capture();
  if (!client || !backend) {
    logger?.warn(
      { localHostId: input.localHostId },
      "D3.a host_outbound_attach skipped — host client or backend not ready",
    );
    return;
  }

  let result;
  try {
    result = await client.hostOutboundAttach({ config_path: input.configPath });
  } catch (err) {
    logger?.warn(
      { err, localHostId: input.localHostId },
      "D3.a host_outbound_attach IPC failed (transport)",
    );
    return;
  }

  if (!isCurrent()) return;

  if (!result.attached) {
    logger?.warn(
      {
        localHostId: input.localHostId,
        reason: result.reason,
        message: result.message,
      },
      "D3.a host_outbound_attach refused by daemon",
    );
    return;
  }

  logger?.info(
    {
      localHostId: input.localHostId,
      action: result.action,
    },
    "host_outbound_attach ack",
  );

  // Cancel any previously-running watchdog (rotate-token / re-login
  // can fire while a prior one is still polling).
  activeAttachWatchdog?.abort();
  if (!hasLocalRegistryObserver()) {
    return;
  }
}

/** Rotate the long-lived credential for the desktop's CURRENT local
 *  host. Only allowed for `hostId === daemon-config.host_id` (the
 *  source-of-truth for what identity this desktop is currently using).
 *  Mid-flight token bytes never cross the renderer IPC boundary. */
async function rotateHostToken(hostId: string): Promise<DesktopHostMutationResult> {
  const token = userSessionToken;
  const generationIsCurrent = clientSessionGeneration.capture();
  const serverUrl = backend ? requireBackendUrl() : null;
  const isCurrent = () =>
    generationIsCurrent() &&
    userSessionToken === token &&
    backend !== null &&
    requireBackendUrl() === serverUrl;
  if (!token || !backend || switchingServer) {
    return { ok: false, code: "unauthorized" };
  }
  if (!hostId) {
    return { ok: false, code: "host_not_found", message: "missing host id" };
  }
  const daemonConfigPath = desktopAppPaths().desktop.computerBinding;
  const existingConfig = readDaemonHostConfig(daemonConfigPath);

  if (!existingConfig || existingConfig.host_id !== hostId) {
    return {
      ok: false,
      code: "not_current_host",
      message: "rotate is only supported for the host this desktop is currently bound to",
    };
  }

  let res: Response;
  try {
    res = await backendFetch(backendUrl(apiPaths.hostRotateToken(hostId)), {
      method: "POST",
      headers: { Authorization: `Bearer ${token}` },
    });
  } catch (err) {
    logger?.warn({ err }, "host rotate-token request failed");
    return { ok: false, code: "network_error" };
  }
  const text = await res.text();
  const body = parseJson(text);
  if (!isCurrent()) return { ok: false, code: "unauthorized", message: "Session changed" };
  if (res.status === 401) {
    await clearUserSessionAfterUnauthorized("host rotate");
    return { ok: false, code: "unauthorized" };
  }
  if (res.status === 404) {
    return { ok: false, code: "host_not_found" };
  }
  if (res.status === 409) {
    const code =
      typeof body === "object" && body !== null && "error_code" in body
        ? String((body as { error_code: unknown }).error_code)
        : "host_revoked";
    return {
      ok: false,
      code: code === "kind_not_supported" ? "kind_not_supported" : "host_revoked",
    };
  }
  if (!res.ok) {
    return { ok: false, code: "network_error", message: `${res.status} ${res.statusText}` };
  }

  const parsed = HostRotateTokenEmbeddedResponseSchema.safeParse(body);
  if (!parsed.success || parsed.data.host_id !== hostId) {
    return { ok: false, code: "invalid_response" };
  }

  const currentConfig = readDaemonHostConfig(daemonConfigPath);
  if (
    !currentConfig ||
    currentConfig.host_id !== existingConfig.host_id ||
    currentConfig.host_token !== existingConfig.host_token ||
    currentConfig.server_url !== existingConfig.server_url
  ) {
    return { ok: false, code: "not_current_host" };
  }

  // Token bytes land here and ONLY here. Persist to daemon-config
  // before we return; the renderer reply below has no token.
  applyRotateSideEffects({
    daemonConfigPath,
    serverUrl: serverUrl!,
    hostId: parsed.data.host_id,
    hostToken: parsed.data.host_token,
  });
  localHostClaim = { status: "claimed", hostId: parsed.data.host_id };
  return {
    ok: true,
    host: parsed.data.host,
    ...snapshotLocalHostInfo(),
  };
}

/** Server revocation is a client operation; only local binding cleanup is native. */
function reconcileRevokedLocalHost(
  input: BackendProxyRequest,
  response: BackendProxyResponse,
): void {
  if (input.method !== "POST" || response.status < 200 || response.status >= 300) return;
  const parsed = HostRevokeResponseSchema.safeParse(parseJson(response.bodyText ?? ""));
  if (!parsed.success || input.path !== apiPaths.hostRevoke(parsed.data.host.id)) return;
  const hostId = parsed.data.host.id;
  const reconcile = applyRevokeSideEffects({
    daemonConfigPath: desktopAppPaths().desktop.computerBinding,
    hostId,
  });
  if (reconcile === "cleared") {
    localHostClaim = { status: "revoked", hostId };
  }
}

function normalizeDesktopHostProxyConfig(input: unknown): DesktopHostProxyConfig | null {
  const parsed = HostProxyConfigSchema.safeParse(input);
  return parsed.success ? parsed.data : null;
}

async function getHostProxyConfig(hostId: string): Promise<DesktopHostProxyConfigResult> {
  const token = userSessionToken;
  const generationIsCurrent = clientSessionGeneration.capture();
  const serverUrl = backend ? requireBackendUrl() : null;
  const isCurrent = () =>
    generationIsCurrent() &&
    userSessionToken === token &&
    backend !== null &&
    requireBackendUrl() === serverUrl;
  if (!token || !backend || switchingServer) {
    return { ok: false, code: "unauthorized" };
  }
  if (!hostId) {
    return { ok: false, code: "host_not_found", message: "missing host id" };
  }

  let res: Response;
  try {
    res = await backendFetch(backendUrl(apiPaths.hostProxyConfig(hostId)), {
      headers: { Authorization: `Bearer ${token}` },
    });
  } catch (err) {
    logger?.warn(
      { err: err instanceof Error ? err.message : String(err), hostId },
      "host proxy config get request failed",
    );
    return { ok: false, code: "daemon_error", message: networkErrorMessage(err) };
  }

  const body = parseJson(await res.text());
  if (!isCurrent()) return { ok: false, code: "unauthorized", message: "Session changed" };
  if (res.status === 401) {
    await clearUserSessionAfterUnauthorized("host proxy config get");
    return { ok: false, code: "unauthorized" };
  }
  if (!res.ok) {
    return hostProxyConfigErrorResult(body, res.status, res.statusText);
  }

  const parsed = HostProxyConfigResponseSchema.safeParse(body);
  if (!parsed.success) {
    return {
      ok: false,
      code: "daemon_error",
      message: "invalid proxy config response",
    };
  }
  return { ok: true, config: parsed.data.config };
}

async function saveHostProxyConfig(input: {
  hostId: string;
  config: unknown;
}): Promise<DesktopHostProxyConfigResult> {
  const token = userSessionToken;
  const generationIsCurrent = clientSessionGeneration.capture();
  const serverUrl = backend ? requireBackendUrl() : null;
  const isCurrent = () =>
    generationIsCurrent() &&
    userSessionToken === token &&
    backend !== null &&
    requireBackendUrl() === serverUrl;
  if (!token || !backend || switchingServer) {
    return { ok: false, code: "unauthorized" };
  }
  if (!input.hostId) {
    return { ok: false, code: "host_not_found", message: "missing host id" };
  }
  const config = normalizeDesktopHostProxyConfig(input.config);
  if (!config) return { ok: false, code: "invalid_input" };

  let res: Response;
  try {
    res = await backendFetch(backendUrl(apiPaths.hostProxyConfig(input.hostId)), {
      method: "PUT",
      headers: {
        Authorization: `Bearer ${token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify(config),
    });
  } catch (err) {
    logger?.warn(
      { err: err instanceof Error ? err.message : String(err), hostId: input.hostId },
      "host proxy config set request failed",
    );
    return { ok: false, code: "daemon_error", message: networkErrorMessage(err) };
  }

  const body = parseJson(await res.text());
  if (!isCurrent()) return { ok: false, code: "unauthorized", message: "Session changed" };
  if (res.status === 401) {
    await clearUserSessionAfterUnauthorized("host proxy config set");
    return { ok: false, code: "unauthorized" };
  }
  if (!res.ok) {
    return hostProxyConfigErrorResult(body, res.status, res.statusText);
  }

  const parsed = HostProxyConfigResponseSchema.safeParse(body);
  if (!parsed.success) {
    return {
      ok: false,
      code: "daemon_error",
      message: "invalid proxy config response",
    };
  }
  return { ok: true, config: parsed.data.config };
}

function hostProxyConfigErrorResult(
  body: unknown,
  status: number,
  statusText: string,
): DesktopHostProxyConfigResult {
  const error = body && typeof body === "object" ? (body as Record<string, unknown>) : {};
  const code = typeof error.error_code === "string" ? error.error_code : null;
  const message = typeof error.error === "string" ? error.error : `${status} ${statusText}`;
  switch (code) {
    case "host_not_found":
      return { ok: false, code: "host_not_found", message };
    case "host_not_active":
      return { ok: false, code: "host_not_active", message };
    case "host_revoked":
      return { ok: false, code: "host_revoked", message };
    case HOST_PROXY_CONFIG_OFFLINE_ERROR_CODE:
      return { ok: false, code: HOST_PROXY_CONFIG_OFFLINE_ERROR_CODE, message };
    case "invalid_input":
      return { ok: false, code: "invalid_input", message };
    default:
      return { ok: false, code: "daemon_error", message };
  }
}

function networkErrorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

async function saveServerUrlFromSettings(rawServerUrl: string): Promise<OutboundReloadOutcome> {
  const client = hostClient;
  if (!client || !backend) {
    // Pre-backend / post-shutdown — the orchestrator would itself
    // surface this as a reload IPC transport failure mapped to
    // `not_attached`, but we short-circuit here so the renderer
    // sees the actionable reason without the daemon-side
    // round-trip-attempt log spam.
    return {
      ok: false,
      kind: "reload_rejected",
      reason: "not_attached",
      message: "backend or host client not ready",
    };
  }

  const daemonConfigPath = desktopAppPaths().desktop.computerBinding;
  const controller = null;
  const allowInsecureRemote = desktopAllowsInsecureRemote();

  const outcome = await performOutboundReload(
    { serverUrl: rawServerUrl },
    {
      allowInsecureRemote,
      writeDaemonHostConfig: (config) => writeDaemonHostConfig(daemonConfigPath, config),
      hostOutboundReload: () => client.hostOutboundReload(),
      getAttachment: bindAttachmentAccessor(controller),
      remoteMode: !hasLocalRegistryObserver(),
      currentLocalHostId: () =>
        localHostClaim.status === "claimed" ? localHostClaim.hostId : null,
      currentHostToken: () => readDaemonHostConfig(daemonConfigPath)?.host_token ?? null,
      currentProfileVersion: () => readDaemonHostConfig(daemonConfigPath)?.profile_version,
    },
  );

  // On a save that actually re-attached we cancel any in-flight
  // post-login attach watchdog — the reload-side watchdog inside
  // `performOutboundReload` already observed the fresh
  // attachment, so the old poll has no work to do. Mirrors the
  // rotate-token path (which also calls
  // `triggerLocalHostOutboundAttach` to spawn a fresh watchdog
  // and aborts the previous one inside that helper).
  if (outcome.ok && outcome.kind === "reloaded") {
    activeAttachWatchdog?.abort();
    activeAttachWatchdog = null;
  }

  // Diagnostic logging — every outcome arm gets a single line so
  // operators can correlate Settings saves against subsequent
  // daemon / backend log entries.
  if (outcome.ok) {
    logger?.info(
      { kind: outcome.kind, elapsedMs: "elapsedMs" in outcome ? outcome.elapsedMs : null },
      "D3.c settings:saveServerUrl ok",
    );
  } else {
    logger?.warn(
      {
        kind: outcome.kind,
        reason: "reason" in outcome ? outcome.reason : null,
        message: "message" in outcome ? outcome.message : null,
      },
      "D3.c settings:saveServerUrl failed",
    );
  }

  return outcome;
}

function proxyHeaders(input: Record<string, string> | undefined): Headers {
  const headers = new Headers();
  const blocked = new Set([
    "authorization",
    X_SILVERHAND_PRINCIPAL_HEADER.toLowerCase(),
    X_SILVERHAND_LEGACY_USER_ID_HEADER.toLowerCase(),
    "host",
    "content-length",
    "connection",
  ]);
  for (const [name, value] of Object.entries(input ?? {})) {
    const key = name.toLowerCase();
    if (blocked.has(key)) continue;
    headers.set(name, value);
  }
  return headers;
}

function responseHeaders(headers: Headers): Record<string, string> {
  const out: Record<string, string> = {};
  headers.forEach((value, key) => {
    out[key] = value;
  });
  return out;
}

async function marshalBackendResponse(
  res: Response,
  responseType: "text" | "arrayBuffer",
): Promise<BackendProxyResponse> {
  const base = {
    status: res.status,
    statusText: res.statusText,
    headers: responseHeaders(res.headers),
  };
  if (responseType === "arrayBuffer" && res.ok) {
    return {
      ...base,
      bodyBytes: new Uint8Array(await res.arrayBuffer()),
    };
  }
  return { ...base, bodyText: await res.text() };
}

async function proxyBackendRequest(input: BackendProxyRequest): Promise<BackendProxyResponse> {
  assertApiProxyPath(input.path);
  if (!userSessionToken || switchingServer) {
    return {
      status: 401,
      statusText: "Unauthorized",
      headers: { "content-type": "application/json" },
      bodyText: JSON.stringify({ error: "unauthorized" }),
    };
  }
  const requestSessionToken = userSessionToken;
  const isCurrent = clientSessionGeneration.capture();
  const headers = proxyHeaders(input.headers);
  headers.set("Authorization", `Bearer ${requestSessionToken}`);
  if (input.bodyText !== undefined && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }

  let res: Response;
  try {
    res = await backendFetch(backendUrl(input.path), {
      method: input.method ?? "GET",
      headers,
      body: input.bodyText,
    });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return {
      status: 0,
      statusText: "Network Error",
      headers: { "content-type": "application/json" },
      bodyText: JSON.stringify({ error: "backend_unreachable", message }),
    };
  }
  const response = await marshalBackendResponse(res, input.responseType ?? "text");
  if (!isCurrent() || userSessionToken !== requestSessionToken) {
    return {
      status: 409,
      statusText: "Session Changed",
      headers: {},
      bodyText: '{"error":"session_changed"}',
    };
  }
  if (res.status === 401) {
    await clearUserSessionAfterUnauthorized("backend request");
    return response;
  }
  if (shouldClearPasswordSession(input, response, requestSessionToken, userSessionToken)) {
    await clearUserSession(true);
    return response;
  }
  if (input.method === "POST" && /^\/api\/hosts\/[^/]+\/revoke$/.test(input.path)) {
    reconcileRevokedLocalHost(input, response);
  }
  runtimeUpgradeMonitor.onRestResponse(response.status, response.bodyText);
  reseatCachedUserSessionFromProxy(
    { path: input.path, method: input.method },
    response,
    requestSessionToken,
  );
  return response;
}

async function proxyBackendUpload(input: BackendUploadRequest): Promise<BackendProxyResponse> {
  assertApiProxyPath(input.path);
  if (!userSessionToken || switchingServer) {
    return {
      status: 401,
      statusText: "Unauthorized",
      headers: { "content-type": "application/json" },
      bodyText: JSON.stringify({ error: "unauthorized" }),
    };
  }
  const requestSessionToken = userSessionToken;
  const isCurrent = clientSessionGeneration.capture();
  const form = new FormData();
  for (const field of input.fields ?? []) {
    form.append(field.name, field.value);
  }
  for (const file of input.files) {
    const bytes = Buffer.from(file.bytes);
    const blob = new Blob([bytes], {
      type: file.contentType || "application/octet-stream",
    });
    form.append(file.fieldName, blob, file.filename);
  }

  let res: Response;
  try {
    res = await backendFetch(backendUrl(input.path), {
      method: "POST",
      headers: { Authorization: `Bearer ${requestSessionToken}` },
      body: form,
    });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return {
      status: 0,
      statusText: "Network Error",
      headers: { "content-type": "application/json" },
      bodyText: JSON.stringify({ error: "backend_unreachable", message }),
    };
  }
  const response = await marshalBackendResponse(res, "text");
  if (!isCurrent() || userSessionToken !== requestSessionToken) {
    return {
      status: 409,
      statusText: "Session Changed",
      headers: {},
      bodyText: '{"error":"session_changed"}',
    };
  }
  if (res.status === 401) {
    await clearUserSessionAfterUnauthorized("backend request");
    return response;
  }
  runtimeUpgradeMonitor.onRestResponse(response.status, response.bodyText);
  reseatCachedUserSessionFromProxy(
    { path: input.path, method: "POST" },
    response,
    requestSessionToken,
  );
  return response;
}

async function bootstrap(): Promise<void> {
  try {
    const pathResult = patchPathFromLoginShell();

    logger = createFileLogger({
      dir: desktopAppPaths().logs,
      alsoConsole: !app.isPackaged,
      base: { app: "silverhand-desktop" },
    });
    logger.info(
      {
        logFile: logger.filePath,
        pathPatch: pathResult,
        nodeSystemCa: nodeSystemCaActivation,
      },
      "logger initialized",
    );

    const savedBootProfileView = readServerConnectionProfileViewOrNull({
      profilePath: desktopAppPaths().desktop.serverProfile,
      logger: logger
        ? {
            warn: (payload, msg) => logger?.warn(payload, msg),
            info: (payload, msg) => logger?.info(payload, msg),
          }
        : undefined,
    });
    const allowInsecureRemote = desktopAllowsInsecureRemote();
    const forceEmbedded = readForceEmbeddedFlag(process.env);
    const effectiveBootProfile = resolveEffectiveBootProfile({
      savedProfileView: savedBootProfileView,
      packagedDefaultServerUrl: PACKAGED_DEFAULT_SERVER_URL,
      allowInsecureRemote,
      remoteOnly: DESKTOP_REMOTE_ONLY,
    });
    if (effectiveBootProfile.warning) {
      logger.warn(
        {
          reason: effectiveBootProfile.warning.reason,
          packagedDefaultServerUrl: PACKAGED_DEFAULT_SERVER_URL,
          remoteOnly: DESKTOP_REMOTE_ONLY,
          allowInsecureRemote: DESKTOP_ALLOW_INSECURE_REMOTE,
        },
        effectiveBootProfile.warning.message,
      );
    }
    if (effectiveBootProfile.source === "packaged_default") {
      logger.info(
        {
          serverUrl: effectiveBootProfile.profileView.server_url,
          remoteOnly: DESKTOP_REMOTE_ONLY,
          allowInsecureRemote: DESKTOP_ALLOW_INSECURE_REMOTE,
        },
        "desktop packaged default server URL selected for boot",
      );
    }
    if (DESKTOP_REMOTE_ONLY && forceEmbedded) {
      logger.warn({}, "SILVERHAND_FORCE_EMBEDDED=1 ignored because this package is remote-only");
    }
    const bootDecision = decideBackendBootMode({
      profileView: effectiveBootProfile.profileView,
      forceEmbedded: DESKTOP_REMOTE_ONLY ? false : forceEmbedded,
      allowInsecureRemote,
    });

    const startupPlan = planBackendStartup(bootDecision);
    if (startupPlan.mode === "refuse") {
      logger.error(
        {
          profileUrl: startupPlan.profileUrl,
          classifyMessage: startupPlan.classifyMessage,
        },
        "profile present but classifier refused it, throwing (no silent embedded fallback)",
      );

      throw new ProfileUnusableError({
        mode: "profile_invalid",
        profileUrl: startupPlan.profileUrl,
        classifyMessage: startupPlan.classifyMessage,
      });
    }
    if (DESKTOP_REMOTE_ONLY && startupPlan.mode !== "remote") {
      logger.error(
        {
          startupPlan,
          packagedDefaultServerUrl: PACKAGED_DEFAULT_SERVER_URL,
          effectiveProfileSource: effectiveBootProfile.source,
        },
        "remote-only desktop refused embedded backend startup",
      );
      throw new RemoteOnlyServerProfileRequiredError(
        startupPlan.mode === "embedded"
          ? `Boot decision landed on embedded/${startupPlan.embeddedReason}.`
          : "Boot decision did not land on remote mode.",
      );
    }
    if (startupPlan.mode === "remote") {
      logger.info(
        {
          serverUrl: startupPlan.serverUrl,
          insecure: startupPlan.insecure,
        },
        "boot decision: remote",
      );
      if (startupPlan.insecure) {
        logger.warn(
          {
            serverUrl: startupPlan.serverUrl,
            packagedAllowInsecureRemote: DESKTOP_ALLOW_INSECURE_REMOTE,
            envAllowInsecureRemote: readAllowInsecureRemoteFlag(process.env),
          },
          "remote-mode boot admitted insecure http URL; see runbook §12 for production guidance",
        );
      }
    } else {
      logger.info(
        {
          reason: startupPlan.embeddedReason,
        },
        "boot decision: embedded",
      );
    }
    if (remoteDebugging.enabled) {
      logger.info(
        {
          port: remoteDebugging.port,
          url: `http://127.0.0.1:${remoteDebugging.port}`,
          env: remoteDebugging.envName ?? "dev-default",
        },
        "CDP remote debugging enabled",
      );
    }

    const paths = daemonPaths();
    if (startupPlan.mode === "embedded") {
      await fsRm(paths.controlPlaneServerUrlPath, { force: true });
    }
    const daemonLauncher = resolveDaemonLauncher();
    daemon = await spawnDaemonChild({
      launcher: daemonLauncher,
      socketPath: paths.socketPath,
      pidFilePath: paths.pidFilePath,
      logDir: paths.logDir,
      runtimePidRootDir: paths.runtimePidRootDir,
      computersRootDir: paths.computersRootDir,
      silverhandStateRootDir: paths.silverhandStateRootDir,
      daemonIdentityPath: paths.daemonIdentityPath,
      // 30s tolerates slower machines while retaining a finite stuck-start boundary.
      readyTimeoutMs: 30_000,
      proxyConfigPath: null,
      controlPlaneServerUrlPath:
        startupPlan.mode === "embedded" ? paths.controlPlaneServerUrlPath : null,
      localOutboundSocketPath: null,
      alsoStderr: !app.isPackaged,
    });
    logger.info(
      {
        pid: daemon.pid,
        socketPath: daemon.socketPath,
        daemonVersion: daemon.daemonVersion,
      },
      "silverhand-daemon spawned",
    );
    hostClient = new HostClient({ socketPath: paths.socketPath });
    try {
      await hostClient.connect();
    } catch (err) {
      await hostClient.close().catch(() => undefined);
      hostClient = null;
      await daemon?.kill({ gracefulMs: 1_000 }).catch(() => undefined);
      daemon = null;
      throw new Error(
        "silverhand-daemon: HostClient failed to connect to the spawned daemon. " +
          "There is no runtime fallback — daemon is the only supported runtime path. " +
          "Recovery: ensure silverhand-daemon is spawnable / its socket is reachable. " +
          `Underlying error: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
    daemon.onExit((info) => {
      logger?.error(
        { kind: info.kind, reason: info.reason, code: info.code },
        "silverhand-daemon exited fatally",
      );
      localHostClaim = applyFatalExitToClaim(localHostClaim, info);
    });

    const reattachedGenerations = new Set<string>();
    daemon.onDaemonGenerationChanged((info) => {
      if (reattachedGenerations.has(info.instanceId)) {
        logger?.debug?.(
          { instanceId: info.instanceId, attempt: info.attempt },
          "D3.b daemon generation hook: already reattached, skipping",
        );
        return;
      }
      reattachedGenerations.add(info.instanceId);
      void (async () => {
        logger?.info(
          {
            pid: info.pid,
            daemonVersion: info.daemonVersion,
            instanceId: info.instanceId,
            startedAt: info.startedAt,
            attempt: info.attempt,
          },
          "D3.b daemon generation changed (post-crash respawn)",
        );
        const claim = localHostClaim;
        if (claim.status !== "claimed" || claim.hostId === null) {
          // No active host claim yet — login flow has not run, or the
          // user is logged out. The post-login claim path will fire
          // its own `triggerLocalHostOutboundAttach` against the fresh
          // daemon when it runs.
          logger?.info(
            { instanceId: info.instanceId, status: claim.status },
            "D3.b daemon generation changed but no active local host claim; skipping re-attach",
          );
          return;
        }
        const client = hostClient;
        if (!client) {
          logger?.warn(
            { instanceId: info.instanceId },
            "daemon generation changed but hostClient is null; skipping re-attach",
          );
          return;
        }
        try {
          const ready = await waitForDaemonGeneration(client, info.instanceId);
          if (!ready.ok) {
            logger?.warn(
              {
                instanceId: info.instanceId,
                reason: ready.reason,
                err: ready.error,
              },
              "host client did not reach expected daemon generation; skipping re-attach",
            );
            return;
          }
        } catch (err) {
          logger?.warn(
            { err, instanceId: info.instanceId },
            "host client generation gate threw unexpectedly",
          );
          return;
        }
        const localHostId = claim.hostId;
        // Re-capture freshness window — the previous attachment died
        // with the old daemon, so its `previousConnectionId` is stale.
        const attachWindow = captureAttachWindow(localHostId);
        await triggerLocalHostOutboundAttach({
          localHostId,
          configPath: desktopAppPaths().desktop.computerBinding,
          attachWindow,
        });
      })().catch((err) => {
        logger?.error(
          { err, instanceId: info.instanceId },
          "D3.b daemon generation re-attach unexpectedly threw",
        );
      });
    });

    const startedBackend = await executeBackendStartup<
      EmbeddedRustBackendHandle,
      RemoteBackendHandle
    >(startupPlan, {
      loadOrCreateServerConfig: () => loadOrCreateServerConfig(),
      startEmbeddedAndPersistConfig: (config) =>
        embeddedBackendOwner.start(async () => {
          const appPaths = desktopAppPaths();
          const bootstrapTokenPath = embeddedBootstrapPath();
          const serverConfig = config as Awaited<ReturnType<typeof loadOrCreateServerConfig>>;
          const daemonNpmSpec = process.env.SILVERHAND_DAEMON_NPM_SPEC?.trim();
          const rendererOrigin = process.env.ELECTRON_RENDERER_URL
            ? new URL(process.env.ELECTRON_RENDERER_URL).origin
            : null;
          const handle = await startDesktopEmbeddedBackend(
            {
              isPackaged: app.isPackaged,
              resourcesPath: process.resourcesPath,
              appPath: app.getAppPath(),
            },
            {
              databasePath: appPaths.embeddedServer.database,
              signal: embeddedBackendOwner.signal,
              blobsRoot: appPaths.embeddedServer.blobs,
              logDir: appPaths.logs,
              bootstrapTokenPath,
              listen: { hostname: "127.0.0.1", port: 0 },
              // The Rust backend self-derives its finalized bound loopback origin into the
              // event-WS allowlist (TS parity); the launcher supplies the renderer dev
              // origin (when present) so a dev renderer's WS upgrade is allowed too.
              allowedOrigins: rendererOrigin ? [rendererOrigin] : ["http://127.0.0.1:0"],
              ...(daemonNpmSpec ? { daemonNpmSpec } : {}),
              disableSignup: false,
              apiEpoch: {
                clientEpoch: DESKTOP_SERVER_API_EPOCH,
                minimumServerEpoch: MINIMUM_SERVER_API_EPOCH,
              },
            },
            logger ?? undefined,
          ).catch(async (error) => {
            await invalidateEmbeddedServerUrl(serverConfig);
            throw error;
          });

          await persistEmbeddedBackend(
            handle,
            {
              config: () => writeServerConfig({ ...serverConfig, url: handle.url }),
              controlUrl: () =>
                writeFileSync(paths.controlPlaneServerUrlPath, `${handle.url}\n`, { mode: 0o600 }),
              invalidate: async () => {
                try {
                  await fsRm(paths.controlPlaneServerUrlPath, { force: true });
                } finally {
                  await invalidateEmbeddedServerUrl(serverConfig);
                }
              },
            },
            embeddedBackendOwner.signal,
          );

          return handle;
        }),
      buildRemoteHandle: (input) => buildRemoteBackendHandle(input),
      refuse: (refusePlan) => {
        // Throws — typed as `(plan) => never` so the union narrows
        // for the caller. Logged on the way out so a pre-bootstrap
        // crash trace carries the same diagnosis as the embedded
        // boot's WARN line above.
        logger?.error(
          {
            profileUrl: refusePlan.profileUrl,
            classifyMessage: refusePlan.classifyMessage,
          },
          "refuse arm reached, throwing ProfileUnusableError",
        );
        throw new ProfileUnusableError({
          mode: "profile_invalid",
          profileUrl: refusePlan.profileUrl,
          classifyMessage: refusePlan.classifyMessage,
        });
      },
    });
    if (embeddedBackendOwner.isStopping) {
      if (startedBackend.kind === "embedded")
        await embeddedBackendOwner.close(startedBackend.running);
      throw new Error("backend startup cancelled before main publication");
    }
    backend = startedBackend;
    if (backend.kind === "embedded") {
      const handle = backend.running;
      handle.onTerminal((reason) => {
        void handlePostPublishEmbeddedFatal(handle, reason);
      });
      if (!backend) throw new Error("embedded backend terminated during publication");
    }
    installBackendEventWsOriginOverride();
    if (!HEADLESS) {
      const hydrated = await hydrateDesktopSession();
      if (!hydrated) {
        await bootstrapEmbeddedUserSession();
      }
    }

    void runBootReconciliation();
  } catch (err) {
    console.error("[main] bootstrap failed:", err);
    logger?.error({ err }, "bootstrap failed");
    throw err;
  }
}

async function runBootReconciliation(): Promise<void> {
  try {
    const state = await reconcilePartialApplyAtBoot({
      readProfile: () =>
        readServerConnectionProfile({
          profilePath: desktopAppPaths().desktop.serverProfile,
          logger: logger ?? undefined,
        }),
      readDaemonHostConfig: () => readDaemonHostConfig(desktopAppPaths().desktop.computerBinding),
      applyProfile: (profile) => runServerProfileApply(profile),
    });
    partialApplyState = state;
    if (state.kind === "partial_apply_pending") {
      logger?.warn(
        {
          profileVersion: state.profileVersion,
          reason: state.reason,
          reloadRejectionReason: state.reloadRejectionReason,
        },
        "boot reconciliation: partial_apply_pending — Retry button surfaced",
      );
    } else if (state.kind === "unusable_profile") {
      logger?.warn(
        { reason: state.reason, detail: state.detail },
        "boot reconciliation: unusable_profile — renderer should offer reset affordance",
      );
    } else {
      logger?.info({ kind: state.kind }, "boot reconciliation: ok");
    }
  } catch (err) {
    // Reconciliation failed unexpectedly. Leave `partialApplyState`
    // at its initial `fully_applied` so the renderer doesn't surface
    // a banner with stale info; log the error so the issue is
    // visible to operators. Re-throwing here would propagate into
    // bootstrap() because of the `void` site, but `void` swallows
    // it — keeping this try/catch makes the swallow explicit.
    logger?.error(
      { err },
      "boot reconciliation threw unexpectedly — leaving partialApplyState=fully_applied",
    );
  }
}

function createWindow(): void {
  const win = new BrowserWindow({
    width: 1280,
    height: 800,
    minWidth: 800,
    minHeight: 600,
    show: false,
    // macOS: hide the native title bar so the renderer's <Titlebar/>
    // can draw across the full window width and the channel/workspace
    // info gets the prime real estate. Traffic-lights stay native (we
    // just nudge them to vertically center inside the 36px bar).
    // Win/Linux keep the native frame for now — self-drawn min/max/close
    // is deferred (see docs/app-layout.md §0).
    titleBarStyle: process.platform === "darwin" ? "hiddenInset" : undefined,
    trafficLightPosition: process.platform === "darwin" ? { x: 12, y: 11 } : undefined,
    webPreferences: {
      preload: join(__dirname, "../preload/index.js"),
      sandbox: false,
      contextIsolation: true,
      zoomFactor: 1,
    },
  });
  mainWindow = win;

  // Mouse-wheel page zoom does not use the application menu.
  win.webContents.on("zoom-changed", (event) => event.preventDefault());

  win.on("ready-to-show", () => win.show());
  if (process.platform === "darwin") {
    const sendFullScreen = () => win.webContents.send("window:full-screen", win.isFullScreen());
    win.on("enter-full-screen", sendFullScreen);
    win.on("leave-full-screen", sendFullScreen);
    win.webContents.on("did-finish-load", sendFullScreen);
  }
  win.on("closed", () => {
    if (mainWindow === win) mainWindow = null;
  });

  win.webContents.setWindowOpenHandler((details) => {
    shell.openExternal(details.url);
    return { action: "deny" };
  });

  if (rendererEntry.kind === "dev-server") {
    win.loadURL(rendererEntry.url);
  } else {
    win.loadFile(rendererEntry.filePath);
  }
}

/**
 * Build a full mac-style application menu. We can't add a single item
 * to the default menu without rebuilding it, so this template covers
 * the standard roles plus the app-specific Settings and Setup entries.
 */
function buildAppMenu(): Menu {
  const isMac = process.platform === "darwin";
  const labels = nativeCopy();

  // macOS convention: "Settings" lives in the App menu directly under
  // About, with the standard Cmd+, accelerator. Electron's
  // `{ role: 'appMenu' }` shortcut omits Settings, so we hand-build the
  // App submenu here when running on darwin.
  const appMenuItem: MenuItemConstructorOptions = isMac
    ? {
        label: app.name,
        submenu: [
          { role: "about" },
          ...(appUpdate
            ? [{ label: labels.checkForUpdates, click: () => checkForUpdatesFromMenu() }]
            : []),
          { type: "separator" },
          {
            label: labels.settings,
            accelerator: "CmdOrCtrl+,",
            click: () => openSettings(),
          },
          { type: "separator" },
          { role: "services" },
          { type: "separator" },
          { role: "hide" },
          { role: "hideOthers" },
          { role: "unhide" },
          { type: "separator" },
          { role: "quit" },
        ],
      }
    : { role: "appMenu" };

  // ⌘W is a *contextual* Close (mac only), not an unconditional "Close
  // Window", so the item is named neutrally "Close": it closes the topmost
  // in-app surface (thread / side / attachment panel) when one is open and,
  // when none is, sends the app to the background (macOS Hide) rather than
  // destroying the window. A mouse click runs the SAME contextual flow as the
  // ⌘W accelerator — a command and its shortcut must not diverge. Main only
  // intercepts here; the renderer owns the ladder and replies with the outcome
  // (`handleContextualClose`).
  //
  // This custom File menu is gated on `isMac` and only overrides the mac ⌘W
  // owner (whose default was `role: 'fileMenu'`'s `role: 'close'`). Off mac we
  // keep Electron's standard `role: 'fileMenu'` untouched — the contextual-close
  // feature is scoped to macOS ⌘W, and we don't want to strip the platform's
  // File/Quit items or claim Ctrl+W. Mirrors the `appMenuItem` split above.
  const fileMenuItem: MenuItemConstructorOptions = isMac
    ? {
        label: labels.file,
        submenu: [
          {
            label: labels.close,
            accelerator: "CmdOrCtrl+W",
            click: (_item, browserWindow) => {
              if (browserWindow instanceof BrowserWindow) {
                handleContextualClose(browserWindow);
              }
            },
          },
        ],
      }
    : { role: "fileMenu" };

  const template: MenuItemConstructorOptions[] = [
    ...(isMac ? [appMenuItem] : []),
    fileMenuItem,
    { role: "editMenu" },
    {
      label: labels.view,
      submenu: [
        { role: "reload" },
        { role: "forceReload" },
        { role: "toggleDevTools" },
        { type: "separator" },
        // Page zoom is unavailable. Previewers own content zoom without
        // changing the renderer zoom factor.
        { role: "togglefullscreen" },
      ],
    },
    { role: "windowMenu" },
  ];
  return Menu.buildFromTemplate(template);
}

/**
 * Show + focus the main window and tell the renderer to navigate to the
 * Settings page. Triggered by the App menu's "Settings" item (Cmd+,).
 * If the window was closed (mac-style background app) we recreate it so
 * the shortcut also brings the app back from the dock.
 */
function checkForUpdatesFromMenu(): void {
  if (!appUpdate) return;
  if (!mainWindow || mainWindow.isDestroyed()) createWindow();
  const win = mainWindow;
  if (!win) return;
  win.show();
  const send = (): void => {
    if (!win.isDestroyed()) win.webContents.send(APP_UPDATE_SHOW_DIALOG_CHANNEL);
  };
  // A recreated window has no renderer listener until its page loads.
  if (win.webContents.isLoading()) win.webContents.once("did-finish-load", send);
  else send();
  void appUpdate.check();
}

function openSettings(): void {
  if (!mainWindow || mainWindow.isDestroyed()) {
    createWindow();
  }
  const win = mainWindow;
  if (!win) return;
  if (win.isMinimized()) win.restore();
  win.show();
  win.focus();
  const send = (): void => {
    if (!win.isDestroyed()) win.webContents.send("navigate:settings");
  };
  if (win.webContents.isLoading()) {
    win.webContents.once("did-finish-load", send);
  } else {
    send();
  }
}

// ─── ⌘W contextual close (main = single interception owner) ──────────────
// The window is captured at press time and never re-derived via
// `getFocusedWindow()` — focus can move during the async round-trip and we
// must act on the window the user actually invoked. The coordinator's
// registry / correlation / timeout / outcome-gating contract lives in the pure
// `./contextual-close.ts` module (unit-tested there); here we only inject the
// Electron seams: `webContents.send` for the request, `app.hide()` for the
// terminal "send to background" action. The coordinator owns its timeout.

/** How long main waits for the renderer's close outcome before failing
 *  closed. Generous for a normal round-trip, short enough that a wedged
 *  renderer never leaves ⌘W feeling dead for long. */
const CLOSE_OUTCOME_TIMEOUT_MS = 250;

const contextualClose = new ContextualCloseCoordinator(
  CLOSE_OUTCOME_TIMEOUT_MS,
  // Terminal action when ⌘W found nothing in-app to close: send the whole app
  // to the background (macOS Hide). The window is preserved — never destroyed —
  // so the session (drafts, scroll) survives and a dock click / ⌘Tab returns
  // instantly. macOS-only, matching the app's target platform.
  () => app.hide(),
);

/** Wrap a BrowserWindow as the coordinator's window seam. `webContentsId`
 *  doubles as the sender credential the reply is checked against. The liveness
 *  check covers the webContents itself, not just the BrowserWindow — a
 *  destroyed/crashed webContents (with the window shell still nominally alive)
 *  is what makes `send` throw. The terminal action is app-level (Hide), so it
 *  is injected into the coordinator, not carried here per-window. */
function closeTargetFor(window: BrowserWindow): CloseTarget {
  return {
    webContentsId: window.webContents.id,
    isDestroyed: () => window.isDestroyed() || window.webContents.isDestroyed(),
    sendRequest: (reqId) => window.webContents.send("shortcut:contextual-close", reqId),
  };
}

/** Menu "Close" / ⌘W handler. Binds the request to the window that
 *  triggered it and asks its renderer to resolve the press. */
function handleContextualClose(triggeringWindow: BrowserWindow): void {
  contextualClose.begin(closeTargetFor(triggeringWindow));
}

guardedIpc.on("app:set-locale", (_event, value: unknown) => {
  if (setNativeLocale(value)) {
    Menu.setApplicationMenu(buildAppMenu());
  }
});

guardedIpc.on("shortcut:contextual-close-result", (event, reqId: unknown, outcome: unknown) => {
  if (typeof reqId !== "string") return;
  contextualClose.resolve(event.sender.id, reqId, outcome);
});

registerEventWsDiagnosticIpcHandler({ getLogger: () => logger, ipc: guardedIpc });

guardedIpc.handle("backend:info", async () => {
  if (!backend) return null;

  if (backend.kind === "embedded") {
    // Embedded ships both sides from one tree: zero skew by construction,
    // no snapshot fetch and no runtime-promotion feed needed.
    return {
      url: requireBackendUrl(),
      mode: backend.kind,
      outbound_protocol_compatibility: classifyOutboundProtocolCompatibility({
        appProtocolVersion: OUTBOUND_PROTOCOL_VERSION,
        serverProtocolVersion: OUTBOUND_PROTOCOL_VERSION,
      }),
      ...snapshotLocalHostInfo(),
    };
  }
  const snapshot = await fetchRemoteCompatibilitySnapshot();

  runtimeUpgradeMonitor.onAdvertisement(snapshot.desktopApi);
  return {
    url: requireBackendUrl(),
    mode: backend.kind,
    outbound_protocol_compatibility: snapshot.outbound,
    ...snapshotLocalHostInfo(),
  };
});

guardedIpc.handle("auth:getState", () => desktopAuthState());
guardedIpc.handle("server:info", () => fetchServerInfo());

guardedIpc.handle("auth:signup", (_event, input: AuthSignupInput) =>
  postAuthSession(authPaths.signup, input),
);

guardedIpc.handle("auth:login", (_event, input: AuthLoginInput) =>
  postAuthSession(authPaths.login, input),
);

guardedIpc.handle("auth:logout", () => logoutUserSession());

guardedIpc.handle("auth:passwordRecovery", (_event, input: PasswordRecoveryCall) =>
  passwordRecovery(input),
);

guardedIpc.handle("backend:request", (_event, input: BackendProxyRequest) =>
  proxyBackendRequest(input),
);

guardedIpc.handle("backend:upload", (_event, input: BackendUploadRequest) =>
  proxyBackendUpload(input),
);

guardedIpc.handle("hosts:rotate", (_event, input: { hostId: string }) =>
  rotateHostToken(input?.hostId ?? ""),
);

guardedIpc.handle("hosts:proxyConfig:get", (_event, input: { hostId: string }) =>
  getHostProxyConfig(input?.hostId ?? ""),
);

guardedIpc.handle("hosts:proxyConfig:set", (_event, input: { hostId: string; config: unknown }) =>
  saveHostProxyConfig({
    hostId: input?.hostId ?? "",
    config: input?.config,
  }),
);

guardedIpc.handle("settings:saveServerUrl", (_event, input: { serverUrl: string }) =>
  saveServerUrlFromSettings(input?.serverUrl ?? ""),
);

function buildServerProfileApplyDeps(): ApplyServerProfileDeps | null {
  const client = hostClient;
  if (!client || !backend) return null;

  const daemonConfigPath = desktopAppPaths().desktop.computerBinding;
  const controller = null;

  return {
    allowInsecureRemote: desktopAllowsInsecureRemote(),
    writeDaemonHostConfig: (config) => writeDaemonHostConfig(daemonConfigPath, config),
    hostOutboundReload: () => client.hostOutboundReload(),
    getAttachment: bindAttachmentAccessor(controller),
    currentLocalHostId: () => (localHostClaim.status === "claimed" ? localHostClaim.hostId : null),
    currentHostToken: () => readDaemonHostConfig(daemonConfigPath)?.host_token ?? null,
  };
}

async function runServerProfileApply(
  profile: ServerConnectionProfile,
): Promise<ServerProfileApplyOutcome> {
  const deps = buildServerProfileApplyDeps();
  if (!deps) {
    // Backend / host client not ready (pre-bootstrap or mid-shutdown).
    // Return `partial_apply_pending` rather than throwing so the IPC
    // handler can surface a typed result and the in-memory state
    // stays in a Retry-able shape. `no_attachment_controller` is the
    // structural-impossibility arm that matches this case (the legacy
    // outbound-reload path uses the same kind for the same reason).
    return {
      kind: "partial_apply_pending",
      profileVersion: profile.version,
      reason: "no_attachment_controller",
      reloadRejectionReason: null,
      detail: "backend or host client not ready",
    };
  }
  return applyServerProfile({ profile }, deps);
}

function applyProfileToActiveRemoteBackend(input: {
  profile: ServerConnectionProfile;
  testKind: (ProbeServerInfoResult & { ok: true })["kind"];
}): void {
  if (!backend || backend.kind !== "remote") return;
  const insecure = input.testKind === "insecure_dev_remote";
  if (backend.url === input.profile.server_url && backend.insecure === insecure) {
    return;
  }

  const previousUrl = backend.url;
  backend = buildRemoteBackendHandle({
    serverUrl: input.profile.server_url,
    insecure,
  });
  runtimeUpgradeMonitor.reset();
  installBackendEventWsOriginOverride();
  void syncEventWsSessionCookie(null);
  logger?.info(
    {
      previousUrl,
      serverUrl: input.profile.server_url,
      insecure,
    },
    "profile:saveWithTest retargeted active remote backend handle",
  );
}

async function handleProfileSaveWithTest(
  input: { serverUrl: string } | undefined,
): Promise<ProfileSaveWithTestResult> {
  const serverUrl = (input?.serverUrl ?? "").trim();
  const allowInsecureRemote = desktopAllowsInsecureRemote();
  const test = await probeServerInfo({ serverUrl, allowInsecureRemote });
  if (!test.ok) {
    logger?.warn({ kind: test.kind, serverUrl }, "profile:saveWithTest test phase failed");
    return { ok: false, phase: "test", test };
  }

  const changesServer =
    backend?.kind === "remote" && new URL(backend.url).origin !== new URL(test.originUrl).origin;
  if (changesServer) switchingServer = true;
  try {
    if (changesServer) {
      // Clear credentials before committing the new profile, including crash/restart paths.
      await clearUserSession(false);
      hydrateInFlight = null;
      if (hostClient) await gracefullyDetachOutbound({ client: hostClient, hostId: null });
    }
    const writeResult = writeServerConnectionProfile({
      profilePath: desktopAppPaths().desktop.serverProfile,
      serverUrl: test.originUrl,
      logger: logger ?? undefined,
    });
    if (!writeResult.ok) {
      logger?.warn(
        { reason: writeResult.reason, serverUrl },
        "profile:saveWithTest write phase failed",
      );
      return {
        ok: false,
        phase: "write",
        reason: writeResult.reason,
        detail: "detail" in writeResult ? writeResult.detail : undefined,
      };
    }
    logger?.info(
      {
        serverUrl,
        classification: test.kind,
        profileVersion: writeResult.profile.version,
      },
      "profile:saveWithTest committed",
    );

    const applyOutcome: ServerProfileApplyOutcome = changesServer
      ? { kind: "no_op_no_host_claim", profileVersion: writeResult.profile.version }
      : await runServerProfileApply(writeResult.profile);
    applyProfileToActiveRemoteBackend({
      profile: writeResult.profile,
      testKind: test.kind,
    });
    partialApplyState = derivePartialApplyState(applyOutcome, Date.now());
    if (applyOutcome.kind === "partial_apply_pending") {
      logger?.warn(
        {
          profileVersion: applyOutcome.profileVersion,
          reason: applyOutcome.reason,
          reloadRejectionReason: applyOutcome.reloadRejectionReason,
        },
        "profile:saveWithTest partial_apply_pending",
      );
    } else if (applyOutcome.kind === "fully_applied") {
      logger?.info(
        {
          profileVersion: applyOutcome.profileVersion,
          reloadKind: applyOutcome.reloadKind,
          elapsedMs: applyOutcome.elapsedMs,
        },
        "profile:saveWithTest fully_applied",
      );
    } else {
      logger?.info(
        { profileVersion: applyOutcome.profileVersion },
        "profile:saveWithTest no_op_no_host_claim (pre-auth)",
      );
    }
    return {
      ok: true,
      test,
      profile: redactServerConnectionProfile(writeResult.profile),
      phase2: applyOutcome,
    };
  } finally {
    switchingServer = false;
  }
}

async function handlePartialApplyRetry(): Promise<PartialApplyRetryResult> {
  const read = readServerConnectionProfile({
    profilePath: desktopAppPaths().desktop.serverProfile,
    logger: logger ?? undefined,
  });
  if (!read.ok) {
    if (read.reason === "not_found") {
      logger?.warn(
        {},
        "partialApply:retry — no committed profile (renderer triggered without a save)",
      );
      partialApplyState = { kind: "no_profile" };
      return { ok: false, reason: "no_profile" };
    }

    const detail = read.detail;
    logger?.warn(
      { reason: read.reason, detail },
      "partialApply:retry — profile read failed → unusable_profile state",
    );
    partialApplyState = {
      kind: "unusable_profile",
      reason: read.reason,
      detail,
      lastReadAt: Date.now(),
    };
    return { ok: false, reason: "profile_unreadable", detail };
  }
  const outcome = await runServerProfileApply(read.profile);
  partialApplyState = derivePartialApplyState(outcome, Date.now());
  if (outcome.kind === "partial_apply_pending") {
    logger?.warn(
      {
        profileVersion: outcome.profileVersion,
        reason: outcome.reason,
        reloadRejectionReason: outcome.reloadRejectionReason,
      },
      "partialApply:retry — still partial_apply_pending",
    );
  } else {
    logger?.info(
      { kind: outcome.kind, profileVersion: outcome.profileVersion },
      "partialApply:retry — phase-2 succeeded",
    );
  }
  return { ok: true, outcome, state: partialApplyState };
}

guardedIpc.handle("profile:get", (): ServerConnectionProfileView | null =>
  readServerConnectionProfileViewOrNull({
    profilePath: desktopAppPaths().desktop.serverProfile,
    logger: logger ?? undefined,
  }),
);

let profileSaveQueue: Promise<unknown> = Promise.resolve();
guardedIpc.handle("profile:saveWithTest", (_event, input: { serverUrl: string }) => {
  const save = profileSaveQueue.then(() => handleProfileSaveWithTest(input));
  profileSaveQueue = save.catch(() => undefined);
  return save;
});

guardedIpc.handle("profile:clear", () => {
  const result = clearServerConnectionProfile({
    profilePath: desktopAppPaths().desktop.serverProfile,
    logger: logger ?? undefined,
  });
  if (result.ok) {
    partialApplyState = { kind: "no_profile" };
  }
  return result;
});

guardedIpc.handle("partialApply:get", (): PartialApplyState => partialApplyState);

guardedIpc.handle("partialApply:retry", (): Promise<PartialApplyRetryResult> =>
  handlePartialApplyRetry(),
);

registerSurfaceIpcHandlers(guardedIpc);
registerAppUpdateIpcHandlers(guardedIpc, () => appUpdate);
registerSelectionExportIpcHandlers(guardedIpc);

// Exactly one desktop instance may own the local daemon and embedded data at a
// time. A second instance exits immediately; the first keeps working.
if (!app.requestSingleInstanceLock()) {
  app.quit();
}

app
  .whenReady()
  .then(async () => {
    if (!app.hasSingleInstanceLock()) return;
    await bootstrap();
    if (HEADLESS) {
      // One-line machine-readable handshake on stdout. Test scripts pipe
      // and JSON.parse this. Human-readable extras go to stderr below.
      const embedded = requireEmbeddedBackend("HEADLESS handshake (SILVERHAND_HEADLESS=1)");

      process.stdout.write(
        JSON.stringify({
          url: embedded.url,
          port: embedded.port,
          bootstrap_token_path: embeddedBootstrapPath(),
          silverhand_dir: silverhandDir(),
        }) + "\n",
      );
      console.error("[main] headless backend ready — Ctrl-C to stop");
      return;
    }
    const rendererNetwork = await configureRendererBackendNetwork({
      session: electronSession.defaultSession,
      backendUrl: requireBackendUrl(),
    });
    rendererBackendResolvedProxy = rendererNetwork.resolvedProxy;
    logger?.info(
      {
        backend_origin: rendererNetwork.backendOrigin,
        resolved_proxy: rendererNetwork.resolvedProxy,
      },
      "renderer backend network configured",
    );
    Menu.setApplicationMenu(buildAppMenu());
    createWindow();
    if (appUpdate) {
      appUpdate.subscribe((state) => mainWindow?.webContents.send(APP_UPDATE_STATE_CHANNEL, state));
      appUpdate.start();
    }

    app.on("activate", () => {
      if (BrowserWindow.getAllWindows().length === 0) createWindow();
    });
  })
  .catch(reportStartupFailure);

/**
 * The single startup failure exit. `bootstrap()` and the window bring-up
 * above are fail-closed on purpose (refused server profile or
 * unreachable-but-required work); this is the only place
 * that turns such a refusal into something the user can see and act on.
 * Without it the rethrow died in an un-`.catch`ed promise and a
 * double-click produced no window and no message at all.
 */
function reportStartupFailure(err: unknown): void {
  const notice = describeStartupFailure(err);
  console.error("[main] startup failed:", err);
  logger?.error({ err, code: notice.code }, "startup failed");
  if (HEADLESS) {
    // Headless is a test/CI harness: stderr above plus a non-zero exit
    // IS its failure surface. A window here would hang the harness.
    app.exit(1);
    return;
  }
  // Evaluated at failure time, and defensively: the logs directory is
  // layout-version independent (it is shared across v1/v2), but a
  // failure this early must not depend on any path resolution
  // succeeding to be able to say what happened.
  let logsDir: string | null = null;
  try {
    logsDir = desktopAppPaths().logs;
  } catch {
    logsDir = null;
  }
  showStartupFailureWindow({ notice, logsDir });
}

async function handlePostPublishEmbeddedFatal(
  handle: EmbeddedRustBackendHandle,
  reason: EmbeddedRustBackendTerminalReason,
): Promise<void> {
  await embeddedBackendOwner
    .own(() =>
      invalidateEmbeddedRuntime(reason, {
        isCurrent: () =>
          !isShuttingDown && backend?.kind === "embedded" && backend.running === handle,
        invalidate: () => {
          backend = null;
        },
        clearUrls: async () => {
          try {
            rmSync(daemonPaths().controlPlaneServerUrlPath, { force: true });
          } finally {
            await invalidateEmbeddedServerUrl();
          }
        },
        close: () => embeddedBackendOwner.close(handle),
        report: (terminal) =>
          reportStartupFailure(
            new Error(`embedded backend terminated at runtime (${terminal.kind})`),
          ),
      }),
    )
    .catch((err) => logger?.error({ err }, "embedded backend runtime cleanup failed"));
}

app.on("window-all-closed", () => {
  // In headless mode there are no windows to begin with — that signal
  // would quit the app immediately. Keep alive until SIGINT.
  if (HEADLESS) return;
  if (process.platform !== "darwin") app.quit();
});

if (HEADLESS) {
  for (const sig of ["SIGINT", "SIGTERM"] as const) {
    process.on(sig, () => {
      console.error(`[main] received ${sig}, shutting down`);
      app.quit();
    });
  }
}

// Set true once we've started the async shutdown path so a re-fire of
// `before-quit` (Electron may emit it again after `app.exit`/`app.quit`
// in some platform sequences) doesn't restart the work or loop.
let isShuttingDown = false;

async function shutdown(): Promise<void> {
  embeddedBackendOwner.beginShutdown();
  // Notify the renderer so it can show a progress overlay while we
  // clean up — prevents the window from looking frozen.
  const pushProgress = (step: string): void => {
    try {
      mainWindow?.webContents.send("shutdown:progress", step);
    } catch {
      /* window may already be destroyed — ignore */
    }
  };

  try {
    // Ask the daemon to detach while its backend is still reachable. Both backend
    // modes use the authenticated host WebSocket; main has no registry observer.
    // The daemon RPC has its own bounded shutdown budget.
    if (hostClient) {
      const claimedHostId = localHostClaim.status === "claimed" ? localHostClaim.hostId : null;

      const getAttachment = undefined;
      const detachOutcome = await gracefullyDetachOutbound({
        client: hostClient,
        hostId: claimedHostId,
        getAttachment,
      });
      logger?.info(
        {
          kind: "shutdown.outbound_detach",
          rpc_ok: detachOutcome.rpc.ok,
          rpc_action: detachOutcome.rpc.ok ? detachOutcome.rpc.action : null,
          rpc_error: !detachOutcome.rpc.ok ? detachOutcome.rpc.error : null,
          rpc_error_code: !detachOutcome.rpc.ok ? (detachOutcome.rpc.code ?? null) : null,
          registry_cleared: detachOutcome.registryCleared,
          registry_error: detachOutcome.registryError ?? null,
          elapsed_ms: detachOutcome.elapsedMs,
          budget_exhausted: detachOutcome.budgetExhausted,
          host_id: claimedHostId,
        },
        "outbound detach hint result on shutdown",
      );
    }
  } catch (err) {
    // The helper itself never throws — this catch covers truly
    // unexpected exceptions (e.g. logger blowing up). Swallow so
    // the rest of shutdown still runs.
    console.error("[main] outbound detach hint failed unexpectedly:", err);
  }

  // Join unpublished startup and fatal cleanup as well as any published handle.
  // Detach above retains its existing chance to use the still-running backend.
  await embeddedBackendOwner.shutdown();
  try {
    if (backend) {
      pushProgress("Stopping server…");
      // Embedded owns a child process; remote has no local backend to reclaim.
      if (backend.kind === "embedded") {
        try {
          await embeddedBackendOwner.close(backend.running);
        } finally {
          rmSync(daemonPaths().controlPlaneServerUrlPath, { force: true });
          await invalidateEmbeddedServerUrl();
        }
      } else {
        await backend.close();
      }
      backend = null;
    }
  } catch (err) {
    console.error("[main] backend.close() failed:", err);
  }
  try {
    // Tear down host plumbing AFTER backend so any final pings during
    // shutdown have something to talk to. HostClient first (drops the
    // socket cleanly so daemon doesn't log a "peer error"); then
    // daemon (SIGTERM → graceful drain → SIGKILL fallback).
    if (hostClient) {
      await hostClient.close().catch(() => undefined);
      hostClient = null;
    }
    if (daemon) {
      pushProgress("Stopping agents…");
      // Supervisor gives runtimes 5s before SIGKILL and up to 10s to
      // settle. Do not kill their owner before that drain can complete.
      await daemon.kill().catch(() => undefined);
      daemon = null;
    }
    pushProgress("Cleanup finished");
  } catch (err) {
    console.error("[main] host shutdown failed:", err);
  }
  try {
    if (logger) {
      await logger.close();
      logger = null;
    }
  } catch (err) {
    console.error("[main] logger.close() failed:", err);
  }
}

app.on("before-quit", (event) => {
  // Async work in `before-quit` is silently dropped by Electron unless
  // we preventDefault and re-quit ourselves: the handler's returned
  // Promise is NOT awaited by the quit sequence, so without this guard
  // the main process exits while `backend.close()` is still running and
  // every spawned agent child is left orphaned. We saw this in practice —
  // claude/codex processes survived an app close, then the next launch
  // spawned duplicates and codex's
  // resume failed with `thread not found` because the orphan still
  // owned the thread server-side.
  if (isShuttingDown) return;
  event.preventDefault();
  isShuttingDown = true;

  // Clear the dock badge synchronously before any async shutdown work —
  // otherwise a stale unread number lingers on the dock icon for the
  // ~10s the shutdown takes, which looks like the app is still alive
  // with new messages waiting.
  app.setBadgeCount(0);

  // Hard ceiling so a wedged shutdown can't hang the app forever.
  // Embedded startup/fatal/published ownership is drained before successful exit:
  // the Rust child gets its bounded SIGTERM→SIGKILL grace, then the daemon gets
  // 15s to drain its runtime children. The remaining margin covers detach and
  // logging. This watchdog is an emergency failed exit, never a substitute for
  // the normal reclaim join.
  const watchdog = setTimeout(() => {
    console.error("[main] shutdown watchdog fired; forcing exit");
    app.exit(1);
  }, 35_000);
  watchdog.unref();

  void shutdown().finally(() => {
    clearTimeout(watchdog);
    // app.exit (not app.quit) — we've already done the cleanup; quit()
    // would re-fire before-quit and loop through this handler.
    app.exit(0);
  });
});
