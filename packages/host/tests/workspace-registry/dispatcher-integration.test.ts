import { describe, expect, it } from 'vitest';
import { IntentDispatcher } from '../../src/daemon/dispatcher.js';
import { AgentHomeMutationCoordinator } from '../../src/daemon/agent-home-mutation-coordinator.js';
import {
  AgentWorkspaceIndexError,
  AgentL2SeedError,
  AgentFilesPurgeError,
} from '../../src/daemon/workspace-registry/index.js';
import { createDaemonLogControl, type DaemonLogger } from '../../src/daemon/logger.js';
import type {
  AgentWorkspaceIndexParams,
  AgentWorkspaceIndexResult,
  AgentL2SeedParams,
  AgentL2SeedResult,
  AgentFilesPurgeParams,
  AgentFilesPurgeResult,
} from '../../src/protocol.js';

/**
 * D4.e-1 slice ② dispatcher integration for the registry read/seed/purge
 * intents. Mirrors the agent_materialize integration pins:
 *   1. each slot is wired (route → injected handler, params validated via
 *      `intentSchemas`);
 *   2. typed handler errors (`AgentWorkspaceIndexError`, a `DaemonTypedError`)
 *      survive the catch arm with their code (NOT squashed to
 *      `internal_error`), and ONLY codes in that intent's error schema pass —
 *      foreign codes are re-squashed by `intentErrorSchema(intent)`;
 *   3. bare throws squash to `internal_error`;
 *   4. malformed params reject with `invalid_params` before the handler;
 *   5. an unwired slot surfaces `internal_error`.
 */

function silentLogger(): DaemonLogger {
  return {
    filePath: '/dev/null',
    logControl: createDaemonLogControl('debug'),
    debug: () => {},
    info: () => {},
    warn: () => {},
    error: () => {},
    close: async () => {},
  } as unknown as DaemonLogger;
}

const indexParams: AgentWorkspaceIndexParams = {
  agent_id: 'aid_x',
  workspace_ids: ['ws_1'],
};
const l2SeedParams: AgentL2SeedParams = {
  agent_id: 'aid_member',
  workspace_id: 'ws_1',
  template_choice: 'default',
};
const purgeParams: AgentFilesPurgeParams = {
  agent_id: 'aid_member',
};

type DispatcherOptions = ConstructorParameters<typeof IntentDispatcher>[0];
type RegistryHandlerKey =
  | 'agentWorkspaceIndexHandler'
  | 'agentL2SeedHandler'
  | 'agentFilesPurgeHandler';

interface RegistryIntentCase {
  label: string;
  intent: 'agent_workspace_index' | 'agent_l2_seed' | 'agent_files_purge';
  requestPrefix: string;
  handlerKey: RegistryHandlerKey;
  params: Record<string, unknown>;
  result: Record<string, unknown>;
  errorName: string;
  typedCodes: readonly string[];
  makeTypedError: (code: string, message: string) => Error;
  foreignError: () => Error;
  malformedParams: Record<string, unknown>;
  malformedComment: string;
  bareError?: boolean;
}

function dispatcherWithHandler(
  handlerKey: RegistryHandlerKey,
  handler: (params: never) => unknown,
): IntentDispatcher {
  const coordinator = new AgentHomeMutationCoordinator();
  return new IntentDispatcher({
    logger: silentLogger(),
    ...(handlerKey === 'agentFilesPurgeHandler'
      ? {
          supervisor: {
            agentHomeMutationCoordinator: () => coordinator,
            fenceAndQuiesceAgent: async () => ({ quiesced_count: 0 }),
          },
        }
      : {}),
    [handlerKey]: handler,
    ...(handlerKey === 'agentFilesPurgeHandler'
      ? { agentHomeMutationCoordinator: coordinator }
      : {}),
  } as unknown as DispatcherOptions);
}

async function expectErrorCode(
  promise: Promise<Awaited<ReturnType<IntentDispatcher['dispatch']>>>,
  code: string,
): Promise<Extract<Awaited<ReturnType<IntentDispatcher['dispatch']>>, { error: unknown }>> {
  const r = await promise;
  expect('error' in r).toBe(true);
  if (!('error' in r)) throw new Error('expected error response');
  expect(r.error.code).toBe(code);
  return r;
}

