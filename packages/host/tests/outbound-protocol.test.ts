import { describe, expect, it } from 'vitest';
import {
  EventFrameSchema,
  KNOWN_CAPABILITIES_V1,
  ResponseErrorSchema,
  intentSchemas,
} from '../src/protocol.js';
import {
  AgentMaterializeErrorCodeSchema,
  AgentMaterializeParamsSchema,
  AgentMaterializeResultSchema,
  OUTBOUND_DAEMON_EVENT_KINDS,
  OUTBOUND_PROTOCOL_VERSION,
  OUTBOUND_SERVER_COMMAND_KINDS,
  OutboundDaemonToServerCommandResponseFrameSchema,
  OutboundFrameSchema,
  OutboundHeartbeatFrameSchema,
  OutboundServerToDaemonCommandFrameSchema,
  RotateTokenParamsSchema,
  encodeOutboundFrame,
} from '../src/outbound-protocol.js';
import { testStartOptions } from './_helpers/runtime-start-options.js';

const RUN_STATE = {
  runId: 'r1',
  agentId: 'a1',
  conversationId: 'c1',
  status: 'idle' as const,
  startedAt: 0,
  updatedAt: 0,
  lastActivityAt: 0,
};

describe('outbound protocol — command catalogue', () => {
  it('contains the D2.0 runtime request surface plus rotate_token, agent_materialize, the D4.e data-plane intents, the agent home workspace tree+read pair, host_status, and proxy config commands', () => {
    expect([...OUTBOUND_SERVER_COMMAND_KINDS].sort()).toEqual([
      'agent_files_purge',
      'agent_l2_seed',
      'agent_materialize',
      'agent_profile_mcp_read',
      'agent_profile_mcp_write',
      'agent_workspace_file_read',
      'agent_workspace_index',
      'agent_workspace_tree',
      'host_proxy_config_get',
      'host_proxy_config_set',
      'host_runtime_catalog',
      'host_status',
      'mcp_probe',
      'rotate_token',
      'runtime_clear_session',
      'runtime_close_activity_log',
      'runtime_deliver',
      'runtime_output_pause',
      'runtime_output_resume',
      'runtime_start',
      'runtime_status',
      'runtime_stop',
      'runtime_stop_all',
    ]);

    expect(OUTBOUND_SERVER_COMMAND_KINDS).not.toContain('hello' as never);
    expect(OUTBOUND_SERVER_COMMAND_KINDS).not.toContain('ping' as never);
    expect(OUTBOUND_SERVER_COMMAND_KINDS).not.toContain('echo' as never);
    expect(OUTBOUND_SERVER_COMMAND_KINDS).not.toContain('conversation_log_debug' as never);
  });

  it('keeps every runtime_* intent reachable through the outbound command catalogue', () => {
    const runtimeIntentKinds = Object.keys(intentSchemas)
      .filter((kind) => kind.startsWith('runtime_'))
      .sort();
    const outboundRuntimeCommandKinds = OUTBOUND_SERVER_COMMAND_KINDS.filter((kind) =>
      kind.startsWith('runtime_'),
    ).sort();

    expect(outboundRuntimeCommandKinds).toEqual(runtimeIntentKinds);
  });

  it('parses a server_to_daemon runtime command frame', () => {
    const parsed = OutboundServerToDaemonCommandFrameSchema.safeParse({
      direction: 'server_to_daemon',
      type: 'command',
      id: 'cmd-1',
      kind: 'runtime_deliver',
      params: {
        runtime_generation: 1,
        conversation_id: 'c1',
        target: { conversation_id: 'c1', workspace: null },
        agent_id: 'a1',
        delivery_ids: [1, 2],
        deliveries: [1, 2].map((id) => ({
          delivery_id: id,
          seq: id,
          presentation: { body: 'hello', instruction: 'system' as const },
        })),
        kind: 'message',
        text: 'hello',
        spawn_options: testStartOptions(),
      },
    });

    expect(parsed.success).toBe(true);
  });

  it('parses rotate_token with a strictly future grace window', () => {
    expect(
      RotateTokenParamsSchema.safeParse({
        host_id: 'h1',
        host_token: 'new-token',
        issued_at: 1000,
        grace_until: 61_000,
      }).success,
    ).toBe(true);

    expect(
      RotateTokenParamsSchema.safeParse({
        host_id: 'h1',
        host_token: 'new-token',
        issued_at: 1000,
        grace_until: 1000,
      }).success,
    ).toBe(false);
  });

  it('parses daemon command responses without mixing them with event arms', () => {
    const ok = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-1',
      kind: 'runtime_deliver',
      result: { disposition: 'admitted', pid: 123, started_at: 1_700_000_000_000 },
    });
    expect(ok.success).toBe(true);

    const err = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-2',
      kind: 'runtime_deliver',
      error: { code: 'internal_error', message: 'boom' },
    });
    expect(err.success).toBe(true);
    expect(ResponseErrorSchema.safeParse(err.success ? err.data.error : null).success).toBe(true);
  });

  it('parses an agent_materialize command frame round-trip (server → daemon)', () => {
    const parsed = OutboundServerToDaemonCommandFrameSchema.safeParse({
      direction: 'server_to_daemon',
      type: 'command',
      id: 'cmd-mat-1',
      kind: 'agent_materialize',
      params: {
        agent_id: 'AbCdEfGhIjKlMnOp',
        agent_home_workspace_id: 'wsah_AbCdEfGhIjKlMnOp',
        name: 'alice',
        runtime: 'claude',
        model: null,
        description: null,
        template_choice: 'default',
      },
    });
    expect(parsed.success).toBe(true);
  });

  it('parses an agent_materialize command response round-trip (daemon → server)', () => {
    const ok = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-mat-1',
      kind: 'agent_materialize',
      result: {
        ok: true,
        agent_dir: '/Users/test/.silverhand/agents/AbCdEfGhIjKlMnOp',
        // §6.5 invariant: agent_home_workspace_path === agent_dir.
        agent_home_workspace_path: '/Users/test/.silverhand/agents/AbCdEfGhIjKlMnOp',
        materialized_at: 1_700_000_000_000,
      },
    });
    expect(ok.success).toBe(true);

    const err = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-mat-2',
      kind: 'agent_materialize',
      error: { code: 'agent_dir_collision', message: 'foreign content' },
    });
    expect(err.success).toBe(true);
  });

  it('pins the agent_materialize error-code catalogue and rejects unknown codes', () => {
    expect(AgentMaterializeErrorCodeSchema.options).toEqual([
      'agent_deleting',
      'fs_quota_exceeded',
      'fs_unavailable',
      'agent_dir_collision',
      'unsupported_template',
      'unsupported_runtime',
      'internal_error',
    ]);
    expect(AgentMaterializeErrorCodeSchema.safeParse('already_exists').success).toBe(false);
    expect(AgentMaterializeErrorCodeSchema.safeParse('materialize_timeout').success).toBe(false);
  });

  it.each([
    { agent_dir: '', agent_home_workspace_path: '' },
    { agent_home_workspace_path: '/different/home' },
    { agent_dir: 123 },
    { agent_home_workspace_path: 123 },
    { materialized_at: 0 },
  ])('rejects only the invalid materialize result boundary: %j', (invalid) => {
    const valid = {
      ok: true,
      agent_dir: '/agent/home',
      agent_home_workspace_path: '/agent/home',
      materialized_at: 1,
    };
    expect(AgentMaterializeResultSchema.safeParse(valid).success).toBe(true);
    expect(AgentMaterializeResultSchema.safeParse({ ...valid, ...invalid }).success).toBe(false);
  });

  // Cindy D1 round-1 P1: command-specific typed error codes MUST be
  // scoped per-kind. A regression that re-introduces a single global
  // error union would silently widen every command's error surface
  // (e.g., runtime_start could "return" agent_dir_collision and the
  // wire schema would accept it). Pin both directions: the materialize
  // codes pass for `agent_materialize`, and the same codes fail for
  // runtime / rotate command arms.
  it('per-kind error scoping: runtime_start rejects an agent_materialize-only typed code', () => {
    const bad = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-x',
      kind: 'runtime_start',
      error: { code: 'agent_dir_collision', message: 'wrong' },
    });
    expect(bad.success).toBe(false);

    const badRotate = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-y',
      kind: 'rotate_token',
      error: { code: 'fs_quota_exceeded', message: 'wrong' },
    });
    expect(badRotate.success).toBe(false);
  });

  it('per-kind error scoping: agent_materialize accepts both transport codes and typed codes', () => {
    // Transport-level code (shared with all commands)
    const transportOk = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-a',
      kind: 'agent_materialize',
      error: { code: 'internal_error', message: 'boom' },
    });
    expect(transportOk.success).toBe(true);

    // Command-specific typed code
    const typedOk = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-b',
      kind: 'agent_materialize',
      error: { code: 'agent_dir_collision', message: 'foreign' },
    });
    expect(typedOk.success).toBe(true);

    // Still rejects entirely unknown codes (not in any union arm).
    const unknown = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-c',
      kind: 'agent_materialize',
      error: { code: 'materialize_timeout', message: 'wrong scope (server-only)' },
    });
    expect(unknown.success).toBe(false);
  });

  // D4.e-1 (Cindy P1-2): per-kind error scoping must hold for the
  // agent_workspace_index data-plane intent — its typed taxonomy must not
  // accept foreign codes from any other intent family or agent_materialize.
  it('per-kind error scoping: agent_workspace_index rejects a foreign typed code', () => {
    const bad = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-awi-foreign',
      kind: 'agent_workspace_index',
      error: { code: 'unsupported_kind', message: 'wrong scope' },
    });
    expect(bad.success).toBe(false);

    // fs_* is not an agent_workspace_index code.
    const badFs = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-awi-fs',
      kind: 'agent_workspace_index',
      error: { code: 'fs_quota_exceeded', message: 'wrong scope' },
    });
    expect(badFs.success).toBe(false);

    // Accepts its own typed code + the shared transport code.
    const typedOk = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-awi-typed',
      kind: 'agent_workspace_index',
      error: { code: 'registry_unavailable', message: 'corrupt' },
    });
    expect(typedOk.success).toBe(true);

    const transportOk = OutboundDaemonToServerCommandResponseFrameSchema.safeParse({
      direction: 'daemon_to_server',
      type: 'command_response',
      id: 'cmd-awi-transport',
      kind: 'agent_workspace_index',
      error: { code: 'internal_error', message: 'boom' },
    });
    expect(transportOk.success).toBe(true);
  });
});

