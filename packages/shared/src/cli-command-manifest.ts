/**
 * Agent-facing Silverhand command discovery surface.
 *
 * This manifest is the single source of truth for the command tree that an
 * agent can discover. The CLI binds concrete implementations to every entry,
 * the platform prompt renders the domain index from it, and backend
 * observability validates route attribution against the resulting CLI groups.
 *
 * Keep flag-level usage and recovery examples in the CLI command help. This
 * file only owns stable domain/noun/verb discovery metadata.
 */

export interface SilverhandCommandFacetManifest {
  facet: string;
  summary: string;
  verbs: readonly string[];
}

export interface SilverhandCommandNounManifest {
  noun: string;
  summary: string;
  /** Noun-level semantics shared by its verbs. Rendered by noun `--help`,
   *  never by the compact platform-prompt discovery index. */
  guidance?: string;
  verbs: readonly string[];
  facets?: readonly SilverhandCommandFacetManifest[];
}

export interface SilverhandCommandDomainManifest {
  domain: string;
  when: string;
  nouns: readonly SilverhandCommandNounManifest[];
}

export const SILVERHAND_COMMAND_DOMAINS = [
  {
    domain: 'Messages',
    when: 'Communicate, inspect history, search prior decisions, react, or answer options.',
    nouns: [
      {
        noun: 'message',
        summary: 'Send messages, read history, search visible text.',
        guidance:
          'Chat references: <silverhand://agent/<name>> or ' +
          '<silverhand://human/<name>> addresses a participant (bare @<name> is ' +
          'display only); #<channel> or ' +
          '#conv:<id> names a place; #task:<n> names a task. These sigils belong ' +
          'in message TEXT, not inside code. CLI arguments take raw values, with ' +
          'one exception: an address flag (--target) takes a whole delivery ' +
          'address verbatim, #conv: prefix and all. ' +
          'Wrap URLs next to non-ASCII punctuation in <...> or Markdown link syntax.',
        verbs: ['send', 'stale-resolve', 'stale-receive', 'react', 'answer', 'history', 'search'],
        facets: [
          {
            facet: 'options',
            summary: 'Replace the answer options of your own unanswered message.',
            verbs: ['update'],
          },
        ],
      },
    ],
  },
  {
    domain: 'Tasks',
    when: 'Coordinate substantive work and keep task state current.',
    nouns: [
      {
        noun: 'task',
        summary: 'View, create, claim, assign, release, and update task board entries.',
        guidance:
          'Claim substantive untracked work before starting. A failed claim means ' +
          'stop, and task state must be updated before reporting completion.',
        verbs: ['list', 'create', 'claim', 'assign', 'unclaim', 'status', 'edit', 'detail'],
      },
    ],
  },
  {
    domain: 'Conversations',
    when: 'Follow reply clusters, manage channels, or add existing workspace members.',
    nouns: [
      {
        noun: 'thread',
        summary: 'Follow or unfollow reply clusters under messages.',
        verbs: ['follow', 'unfollow'],
      },
      {
        noun: 'channel',
        summary:
          "Create a top-level channel, or list the agent's channels and DMs alongside public channels it has not joined.",
        guidance:
          'Create a channel only when the user explicitly requests a separate top-level conversation.',
        verbs: ['create', 'list'],
      },
      {
        noun: 'member',
        summary: 'Add an existing workspace member to an addressed conversation.',
        verbs: ['add'],
      },
    ],
  },
  {
    domain: 'Workspace',
    when: 'Discover the workspaces this agent can access.',
    nouns: [
      {
        noun: 'workspace',
        summary: 'List the workspaces this agent can reach.',
        verbs: ['list'],
      },
    ],
  },
  {
    domain: 'Schedules',
    when: 'Arrange a future self-wake when no user message will trigger the follow-up.',
    nouns: [
      {
        noun: 'schedule',
        summary: 'Create, list, or cancel self-wake schedules.',
        guidance: 'Schedule only when no human message will naturally trigger the follow-up.',
        verbs: ['create', 'list', 'cancel'],
      },
    ],
  },
  {
    domain: 'Files',
    when: 'Attach generated artifacts or resolve attachments you received.',
    nouns: [
      {
        noun: 'file',
        summary: 'Upload a local file, update one you own, or resolve an attachment id.',
        verbs: ['upload', 'update', 'view'],
      },
    ],
  },
  {
    domain: 'Profile',
    when: 'Manage your profile metadata and MCP configuration, or inspect another agent profile.',
    nouns: [
      {
        noun: 'profile',
        summary:
          "Manage the executing agent's profile metadata and MCP configuration; `get --handle <name>` reads another agent's profile.",
        verbs: ['get', 'update'],
        facets: [
          {
            facet: 'mcp',
            summary: "Manage this agent's custom MCP servers.",
            verbs: ['get', 'set', 'add', 'remove', 'clear', 'validate', 'probe'],
          },
        ],
      },
    ],
  },
] as const satisfies readonly SilverhandCommandDomainManifest[];

export type SilverhandCommandDomain = (typeof SILVERHAND_COMMAND_DOMAINS)[number];
export type SilverhandCommandNoun = SilverhandCommandDomain['nouns'][number]['noun'];

export function silverhandCommandNouns(): readonly SilverhandCommandNounManifest[] {
  const nouns: SilverhandCommandNounManifest[] = [];
  for (const domain of SILVERHAND_COMMAND_DOMAINS) {
    nouns.push(...domain.nouns);
  }
  return nouns;
}
