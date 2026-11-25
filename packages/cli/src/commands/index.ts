/**
 * Command catalogue. The CLI is a fixed two-level dispatch:
 *
 *   silverhand <noun> <verb> [flags...]
 *
 * Each leaf is a `Command` with a short usage string (printed in
 * `--help`) and a `run` function that takes the already-loaded env,
 * the BackendClient, and parsed argv. `run` returns a value to write
 * to stdout (as JSON) or `null` for "no body" (e.g. simple ack).
 *
 * Adding a new subcommand: declare a `Command`, register it under
 * the right noun in `COMMANDS`. Help text is generated from the
 * declarations — there is no separate doc to keep in sync.
 */
import type { ParsedArgs } from '../parse.js';
import type { BackendClient } from '../client.js';
import type { CliEnv } from '../env.js';
import type { RemovedFlagSpec } from '../strict-flags.js';
import {
  silverhandCommandNouns,
  type SilverhandCommandNoun,
  type SilverhandCommandNounManifest,
} from '@silverhand/shared/cli-command-manifest';
import { messageCommands, messageFacets } from './message.js';
import { taskCommands } from './task.js';
import { threadCommands } from './thread.js';
import { channelCommands } from './channel.js';
import { memberCommands } from './member.js';
import { workspaceCommands } from './workspace.js';
import { scheduleCommands } from './schedule.js';
import { fileCommands } from './file.js';
import { profileFacets, profileCommands } from './profile.js';

export interface Command {
  /** "send", "create", "list" — the verb part. */
  verb: string;
  /** One-line summary (printed in `silverhand <noun> --help`). */
  summary: string;
  /** Multi-line usage block, including flag descriptions. Printed by
   *  `silverhand <noun> <verb> --help`. */
  help: string;
  /** Value-bearing flag names this command accepts (e.g. `--task-number`,
   *  `--status`). The dispatcher rejects any flag not listed here or in
   *  `booleanFlags` with a `validation_error` so silent typos never
   *  silently widen the request. Including `--help` is unnecessary —
   *  the dispatcher handles it before it reaches `parseArgs`. */
  flags?: readonly string[];
  /** Boolean flag names used by this command. Listed here so:
   *  (1) the parser doesn't consume the next token as a value;
   *  (2) the dispatcher can include them in the known-flag set used
   *      by strict validation. */
  booleanFlags?: readonly string[];
  /** Flags this command USED to accept but no longer does. When the
   *  agent supplies one, the dispatcher throws `validation_error` with
   *  `subkind: 'flag_removed'` and the supplied migration `hint`,
   *  BEFORE the unknown-flag / positional-rest guards run. Use this
   *  when removing a flag in a backwards-incompatible way. */
  removedFlags?: Readonly<Record<string, RemovedFlagSpec>>;
  /** Allow bare positional arguments to reach the command handler.
   *  Keep this rare: most commands must stay flag-only so shell
   *  quoting mistakes fail loudly at the dispatcher. */
  allowPositional?: boolean;
  /** Run the command. Return value (or null) is written to stdout
   *  as a JSON line. Throw `CliError` (or anything else — fromUnknown
   *  will normalize) on failure. */
  run(ctx: { env: CliEnv; client: BackendClient; args: ParsedArgs }): Promise<unknown>;
}

export interface NounGroup {
  noun: string;
  summary: string;
  /** Cross-verb semantics rendered by `silverhand <noun> --help`. */
  guidance?: string;
  commands: readonly Command[];
  /** Optional THIRD dispatch level. A faceted noun groups its verbs under
   *  a facet token: `silverhand <noun> <facet> <verb>`. Used by `profile`
   *  (`profile mcp get|set|validate`) and reserved for future domain-scoped
   *  config (`workspace mcp …`). A faceted noun's flat `commands` is `[]`;
   *  the dispatcher tries facets first, so the two layers never collide. */
  facets?: readonly FacetGroup[];
  /** Verbs this noun USED to accept (or that agents commonly reach for
   *  by mistake). When the agent types one, the dispatcher throws
   *  `validation_error` with `subkind: 'verb_removed'` and the supplied
   *  migration hint pointing at the canonical verb — far more useful
   *  than the generic `unknown verb` line, since agents seeing the
   *  generic line tend to retry with another guess instead of reading
   *  the help. */
  removedVerbs?: Readonly<Record<string, RemovedVerbSpec>>;
}

export interface FacetGroup {
  /** "mcp" — the facet part of `silverhand <noun> <facet> <verb>`. */
  facet: string;
  /** One-line summary (printed in `silverhand <noun> --help`). */
  summary: string;
  commands: readonly Command[];
}

export interface RemovedVerbSpec {
  /** Canonical verb the agent should switch to. Surfaced in the error
   *  message and `details.replacement`. */
  replacement: string;
  /** One-sentence actionable hint. Should name the canonical verb and
   *  any cross-cutting consequence (e.g. flag changes). */
  hint: string;
}

const COMMAND_IMPLEMENTATIONS: Record<SilverhandCommandNoun, readonly Command[]> = {
  message: messageCommands,
  task: taskCommands,
  thread: threadCommands,
  channel: channelCommands,
  member: memberCommands,
  file: fileCommands,
  schedule: scheduleCommands,
  workspace: workspaceCommands,
  profile: profileCommands,
};

