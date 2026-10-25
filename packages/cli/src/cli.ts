#!/usr/bin/env node
/**
 * `silverhand` CLI entry. Dispatch is fixed two-level:
 *   silverhand <noun> <verb> [flags...]
 *
 * Boundary: this CLI never touches SQLite (no native deps, no ABI swap)
 * and does not serve as a long-lived process. In spawned runtimes,
 * freshness-sensitive message sends route through silverhand-daemon
 * so the host-owned runtime coordinator stamps the observed boundary;
 * other commands still call backend internal routes directly.
 */
import { performance } from 'node:perf_hooks';
import { BackendClient } from './client.js';
import { reportPreflightFailure, wasRequestAttempted } from './cli-trace.js';
import { findCommand, findFacet, findGroup } from './commands/index.js';
import type { Command } from './commands/index.js';
import { loadEnv } from './env.js';
import { CliError, exitCodeFor, fromUnknown, writeError } from './errors.js';
import { nounHelp, topLevelHelp, verbHelp, facetHelp, facetVerbHelp } from './help.js';
import { writeJson } from './output.js';
import { parseArgs } from './parse.js';
import { assertKnownFlags } from './strict-flags.js';
import { ensureSystemCaTrusted } from './system-ca.js';

/**
 * Per-process trace context populated as the dispatcher resolves
 * noun → verb → flags. The error handler reads this so the
 * `cli.preflight_failure` payload is as informative as possible
 * (typo telemetry should always show what the agent actually typed).
 */
interface TraceContext {
  noun?: string;
  verb?: string;
  flagsSeen?: string[];
}
const traceCtx: TraceContext = {};

async function main(argv: string[]): Promise<number> {
  // Top-level help / version handling. These never need env, so they
  // run before `loadEnv` to keep `silverhand --help` usable in any
  // shell, regardless of whether the SILVERHAND_* vars are set.
  if (argv.length === 0 || argv[0] === '--help' || argv[0] === '-h') {
    process.stdout.write(topLevelHelp() + '\n');
    return 0;
  }
  if (argv[0] === '--version' || argv[0] === '-V') {
    process.stdout.write('silverhand 0.0.0\n');
    return 0;
  }

  const noun = argv[0]!;
  traceCtx.noun = noun;
  const group = findGroup(noun);
  if (!group) {
    throw new CliError({
      code: 'validation_error',
      message: `unknown noun "${noun}"; run \`silverhand --help\` for available nouns`,
      retriable: false,
    });
  }

  // `silverhand <noun>` or `silverhand <noun> --help` → list verbs.
  if (argv.length === 1 || argv[1] === '--help' || argv[1] === '-h') {
    const help = nounHelp(noun);
    if (!help) {
      throw new CliError({
        code: 'validation_error',
        message: `no help for noun "${noun}"`,
        retriable: false,
      });
    }
    process.stdout.write(help + '\n');
    return 0;
  }

  // Faceted noun (three-level grammar): `silverhand <noun> <facet> <verb>`.
  // A faceted noun (e.g. `profile`) groups its verbs under a facet token;
  // its flat `commands` list is empty, so we resolve facet → verb here
  // instead of the two-level path below.
  if (group.facets && group.facets.length > 0) {
    const facetToken = argv[1]!;
    const facet = findFacet(noun, facetToken);
    if (facet) {
      traceCtx.verb = facetToken;
      // `silverhand <noun> <facet>` or `... --help` → list the facet's verbs.
      if (argv.length === 2 || argv[2] === '--help' || argv[2] === '-h') {
        const help = facetHelp(noun, facetToken);
        if (help) process.stdout.write(help + '\n');
        return 0;
      }
      const fverb = argv[2]!;
      traceCtx.verb = `${facetToken} ${fverb}`;
      const fcmd = facet.commands.find((c) => c.verb === fverb);
      if (!fcmd) {
        throw new CliError({
          code: 'validation_error',
          message: `unknown verb "${fverb}" under "${noun} ${facetToken}"; run \`silverhand ${noun} ${facetToken} --help\``,
          retriable: false,
        });
      }
      return executeCommand(fcmd, argv.slice(3), {
        noun,
        verb: `${facetToken} ${fverb}`,
        helpText: facetVerbHelp(noun, facetToken, fverb),
      });
    }
    // Not a known facet → fall through to flat verb resolution below, so a
    // faceted noun can ALSO carry flat verbs: `profile mcp <verb>` (facet)
    // and `profile get|update` (flat) coexist without colliding (#task:1178).
  }

  const verb = argv[1]!;
  traceCtx.verb = verb;
  const cmd = findCommand(noun, verb);
  if (!cmd) {
    // Removed-verb migration hint takes priority over the generic
    // `unknown verb` line. Without this, agents who type
    // `silverhand message read` (extremely common — every IM CLI in
    // their training data uses `read`) get a typo error and try
    // another guess; the hint redirects them to `history` directly.
    const removed = group.removedVerbs?.[verb];
    if (removed) {
      throw new CliError({
        code: 'validation_error',
        subkind: 'verb_removed',
        message: `silverhand ${noun} ${verb} is not a verb; ${removed.hint}`,
        retriable: false,
        details: { noun, verb, replacement: removed.replacement },
      });
    }
    throw new CliError({
      code: 'validation_error',
      message: `unknown verb "${verb}" under noun "${noun}"; run \`silverhand ${noun} --help\``,
      retriable: false,
    });
  }

  return executeCommand(cmd, argv.slice(2), {
    noun,
    verb,
    helpText: verbHelp(noun, verb),
  });
}

