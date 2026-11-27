import { delimiter } from 'node:path';
import { describe, expect, it } from 'vitest';
import {
  SILVERHAND_ACP_AUTO_COMPACT_ENV,
  SILVERHAND_AGENT_ID_ENV,
  SILVERHAND_AUTH_TOKEN_ENV,
  SILVERHAND_CONVERSATION_ID_ENV,
  SILVERHAND_DAEMON_SOCKET_ENV,
  SILVERHAND_LAST_DELIVERED_SEQ_FILE_ENV,
  SILVERHAND_SERVER_URL_ENV,
} from '@silverhand/shared';
import type { Driver } from '../src/drivers/index.js';
import { applyHostAuthoritativeSpawnEnv, buildDaemonSpawnEnv } from '../src/daemon/spawn-env.js';
import { testStartOptions } from './_helpers/runtime-start-options.js';

function makeDriver(applyEnv: Driver['applyEnv']): Driver {
  return {
    kind: 'claude',
    binary: 'claude',
    applyEnv,
    buildArgs: () => [],
    parseLine: () => [],
    encodeStdinMessage: () => '',
    encodeInterruptControl: () => '',
    oneShot: false,
    kickoffMode: 'self_init',
    releaseProfile: 'tool-boundary-steer',
  } as unknown as Driver;
}

const BASE_ENV: NodeJS.ProcessEnv = {
  PATH: '/usr/local/bin:/usr/bin:/bin',
  HOME: '/Users/test',
  LANG: 'en_US.UTF-8',
};

