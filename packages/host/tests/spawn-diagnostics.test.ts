import { describe, expect, it } from 'vitest';
import {
  describeSpawnError,
  redactSpawnArgs,
  summarizeSpawnEnv,
  summarizeSpawnPath,
  summarizeSilverhandEnvPresence,
} from '../src/daemon/spawn-diagnostics.js';

describe('redactSpawnArgs', () => {
  it('redacts the inline Codex `-c` config body (MCP headers) but keeps shape', () => {
    const args = [
      'app-server',
      '--listen',
      'stdio://',
      '-c',
      'mcp_servers.chat={http_headers={Authorization="Bearer sk-super-secret"}}',
      '-c',
      'model_reasoning_effort="high"',
    ];
    const redacted = redactSpawnArgs(args);
    const serialized = JSON.stringify(redacted);
    expect(serialized).not.toContain('sk-super-secret');
    expect(serialized).not.toContain('Bearer');
    // Shape is preserved: same count, flags intact, config keys visible.
    expect(redacted).toHaveLength(args.length);
    expect(redacted.slice(0, 4)).toEqual(['app-server', '--listen', 'stdio://', '-c']);
    expect(redacted[5]).toBe('-c');
    // The non-secret config key survives; only the value body is withheld.
    expect(redacted[6]).toMatch(/^model_reasoning_effort=<redacted:/);
  });

  it('redacts an inline standing prompt but keeps the flag', () => {
    const prompt = 'You are Alice. L2 memory: internal workspace rules ...';
    const redacted = redactSpawnArgs(['--append-system-prompt', prompt, '--model', 'opus']);
    const serialized = JSON.stringify(redacted);
    expect(serialized).not.toContain('internal workspace rules');
    expect(redacted[0]).toBe('--append-system-prompt');
    expect(redacted[1]).toMatch(/^<redacted:len=/);
    // Non-sensitive flags pass through untouched.
    expect(redacted.slice(2)).toEqual(['--model', 'opus']);
  });

  it('keeps path-valued and non-sensitive flags verbatim', () => {
    const args = [
      '--append-system-prompt-file',
      '/tmp/spawn/system-prompt.md',
      '--mcp-config',
      '/tmp/spawn/profile-mcp.json',
      '--output-format',
      'stream-json',
    ];
    expect(redactSpawnArgs(args)).toEqual(args);
  });

  it('redacts a `--flag=value` token whose value smells sensitive', () => {
    const redacted = redactSpawnArgs(['--header=Authorization: Bearer tok-leak']);
    expect(JSON.stringify(redacted)).not.toContain('tok-leak');
    expect(redacted[0]).toMatch(/^--header=<redacted:/);
  });

  it('redacts the value of a two-token secret flag, keeping the flag name', () => {
    const redacted = redactSpawnArgs(['--api-key', 'sk-live-secret', '--model', 'opus']);
    const serialized = JSON.stringify(redacted);
    expect(serialized).not.toContain('sk-live-secret');
    expect(redacted[0]).toBe('--api-key');
    expect(redacted[1]).toMatch(/^<redacted:len=/);
    // Non-sensitive flag + value are untouched.
    expect(redacted.slice(2)).toEqual(['--model', 'opus']);
  });

  it('redacts two-token --token and --authorization values', () => {
    expect(JSON.stringify(redactSpawnArgs(['--token', 'abc123']))).not.toContain('abc123');
    expect(JSON.stringify(redactSpawnArgs(['--authorization', 'Bearer abc123']))).not.toContain(
      'abc123',
    );
  });

  it('redacts generic --auth / --header flags (no http prefix required)', () => {
    const redacted = redactSpawnArgs([
      '--auth',
      'abc123',
      '--header',
      'X-Internal: value',
      '--model',
      'opus',
    ]);
    const serialized = JSON.stringify(redacted);
    expect(serialized).not.toContain('abc123');
    expect(serialized).not.toContain('value');
    expect(redacted[0]).toBe('--auth');
    expect(redacted[2]).toBe('--header');
    expect(redacted.slice(4)).toEqual(['--model', 'opus']);
  });
});

