// Exercise the actual frontend config and TS parser, including import aliases.
import assert from 'node:assert/strict';
import test from 'node:test';
import { ESLint } from 'eslint';

const eslint = new ESLint();
const filePath = 'packages/app-ui/src/lint-fixture.tsx';
async function unstableBindings(code) {
  const [result] = await eslint.lintText(code, { filePath });
  assert.equal(result.fatalErrorCount, 0);
  return result.messages
    .filter((message) => message.ruleId === 'silverhand/stable-hook-default')
    .map((message) => message.message.split("'")[1]);
}

test('fresh destructured defaults invalidate hook dependencies', async () => {
  assert.deepEqual(
    await unstableBindings(`
    import * as React from "react";
    import { useMemo as memo } from "react";
    function useLookup(source, { options = {} }) {
      const { data: channels = [] as string[] } = source;
      React.useEffect(() => consume(options), [options]);
      return memo(() => index(channels), [channels]);
    }
  `),
    ['options', 'channels'],
  );
});

test('stable defaults, unrelated bindings and locally defined functions are allowed', async () => {
  assert.deepEqual(
    await unstableBindings(`
    import { useMemo } from "react";
    const EMPTY = [];
    function useLookup(source) {
      const { data: channels = EMPTY, unused = [] } = source;
      function render(channels) {
        function useMemo(fn, deps) { return fn(); }
        const { data = [] } = source;
        return useMemo(() => index(channels, data), [channels, data]);
      }
      return useMemo(() => render(channels), [channels]);
    }
  `),
    [],
  );
});
