/**
 * Strict flag validation for the CLI dispatcher.
 *
 * Each `Command` declares the flags it accepts (`flags` for value-bearing,
 * `booleanFlags` for switches). After argv parsing, the dispatcher calls
 * `assertKnownFlags` to reject any flag the command did NOT declare —
 * with a `did-you-mean` suggestion when the typo is within edit distance
 * 2 of a known flag.
 *
 * This is the answer to the original footgun where an agent typed
 * `--task-numbers 191` (typo for `--task-number`), the parser silently
 * accepted it, the route ignored it, and the agent received the full
 * 100KB task board back. Strict mode turns that into an immediate
 * `validation_error` exit code 1.
 */
import { CliError } from './errors.js';

export interface RemovedFlagSpec {
  /** What the agent should use instead — short token like `'stdin'`,
   *  surfaced in `details.replacement` and the user-facing message. */
  replacement: string;
  /** One-sentence actionable guidance. Suggest the literal command shape
   *  the agent should switch to. Surfaced verbatim in the error message. */
  hint: string;
}

export interface StrictFlagOptions {
  noun: string;
  verb: string;
  flags?: readonly string[];
  booleanFlags?: readonly string[];
  /** Flags that this command USED to accept but no longer does. When
   *  observed, the dispatcher throws a `validation_error` with
   *  `subkind: 'flag_removed'` BEFORE the unknown-flag check runs, so
   *  agents migrating off the old shape see an actionable migration
   *  hint instead of a generic typo error. */
  removedFlags?: Readonly<Record<string, RemovedFlagSpec>>;
}

/**
 * Global flags handled by the dispatcher rather than any individual
 * command. They are accepted on every subcommand as transparent no-ops
 * so agents (and the platform prompt) can rely on `--json` without each
 * command having to redeclare it.
 */
export const GLOBAL_NOOP_FLAGS: ReadonlySet<string> = new Set(['json']);

/** Removed process-wide flags. Kept only as refusal entries so every command
 * reports the same migration error instead of an edit-distance-dependent typo. */
export const GLOBAL_REMOVED_FLAGS: Readonly<Record<string, RemovedFlagSpec>> = {
  'route-conversation-id': {
    replacement: 'none',
    hint: 'the runtime bearer selects its session scope and the agent-global route needs no conversation anchor',
  },
};

/** Throw `CliError` with code `validation_error` if any observed flag
 *  is incompatible with the command:
 *    1. removed-flag check first — `subkind: 'flag_removed'` with the
 *       command's migration `hint`. This wins over the unknown-flag
 *       check so agents passing a deprecated `--foo` see migration
 *       guidance instead of a typo suggestion.
 *    2. unknown-flag check — anything not in
 *       `flags ∪ booleanFlags ∪ removedFlags ∪ GLOBAL_NOOP_FLAGS`
 *       throws `validation_error` with `subkind: 'unknown_flag'` and
 *       `details: { flag, suggestion?, other_unknown? }` so the trace
 *       sink can group typos by their canonical close-match. */
export function assertKnownFlags(observed: Record<string, unknown>, opts: StrictFlagOptions): void {
  // (1) removed-flag check — actionable migration hint takes priority.
  const removedFlags = { ...GLOBAL_REMOVED_FLAGS, ...opts.removedFlags };
  if (Object.keys(removedFlags).length > 0) {
    for (const flag of Object.keys(observed)) {
      const spec = removedFlags[flag];
      if (!spec) continue;
      throw new CliError({
        code: 'validation_error',
        subkind: 'flag_removed',
        message: `--${flag} was removed; ${spec.hint}`,
        retriable: false,
        details: { flag, replacement: spec.replacement },
      });
    }
  }

  // (2) unknown-flag check.
  const known = new Set<string>([
    ...(opts.flags ?? []),
    ...(opts.booleanFlags ?? []),
    ...Object.keys(removedFlags),
    ...GLOBAL_NOOP_FLAGS,
  ]);
  const unknown = Object.keys(observed).filter((k) => !known.has(k));
  if (unknown.length === 0) return;
  const flag = unknown[0]!;
  const suggestion = closestFlag(flag, known);
  const hint = suggestion
    ? `did you mean --${suggestion}?`
    : `run \`silverhand ${opts.noun} ${opts.verb} --help\` for accepted flags`;
  const tail =
    unknown.length > 1
      ? ` (also unrecognized: ${unknown
          .slice(1)
          .map((f) => `--${f}`)
          .join(', ')})`
      : '';
  throw new CliError({
    code: 'validation_error',
    subkind: 'unknown_flag',
    message: `unknown flag --${flag}; ${hint}${tail}`,
    retriable: false,
    details: {
      flag,
      ...(suggestion ? { suggestion } : {}),
      ...(unknown.length > 1 ? { other_unknown: unknown.slice(1) } : {}),
    },
  });
}

/** Return the closest known flag name within edit distance ≤ 2, or null.
 *  A unique known flag extending the input by whole `-` segments wins first
 *  (`--source-message` → `--source-message-id`). */
function closestFlag(input: string, known: ReadonlySet<string>): string | null {
  const extensions = [...known].filter((candidate) => candidate.startsWith(`${input}-`));
  if (extensions.length === 1) return extensions[0]!;
  let best: { name: string; distance: number } | null = null;
  for (const candidate of known) {
    const d = levenshtein(input, candidate);
    if (d <= 2 && (!best || d < best.distance)) {
      best = { name: candidate, distance: d };
    }
  }
  return best?.name ?? null;
}

/** Iterative Levenshtein distance. Two-row variant — O(min(a.length, b.length))
 *  memory. Plenty fast for flag-name comparison. */
function levenshtein(a: string, b: string): number {
  if (a === b) return 0;
  if (a.length === 0) return b.length;
  if (b.length === 0) return a.length;
  let prev = new Array<number>(b.length + 1);
  for (let j = 0; j <= b.length; j += 1) prev[j] = j;
  for (let i = 1; i <= a.length; i += 1) {
    const cur = new Array<number>(b.length + 1);
    cur[0] = i;
    for (let j = 1; j <= b.length; j += 1) {
      const cost = a.charCodeAt(i - 1) === b.charCodeAt(j - 1) ? 0 : 1;
      cur[j] = Math.min(cur[j - 1]! + 1, prev[j]! + 1, prev[j - 1]! + cost);
    }
    prev = cur;
  }
  return prev[b.length]!;
}