describe('summarizeSpawnEnv', () => {
  it('never emits sensitive values, only their key names', () => {
    const summary = summarizeSpawnEnv({
      SILVERHAND_AUTH_TOKEN: 'tok_super_secret',
      OPENAI_API_KEY: 'sk-do-not-leak',
      ANTHROPIC_API_KEY: 'ant-do-not-leak',
      MY_SESSION_COOKIE: 'cookie-do-not-leak',
      SOME_PRIVATE_KEY: 'pk-do-not-leak',
    });
    const serialized = JSON.stringify(summary);
    expect(serialized).not.toContain('tok_super_secret');
    expect(serialized).not.toContain('sk-do-not-leak');
    expect(serialized).not.toContain('ant-do-not-leak');
    expect(serialized).not.toContain('cookie-do-not-leak');
    expect(serialized).not.toContain('pk-do-not-leak');
    expect(summary.sensitive_keys_redacted).toEqual(
      [
        'ANTHROPIC_API_KEY',
        'MY_SESSION_COOKIE',
        'OPENAI_API_KEY',
        'SILVERHAND_AUTH_TOKEN',
        'SOME_PRIVATE_KEY',
      ].sort(),
    );
    expect(summary.total_keys).toBe(5);
  });

  it('keeps identity and launch-context values (the diagnostic signal)', () => {
    const summary = summarizeSpawnEnv({
      HOME: '/Users/test',
      SHELL: '/bin/zsh',
      LANG: 'en_US.UTF-8',
      PWD: '/tmp/session',
      NODE_OPTIONS: '--max-old-space-size=4096',
      DYLD_INSERT_LIBRARIES: '/opt/evil/hook.dylib',
      ELECTRON_RUN_AS_NODE: '1',
    });
    expect(summary.identity).toMatchObject({
      HOME: '/Users/test',
      SHELL: '/bin/zsh',
      LANG: 'en_US.UTF-8',
    });
    expect(summary.launch_context).toMatchObject({
      PWD: '/tmp/session',
      NODE_OPTIONS: '--max-old-space-size=4096',
      DYLD_INSERT_LIBRARIES: '/opt/evil/hook.dylib',
      ELECTRON_RUN_AS_NODE: '1',
    });
  });

  it('reports cert vars as path + existence, never content', () => {
    const summary = summarizeSpawnEnv({
      SSL_CERT_DIR: '/usr/bin',
      SSL_CERT_FILE: '/nonexistent/does-not-exist.pem',
    });
    expect(summary.cert_files.SSL_CERT_DIR).toEqual({
      path: '/usr/bin',
      exists: true,
    });
    expect(summary.cert_files.SSL_CERT_FILE).toEqual({
      path: '/nonexistent/does-not-exist.pem',
      exists: false,
    });
  });

  it('masks proxy credentials while keeping host visible', () => {
    const summary = summarizeSpawnEnv({
      HTTPS_PROXY: 'http://user:secretpass@proxy.example:8080',
      NO_PROXY: 'localhost,127.0.0.1',
    });
    const serialized = JSON.stringify(summary);
    expect(serialized).not.toContain('secretpass');
    expect(serialized).not.toContain('user:');
    expect(summary.proxy.HTTPS_PROXY).toContain('proxy.example');
    expect(summary.proxy.NO_PROXY).toBe('localhost,127.0.0.1');
  });

  it('lists other and notable keys by name only, no values', () => {
    const summary = summarizeSpawnEnv({
      npm_config_registry: 'https://registry.example',
      SOME_TOOL_HOME: '/opt/tool',
    });
    expect(summary.notable_keys).toContain('npm_config_registry');
    expect(summary.other_keys).toContain('SOME_TOOL_HOME');
    const serialized = JSON.stringify(summary);
    expect(serialized).not.toContain('/opt/tool');
    expect(serialized).not.toContain('registry.example');
  });

  it('redacts user-configured env values even on value-bearing buckets and PATH', () => {
    const marker = 'marker-must-not-appear-in-spawn-logs';
    const userKeys = new Set([
      'HOME',
      'NODE_OPTIONS',
      'NO_PROXY',
      'SSL_CERT_FILE',
      'BENIGN_VAR',
      'PATH',
    ]);
    const envSummary = summarizeSpawnEnv(
      {
        HOME: `/tmp/${marker}`,
        NODE_OPTIONS: `--title=${marker}`,
        NO_PROXY: `${marker}.internal`,
        SSL_CERT_FILE: `/tmp/${marker}.pem`,
        BENIGN_VAR: marker,
      },
      { redactedKeys: userKeys },
    );
    expect(JSON.stringify(envSummary)).not.toContain(marker);
    expect(envSummary.sensitive_keys_redacted).toEqual([
      'BENIGN_VAR',
      'HOME',
      'NODE_OPTIONS',
      'NO_PROXY',
      'SSL_CERT_FILE',
    ]);

    const pathSummary = summarizeSpawnPath(`/tmp/shim-bin:/opt/${marker}/bin`, {
      redactEntriesAfterDir: '/tmp/shim-bin',
    });
    expect(JSON.stringify(pathSummary)).not.toContain(marker);
    expect(pathSummary.path_dirs).toEqual(['/tmp/shim-bin', '<redacted-user-path>']);
    expect(pathSummary.path_dir_count).toBe(2);
  });
});

