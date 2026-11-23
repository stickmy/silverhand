import { ToolSchema } from '@silverhand/shared';
import { describe, expect, it } from 'vitest';
import { acpTool, claudeTool, codexFileChange, piTool } from '../../src/drivers/tools.js';

// Frames below are captured from the real CLIs (agy 1.5.14, grok, opencode, pi).
describe('tool descriptions', () => {
  it('maps Claude tool names', () => {
    expect(claudeTool('Bash', { command: 'ls' })).toEqual({ type: 'command', command: 'ls' });
    expect(claudeTool('Read', { file_path: '/a' })).toEqual({ type: 'read', path: '/a' });
    expect(claudeTool('Write', { file_path: '/a', content: 'x' })).toEqual({
      type: 'write',
      path: '/a',
      content: 'x',
    });
    expect(claudeTool('Edit', { file_path: '/a', old_string: 'x', new_string: 'y' })).toEqual({
      type: 'edit',
      path: '/a',
      edits: [{ old: 'x', new: 'y' }],
    });
    expect(
      claudeTool('MultiEdit', { file_path: '/a', edits: [{ old_string: 'a', new_string: 'b' }] }),
    ).toEqual({ type: 'edit', path: '/a', edits: [{ old: 'a', new: 'b' }] });
    expect(claudeTool('Grep', { pattern: 'p', path: '/w', glob: '*.ts' })).toEqual({
      type: 'search',
      pattern: 'p',
      path: '/w',
      glob: '*.ts',
    });
    expect(claudeTool('Glob', { pattern: '*.ts' })).toEqual({ type: 'find', pattern: '*.ts' });
    expect(claudeTool('Task', { description: 'look' })).toEqual({
      type: 'subagent',
      description: 'look',
    });
    expect(claudeTool('Skill', { skill: 'dws' })).toEqual({ type: 'skill', name: 'dws' });
    expect(claudeTool('WebFetch', { url: 'https://x' })).toEqual({
      type: 'web_fetch',
      url: 'https://x',
    });
    expect(claudeTool('WebSearch', { query: 'q' })).toEqual({ type: 'web_search', query: 'q' });
    expect(claudeTool('mcp__chat__send', {})).toEqual({
      type: 'mcp',
      server: 'chat',
      tool: 'send',
    });
    expect(claudeTool('Bash', {})).toEqual({ type: 'other' });
    expect(claudeTool('TodoWrite', { todos: [] })).toEqual({ type: 'other' });
  });

  it('maps Pi built-in tools', () => {
    expect(piTool('bash', { command: 'ls' })).toEqual({ type: 'command', command: 'ls' });
    expect(piTool('read', { path: '/a' })).toEqual({ type: 'read', path: '/a' });
    expect(piTool('edit', { path: '/a', edits: [{ oldText: 'x', newText: 'y' }] })).toEqual({
      type: 'edit',
      path: '/a',
      edits: [{ old: 'x', new: 'y' }],
    });
    expect(piTool('grep', { pattern: 'p', path: '/w' })).toEqual({
      type: 'search',
      pattern: 'p',
      path: '/w',
    });
    expect(piTool('find', { pattern: '*.md' })).toEqual({ type: 'find', pattern: '*.md' });
    expect(piTool('ls', {})).toEqual({ type: 'list' });
    expect(piTool('silverhand_noop', {})).toEqual({ type: 'other' });
  });

  it('maps a Codex file change', () => {
    expect(codexFileChange({ path: '/a', kind: { type: 'update', move_path: null } })).toEqual({
      type: 'edit',
      path: '/a',
      change: 'update',
    });
    expect(codexFileChange({ path: '/a', kind: 'add' })).toEqual({
      type: 'edit',
      path: '/a',
      change: 'add',
    });
    expect(codexFileChange({})).toEqual({ type: 'other' });
  });

  it('maps Antigravity ACP frames', () => {
    const agy = (kind: string, rawInput: unknown) => acpTool({ kind, title: 't', rawInput });
    expect(agy('execute', { CommandLine: 'echo hi', WaitMsBeforeAsync: 5000 })).toEqual({
      type: 'command',
      command: 'echo hi',
    });
    expect(agy('read', { Path: '/w/a.txt' })).toEqual({ type: 'read', path: '/w/a.txt' });
    expect(agy('edit', { Content: 'x', Path: '/w/b.txt' })).toEqual({
      type: 'write',
      path: '/w/b.txt',
      content: 'x',
    });
    expect(agy('edit', { Path: '/w/b.txt', Replacement: 'y', Target: 'x' })).toEqual({
      type: 'edit',
      path: '/w/b.txt',
      edits: [{ old: 'x', new: 'y' }],
    });
    expect(agy('search', { query: 'antigravity ide' })).toEqual({
      type: 'web_search',
      query: 'antigravity ide',
    });
  });

  it('maps Grok ACP frames, including the kind-less creating frame', () => {
    const grok = (title: string, rawInput: unknown, kind?: string) =>
      acpTool({ title, rawInput, ...(kind ? { kind } : {}) });
    expect(grok('run_terminal_command', { command: 'echo hi', description: 'd' })).toEqual({
      type: 'command',
      command: 'echo hi',
    });
    expect(grok('read_file', { target_file: 'a.txt' })).toEqual({ type: 'read', path: 'a.txt' });
    expect(grok('list_dir', { target_directory: '.' })).toEqual({ type: 'list', path: '.' });
    expect(grok('grep', { glob: '*.txt', '-i': false, pattern: '.*' })).toEqual({
      type: 'search',
      pattern: '.*',
      glob: '*.txt',
    });
    expect(grok('write', { content: 'x', file_path: '/w/b.txt' })).toEqual({
      type: 'write',
      path: '/w/b.txt',
      content: 'x',
    });
    expect(
      grok(
        'Edit `b.txt`',
        { variant: 'SearchReplace', file_path: 'b.txt', old_string: 'x', new_string: 'y' },
        'edit',
      ),
    ).toEqual({ type: 'edit', path: 'b.txt', edits: [{ old: 'x', new: 'y' }] });
  });

  it('maps OpenCode ACP frames', () => {
    const oc = (title: string, kind: string, rawInput: unknown) =>
      acpTool({ title, kind, rawInput });
    expect(oc('bash', 'execute', { command: 'ls', timeout: 15000 })).toEqual({
      type: 'command',
      command: 'ls',
    });
    expect(oc('read', 'read', { filePath: '/a' })).toEqual({ type: 'read', path: '/a' });
    expect(oc('edit', 'edit', { filePath: '/a', oldString: 'x', newString: 'y' })).toEqual({
      type: 'edit',
      path: '/a',
      edits: [{ old: 'x', new: 'y' }],
    });
    expect(oc('glob', 'search', { pattern: '*.ts', path: '/w' })).toEqual({
      type: 'find',
      pattern: '*.ts',
      path: '/w',
    });
    expect(oc('grep', 'search', { pattern: 'p', path: '/w' })).toEqual({
      type: 'search',
      pattern: 'p',
      path: '/w',
    });
    expect(oc('webfetch', 'fetch', { url: 'https://x' })).toEqual({
      type: 'web_fetch',
      url: 'https://x',
    });
    expect(oc('list', 'read', { path: '/w' })).toEqual({ type: 'list', path: '/w' });
    expect(oc('task', 'other', { description: 'look', prompt: 'p' })).toEqual({
      type: 'subagent',
      description: 'look',
    });
    expect(oc('bash', 'execute', { cwd: '/w' })).toEqual({ type: 'other' });
  });

  it('bounds repeated text so a large write cannot double the frame', () => {
    const tool = claudeTool('Write', { file_path: '/a', content: 'x'.repeat(100_000) });
    expect(tool.type === 'write' && tool.content?.length).toBe(4096);
  });

  it('every mapping is a valid shared tool description', () => {
    for (const tool of [
      claudeTool('Edit', { file_path: '/a', old_string: 'x', new_string: 'y' }),
      piTool('ls', {}),
      acpTool({ kind: 'execute', rawInput: { CommandLine: 'x' } }),
      codexFileChange({ path: '/a', kind: 'delete' }),
    ]) {
      expect(ToolSchema.safeParse(tool).success).toBe(true);
    }
  });
});
