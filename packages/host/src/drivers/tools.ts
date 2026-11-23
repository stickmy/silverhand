import type { Tool } from '@silverhand/shared';

/** Native tool frames → the shared {@link Tool} description, per tool vocabulary. */

type Input = Record<string, unknown>;

function record(value: unknown): Input {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Input)
    : {};
}

// `tool` repeats strings already in `input`; the bound keeps a large write from
// doubling the frame past MAX_FRAME_BYTES. The backend clips further for display.
const TEXT_MAX = 4096;

function text(input: Input, ...keys: string[]): string | undefined {
  for (const key of keys) {
    const value = input[key];
    if (typeof value === 'string') return value.slice(0, TEXT_MAX);
  }
  return undefined;
}

function optional<K extends string>(key: K, value: string | undefined): { [P in K]?: string } {
  return (value === undefined ? {} : { [key]: value }) as { [P in K]?: string };
}

function edits(value: unknown, oldKey: string, newKey: string) {
  if (!Array.isArray(value)) return undefined;
  return value.slice(0, 20).map((entry) => ({
    old: text(record(entry), oldKey) ?? '',
    new: text(record(entry), newKey) ?? '',
  }));
}

/** `mcp__<server>__<tool>`, the MCP naming Claude-family runtimes share. */
function mcpTool(name: string): Tool | null {
  const match = /^mcp__(.+?)__(.+)$/.exec(name);
  return match ? { type: 'mcp', server: match[1]!, tool: match[2]! } : null;
}

/** Claude Code tool names and inputs; also spoken by Qoder and Cursor. */
export function claudeTool(name: string, raw: unknown): Tool {
  const input = record(raw);
  const path = text(input, 'file_path', 'notebook_path', 'path');
  switch (name) {
    case 'Bash': {
      const command = text(input, 'command');
      if (command !== undefined) return { type: 'command', command };
      break;
    }
    case 'Read':
      if (path) return { type: 'read', path };
      break;
    case 'Write':
      if (path) return { type: 'write', path, ...optional('content', text(input, 'content')) };
      break;
    case 'Edit':
      if (path) {
        const before = text(input, 'old_string');
        const after = text(input, 'new_string');
        return before === undefined && after === undefined
          ? { type: 'edit', path }
          : { type: 'edit', path, edits: [{ old: before ?? '', new: after ?? '' }] };
      }
      break;
    case 'MultiEdit':
      if (path) {
        const list = edits(input.edits, 'old_string', 'new_string');
        return list ? { type: 'edit', path, edits: list } : { type: 'edit', path };
      }
      break;
    case 'NotebookEdit':
      if (path) return { type: 'edit', path };
      break;
    case 'Grep': {
      const pattern = text(input, 'pattern');
      if (pattern !== undefined) {
        return {
          type: 'search',
          pattern,
          ...optional('path', text(input, 'path')),
          ...optional('glob', text(input, 'glob')),
        };
      }
      break;
    }
    case 'Glob': {
      const pattern = text(input, 'pattern');
      if (pattern !== undefined)
        return { type: 'find', pattern, ...optional('path', text(input, 'path')) };
      break;
    }
    case 'LS':
      return { type: 'list', ...optional('path', text(input, 'path')) };
    case 'Task':
    case 'Agent': {
      const description = text(input, 'description', 'subagent_type');
      if (description !== undefined) return { type: 'subagent', description };
      break;
    }
    case 'Skill': {
      const skill = text(input, 'skill', 'command');
      if (skill !== undefined) return { type: 'skill', name: skill };
      break;
    }
    case 'WebFetch': {
      const url = text(input, 'url');
      if (url !== undefined) return { type: 'web_fetch', url };
      break;
    }
    case 'WebSearch': {
      const query = text(input, 'query');
      if (query !== undefined) return { type: 'web_search', query };
      break;
    }
  }
  return mcpTool(name) ?? { type: 'other' };
}

