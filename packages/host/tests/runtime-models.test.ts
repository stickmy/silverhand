import { chmod, mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { RuntimeInfo, RuntimeKind } from '@silverhand/shared';
import { probeRuntimeModels, RuntimeModelDiscoveryCache } from '../src/runtime-models.js';

const roots: string[] = [];
afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});
async function fake(kind: RuntimeKind, program: string, version = '2.2.7') {
  const root = await mkdtemp(join(tmpdir(), 'models-test-'));
  roots.push(root);
  const binary = join(root, 'runtime');
  await writeFile(binary, `#!${process.execPath}\n${program}`);
  await chmod(binary, 0o700);
  const runtime: RuntimeInfo = {
    kind,
    state: 'present',
    reason: 'resolved',
    installed: true,
    binary,
    version,
  };
  return { root, runtime };
}
const lines = `const readline = require('node:readline');
const send = value => process.stdout.write(JSON.stringify(value)+'\\n');
readline.createInterface({input:process.stdin}).on('line', line => { const m=JSON.parse(line);`;

it('Claude sends only a metadata request, disables hooks/MCP, and preserves aliases plus resolved IDs', async () => {
  const { runtime, root } = await fake(
    'claude',
    `${lines}
    if (m.request.subtype!=='list_models' || !process.argv.includes('--strict-mcp-config') || !process.argv.includes('--no-session-persistence') ||
        !process.argv.includes('{"disableAllHooks":true}')) process.exit(2);
    send({type:'control_response',response:{request_id:'foreign',subtype:'success',response:{models:[{value:'wrong'}]}}});
    send({type:'control_response',response:{request_id:m.request_id,subtype:'success',response:{models:[
      {value:'default'}, {value:'opus',resolvedModel:'claude-opus',supportedEffortLevels:['low','future-tier']}, {value:'claude-opus'}, {value:'disabled',disabled:true}
    ]}}}); });`,
  );
  expect(await probeRuntimeModels(runtime, { root })).toEqual({
    status: 'ok',
    models: ['opus', 'claude-opus'],
    default_model: 'opus',
    reasoning_by_model: {
      opus: { supported_efforts: ['low', 'future-tier'] },
      'claude-opus': { supported_efforts: ['low', 'future-tier'] },
    },
  });
  expect(await readdir(root)).toEqual(['runtime']);
});

it('Codex initializes once and collects all pages without sending a turn', async () => {
  const { runtime, root } = await fake(
    'codex',
    `${lines}
    if(m.method==='initialize') send({id:m.id,result:{}});
    else if(m.method==='model/list') send({id:m.id,result: m.params.cursor
      ? {data:[{id:'second',supportedReasoningEfforts:[]},{id:'first'},{id:'hidden',hidden:true}],nextCursor:null}
      : {data:[{id:'first',model:'first',isDefault:true,supportedReasoningEfforts:[{reasoningEffort:'high'},{reasoningEffort:'future-tier'}],defaultReasoningEffort:'high',serviceTiers:[{id:'default',name:'Standard'},{id:'priority',name:'Fast'},{id:'ultrafast',name:'Ultrafast'},{id:'Bad Tier',name:'x'}]}],nextCursor:'page-2'}});
    else if(m.method!=='initialized') process.exit(2);
  });`,
  );
  expect(await probeRuntimeModels(runtime, { root })).toEqual({
    status: 'ok',
    models: ['first', 'second'],
    default_model: 'first',
    reasoning_by_model: {
      first: { supported_efforts: ['high', 'future-tier'], default_effort: 'high' },
      second: { supported_efforts: [] },
    },
    service_tiers_by_model: {
      first: [
        { id: 'priority', name: 'Fast' },
        { id: 'ultrafast', name: 'Ultrafast' },
      ],
    },
  });
});

it('Codex refuses a repeated pagination cursor instead of looping', async () => {
  const { runtime, root } = await fake(
    'codex',
    `${lines}
    if(m.method==='initialize') send({id:m.id,result:{}});
    if(m.method==='model/list') send({id:m.id,result:{data:[],nextCursor:'same'}});
  });`,
  );
  expect((await probeRuntimeModels(runtime, { root })).status).toBe('error');
});

it('Pi uses in-memory RPC and keeps provider-qualified model IDs', async () => {
  const { runtime, root } = await fake(
    'pi',
    `${lines}
    if(m.type!=='get_available_models'||!process.argv.includes('--no-session')) process.exit(2);
    send({type:'response',id:m.id,success:true,data:{models:[{provider:'p1',id:'m'},{provider:'p2',id:'m'}]}});
  });`,
  );
  expect(await probeRuntimeModels(runtime, { root })).toEqual({
    status: 'ok',
    models: ['p1/m', 'p2/m'],
  });
});

it('Grok reads initialize metadata without creating a session', async () => {
  const { runtime, root } = await fake(
    'grok',
    `${lines}
    if(m.method!=='initialize'||!process.argv.includes('--no-leader')) process.exit(2);
    send({id:m.id,result:{protocolVersion:1,_meta:{modelState:{availableModels:[{modelId:'grok-model'}]}}}});
  });`,
  );
  expect(await probeRuntimeModels(runtime, { root })).toEqual({
    status: 'ok',
    models: ['grok-model'],
  });
});

it.each(['1.9.0', '2.2.7'])(
  'OpenCode %s uses only provider/model output and v2 standalone',
  async (version) => {
    const { runtime, root } = await fake(
      'opencode',
      `
    if(process.argv.includes('--standalone')!==${version.startsWith('2')}) process.exit(2);
    console.log('Loading models...\\np1/m\\np2/m\\np1/m');`,
      version,
    );
    expect(await probeRuntimeModels(runtime, { root })).toEqual({
      status: 'ok',
      models: ['p1/m', 'p2/m'],
    });
  },
);

