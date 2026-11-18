import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { agentHomeWorkspaceId } from '@silverhand/shared';
import { buildPromptContextFromRuntimeStartOptions } from '../src/daemon/prompt-context.js';
import { resolveRuntimeStartPaths } from '../src/daemon/runtime-start-resolve.js';
import { testRuntimeStartResolveDeps, testStartOptions } from './_helpers/runtime-start-options.js';

describe('buildPromptContextFromRuntimeStartOptions', () => {
  it.each([
    ['exact limit', 'a'.repeat(16 * 1024), 'a'.repeat(16 * 1024)],
    ['over limit', 'a'.repeat(16 * 1024) + 'b', 'a'.repeat(16 * 1024) + '\n…(truncated)'],
    ['split UTF-8', 'a'.repeat(16 * 1024 - 1) + '界', 'a'.repeat(16 * 1024 - 1) + '\n…(truncated)'],
  ])('caps actual memory at 16 KiB without breaking UTF-8: %s', async (_label, body, expected) => {
    const isolatedRoot = process.env.SILVERHAND_DIR;
    if (!isolatedRoot) throw new Error('SILVERHAND_DIR must name an isolated test directory');
    mkdirSync(isolatedRoot, { recursive: true });
    const root = mkdtempSync(join(isolatedRoot, 'prompt-cap-'));
    try {
      const agentsRoot = join(root, 'agents');
      const agentHome = join(agentsRoot, 'a1');
      mkdirSync(agentHome, { recursive: true });
      writeFileSync(join(agentHome, 'MEMORY.md'), body);
      const resolved = await resolveRuntimeStartPaths(
        testStartOptions({ sessionScope: 'agent' }),
        testRuntimeStartResolveDeps({ agentHomeRootDir: agentsRoot }),
      );

      const context = await buildPromptContextFromRuntimeStartOptions(resolved);

      expect(context.currentMemory).toEqual({ kind: 'present', body: expected });
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  it('reads the role from the daemon-resolved agent home', async () => {
    const root = mkdtempSync(join(tmpdir(), 'silverhand-prompt-context-'));
    try {
      const agentsRoot = join(root, 'computers', 'computer-1', 'agents');
      const agentHome = join(agentsRoot, 'a1');
      mkdirSync(agentHome, { recursive: true });
      writeFileSync(join(agentHome, 'system_prompt.md'), 'resolved role');

      const resolved = await resolveRuntimeStartPaths(
        testStartOptions(),
        testRuntimeStartResolveDeps({
          agentHomeRootDir: agentsRoot,
          computerId: 'computer-1',
        }),
      );
      const context = await buildPromptContextFromRuntimeStartOptions(resolved);

      expect(context.roleSystemPrompt).toBe('resolved role');
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  it('publishes the agent home only as the agent-scoped memory root', async () => {
    const root = mkdtempSync(join(tmpdir(), 'silverhand-prompt-context-agent-'));
    try {
      const agentsRoot = join(root, 'computers', 'computer-1', 'agents');
      const agentHome = join(agentsRoot, 'a1');
      mkdirSync(agentHome, { recursive: true });

      const context = await buildPromptContextFromRuntimeStartOptions(
        await resolveRuntimeStartPaths(
          testStartOptions({ sessionScope: 'agent' }),
          testRuntimeStartResolveDeps({
            agentHomeRootDir: agentsRoot,
            computerId: 'computer-1',
          }),
        ),
      );

      expect(context.sessionContext).toEqual({
        kind: 'agent',
        computerId: 'computer-1',
        memoryRootPath: agentHome,
      });
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  // The daemon RECOGNIZES an agent-home ref from its id convention (`wsah_`) and
  // selects a filesystem path-resolution strategy from that (#task:2081 retired
  // the stored `kind` word it used to read). It still publishes NO container word
  // into the prompt (#task:2075's stop-publish step). Both prompt-facing carriers
  // are pinned here because they fail differently: the session context would carry
  // a TOKEN (visible to a schema), the memory index carries PROSE (invisible to one).
  it('recognizes the agent home by its id and publishes no container word into the prompt', async () => {
    const root = mkdtempSync(join(tmpdir(), 'silverhand-prompt-context-kind-'));
    try {
      const agentsRoot = join(root, 'computers', 'computer-1', 'agents');
      mkdirSync(join(agentsRoot, 'a1'), { recursive: true });

      const options = testStartOptions({ workspaceKind: 'agent_home' });
      // Precondition, stated rather than assumed: the WIRE carries the agent-home
      // ref by its ID convention (`wsah_<agentId>`), which is now the daemon's
      // sole authority for recognizing one — there is no stored `kind` word left
      // to carry (#task:2081).
      expect(options.workspace?.id).toBe(agentHomeWorkspaceId('a1'));

      const context = await buildPromptContextFromRuntimeStartOptions(
        await resolveRuntimeStartPaths(
          options,
          testRuntimeStartResolveDeps({
            agentHomeRootDir: agentsRoot,
            computerId: 'computer-1',
          }),
        ),
      );

      // The session context publishes the COORDINATE. The absent key is the
      // assertion: this carrier published a decoded word until the stop-publish
      // step, and the prompt renderer now asks `isAgentHomeWorkspaceId` itself.
      expect(context.sessionContext).toMatchObject({
        kind: 'conversation',
        workspaceId: options.workspace!.id,
        workspacePath: join(agentsRoot, 'a1'),
      });
      expect(Object.keys(context.sessionContext)).not.toContain('workspaceKind');
      // The Workspace Memory Index label is the SECOND carrier on the same
      // surface, and it is prose rather than a token — invisible to any schema
      // gate, so it needs a cell of its own. It said `(DM)` until #task:2069 (an
      // agent could be told "DM" for the container a human saw as Agent Space),
      // then a word decoded from the stored one, and now the id CONVENTION.
      expect(context.workspaceMemoryIndex[0]?.label).toContain('(Agent Space)');
      expect(context.workspaceMemoryIndex[0]?.label).not.toContain('(DM)');
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  // The state where the retired decode and its replacement DISAGREE, which is why
  // the swap is not merely equivalent: an index entry whose stored word says
  // `agent_home` but whose id is not THIS agent's home. Reading the word would
  // label a foreign container "Agent Space" in this agent's own prompt; asking
  // the id convention does not. The state is constructible through this very
  // input — `resolveIndexEntry` soft-fails such an entry to `path: null` rather
  // than rejecting the spawn (only the ACTIVE workspace ref is validated).
  it('does not label a foreign agent_home index entry as an Agent Space', async () => {
    const root = mkdtempSync(join(tmpdir(), 'silverhand-prompt-context-foreign-'));
    try {
      const agentsRoot = join(root, 'computers', 'computer-1', 'agents');
      mkdirSync(join(agentsRoot, 'a1'), { recursive: true });

      const base = testStartOptions({ workspaceKind: 'agent_home' });
      const options: typeof base = {
        ...base,
        workspace_memory_index: [
          ...base.workspace_memory_index,
          {
            id: agentHomeWorkspaceId('someone-else'),
            name: 'other-home',
          },
        ],
      };

      const context = await buildPromptContextFromRuntimeStartOptions(
        await resolveRuntimeStartPaths(
          options,
          testRuntimeStartResolveDeps({
            agentHomeRootDir: agentsRoot,
            computerId: 'computer-1',
          }),
        ),
      );

      const foreign = context.workspaceMemoryIndex.find((e) => e.label.startsWith('other-home'));
      expect(foreign).toBeDefined();
      expect(foreign!.label).not.toContain('(Agent Space)');
      // Differential, so the cell cannot pass by resolving nothing at all: the
      // agent's OWN home in the same list still carries the label.
      expect(context.workspaceMemoryIndex.some((e) => e.label.includes('(Agent Space)'))).toBe(
        true,
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