/** Pi's built-in tools: lowercase names, `path`, and `edits[{oldText,newText}]`. */
export function piTool(name: string, raw: unknown): Tool {
  const input = record(raw);
  const path = text(input, 'path');
  switch (name) {
    case 'bash': {
      const command = text(input, 'command');
      if (command !== undefined) return { type: 'command', command };
      break;
    }
    case 'read':
      if (path) return { type: 'read', path };
      break;
    case 'write':
      if (path) return { type: 'write', path, ...optional('content', text(input, 'content')) };
      break;
    case 'edit':
      if (path) {
        const list =
          edits(input.edits, 'oldText', 'newText') ??
          (text(input, 'oldText') !== undefined || text(input, 'newText') !== undefined
            ? [{ old: text(input, 'oldText') ?? '', new: text(input, 'newText') ?? '' }]
            : undefined);
        return list ? { type: 'edit', path, edits: list } : { type: 'edit', path };
      }
      break;
    case 'grep': {
      const pattern = text(input, 'pattern');
      if (pattern !== undefined) {
        return {
          type: 'search',
          pattern,
          ...optional('path', path),
          ...optional('glob', text(input, 'glob')),
        };
      }
      break;
    }
    case 'find': {
      const pattern = text(input, 'pattern');
      if (pattern !== undefined) return { type: 'find', pattern, ...optional('path', path) };
      break;
    }
    case 'ls':
      return { type: 'list', ...optional('path', path) };
  }
  return { type: 'other' };
}

/**
 * ACP tool calls: the protocol's `kind` plus argument names, which differ per
 * agent (OpenCode `filePath`/`oldString`, Grok `target_file`/`old_string`,
 * Antigravity `Path`/`CommandLine`/`Target`). Grok's creating frame has no
 * `kind`, so argument names alone must identify the common tools.
 */
export function acpTool(update: Record<string, any>): Tool {
  const input = record(update.rawInput);
  const kind = typeof update.kind === 'string' ? update.kind : null;
  const title = typeof update.title === 'string' ? update.title.toLowerCase() : '';
  const command = text(input, 'command', 'CommandLine', 'commandLine');
  const path = text(input, 'filePath', 'file_path', 'target_file', 'path', 'Path');
  const content = text(input, 'content', 'Content');
  const before = text(input, 'oldString', 'old_string', 'Target');
  const after = text(input, 'newString', 'new_string', 'Replacement');
  const pattern = text(input, 'pattern', 'Pattern');
  const url = text(input, 'url', 'Url');
  const query = text(input, 'query', 'Query');

  if (command !== undefined && (kind === null || kind === 'execute')) {
    return { type: 'command', command };
  }
  if (path && (kind === null || kind === 'edit') && (before !== undefined || after !== undefined)) {
    return { type: 'edit', path, edits: [{ old: before ?? '', new: after ?? '' }] };
  }
  if (path && (kind === null || kind === 'edit') && content !== undefined) {
    return { type: 'write', path, content };
  }
  const directory = text(input, 'target_directory');
  // OpenCode reports its `list` tool with kind `read`.
  if (directory !== undefined || title === 'list') {
    return { type: 'list', ...optional('path', directory ?? path) };
  }
  if (path && (kind === 'read' || (kind === null && 'target_file' in input))) {
    return { type: 'read', path };
  }
  if (path && kind === 'edit') return { type: 'edit', path };
  if (pattern !== undefined && (title === 'glob' || title === 'find')) {
    return { type: 'find', pattern, ...optional('path', path) };
  }
  if (pattern !== undefined && (kind === null || kind === 'search')) {
    const glob = text(input, 'glob', 'include');
    return { type: 'search', pattern, ...optional('path', path), ...optional('glob', glob) };
  }
  if (query !== undefined && (kind === 'search' || kind === 'fetch')) {
    return { type: 'web_search', query };
  }
  if (url !== undefined && (kind === null || kind === 'fetch')) return { type: 'web_fetch', url };
  const description = text(input, 'description');
  if (title === 'task' && description !== undefined) return { type: 'subagent', description };
  return { type: 'other' };
}

/** Codex `fileChange` entry: `{ path, kind: { type: 'add'|'delete'|'update' } }`. */
export function codexFileChange(change: unknown): Tool {
  const entry = record(change);
  const path = text(entry, 'path');
  if (!path) return { type: 'other' };
  const kind = record(entry.kind).type ?? entry.kind;
  return kind === 'add' || kind === 'update' || kind === 'delete'
    ? { type: 'edit', path, change: kind }
    : { type: 'edit', path };
}

/** Codex command and web-search items carry their one argument directly. */
export function codexTool(type: 'command' | 'web_search', value: unknown): Tool {
  const arg = text({ value }, 'value');
  if (arg === undefined) return { type: 'other' };
  return type === 'command' ? { type, command: arg } : { type, query: arg };
}