describe('outbound protocol — daemon event catalogue', () => {
  it('keeps outbound event names in lockstep with the daemon event surface', () => {
    const daemonEventNames = EventFrameSchema.innerType()
      .options.map((option) => option.shape.event.value)
      .sort();

    expect([...OUTBOUND_DAEMON_EVENT_KINDS].sort()).toEqual(daemonEventNames);
  });
});

describe('outbound protocol — frame envelope', () => {
  it('uses direction/type/kind to reject wrong-way frames', () => {
    const valid = {
      runtime_generation: 1,
      direction: 'daemon_to_server',
      type: 'event',
      kind: 'runtime_state',
      runtime_scope: { kind: 'conversation', conversation_id: 'c1', agent_id: 'a1' },
      conversation_id: 'c1',
      agent_id: 'a1',
      state: RUN_STATE,
      ts: 1700000000000,
    };
    expect(OutboundFrameSchema.safeParse(valid).success).toBe(true);
    expect(OutboundFrameSchema.safeParse({ ...valid, direction: 'server_to_daemon' }).success).toBe(
      false,
    );
  });

  it('parses bidirectional app-level heartbeat frames', () => {
    expect(
      OutboundHeartbeatFrameSchema.safeParse({
        direction: 'daemon_to_server',
        type: 'heartbeat',
        kind: 'ping',
        ts: 1700000000000,
      }).success,
    ).toBe(true);

    expect(
      OutboundHeartbeatFrameSchema.safeParse({
        direction: 'server_to_daemon',
        type: 'heartbeat',
        kind: 'pong',
        ts: 1700000000100,
        echo_ts: 1700000000000,
      }).success,
    ).toBe(true);
  });

  it('encodes a single JSON frame without an NDJSON newline', () => {
    const encoded = encodeOutboundFrame({
      direction: 'daemon_to_server',
      type: 'event',
      kind: 'ready',
      protocol_version: OUTBOUND_PROTOCOL_VERSION,
      daemon_version: '0.0.0-test',
      daemon_id: 'local',
      instance_id: 'instance-1',
      host_descriptor: { name: 'local', kind: 'local' },
      capabilities: [...KNOWN_CAPABILITIES_V1],
      daemon_pid: 123,
      started_at: 1700000000000,
    });

    expect(encoded.endsWith('\n')).toBe(false);
    expect(JSON.parse(encoded)).toMatchObject({
      direction: 'daemon_to_server',
      type: 'event',
      kind: 'ready',
    });
  });
});
