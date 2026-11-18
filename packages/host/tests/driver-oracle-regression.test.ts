import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';

import { ClaudeDriver } from '../src/drivers/claude.js';
import { CodexDriver } from '../src/drivers/codex.js';
import { CursorDriver } from '../src/drivers/cursor.js';
import type { Driver } from '../src/drivers/index.js';
import { DRIVER_PARSE_ORACLES, type DriverParseOracle } from './_fixtures/driver-oracles.js';

function makeDriver(oracle: DriverParseOracle): Driver {
  switch (oracle.runtime) {
    case 'claude':
      return new ClaudeDriver();
    case 'codex':
      return new CodexDriver();
    case 'cursor':
      return new CursorDriver();
  }
}

const fixtureDirs: string[] = [];

afterEach(() => {
  for (const dir of fixtureDirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

async function initializeCodex(driver: CodexDriver, oracle: DriverParseOracle) {
  const dir = mkdtempSync(join(tmpdir(), 'codex-oracle-'));
  fixtureDirs.push(dir);
  writeFileSync(join(dir, '.git'), '');
  const systemPromptPath = join(dir, 'prompt.md');
  writeFileSync(systemPromptPath, 'Oracle fixture');
  const writes: Array<{ id?: number; method: string }> = [];

  driver.bindIO({
    spawnCtx: {
      agentId: 'oracle',
      agentName: 'Oracle',
      workspacePath: dir,
      cliSessionId: oracle.resumeSessionId ?? null,
      model: null,
      reasoningEffort: null,
      serviceTier: null,
      systemPromptPath,
      initialPrompt: '',
    },
    writeLine: async (line) => {
      writes.push(JSON.parse(line));
    },
  });
  await new Promise<void>((resolve) => queueMicrotask(resolve));
  const initialize = writes.find((request) => request.method === 'initialize');
  expect(initialize).toBeDefined();
  expect(initialize!.id).toEqual(expect.any(Number));
  expect(driver.parseLine(JSON.stringify({ id: initialize!.id, result: {} }))).toEqual([]);
  expect(writes.some((request) => request.method === 'initialized')).toBe(true);
  return writes;
}

describe('daemon oracle regression matrix — driver parser fixtures', () => {
  for (const oracle of DRIVER_PARSE_ORACLES) {
    it(`${oracle.id}: ${oracle.description}`, async () => {
      const driver = makeDriver(oracle);
      const writes = driver instanceof CodexDriver ? await initializeCodex(driver, oracle) : [];

      for (const [index, step] of oracle.steps.entries()) {
        let line = step.line;
        if (step.responseTo) {
          const request = writes.find((request) => request.method === step.responseTo);
          expect(request, `${oracle.id}: missing ${step.responseTo} request`).toBeDefined();
          expect(request!.id).toEqual(expect.any(Number));
          line = JSON.stringify({ ...JSON.parse(line), id: request!.id });
        }
        const actual = driver.parseLine(line);
        expect(actual, `${oracle.id} step ${index + 1}`).toEqual(step.expected);
        for (const forbidden of step.forbiddenSubstrings ?? []) {
          expect(JSON.stringify(actual)).not.toContain(forbidden);
        }
      }
    });
  }

  it('keeps every oracle tied to a concrete incident / state-machine risk', () => {
    for (const oracle of DRIVER_PARSE_ORACLES) {
      expect(oracle.incident.trim(), `${oracle.id} incident`).not.toBe('');
      expect(oracle.steps.length, `${oracle.id} steps`).toBeGreaterThan(0);
    }
  });

  it('oracle ids are unique', () => {
    const ids = DRIVER_PARSE_ORACLES.map((oracle) => oracle.id);
    expect(new Set(ids).size).toBe(ids.length);
  });
});
