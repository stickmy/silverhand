import { delimiter } from 'node:path';
import {
  expandProxyEnv,
  SILVERHAND_AGENT_ID_ENV,
  SILVERHAND_AUTH_TOKEN_ENV,
  SILVERHAND_CONVERSATION_ID_ENV,
  SILVERHAND_DAEMON_SOCKET_ENV,
  SILVERHAND_LAST_DELIVERED_SEQ_FILE_ENV,
  SILVERHAND_SERVER_URL_ENV,
} from '@silverhand/shared';
import type { DaemonProxyConfig } from './proxy-config.js';
import type { Driver } from '../drivers/index.js';
import type { StartOptions } from '../protocol.js';

export interface DaemonSpawnEnvInput {
  driver: Driver;
  options: StartOptions;
  conversationId: string;
  agentId: string;
  daemonSocketPath?: string;
  lastDeliveredSeqPath?: string;
  binDir: string;
  /**
   * Daemon-local control-plane origin from the current outbound attachment.
   * Standalone backends do not know how this host should reach them; Desktop
   * writes that URL into daemon config and the daemon owns injecting it into
   * child CLI env.
   */
  controlPlaneServerUrl?: string | null;
  proxyConfig?: DaemonProxyConfig;
  baseProcessEnv?: NodeJS.ProcessEnv;
}

export interface HostAuthoritativeSpawnEnv {
  runtimeScopeKind: StartOptions['runtime_scope']['kind'];
  conversationId: string;
  agentId: string;
  serverUrl: string;
  authToken: string;
  daemonSocketPath?: string;
  lastDeliveredSeqPath?: string;
  binDir: string;
}

/**
 * Re-apply every host-owned Silverhand key and enforce the shim PATH prefix on
 * the final child env. Absent optional keys (agent-scope conversation id,
 * unconfigured daemon socket or delivery cursor) are explicitly deleted so
 * neither ambient daemon env nor user-configured env can spoof them.
 */
export function applyHostAuthoritativeSpawnEnv(
  env: NodeJS.ProcessEnv,
  authority: HostAuthoritativeSpawnEnv,
): NodeJS.ProcessEnv {
  const currentPath = env.PATH ?? '';
  const pathPrefix = `${authority.binDir}${delimiter}`;
  const path =
    currentPath === authority.binDir || currentPath.startsWith(pathPrefix)
      ? currentPath
      : currentPath
        ? `${authority.binDir}${delimiter}${currentPath}`
        : authority.binDir;

  const result: NodeJS.ProcessEnv = {
    ...env,
    [SILVERHAND_AGENT_ID_ENV]: authority.agentId,
    [SILVERHAND_SERVER_URL_ENV]: authority.serverUrl,
    [SILVERHAND_AUTH_TOKEN_ENV]: authority.authToken,
    PATH: path,
  };

  if (authority.runtimeScopeKind === 'conversation') {
    result[SILVERHAND_CONVERSATION_ID_ENV] = authority.conversationId;
  } else {
    delete result[SILVERHAND_CONVERSATION_ID_ENV];
  }

  if (authority.daemonSocketPath) {
    result[SILVERHAND_DAEMON_SOCKET_ENV] = authority.daemonSocketPath;
  } else {
    delete result[SILVERHAND_DAEMON_SOCKET_ENV];
  }

  if (authority.lastDeliveredSeqPath) {
    result[SILVERHAND_LAST_DELIVERED_SEQ_FILE_ENV] = authority.lastDeliveredSeqPath;
  } else {
    delete result[SILVERHAND_LAST_DELIVERED_SEQ_FILE_ENV];
  }

  return result;
}

export function buildDaemonSpawnEnv(input: DaemonSpawnEnvInput): NodeJS.ProcessEnv {
  const options = input.options;
  let env = input.driver.applyEnv(input.baseProcessEnv ?? process.env);
  if (
    input.proxyConfig?.proxy_url &&
    input.proxyConfig.proxy_runtimes.includes(options.driver.kind)
  ) {
    const proxyEnv = expandProxyEnv(input.proxyConfig.proxy_url);
    if (proxyEnv) env = { ...env, ...proxyEnv };
  }

  const userEnv = options.env_policy.user_env;
  if (userEnv && userEnv.length > 0) {
    const userLayer: NodeJS.ProcessEnv = {};
    for (const entry of userEnv) {
      userLayer[entry.name] = entry.value;
    }
    env = { ...env, ...userLayer };
  }

  const silverhandServerUrl = input.controlPlaneServerUrl;
  if (!silverhandServerUrl) {
    throw new Error(
      'runtime_start: no silverhand server URL supplied by server and no daemon-local control-plane URL is attached',
    );
  }

  if (env.PATH === undefined && process.env.PATH !== undefined) {
    env = { ...env, PATH: process.env.PATH };
  }

  return applyHostAuthoritativeSpawnEnv(env, {
    runtimeScopeKind: options.runtime_scope.kind,
    conversationId: input.conversationId,
    agentId: input.agentId,
    serverUrl: silverhandServerUrl,
    authToken: options.env_policy.silverhand.auth_token,
    daemonSocketPath: input.daemonSocketPath,
    lastDeliveredSeqPath: input.lastDeliveredSeqPath,
    binDir: input.binDir,
  });
}
