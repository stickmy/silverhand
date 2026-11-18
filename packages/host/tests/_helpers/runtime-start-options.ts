import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { RuntimeKind } from '@silverhand/shared';
import { agentHomeWorkspaceId } from '@silverhand/shared';
import type {
  AgentStartOptions,
  ConversationStartOptions,
  StartOptions,
} from '../../src/protocol.js';
import type { RuntimeStartResolveDeps } from '../../src/daemon/runtime-start-resolve.js';

/**
 * Build SEMANTIC `runtime_start` options (D4.e-6): no `path`/`home_dir`
 * crosses the wire. The daemon resolves host paths deterministically from the
 * agent id + workspace id, so a test pairs these options with matching deps
 * (`testRuntimeStartResolveDeps`, keyed on the explicit agents root).
 */
interface TestStartOptionsOverrides {
  conversationId?: string;
  agentId?: string;
  agentName?: string;
  runtime?: RuntimeKind;
  model?: string | null;
  cliSessionId?: string | null;
  workspaceKind?: 'regular' | 'agent_home';
  sessionScope?: 'conversation' | 'agent';
}

export function testStartOptions(
  overrides: TestStartOptionsOverrides & { sessionScope: 'agent' },
): AgentStartOptions;
export function testStartOptions(
  overrides?: TestStartOptionsOverrides & { sessionScope?: 'conversation' },
): ConversationStartOptions;
export function testStartOptions(overrides: TestStartOptionsOverrides = {}): StartOptions {
  const agentId = overrides.agentId ?? 'a1';
  const agentName = overrides.agentName ?? 'tester';
  const conversationId = overrides.conversationId ?? 'c1';
  const runtime = overrides.runtime ?? 'claude';
  const workspaceKind = overrides.workspaceKind ?? 'regular';
  const sessionScope = overrides.sessionScope ?? 'conversation';
  // An `agent_home` ref must carry THIS agent's own home workspace id (the
  // daemon resolver validates id == agentHomeWorkspaceId(agentId)); a regular
  // workspace uses an arbitrary id.
  const workspaceId = workspaceKind === 'agent_home' ? agentHomeWorkspaceId(agentId) : 'ws1';

  const common = {
    agent: {
      id: agentId,
      name: agentName,
    },
    driver: {
      kind: runtime,
      cli_session_id: overrides.cliSessionId ?? null,
      model: overrides.model ?? null,
      reasoning_effort: null,
      service_tier: null,
    },
    env_policy: {
      silverhand: {
        auth_token: 'test-agent-token',
      },
      silverhand_cli: {
        command: 'silverhand',
        args: [],
      },
    },
  };
  if (sessionScope === 'agent') {
    return {
      ...common,
      runtime_scope: { kind: 'agent', agent_id: agentId },
    };
  }
  return {
    ...common,
    runtime_scope: {
      kind: 'conversation',
      conversation_id: conversationId,
      agent_id: agentId,
    },
    workspace: {
      id: workspaceId,
      name: 'test-workspace',
    },
    workspace_memory_index: [
      {
        id: workspaceId,
        name: 'test-workspace',
      },
    ],
    session: {
      conversation_id: conversationId,
      placement_kind: 'workspace',
      roster_policy: 'open',
      title: 'test-session',
      spawned_by_agent_id: null,
      members: [
        {
          kind: 'agent',
          agent_id: agentId,
          name: agentName,
          display_name: agentName,
          runtime,
        },
      ],
    },
  };
}

/**
 * Companion to `testStartOptions`: the daemon-side path resolver deps the
 * Supervisor needs (D4.e-6). relocation-v1 §7 task 4 — there is no registry
 * establishment gate any more: both `regular` and `agent_home` cwds resolve
 * deterministically from the agent id + workspace id under the EXPLICIT
 * `agentHomeRootDir` inside `resolveRuntimeStartPaths` (#task:1813 — there is no
 * home-relative default to fall back on), so pass the sandbox agents root when a
 * test reads real L1 prompt files or a materialized bundle.
 *
 * Availability of a resolved coordinate is a separate fs fact
 * (`bundleL2Present`: MEMORY.md at the bundle root). The default seam returns
 * `true` so memory-index entries resolve to their coordinate without touching
 * disk; override it to model the lazy window (never-spawned member workspace)
 * or a half-seed → `path: null`.
 */
export function testRuntimeStartResolveDeps(
  overrides: {
    agentHomeRootDir?: string;
    computerId?: string;
    bundleL2Present?: (bundleDir: string) => Promise<boolean>;
    resolveComputerId?: () => string;
  } = {},
): RuntimeStartResolveDeps {
  const computerId = overrides.computerId ?? 'test-computer';
  return {
    // Never home-derived: a test that does not care which root is used still
    // gets an explicit, obviously-fake one rather than the real user home.
    agentHomeRootDir: overrides.agentHomeRootDir ?? join(tmpdir(), 'silverhand-test-agents-root'),
    bundleL2Present: overrides.bundleL2Present ?? (async () => true),
    resolveComputerId: overrides.resolveComputerId ?? (() => computerId),
  };
}