it('Antigravity lists model ids from agy models rows', async () => {
  const { runtime, root } = await fake(
    'antigravity',
    `
    if(process.argv.slice(2).join(' ')!=='models') process.exit(2);
    console.log('Fetching available models...\\ngemini-3.8-flash-low\\tGemini 3.8 Flash (Low)\\nclaude-sonnet-4-6\\tClaude Sonnet 4.6 (Thinking)');`,
    '1.5.14',
  );
  expect(await probeRuntimeModels(runtime, { root })).toEqual({
    status: 'ok',
    models: ['gemini-3.8-flash-low', 'claude-sonnet-4-6'],
  });
});

it('empty native metadata differs from a failed probe', async () => {
  const { runtime, root } = await fake(
    'pi',
    `${lines}send({id:m.id,type:'response',success:true,data:{models:[]}}); });`,
  );
  expect(await probeRuntimeModels(runtime, { root })).toEqual({ status: 'ok', models: [] });
  expect(await probeRuntimeModels({ ...runtime, binary: join(root, 'missing') }, { root })).toEqual(
    { status: 'error', models: [] },
  );
});

it('unsupported methods remain distinct from authentication errors', async () => {
  const { runtime, root } = await fake(
    'claude',
    `${lines}send({type:'control_response',response:{request_id:m.request_id,subtype:'error',error:'Please log in'}}); });`,
  );
  expect((await probeRuntimeModels(runtime, { root })).status).toBe('error');
  const unsupported = await fake('codex', `${lines}send({id:m.id,error:{code:-32601}}); });`);
  expect((await probeRuntimeModels(unsupported.runtime, { root: unsupported.root })).status).toBe(
    'unsupported',
  );
});

it('Qoder never launches a discovery process', async () => {
  const { runtime, root } = await fake(
    'qoder',
    `require('node:fs').writeFileSync(${JSON.stringify('must-not-exist')},'bad')`,
  );
  expect((await probeRuntimeModels(runtime, { root })).status).toBe('unsupported');
  expect(await readdir(root)).toEqual(['runtime']);
});

it('deadline kills a silent process and removes its private cwd', async () => {
  const { runtime, root } = await fake(
    'pi',
    `require('node:fs').writeFileSync(process.env.PID_FILE,String(process.pid)); setInterval(()=>{},100);`,
  );
  const pidFile = join(root, 'pid');
  expect(
    (
      await probeRuntimeModels(runtime, {
        root,
        timeoutMs: 2000,
        env: { ...process.env, PID_FILE: pidFile },
      })
    ).status,
  ).toBe('error');
  const pid = Number(await readFile(pidFile, 'utf8'));
  expect(() => process.kill(pid, 0)).toThrow();
  expect((await readdir(root)).filter((x) => x.startsWith('probe-'))).toEqual([]);
});

it('bounds output rather than buffering unlimited runtime diagnostics', async () => {
  const { runtime, root } = await fake(
    'pi',
    `process.stdout.write('x'.repeat(3*1024*1024));setInterval(()=>{},100);`,
  );
  expect((await probeRuntimeModels(runtime, { root })).status).toBe('error');
});

describe('model cache', () => {
  it('coalesces requests and scopes results by executable and environment', async () => {
    const { runtime } = await fake('pi', '');
    let release!: (value: { status: 'ok'; models: string[] }) => void;
    const probe = vi.fn(
      () =>
        new Promise<{ status: 'ok'; models: string[] }>((resolve) => {
          release = resolve;
        }),
    );
    const cache = new RuntimeModelDiscoveryCache(probe, () => 10);
    const a = cache.read(runtime, { HOME: 'one' });
    const b = cache.read(runtime, { HOME: 'one' }, true);
    expect(probe).toHaveBeenCalledTimes(1);
    release({ status: 'ok', models: ['one'] });
    await Promise.all([a, b]);
    expect((await cache.read(runtime, { HOME: 'one' })).cached).toBe(true);
    const other = cache.read(runtime, { HOME: 'two' });
    expect(probe).toHaveBeenCalledTimes(2);
    release({ status: 'ok', models: ['two'] });
    await other;
    const newBinary = cache.read({ ...runtime, binary: '/another/runtime' }, { HOME: 'two' });
    expect(probe).toHaveBeenCalledTimes(3);
    release({ status: 'ok', models: ['new'] });
    await newBinary;
  });
  it('does not cache failures and force refresh replaces the prior list', async () => {
    const { runtime } = await fake('pi', '');
    const probe = vi
      .fn()
      .mockResolvedValueOnce({ status: 'error', models: [] })
      .mockResolvedValue({ status: 'ok', models: ['fresh'] });
    const cache = new RuntimeModelDiscoveryCache(probe, () => 10);
    await cache.read(runtime, {});
    await cache.read(runtime, {});
    expect(probe).toHaveBeenCalledTimes(2);
    await cache.read(runtime, {}, true);
    expect(probe).toHaveBeenCalledTimes(3);
  });
});

it('malformed effort metadata does not hide a valid model or invent effort support', async () => {
  const { runtime, root } = await fake(
    'claude',
    `${lines}
    send({type:'control_response',response:{request_id:m.request_id,subtype:'success',response:{models:[
      {value:'unknown'}, {value:'bad',supportedEffortLevels:['High']}, {value:'none',supportedEffortLevels:[]}
    ]}}}); });`,
  );
  expect(await probeRuntimeModels(runtime, { root })).toEqual({
    status: 'ok',
    models: ['unknown', 'bad', 'none'],
    reasoning_by_model: { none: { supported_efforts: [] } },
  });
});