const registryIntentCases: RegistryIntentCase[] = [
  {
    label: 'agent_workspace_index',
    intent: 'agent_workspace_index',
    requestPrefix: 'req-awi',
    handlerKey: 'agentWorkspaceIndexHandler',
    params: indexParams,
    result: {
      ok: true,
      entries: [
        {
          workspace_id: 'ws_1',
          agent_dir: '/tmp/ws_1/.silverhand/agents/aid_x',
        },
      ],
      unresolved: [],
    } satisfies AgentWorkspaceIndexResult,
    errorName: 'AgentWorkspaceIndexError',
    typedCodes: ['registry_unavailable', 'internal_error'],
    makeTypedError: (code, message) => new AgentWorkspaceIndexError(code as never, message),
    foreignError: () =>
      // `unsupported_template` belongs to agent_l2_seed, not here.
      new AgentWorkspaceIndexError('unsupported_template' as never, 'leaked'),
    malformedParams: { agent_id: 'aid_x', workspace_ids: 'ws_1' },
    malformedComment: '`workspace_ids` must be an array.',
  },
  {
    label: 'agent_l2_seed',
    intent: 'agent_l2_seed',
    requestPrefix: 'req-l2s',
    handlerKey: 'agentL2SeedHandler',
    params: l2SeedParams,
    result: {
      ok: true,
      seeded: true,
      l2_present: true,
    } satisfies AgentL2SeedResult,
    errorName: 'AgentL2SeedError',
    // relocation-v1 §7-4 (U5): `workspace_not_provisioned` left this taxonomy —
    // the seed resolves its target from `{agent_id, workspace_id}` alone, so
    // there is no binding to be missing. Seeding IS provisioning.
    typedCodes: [
      'registry_unavailable',
      'unsupported_template',
      'fs_unavailable',
      'internal_error',
    ],
    makeTypedError: (code, message) => new AgentL2SeedError(code as never, message),
    foreignError: () =>
      // `partial_purge` belongs to agent_files_purge, not here.
      new AgentL2SeedError('partial_purge' as never, 'leaked'),
    malformedParams: { agent_id: 'aid_member' },
    malformedComment: 'Missing required `workspace_id`.',
    bareError: true,
  },
  {
    label: 'agent_files_purge',
    intent: 'agent_files_purge',
    requestPrefix: 'req-afp',
    handlerKey: 'agentFilesPurgeHandler',
    params: purgeParams,
    result: {
      ok: true,
      purged_count: 2,
      quiesced_count: 0,
    } satisfies AgentFilesPurgeResult,
    errorName: 'AgentFilesPurgeError',
    typedCodes: ['quiesce_failed', 'partial_purge', 'internal_error'],
    makeTypedError: (code, message) => new AgentFilesPurgeError(code as never, message),
    foreignError: () =>
      // `unsupported_template` belongs to agent_l2_seed, not here.
      new AgentFilesPurgeError('unsupported_template' as never, 'leaked'),
    malformedParams: {},
    malformedComment: 'Missing required `agent_id`.',
    bareError: true,
  },
];

