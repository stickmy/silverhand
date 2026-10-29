import { describe, expect, it } from 'vitest';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, resolve } from 'node:path';
import ts from 'typescript';

/**
 * #task:1598 — the app-data PATH AUTHORITY source guard
 * (docs/app-data-layout-v2.md §3). Production code must not construct
 * app-data physical paths outside the Node resolver
 * (`packages/shared/src/app-data-layout.ts`): no fresh `homedir()`
 * app-root derivations, no app-data directory/file literals in any quote
 * style, and no layout-version literals outside the composition
 * selectors / process-boundary wrappers.
 *
 * Exceptions live HERE, as a structured occurrence inventory bound to
 * (file, token, source-line tokens) — never as source-side pragmas
 * (a comment a regressor could add alongside the new hardcode) and never
 * as whole-file exemptions. An inventory entry that stops matching FAILS
 * the guard too, so the inventory can only shrink deliberately; because
 * the binding ignores only trivia and string quote style, editing a line (e.g. appending a
 * second join to it) breaks the exemption instead of hiding behind it.
 */

const REPO_ROOT = resolve(__dirname, '../../..');

const PRODUCTION_ROOTS = [
  'packages/shared/src',
  'packages/host/src',
  'packages/cli/src',
  'apps/desktop/src/main',
].map((p) => join(REPO_ROOT, p));

/** The resolver itself is the one place layout knowledge may live. */
const AUTHORITY_FILE = 'packages/shared/src/app-data-layout.ts';

/**
 * App-data path fragments (matched in single/double/backtick quotes) that
 * only the resolver may spell, plus `homedir()`.
 */
const TOKENS = [
  '.silverhand',
  'attachments-cache',
  'workspace-data',
  'electron-user-data',
  'server-profile.bin',
  'session.bin',
  'silverhand.db',
  'embedded-bootstrap',
  'server-config.json',
  'daemon-config.json',
  'installation_id',
  'daemon.sock',
  'daemon-outbound.sock',
  'runtime-pids',
  'control-plane-server-url',
  'identity.json',
  'proxy-config.json',
  'data-layout.json',
];

interface OccurrenceExemption {
  file: string;
  /** The violation token this exemption is scoped to. */
  token: string;
  /** Source line matched by tokens, ignoring trivia and string quote style. */
  line: string;
  reason: string;
  /** EXACT number of occurrences allowed; more or fewer both fail. */
  count: number;
}

/**
 * The occurrence inventory. Every entry is a genuinely NON-app-data use
 * (workspace-local conventions, user-home probing, safety guards), a
 * canonical constant declaration the resolver consumes. App-data default
 * coordinates are not listed here — they live in the resolver
 * (`resolveStandaloneDaemonPaths`, `defaultAppRoot`, `computersRootDir`).
 */