describe('daemon spawn env', () => {
  it('materializes final child env locally from typed env_policy and daemon proxy config', () => {
    const driver = makeDriver((env) => ({ ...env, NO_COLOR: '1' }));
    const options = testStartOptions({ runtime: 'codex' });
    options.env_policy = {
      ...options.env_policy,
      silverhand: { auth_token: 'tok_test_123' },
    };

    const env = buildDaemonSpawnEnv({
      driver,
      options,
      conversationId: 'conv-abc',
      agentId: 'agent-xyz',
      binDir: '/tmp/silverhand-spawn/bin',
      controlPlaneServerUrl: 'https://remote.example.test',
      proxyConfig: {
        proxy_url: 'http://127.0.0.1:7897',
        proxy_runtimes: ['codex'],
      },
      baseProcessEnv: BASE_ENV,
    });

    expect(env.NO_COLOR).toBe('1');
    expect(env.HTTP_PROXY).toBe('http://127.0.0.1:7897');
    expect(env.HTTPS_PROXY).toBe('http://127.0.0.1:7897');
    expect(env[SILVERHAND_CONVERSATION_ID_ENV]).toBe('conv-abc');
    expect(env[SILVERHAND_AGENT_ID_ENV]).toBe('agent-xyz');
    expect(env[SILVERHAND_SERVER_URL_ENV]).toBe('https://remote.example.test');
    expect(env[SILVERHAND_AUTH_TOKEN_ENV]).toBe('tok_test_123');
    expect(env.PATH).toBe(`/tmp/silverhand-spawn/bin${delimiter}/usr/local/bin:/usr/bin:/bin`);
  });

  it('does not apply proxy env when the runtime is not allowlisted', () => {
    const driver = makeDriver((env) => ({ ...env }));
    const options = testStartOptions({ runtime: 'claude' });

    const env = buildDaemonSpawnEnv({
      driver,
      options,
      conversationId: options.session.conversation_id,
      agentId: options.agent.id,
      binDir: '/tmp/silverhand-spawn/bin',
      controlPlaneServerUrl: 'http://127.0.0.1:8765',
      proxyConfig: {
        proxy_url: 'http://127.0.0.1:7897',
        proxy_runtimes: ['codex'],
      },
      baseProcessEnv: BASE_ENV,
    });

    expect(env.HTTP_PROXY).toBeUndefined();
    expect(env.HTTPS_PROXY).toBeUndefined();
  });

  it('re-applies Silverhand identity env after driver-specific env shaping', () => {
    const driver = makeDriver((env) => ({
      ...env,
      [SILVERHAND_CONVERSATION_ID_ENV]: 'wrong-conversation',
      [SILVERHAND_AGENT_ID_ENV]: 'wrong-agent',
      [SILVERHAND_SERVER_URL_ENV]: 'https://wrong.example.test',
      [SILVERHAND_AUTH_TOKEN_ENV]: 'wrong-token',
    }));
    const options = testStartOptions({ runtime: 'codex' });
    options.env_policy = {
      ...options.env_policy,
      silverhand: { auth_token: 'tok_expected' },
    };

    const env = buildDaemonSpawnEnv({
      driver,
      options,
      conversationId: 'conv-expected',
      agentId: 'agent-expected',
      binDir: '/tmp/silverhand-spawn/bin',
      controlPlaneServerUrl: 'https://remote.example.test',
      baseProcessEnv: BASE_ENV,
    });

    expect(env[SILVERHAND_CONVERSATION_ID_ENV]).toBe('conv-expected');
    expect(env[SILVERHAND_AGENT_ID_ENV]).toBe('agent-expected');
    expect(env[SILVERHAND_SERVER_URL_ENV]).toBe('https://remote.example.test');
    expect(env[SILVERHAND_AUTH_TOKEN_ENV]).toBe('tok_expected');
  });

  it('AgentSlot deletes an inherited or driver-injected ambient conversation id', () => {
    const driver = makeDriver((env) => ({
      ...env,
      [SILVERHAND_CONVERSATION_ID_ENV]: 'ambient-from-daemon',
    }));
    const options = testStartOptions({
      runtime: 'codex',
      sessionScope: 'agent',
      agentId: 'agent-global',
    });

    const env = buildDaemonSpawnEnv({
      driver,
      options,
      conversationId: 'delivery-target-only',
      agentId: 'agent-global',
      binDir: '/tmp/silverhand-spawn/bin',
      controlPlaneServerUrl: 'https://remote.example.test',
      baseProcessEnv: {
        ...BASE_ENV,
        [SILVERHAND_CONVERSATION_ID_ENV]: 'ambient-from-parent',
      },
    });

    expect(env).not.toHaveProperty(SILVERHAND_CONVERSATION_ID_ENV);
    expect(env[SILVERHAND_AGENT_ID_ENV]).toBe('agent-global');
  });

  it('fails loud when the daemon has no host-local control-plane URL', () => {
    const driver = makeDriver((env) => ({ ...env }));
    const options = testStartOptions({ runtime: 'codex' });

    expect(() =>
      buildDaemonSpawnEnv({
        driver,
        options,
        conversationId: options.session.conversation_id,
        agentId: options.agent.id,
        binDir: '/tmp/silverhand-spawn/bin',
        baseProcessEnv: BASE_ENV,
      }),
    ).toThrow(/no silverhand server URL/);
  });

  it('merges user_env including SILVERHAND_ACP_AUTO_COMPACT while protecting host-owned keys and absent keys', () => {
    const driver = makeDriver((env) => ({ ...env, NO_COLOR: '1' }));
    const options = testStartOptions({
      runtime: 'antigravity',
      sessionScope: 'agent',
      agentId: 'agent-target',
    });
    options.env_policy = {
      silverhand: { auth_token: 'tok_host_real' },
      user_env: [
        { name: SILVERHAND_ACP_AUTO_COMPACT_ENV, value: '200000' },
        { name: 'CUSTOM_MARKER', value: 'marker-only-target' },
        { name: SILVERHAND_AGENT_ID_ENV, value: 'spoofed-agent' },
        { name: SILVERHAND_AUTH_TOKEN_ENV, value: 'spoofed-token' },
        { name: SILVERHAND_SERVER_URL_ENV, value: 'https://spoofed.example' },
        { name: SILVERHAND_CONVERSATION_ID_ENV, value: 'spoofed-conv' },
        { name: SILVERHAND_DAEMON_SOCKET_ENV, value: '/tmp/spoofed.sock' },
        { name: SILVERHAND_LAST_DELIVERED_SEQ_FILE_ENV, value: '/tmp/spoofed.seq' },
        { name: 'PATH', value: '/custom/user/bin:/usr/bin' },
      ],
    };

    const daemonEnv = buildDaemonSpawnEnv({
      driver,
      options,
      conversationId: 'conv-origin',
      agentId: 'agent-target',
      binDir: '/tmp/silverhand-spawn/bin',
      controlPlaneServerUrl: 'https://host.example',
      baseProcessEnv: {
        ...BASE_ENV,
        [SILVERHAND_DAEMON_SOCKET_ENV]: '/tmp/inherited.sock',
        [SILVERHAND_LAST_DELIVERED_SEQ_FILE_ENV]: '/tmp/inherited.seq',
      },
    });

    expect(daemonEnv[SILVERHAND_ACP_AUTO_COMPACT_ENV]).toBe('200000');
    expect(daemonEnv.CUSTOM_MARKER).toBe('marker-only-target');
    expect(daemonEnv[SILVERHAND_AGENT_ID_ENV]).toBe('agent-target');
    expect(daemonEnv[SILVERHAND_AUTH_TOKEN_ENV]).toBe('tok_host_real');
    expect(daemonEnv[SILVERHAND_SERVER_URL_ENV]).toBe('https://host.example');
    expect(daemonEnv).not.toHaveProperty(SILVERHAND_CONVERSATION_ID_ENV);
    expect(daemonEnv).not.toHaveProperty(SILVERHAND_DAEMON_SOCKET_ENV);
    expect(daemonEnv).not.toHaveProperty(SILVERHAND_LAST_DELIVERED_SEQ_FILE_ENV);
    expect(daemonEnv.PATH).toBe(`/tmp/silverhand-spawn/bin${delimiter}/custom/user/bin:/usr/bin`);

    // Final supervisor layer: profile MCP overrides user_env for its own keys
    // while host-owned keys and their absence remain authoritative.
    const finalEnv = applyHostAuthoritativeSpawnEnv(
      {
        ...daemonEnv,
        OPENCODE_CONFIG_CONTENT: '{"mcp":{}}',
      },
      {
        runtimeScopeKind: 'agent',
        conversationId: 'conv-origin',
        agentId: 'agent-target',
        serverUrl: 'https://host.example',
        authToken: 'tok_host_real',
        binDir: '/tmp/silverhand-spawn/bin',
      },
    );
    expect(finalEnv.OPENCODE_CONFIG_CONTENT).toBe('{"mcp":{}}');
    expect(finalEnv.PATH?.startsWith(`/tmp/silverhand-spawn/bin${delimiter}`)).toBe(true);
    expect(finalEnv).not.toHaveProperty(SILVERHAND_CONVERSATION_ID_ENV);
    expect(finalEnv).not.toHaveProperty(SILVERHAND_DAEMON_SOCKET_ENV);
    expect(finalEnv).not.toHaveProperty(SILVERHAND_LAST_DELIVERED_SEQ_FILE_ENV);
  });
});