describe.each(registryIntentCases)('IntentDispatcher.$label slot', (testCase) => {
  it('routes a valid intent to the handler and returns its result', async () => {
    let observed: unknown = null;
    const d = dispatcherWithHandler(testCase.handlerKey, (params) => {
      observed = params;
      return testCase.result;
    });

    const id = `${testCase.requestPrefix}-1`;
    const r = await d.dispatch(id, testCase.intent, testCase.params as never, {
      owner: 'test-owner',
    });

    expect(r).toEqual({ id, result: testCase.result });
    expect(observed).toEqual(testCase.params);
  });

  it.each(testCase.typedCodes)(`preserves typed ${testCase.errorName} code: %s`, async (code) => {
    const d = dispatcherWithHandler(testCase.handlerKey, () => {
      throw testCase.makeTypedError(code, `seeded ${code}`);
    });

    await expectErrorCode(
      d.dispatch(`${testCase.requestPrefix}-typed`, testCase.intent, testCase.params as never, {
        owner: 'test-owner',
      }),
      code,
    );
  });

  it(`re-squashes a foreign typed code not in ${testCase.label} schema`, async () => {
    const d = dispatcherWithHandler(testCase.handlerKey, () => {
      // Typed surface is decided by per-intent schema, not by class.
      throw testCase.foreignError();
    });

    await expectErrorCode(
      d.dispatch(`${testCase.requestPrefix}-foreign`, testCase.intent, testCase.params as never, {
        owner: 'test-owner',
      }),
      'internal_error',
    );
  });

  if (testCase.bareError) {
    it('squashes a bare Error throw to internal_error', async () => {
      const d = dispatcherWithHandler(testCase.handlerKey, () => {
        throw new Error('boom');
      });

      await expectErrorCode(
        d.dispatch(`${testCase.requestPrefix}-bare`, testCase.intent, testCase.params as never, {
          owner: 'test-owner',
        }),
        'internal_error',
      );
    });
  }

  it(`rejects malformed params via intentSchemas before invoking the handler (${testCase.malformedComment})`, async () => {
    let called = false;
    const d = dispatcherWithHandler(testCase.handlerKey, () => {
      called = true;
      throw new Error('unreachable');
    });

    await expectErrorCode(
      d.dispatch(
        `${testCase.requestPrefix}-bad`,
        testCase.intent,
        testCase.malformedParams as never,
        { owner: 'test-owner' },
      ),
      'invalid_params',
    );
    expect(called).toBe(false);
  });

  it('surfaces internal_error when no handler is wired', async () => {
    const coordinator = new AgentHomeMutationCoordinator();
    const d = new IntentDispatcher({
      logger: silentLogger(),
      ...(testCase.intent === 'agent_files_purge'
        ? {
            agentHomeMutationCoordinator: coordinator,
            supervisor: {
              agentHomeMutationCoordinator: () => coordinator,
              fenceAndQuiesceAgent: async () => ({ quiesced_count: 0 }),
            } as never,
          }
        : {}),
    });

    const r = await expectErrorCode(
      d.dispatch(`${testCase.requestPrefix}-unwired`, testCase.intent, testCase.params as never, {
        owner: 'test-owner',
      }),
      'internal_error',
    );
    expect(r.error.message).toContain(testCase.intent);
  });
});

describe('IntentDispatcher.agent_l2_seed extras', () => {
  it('applies the template_choice default when omitted', async () => {
    let observed: AgentL2SeedParams | null = null;
    const d = dispatcherWithHandler('agentL2SeedHandler', (params) => {
      observed = params;
      return { ok: true, seeded: true, l2_present: true } satisfies AgentL2SeedResult;
    });

    await d.dispatch(
      'req-l2s-default',
      'agent_l2_seed',
      {
        agent_id: 'aid_member',
        workspace_id: 'ws_1',
      },
      { owner: 'test-owner' },
    );

    expect((observed as AgentL2SeedParams | null)?.template_choice).toBe('default');
  });

  it('rejects an out-of-enum template_choice via intentSchemas before the handler', async () => {
    let called = false;
    const d = dispatcherWithHandler('agentL2SeedHandler', () => {
      called = true;
      throw new Error('unreachable');
    });

    await expectErrorCode(
      d.dispatch(
        'req-l2s-badtpl',
        'agent_l2_seed',
        {
          agent_id: 'aid_member',
          workspace_id: 'ws_1',
          template_choice: 'bogus',
        },
        { owner: 'test-owner' },
      ),
      'invalid_params',
    );
    expect(called).toBe(false);
  });
});

describe('IntentDispatcher.agent_files_purge extras', () => {
  it.each([{ scope: 'all' }, { workspace_ids: ['ws_1'] }])(
    'rejects retired fields %j before the handler',
    async (retired) => {
      // relocation-v1 §7 task 4 — the intent collapsed to `{ agent_id }`; the
      // retired `scope`/`workspace_ids` must be refused at the wire boundary,
      // never silently ignored into a full-home purge.
      let called = false;
      const d = dispatcherWithHandler('agentFilesPurgeHandler', () => {
        called = true;
        throw new Error('unreachable');
      });

      await expectErrorCode(
        d.dispatch(
          'req-afp-retired',
          'agent_files_purge',
          {
            agent_id: 'aid_member',
            ...retired,
          },
          { owner: 'test-owner' },
        ),
        'invalid_params',
      );
      expect(called).toBe(false);
    },
  );
});