const OCCURRENCE_INVENTORY: OccurrenceExemption[] = [
  {
    file: 'packages/host/src/drivers/pi.ts',
    token: '.silverhand',
    line: "const legacy = join(ctx.workspacePath, '.silverhand', 'pi-sessions', ctx.agentId);",
    reason:
      'read-only lookup of workspace-local Pi sessions written by earlier daemons; never derives an application root',
    count: 1,
  },
  {
    file: 'packages/shared/src/config.ts',
    token: 'server-config.json',
    line: "export const SERVER_CONFIG_FILENAME = 'server-config.json';",
    reason: 'canonical filename constant declaration the resolver consumes',
    count: 1,
  },
  {
    file: 'packages/shared/src/config.ts',
    token: 'installation_id',
    line: "export const INSTALLATION_ID_FILENAME = 'installation_id';",
    reason: 'canonical filename constant declaration the resolver consumes',
    count: 1,
  },
  {
    file: 'packages/shared/src/config.ts',
    token: 'daemon-config.json',
    line: "export const DAEMON_HOST_CONFIG_FILENAME = 'daemon-config.json';",
    reason: 'canonical filename constant declaration the resolver consumes',
    count: 1,
  },
  {
    file: 'packages/shared/src/config.ts',
    token: 'embedded-bootstrap',
    line: "export const EMBEDDED_BOOTSTRAP_FILENAME = 'embedded-bootstrap';",
    reason: 'canonical filename constant declaration the resolver consumes',
    count: 1,
  },
  {
    file: 'packages/host/src/daemon/agent-materialize/handler.ts',
    token: 'app-data dir join',
    line: 'return join(',
    reason:
      'the deterministic (agent, regular-workspace) L2 bundle resolver: the agent-home subtree layout already lives in this file (`resolveAgentDir`, ONE join onto the caller-supplied agents root — #task:1813 removed the homedir()-seeded fallback), and this extends it with the id-keyed `workspaces/<id>` bundle root (id validated via `assertSafeWorkspaceIdSegment` before it becomes a path segment) — the SOLE runtime path authority for regular workspaces (relocation-v1 §3.2/§7 task 1)',
    count: 1,
  },
  {
    file: 'packages/host/src/daemon/local-outbound-frame-server.ts',
    token: 'homedir()',
    line: 'export function deriveLocalOutboundFrameSocketPath(home = homedir()): string {',
    reason: 'historical test seam signature; the body delegates to the resolver',
    count: 1,
  },
  {
    file: 'packages/host/src/daemon/workspace-registry/workspace-skeleton.ts',
    token: '.silverhand',
    line: "const dotDir = join(workspacePath, '.silverhand');",
    reason: 'workspace-LOCAL .silverhand convention (per-workspace L2 skeleton)',
    count: 1,
  },
  {
    file: 'packages/host/src/runtimes.ts',
    token: 'homedir()',
    line: 'const home = options.homeDir ?? homedir();',
    reason: 'runtime-binary PATH probing in the USER home, not app-data',
    count: 1,
  },
  {
    file: 'apps/desktop/src/main/index.ts',
    token: 'homedir()',
    line: "join(homedir(), '.local', 'bin'),",
    reason: 'user PATH augmentation, not app-data',
    count: 1,
  },
  {
    file: 'apps/desktop/src/main/index.ts',
    token: 'homedir()',
    line: "join(homedir(), '.npm-global', 'bin'),",
    reason: 'user PATH augmentation, not app-data',
    count: 1,
  },
  {
    file: 'apps/desktop/src/main/index.ts',
    token: 'homedir()',
    line: "join(homedir(), '.bun', 'bin'),",
    reason: 'user PATH augmentation, not app-data',
    count: 1,
  },
];

/**
 * Blank out comments while preserving line structure and string bodies.
 * A tiny state machine (not a full lexer): tracks line/block comments and
 * single/double/backtick strings so `'http://x'` is not truncated at `//`
 * and a hardcode inside a template literal is still scanned. Falsifier-
 * tested below like every other detection rule.
 */
export function stripCommentsPreservingLines(source: string): string {
  let out = '';
  type State = 'code' | 'line' | 'block' | 'single' | 'double' | 'template';
  let state: State = 'code';
  for (let i = 0; i < source.length; i += 1) {
    const ch = source[i]!;
    const next = source[i + 1];
    switch (state) {
      case 'code':
        if (ch === '/' && next === '/') {
          state = 'line';
          out += '  ';
          i += 1;
        } else if (ch === '/' && next === '*') {
          state = 'block';
          out += '  ';
          i += 1;
        } else {
          if (ch === "'") state = 'single';
          else if (ch === '"') state = 'double';
          else if (ch === '`') state = 'template';
          out += ch;
        }
        break;
      case 'line':
        if (ch === '\n') {
          state = 'code';
          out += ch;
        } else {
          out += ' ';
        }
        break;
      case 'block':
        if (ch === '*' && next === '/') {
          state = 'code';
          out += '  ';
          i += 1;
        } else {
          out += ch === '\n' ? ch : ' ';
        }
        break;
      case 'single':
      case 'double':
      case 'template': {
        out += ch;
        const closer = state === 'single' ? "'" : state === 'double' ? '"' : '`';
        if (ch === '\\') {
          out += next ?? '';
          i += 1;
        } else if (ch === closer) {
          state = 'code';
        } else if (state !== 'template' && ch === '\n') {
          state = 'code'; // unterminated string literal; bail to code
        }
        break;
      }
    }
  }
  return out;
}

export interface PathAuthorityViolation {
  line: number;
  token: string;
  /** The EXACT trimmed source line the violation starts on. */
  text: string;
}

const FILENAME_CONSTANTS =
  /\b(INSTALLATION_ID_FILENAME|DAEMON_HOST_CONFIG_FILENAME|SERVER_CONFIG_FILENAME|EMBEDDED_BOOTSTRAP_FILENAME|SERVER_CONNECTION_PROFILE_FILENAME)\b/;
