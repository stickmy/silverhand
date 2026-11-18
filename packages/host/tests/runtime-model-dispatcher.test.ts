import { afterEach, expect, it, vi } from 'vitest';
import { IntentDispatcher } from '../src/daemon/dispatcher.js';
import { RuntimeModelDiscoveryCache } from '../src/runtime-models.js';
import { detectRuntimeCatalog } from '../src/runtimes.js';
import { createDaemonLogControl } from '../src/daemon/logger.js';
vi.mock('../src/runtimes.js', () => ({ detectRuntimeCatalog: vi.fn() }));
afterEach(() => vi.restoreAllMocks());
const runtime = {
  kind: 'pi' as const,
  state: 'present' as const,
  reason: 'resolved' as const,
  installed: true,
  binary: '/bin/pi',
  version: '1.2.0',
};
const logger = {
  filePath: '/dev/null',
  logControl: createDaemonLogControl('debug'),
  debug: () => {},
  info: () => {},
  warn: () => {},
  error: () => {},
  close: async () => {},
};

it('ordinary catalog reads never probe models; explicit discovery uses the selected runtime and configured proxy', async () => {
  const catalog = { runtimes: [runtime], detected_at: 1 };
  vi.mocked(detectRuntimeCatalog).mockResolvedValue(catalog);
  const models = {
    runtime: 'pi' as const,
    status: 'ok' as const,
    models: ['provider/model'],
    detected_at: 1,
    cached: false,
  };
  const read = vi.spyOn(RuntimeModelDiscoveryCache.prototype, 'read').mockResolvedValue(models);
  const dispatcher = new IntentDispatcher({
    logger,
    hostProxyConfigGetHandler: () => ({
      config: { proxy_url: 'http://127.0.0.1:9999', proxy_runtimes: ['pi'] },
    }),
  });
  const plain = await dispatcher.dispatch('plain', 'host_runtime_catalog', {}, { owner: 'owner' });
  expect(plain).toHaveProperty('result', catalog);
  expect(read).not.toHaveBeenCalled();
  const discovered = await dispatcher.dispatch(
    'models',
    'host_runtime_catalog',
    { models_for: 'pi', force: true },
    { owner: 'owner' },
  );
  expect(discovered).toHaveProperty('result', { ...catalog, model_discovery: models });
  expect(read).toHaveBeenCalledWith(
    runtime,
    expect.objectContaining({ HTTPS_PROXY: 'http://127.0.0.1:9999' }),
    true,
  );
  const invalid = await dispatcher.dispatch(
    'invalid',
    'host_runtime_catalog',
    { models_for: 'other' },
    { owner: 'owner' },
  );
  expect(invalid).toHaveProperty('error');
  expect(read).toHaveBeenCalledTimes(1);
});