/**
 * Shared command execution tail used by both the two-level and faceted
 * dispatch paths: handle `--help`, parse + strict-validate flags, reject
 * stray positionals, then load env and run. `label.verb` is the verb token
 * for error text — for a faceted command it is the compound `"<facet>
 * <verb>"` so the strict-flag / positional errors read
 * `silverhand profile mcp set …`.
 */
async function executeCommand(
  cmd: Command,
  tail: string[],
  label: { noun: string; verb: string; helpText: string | null },
): Promise<number> {
  const { noun, verb } = label;
  // `silverhand <noun> <verb> --help` → command help.
  if (tail.includes('--help') || tail.includes('-h')) {
    if (label.helpText) process.stdout.write(label.helpText + '\n');
    return 0;
  }

  const args = parseArgs(tail, { booleanFlags: cmd.booleanFlags });
  traceCtx.flagsSeen = Object.keys(args.flags).sort();

  // Strict flag validation: reject any flag the command did not declare.
  // We do this AFTER parseArgs so the parser still records every observed
  // flag — that way the dispatcher can produce a meaningful "unknown flag"
  // error including a did-you-mean suggestion. Without this step, an
  // unknown flag like `--task-numbers 191` (typo for `--task-number`) is
  // silently dropped and the agent thinks it filtered when it didn't.
  // Global flags (e.g. `--json`) are accepted transparently inside
  // `assertKnownFlags` itself.
  //
  // Error priority inside `assertKnownFlags`:
  //   (1) `flag_removed` — most actionable (tells the agent how to migrate)
  //   (2) generic `unknown flag` — typo suggestion
  // Both run BEFORE the positional/rest guard below, so a deprecated
  // `--content x extra` reports `flag_removed` (not `unexpected_arguments`).
  assertKnownFlags(args.flags, {
    noun,
    verb,
    flags: cmd.flags,
    booleanFlags: cmd.booleanFlags,
    removedFlags: cmd.removedFlags,
  });

  // Dispatcher-level hard guard (A1): commands are flag-only unless they opt in
  // to a positional surface; there, tokens after `--` are ordinary operands
  // (POSIX end-of-options). Stray tokens on a flag-only command — typically a
  // word-split value — are refused rather than silently dropped.
  if (cmd.allowPositional) {
    args.positional.push(...args.rest.splice(0));
  } else if (args.positional.length > 0 || args.rest.length > 0) {
    const parts: string[] = [];
    if (args.positional.length > 0) parts.push(`positional ${JSON.stringify(args.positional)}`);
    if (args.rest.length > 0) parts.push(`after-\`--\` ${JSON.stringify(args.rest)}`);
    throw new CliError({
      code: 'validation_error',
      subkind: 'unexpected_arguments',
      message:
        `silverhand ${noun} ${verb} accepts flags only; got unexpected ${parts.join(' and ')}. ` +
        'If a value contains spaces or quotes, pipe it via stdin or use a quoted heredoc.',
      retriable: false,
      details: { positional: args.positional, rest: args.rest },
    });
  }

  // Lazy: only load env when we're actually about to make a request.
  // Help/version never reach this line.
  const env = loadEnv(process.env);
  const client = new BackendClient(env);
  const result = await cmd.run({ env, client, args });
  if (result !== undefined && result !== null) {
    writeJson(result);
  }
  return 0;
}

// Trust the OS / corporate CA store as the very first thing this process does,
// before ANY network I/O. This must precede `void main(...)` so it also covers
// the top-level rejection handler's out-of-band `reportPreflightFailure()`
// fetch — not just command business requests. `setDefaultCACertificates()` is
// process-local and the daemon spawns this CLI as a separate process, so the
// main/daemon activation never reaches us. See system-ca.ts (#task:1464).
ensureSystemCaTrusted();

const startedAt = performance.now();
void main(process.argv.slice(2)).then(
  (code) => {
    process.exit(code);
  },
  async (err) => {
    const cliErr = fromUnknown(err);
    writeError(cliErr, { json: true });
    // Best-effort: tell the backend about a CLI-side failure that
    // never reached an internal route, so `cli-*.log` shows the
    // typo / missing-flag pattern. Suppressed when a business
    // request was already attempted (the backend has its own
    // `cli.invoke` line for that case).
    if (!wasRequestAttempted()) {
      await reportPreflightFailure({
        noun: traceCtx.noun,
        verb: traceCtx.verb,
        flags_seen: traceCtx.flagsSeen,
        error_code: cliErr.code,
        ...(cliErr.subkind ? { error_subkind: cliErr.subkind } : {}),
        error_message: cliErr.message.split('\n', 1)[0]?.slice(0, 500),
        duration_ms: Math.round((performance.now() - startedAt) * 1000) / 1000,
      });
    }
    process.exit(exitCodeFor(cliErr.code));
  },
);