const FACET_IMPLEMENTATIONS: Partial<Record<SilverhandCommandNoun, readonly FacetGroup[]>> = {
  message: messageFacets,
  profile: profileFacets,
};

const REMOVED_VERBS: Partial<
  Record<SilverhandCommandNoun, Readonly<Record<string, RemovedVerbSpec>>>
> = {
  // Agents reach for `silverhand message read` because every IM-style
  // CLI uses `read` for "show me messages". Redirect the mistake instead
  // of leaving the agent to guess another verb.
  message: {
    read: {
      replacement: 'history',
      hint:
        'use `silverhand message history` to read recent messages ' +
        '(or pass --before-seq | --after-seq | --around for an anchor). For full-text ' +
        'lookup across the workspace use `silverhand message search`.',
    },
  },
  thread: {
    create: {
      replacement: 'follow',
      hint:
        'isolated thread creation has been removed. Reply to a message with ' +
        "`silverhand message send --target '#conv:<conversation_id>/thread:<root_message_id>'`; " +
        'use `silverhand channel create` only when the user explicitly asks ' +
        'for an isolated channel.',
    },
    close: {
      replacement: 'unfollow',
      hint:
        'reply clusters have no close operation. Use ' +
        '`silverhand thread unfollow --root <message_id>` to stop ' +
        'receiving a cluster, or read a conversation via ' +
        "`silverhand message history --target '#conv:<conversation_id>'`.",
    },
  },
};

/**
 * Bind concrete commands to the shared discovery manifest. Module
 * initialization fails loudly on a missing, duplicate, or extra verb so the
 * prompt, help tree, dispatcher, and route-map guard cannot drift apart.
 */
function bindCommands(
  spec: SilverhandCommandNounManifest,
  implementations: readonly Command[],
): readonly Command[] {
  const byVerb = new Map<string, Command>();
  for (const command of implementations) {
    if (byVerb.has(command.verb)) {
      throw new Error(`duplicate CLI implementation: ${spec.noun} ${command.verb}`);
    }
    byVerb.set(command.verb, command);
  }

  const bound = spec.verbs.map((verb) => {
    const command = byVerb.get(verb);
    if (!command) {
      throw new Error(`missing CLI implementation: ${spec.noun} ${verb}`);
    }
    return command;
  });
  const extras = [...byVerb.keys()].filter((verb) => !spec.verbs.includes(verb));
  if (extras.length > 0) {
    throw new Error(
      `CLI implementations absent from manifest for ${spec.noun}: ${extras.join(', ')}`,
    );
  }
  return bound;
}

function bindFacets(
  spec: SilverhandCommandNounManifest,
  implementations: readonly FacetGroup[],
): readonly FacetGroup[] {
  const byFacet = new Map(implementations.map((facet) => [facet.facet, facet]));
  const bound = (spec.facets ?? []).map((facetSpec) => {
    const implementation = byFacet.get(facetSpec.facet);
    if (!implementation) {
      throw new Error(`missing CLI facet implementation: ${spec.noun} ${facetSpec.facet}`);
    }
    return {
      facet: facetSpec.facet,
      summary: facetSpec.summary,
      commands: bindCommands(
        {
          noun: `${spec.noun} ${facetSpec.facet}`,
          summary: facetSpec.summary,
          verbs: facetSpec.verbs,
        },
        implementation.commands,
      ),
    };
  });
  const extras = [...byFacet.keys()].filter(
    (facet) => !(spec.facets ?? []).some((candidate) => candidate.facet === facet),
  );
  if (extras.length > 0) {
    throw new Error(`CLI facets absent from manifest for ${spec.noun}: ${extras.join(', ')}`);
  }
  return bound;
}

export const COMMAND_GROUPS: readonly NounGroup[] = silverhandCommandNouns().map((spec) => {
  const facets = bindFacets(spec, FACET_IMPLEMENTATIONS[spec.noun as SilverhandCommandNoun] ?? []);
  return {
    noun: spec.noun,
    summary: spec.summary,
    ...(spec.guidance ? { guidance: spec.guidance } : {}),
    commands: bindCommands(spec, COMMAND_IMPLEMENTATIONS[spec.noun as SilverhandCommandNoun]),
    ...(facets.length > 0 ? { facets } : {}),
    ...(REMOVED_VERBS[spec.noun as SilverhandCommandNoun]
      ? { removedVerbs: REMOVED_VERBS[spec.noun as SilverhandCommandNoun] }
      : {}),
  };
});

export function findGroup(noun: string): NounGroup | undefined {
  return COMMAND_GROUPS.find((g) => g.noun === noun);
}

export function findCommand(noun: string, verb: string): Command | undefined {
  return findGroup(noun)?.commands.find((c) => c.verb === verb);
}

export function findFacet(noun: string, facet: string): FacetGroup | undefined {
  return findGroup(noun)?.facets?.find((f) => f.facet === facet);
}

export function findFacetCommand(noun: string, facet: string, verb: string): Command | undefined {
  return findFacet(noun, facet)?.commands.find((c) => c.verb === verb);
}