describe('summarizeSilverhandEnvPresence', () => {
  it('reports required key presence without returning any values', () => {
    const summary = summarizeSilverhandEnvPresence({
      SILVERHAND_CONVERSATION_ID: 'conversation-secret-ish',
      SILVERHAND_AGENT_ID: 'agent-secret-ish',
      SILVERHAND_SERVER_URL: 'https://internal.example.test',
      SILVERHAND_AUTH_TOKEN: 'tok-must-never-leak',
    });

    expect(summary).toEqual({
      all_present: true,
      present_keys: [
        'SILVERHAND_CONVERSATION_ID',
        'SILVERHAND_AGENT_ID',
        'SILVERHAND_SERVER_URL',
        'SILVERHAND_AUTH_TOKEN',
      ],
      missing_keys: [],
    });
  });

  it('treats missing and empty values as absent', () => {
    expect(
      summarizeSilverhandEnvPresence({
        SILVERHAND_CONVERSATION_ID: 'conv',
        SILVERHAND_AGENT_ID: '',
      }),
    ).toEqual({
      all_present: false,
      present_keys: ['SILVERHAND_CONVERSATION_ID'],
      missing_keys: ['SILVERHAND_AGENT_ID', 'SILVERHAND_SERVER_URL', 'SILVERHAND_AUTH_TOKEN'],
    });
  });
});

describe('summarizeSpawnPath', () => {
  it('flags suspicious, relative, and missing dirs while listing all in order', () => {
    const path = [
      '/Users/test/.codex/tmp/arg0/codex-arg0XY',
      '/usr/bin',
      '/nonexistent-dir-xyz',
      'relative/dir',
    ].join(':');
    const summary = summarizeSpawnPath(path);
    expect(summary.path_dir_count).toBe(4);
    expect(summary.path_dirs[0]).toBe('/Users/test/.codex/tmp/arg0/codex-arg0XY');
    expect(summary.path_suspicious_dirs).toContain('/Users/test/.codex/tmp/arg0/codex-arg0XY');
    expect(summary.path_relative_dirs).toEqual(['relative/dir']);
    expect(summary.path_nonexistent_dirs).toContain('/nonexistent-dir-xyz');
  });

  it('handles an undefined PATH', () => {
    expect(summarizeSpawnPath(undefined)).toEqual({
      path_dir_count: 0,
      path_dirs: [],
      path_suspicious_dirs: [],
      path_nonexistent_dirs: [],
      path_relative_dirs: [],
    });
  });
});

describe('describeSpawnError', () => {
  it('maps the opaque "Unknown system error -8" to ENOEXEC', () => {
    const info = describeSpawnError(
      Object.assign(new Error('spawn Unknown system error -8'), {
        errno: -8,
        syscall: 'spawn',
        path: '/Applications/Codex.app/Contents/Resources/codex',
        spawnargs: ['app-server', '--listen', 'stdio://'],
      }),
    );
    expect(info.errno).toBe(-8);
    expect(info.syscall).toBe('spawn');
    expect(info.path).toBe('/Applications/Codex.app/Contents/Resources/codex');
    expect(info.spawnargs).toEqual(['app-server', '--listen', 'stdio://']);
    expect(info.readable).toContain('ENOEXEC');
  });

  it('maps EBADARCH, EACCES, and ENOENT', () => {
    expect(
      describeSpawnError(Object.assign(new Error('x'), { code: 'EBADARCH' })).readable,
    ).toContain('EBADARCH');
    expect(describeSpawnError(Object.assign(new Error('x'), { errno: -13 })).readable).toContain(
      'EACCES',
    );
    expect(
      describeSpawnError(Object.assign(new Error('x'), { code: 'ENOENT' })).readable,
    ).toContain('ENOENT');
  });

  it('degrades gracefully on an unmapped error', () => {
    const info = describeSpawnError(Object.assign(new Error('weird'), { code: 'EPERM' }));
    expect(info.code).toBe('EPERM');
    expect(info.readable).toContain('Unmapped');
  });

  it('redacts secrets in spawnargs carried on the error object', () => {
    const info = describeSpawnError(
      Object.assign(new Error('spawn EACCES'), {
        code: 'EACCES',
        spawnargs: ['--api-key', 'sk-live-secret', '--model', 'opus'],
      }),
    );
    expect(JSON.stringify(info.spawnargs)).not.toContain('sk-live-secret');
    expect(info.spawnargs).toEqual([
      '--api-key',
      expect.stringMatching(/^<redacted:len=/),
      '--model',
      'opus',
    ]);
  });
});