const APP_DATA_DIR_JOIN = /['"`](computers|logs|workspaces|blobs)['"`]/;

/** Extract the balanced-paren argument span following `join(`. */
function joinArgSpan(source: string, openParen: number): string {
  let depth = 0;
  const cap = Math.min(source.length, openParen + 500);
  for (let i = openParen; i < cap; i += 1) {
    const ch = source[i];
    if (ch === '(') depth += 1;
    else if (ch === ')') {
      depth -= 1;
      if (depth === 0) return source.slice(openParen, i + 1);
    }
  }
  return source.slice(openParen, cap);
}

/**
 * Pure scanner over one file's source. Exported (and separately
 * falsifier-tested below) so the detection rules themselves are pinned —
 * a guard that cannot demonstrably catch a regression is not a guard.
 * Every match is reported (no early-out per line): two hardcodes on one
 * line are two violations, so a line-level exemption can never authorize
 * a second occurrence added later. NOTE: it takes no exemption input;
 * exemption matching happens against the structured inventory in the
 * guard test, never in the scanner. Join rules scan the balanced-paren
 * SPAN, so a formatter splitting `join(root, CONST)` across lines does
 * not evade detection.
 */
export function scanSourceForPathAuthorityViolations(source: string): PathAuthorityViolation[] {
  const stripped = stripCommentsPreservingLines(source);
  const lines = stripped.split('\n');
  const lineStarts: number[] = [0];
  for (let i = 0; i < stripped.length; i += 1) {
    if (stripped[i] === '\n') lineStarts.push(i + 1);
  }
  const lineOf = (index: number): number => {
    let lo = 0;
    let hi = lineStarts.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (lineStarts[mid]! <= index) lo = mid;
      else hi = mid - 1;
    }
    return lo;
  };
  const out: Array<PathAuthorityViolation & { idx: number }> = [];
  const push = (index: number, token: string): void => {
    const line = lineOf(index);
    out.push({ idx: index, line: line + 1, token, text: lines[line]!.trim() });
  };

  for (const m of stripped.matchAll(/\bhomedir\(\)/g)) {
    push(m.index!, 'homedir()');
  }

  // Join spans: imported filename constants and app-data directory names,
  // scanned over the BALANCED parenthesis span (multiline-safe).
  for (const m of stripped.matchAll(/\bjoin\s*\(/g)) {
    const open = m.index! + m[0].length - 1;
    const span = joinArgSpan(stripped, open);
    if (FILENAME_CONSTANTS.test(span)) {
      push(m.index!, 'imported-filename-constant join');
    }
    if (APP_DATA_DIR_JOIN.test(span)) {
      push(m.index!, 'app-data dir join');
    }
  }

  // Parent inversions reconstructing a root from a child path. `\s*`
  // crosses newlines, so formatter-split forms are caught too.
  for (const m of stripped.matchAll(/dirname\(\s*dirname\(/g)) {
    push(m.index!, 'parent inversion');
  }
  for (const m of stripped.matchAll(/dirname\(\s*(resolve\(\s*)?\w*[Ii]dentity\w*/g)) {
    push(m.index!, 'parent inversion');
  }
  for (const m of stripped.matchAll(/dirname\(\s*(resolve\(\s*)?workspacesRoot/g)) {
    push(m.index!, 'parent inversion');
  }

  // Quoted app-data fragments — every occurrence of every token.
  for (const token of TOKENS) {
    const quoted = new RegExp(`['"\`]${token.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}['"\`]`, 'g');
    for (const m of stripped.matchAll(quoted)) {
      push(m.index!, token);
    }
  }

  out.sort((a, b) => a.idx - b.idx || a.token.localeCompare(b.token));
  return out.map(({ idx: _idx, ...v }) => v);
}

function collectTsFiles(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) {
      if (name === 'node_modules' || name === 'dist' || name === '__tests__') {
        continue;
      }
      collectTsFiles(path, out);
    } else if (name.endsWith('.ts') && !name.endsWith('.d.ts') && !name.endsWith('.test.ts')) {
      out.push(path);
    }
  }
  return out;
}

function sourceLineSignature(line: string): string {
  const scanner = ts.createScanner(ts.ScriptTarget.Latest, true, ts.LanguageVariant.Standard, line);
  const tokens: Array<[ts.SyntaxKind, string]> = [];
  for (let kind = scanner.scan(); kind !== ts.SyntaxKind.EndOfFileToken; kind = scanner.scan()) {
    tokens.push([
      kind,
      kind === ts.SyntaxKind.StringLiteral ? scanner.getTokenValue() : scanner.getTokenText(),
    ]);
  }
  return JSON.stringify(tokens);
}

describe('app-data path authority (docs/app-data-layout-v2.md §3)', () => {
  it('production sources spell app-data path fragments only in the resolver or the structured occurrence inventory', () => {
    const violations: string[] = [];
    const exemptionHits = new Map<OccurrenceExemption, number>();
    for (const root of PRODUCTION_ROOTS) {
      for (const file of collectTsFiles(root)) {
        const rel = relative(REPO_ROOT, file);
        if (rel === AUTHORITY_FILE) continue;
        const source = readFileSync(file, 'utf-8');
        for (const v of scanSourceForPathAuthorityViolations(source)) {
          // Match the whole token sequence, never a substring. Formatting
          // cannot invalidate an exemption; added expressions still do.
          const exemption = OCCURRENCE_INVENTORY.find(
            (e) =>
              e.file === rel &&
              e.token === v.token &&
              sourceLineSignature(e.line) === sourceLineSignature(v.text),
          );
          if (exemption) {
            exemptionHits.set(exemption, (exemptionHits.get(exemption) ?? 0) + 1);
            continue;
          }
          violations.push(`${rel}:${v.line} — ${v.token} (${v.text.slice(0, 120)})`);
        }
      }
    }
    expect(
      violations,
      'app-data paths must come from the shared resolver; a genuinely non-app-data occurrence must be added ' +
        'to OCCURRENCE_INVENTORY in this guard with file + token + source line + reason:\n' +
        violations.join('\n'),
    ).toEqual([]);

    // The inventory can only shrink deliberately: a stale entry fails, and
    // an entry matching MORE occurrences than declared fails too (a new
    // hardcode cannot hide behind an existing exemption).
    const countMismatches = OCCURRENCE_INVENTORY.filter(
      (e) => (exemptionHits.get(e) ?? 0) !== e.count,
    ).map(
      (e) =>
        `${e.file} :: ${e.token} :: ${e.line} — expected ${e.count}, saw ${exemptionHits.get(e) ?? 0}`,
    );
    expect(
      countMismatches,
      'occurrence exemptions must match their declared count exactly (stale or newly-shadowed entries fail)',
    ).toEqual([]);
  });

  // ── Scanner falsifiers: the guard must demonstrably CATCH regressions ──

  it.each([
    ['join( homedir(), ".local", "bin" ),', true],
    ["join(homedir(), '.silverhand', 'bin'),", false],
    ["join(homedir(), '.local', 'bin'), join(homedir(), '.silverhand'),", false],
  ])('exemption binding for %s is %s', (line, allowed) => {
    const original = "join(homedir(), '.local', 'bin'),";
    expect(sourceLineSignature(line) === sourceLineSignature(original)).toBe(allowed);
  });

  it('catches single/double/backtick-quoted app-data literals', () => {
    expect(
      scanSourceForPathAuthorityViolations(`const a = join(root, 'workspace-data');`),
    ).toHaveLength(1);
    expect(
      scanSourceForPathAuthorityViolations(`const b = join(root, "attachments-cache");`),
    ).toHaveLength(1);
    expect(
      scanSourceForPathAuthorityViolations('const c = join(root, `silverhand.db`);'),
    ).toHaveLength(1);
  });

  it('catches fresh homedir() derivations and .silverhand roots — reporting EVERY match on the line', () => {
    expect(
      scanSourceForPathAuthorityViolations(`const root = join(homedir(), '.silverhand');`).map(
        (v) => v.token,
      ),
    ).toEqual(['homedir()', '.silverhand']);
    expect(
      scanSourceForPathAuthorityViolations(`const root = join(base, '.silverhand');`),
    ).toHaveLength(1);
  });

  it('catches computer-profile and daemon-config re-derivations', () => {
    expect(
      scanSourceForPathAuthorityViolations(
        `const p = join(stateRoot, 'computers', id, 'proxy-config.json');`,
      ).map((v) => v.token),
    ).toEqual(['app-data dir join', 'proxy-config.json']);
    expect(
      scanSourceForPathAuthorityViolations(`const q = join(appRoot, "daemon-config.json");`),
    ).toHaveLength(1);
  });

  it('catches imported-filename-constant joins, app-data dir joins, and parent inversions', () => {
    expect(
      scanSourceForPathAuthorityViolations(`const a = join(dir, INSTALLATION_ID_FILENAME);`).map(
        (v) => v.token,
      ),
    ).toEqual(['imported-filename-constant join']);
    expect(
      scanSourceForPathAuthorityViolations(`const b = join(root, DAEMON_HOST_CONFIG_FILENAME);`),
    ).toHaveLength(1);
    expect(scanSourceForPathAuthorityViolations(`const c = join(root, 'computers');`)).toHaveLength(
      1,
    );
    expect(
      scanSourceForPathAuthorityViolations(`const d = join(config.dataDir.value, 'logs');`),
    ).toHaveLength(1);
    expect(
      scanSourceForPathAuthorityViolations(`const e = join(stateRoot, 'workspaces');`),
    ).toHaveLength(1);
    // Nested inversion: both the dirname(dirname( rule and the
    // identity-child rule independently flag it — two reports, not one.
    expect(
      scanSourceForPathAuthorityViolations(`const f = dirname(dirname(daemonIdentityPath));`).map(
        (v) => v.token,
      ),
    ).toEqual(['parent inversion', 'parent inversion']);
    expect(
      scanSourceForPathAuthorityViolations(`const g = dirname(resolve(workspacesRoot));`).map(
        (v) => v.token,
      ),
    ).toEqual(['parent inversion']);
  });

  it('catches formatter-split MULTILINE joins and inversions (span scan, not line scan)', () => {
    expect(
      scanSourceForPathAuthorityViolations(
        ['const a = join(', '  root,', '  DAEMON_HOST_CONFIG_FILENAME,', ');'].join('\n'),
      ).map((v) => v.token),
    ).toEqual(['imported-filename-constant join']);
    expect(
      scanSourceForPathAuthorityViolations(
        ['const b = join(', '  base,', "  'computers',", ');'].join('\n'),
      ).map((v) => v.token),
    ).toEqual(['app-data dir join']);
    expect(
      scanSourceForPathAuthorityViolations(
        ['const c = dirname(', '  dirname(daemonIdentityPath),', ');'].join('\n'),
      ).map((v) => v.token),
    ).toEqual(['parent inversion', 'parent inversion']);
  });

  it('a second hardcode appended to an exempted line is a SEPARATE violation (no whole-line amnesty)', () => {
    // The inventory binds (file, token, exact line); scanner-side, both
    // joins on one line are reported independently, so neither the exact
    // line match nor the count can absorb the newcomer.
    const line = `const dotDir = join(workspacePath, '.silverhand'); const evil = join(root, 'blobs');`;
    expect(scanSourceForPathAuthorityViolations(line).map((v) => v.token)).toEqual([
      '.silverhand',
      'app-data dir join',
    ]);
  });

  it('a source-side pragma comment is NOT an escape: only the structured inventory exempts', () => {
    // The scanner reports the hardcode regardless of any adjacent comment;
    // exemption matching is inventory-only, so adding a comment alongside a
    // new hardcode cannot bypass the gate.
    const withPragma = [
      `// path-authority-exempt: nice try`,
      `const bad = join(homedir(), '.silverhand');`,
    ].join('\n');
    expect(scanSourceForPathAuthorityViolations(withPragma)).toHaveLength(2);
  });

  it('ignores comment mentions (line AND block) but not string contents', () => {
    expect(
      scanSourceForPathAuthorityViolations(`// the registry lives under 'workspace-data'`),
    ).toHaveLength(0);
    expect(
      scanSourceForPathAuthorityViolations(
        ['/**', " * lives under 'workspace-data' and `daemon-config.json`", ' */'].join('\n'),
      ),
    ).toHaveLength(0);
    // A URL string containing `//` must not truncate the rest of the line.
    expect(
      scanSourceForPathAuthorityViolations(
        `const u = 'http://x'; const bad = join(root, 'workspace-data');`,
      ),
    ).toHaveLength(1);
  });
});
