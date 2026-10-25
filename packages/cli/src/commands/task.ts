/**
 * `silverhand task` — list, create, claim, unclaim, status, edit, detail.
 */
import type { TaskStatus } from '@silverhand/shared';
import {
  CONVERSATION_ADDRESS_PREFIX,
  NAMED_STREAM_ADDRESS_PREFIX,
  TASK_CONTENT_MAX_LEN,
  THREAD_ADDRESS_SEPARATOR,
  TaskBodySchema,
} from '@silverhand/shared';
import { CliError, fromUnknown } from '../errors.js';
import {
  flagBool,
  flagNumber,
  flagNumberArray,
  flagString,
  flagStringArray,
  type ParsedArgs,
} from '../parse.js';
import { decodeStdinUtf8, readAllStdin } from '../stdin.js';
import type { RemovedFlagSpec } from '../strict-flags.js';
import { requireAddressBeforeSideEffects } from '../target-flag.js';
import type { Command } from './index.js';

/**
 * Byte ceiling for a piped task body. A body of at most
 * `TASK_CONTENT_MAX_LEN` UTF-16 code units encodes to at most 3 UTF-8 bytes
 * per code unit (BMP chars are ≤3 bytes/unit; astral chars are 4 bytes over
 * 2 units = 2 bytes/unit), so any VALID body fits under `3 × MAX`. Conversely
 * more than `3 × MAX` bytes is provably more than `MAX` code units and can be
 * rejected without decoding. This bounds `readAllStdin` memory to ~30 KB so a
 * `cat huge_file |` can't OOM the CLI before the exact length check runs; it
 * is DERIVED from the shared cap, not an independent magic number.
 */
const MAX_TASK_BODY_BYTES = TASK_CONTENT_MAX_LEN * 3;

/**
 * Resolve the `--body` switch into the wire body for `task create` / `task
 * edit`. `--body` is VALUE-LESS: the Markdown body is read from stdin
 * (heredoc/pipe), never from an inline argument. This mirrors `message send`
 * and removes the shell-quoting footgun where `--body "line1\nline2"` stored
 * a literal backslash-n instead of a newline.
 *
 *   --body omitted            → undefined  (create: empty body; edit: unchanged)
 *   --body + piped bytes      → the decoded UTF-8 string
 *   --body + EMPTY pipe       → CliError `empty_stdin` — fail-closed, writes
 *                               NOTHING. An empty pipe is almost always a
 *                               forgotten heredoc, and silently clearing a
 *                               task's description on that mistake is data loss
 *                               the agent cannot detect. Clearing is a separate,
 *                               EXPLICIT intent: `task edit --clear-body`.
 *   --body <string> / =string → CliError    (the removed inline form)
 *   --body on a TTY           → CliError     (forgot to pipe)
 *   --body over the size cap  → CliError     (bounded read, then exact check)
 *
 * All the branches that don't need stdin throw BEFORE the blocking
 * `readAllStdin()` so an inline value / TTY / (upstream) missing required flag
 * fails fast instead of hanging on a read whose EOF never comes. The read
 * itself is bounded (`MAX_TASK_BODY_BYTES`) so a pathologically large pipe is
 * rejected mid-read; a within-bound body is then length-checked EXACTLY against
 * `TaskBodySchema` (UTF-16 code units) — the same authority the backend
 * enforces. `verb` only tailors the `empty_stdin` guidance (create has no body
 * to clear; edit points at `--clear-body`).
 */
async function resolveBodySwitch(
  args: ParsedArgs,
  verb: 'create' | 'edit',
): Promise<string | undefined> {
  const raw = args.flags['body'];
  if (raw === undefined) return undefined;
  // The parser binds any inline token (`--body text` / `--body=text`, and a
  // repeated flag → array) directly to `body`. A value-less switch parses to
  // boolean `true`; anything else is the removed inline form.
  if (raw !== true) {
    throw new CliError({
      code: 'validation_error',
      subkind: 'body_requires_stdin',
      message:
        '--body no longer takes an inline value; it reads the Markdown body from stdin. ' +
        "Pipe it with a heredoc — e.g. `silverhand task edit 42 --body <<'SILVERHAND_EOF'\n" +
        '...\nSILVERHAND_EOF`. Omit --body to leave the body unchanged; ' +
        'use `task edit --clear-body` to clear it.',
      retriable: false,
      details: { flag: 'body' },
    });
  }
  if (process.stdin.isTTY) {
    throw new CliError({
      code: 'validation_error',
      subkind: 'stdin_is_tty',
      message:
        '--body reads the body from stdin, but stdin is a TTY. ' +
        "Pipe it — e.g. `silverhand task edit 42 --body <<'SILVERHAND_EOF' ... SILVERHAND_EOF`, " +
        'or use `task edit --clear-body` to clear the body.',
      retriable: false,
    });
  }
  // The read is bounded first (memory guard); the decoded string is then
  // checked EXACTLY against the shared length cap so an over-limit body is
  // rejected client-side with the precise UTF-16 count rather than deferred.
  const body = decodeStdinUtf8(await readAllStdin(MAX_TASK_BODY_BYTES), 'task body');
  if (body === '') {
    // Fail-closed on an empty pipe — NEVER a silent clear. A forgotten heredoc
    // must not wipe a real description with the agent none the wiser; emptying
    // is the explicit, separate `--clear-body` intent (edit only).
    throw new CliError({
      code: 'validation_error',
      subkind: 'empty_stdin',
      message:
        verb === 'edit'
          ? '--body read an empty stdin and will NOT clear the body (a forgotten ' +
            'heredoc must never silently wipe a task description). To clear it, use ' +
            '`silverhand task edit 42 --clear-body`; to leave it ' +
            'unchanged, omit --body; otherwise pipe non-empty Markdown.'
          : '--body read an empty stdin. Omit --body to create the task with an empty ' +
            'description, or pipe non-empty Markdown. (An empty pipe is refused so a ' +
            'forgotten heredoc cannot silently create an empty body.)',
      retriable: false,
      details: { flag: 'body' },
    });
  }
  const parsed = TaskBodySchema.safeParse(body);
  if (!parsed.success) {
    throw new CliError({
      code: 'validation_error',
      subkind: 'content_too_long',
      message:
        `task body is ${body.length} characters, over the maximum of ` +
        `${TASK_CONTENT_MAX_LEN}. Shorten it and pipe it again.`,
      retriable: false,
      details: { max_length: TASK_CONTENT_MAX_LEN, length: body.length },
    });
  }
  return parsed.data;
}

const VALID_STATUSES: readonly TaskStatus[] = [
  'todo',
  'in_progress',
  'in_review',
  'done',
  'closed',
];
const VALID_UPDATE_STATUSES: readonly TaskStatus[] = [
  'todo',
  'in_progress',
  'in_review',
  'done',
  'closed',
];

/**
 * List-only filter macros for `task list --status`. Each macro expands
 * to one or more canonical `TaskStatus` values that the agent passes to
 * the backend filter. Macros are NOT new task states: a task in the DB
 * is always one of `todo|in_progress|in_review|done|closed`. They exist purely
 * to match common natural-language asks ("show me active work", "show
 * everything") with the ergonomic words agents reach for first.
 *
 * `all` is special: its semantic is **"do not filter"**, NOT "include
 * every currently-known status". Treating it as the latter would (a)
 * still emit a `status` query key into `cli-*.log`, and (b) silently
 * exclude any new canonical status added in the future. So `all`
 * short-circuits to `null` and overrides every other token in the same
 * `--status` invocation.
 *
 * `task status --status` (the mutation) MUST NOT accept these — see
 * `LIST_ONLY_STATUS_MACROS` enforcement below.
 */
const STATUS_FILTER_MACROS: Readonly<Record<string, readonly TaskStatus[]>> = {
  active: ['todo', 'in_progress', 'in_review'],
  open: ['todo', 'in_progress', 'in_review'],
  pending: ['todo'],
};
const NO_FILTER_MACRO = 'all';
/** Every token `task status --status` must reject as a list-only macro
 *  (the `STATUS_FILTER_MACROS` keys plus the no-filter sentinel). */
const LIST_ONLY_STATUS_MACROS = new Set([...Object.keys(STATUS_FILTER_MACROS), NO_FILTER_MACRO]);

/** Expand `--status` values into a list of canonical statuses to filter
 *  by, OR `null` if the agent asked for `all` (no filter). Throws
 *  `enum_invalid` on any unknown token. Used only by `task list`;
 *  `task status` uses the stricter `assertSingleUpdateStatus` path.
 *
 *  Why comma split: agents reach for `--status active,in_review` because
 *  every other CLI in their training data does it. Without explicit
 *  support each token would be passed verbatim (`'active,in_review'`)
 *  and fail validation as a single unknown enum.
 *
 *  Why `all` overrides: mixing the no-filter sentinel with narrower
 *  filters (`--status all,done`) is contradictory; the broadest token
 *  wins. Lets the agent express "I just want everything" without
 *  worrying about other tokens already in the call. */
function expandFilterStatuses(raw: readonly string[]): TaskStatus[] | null {
  const out = new Set<TaskStatus>();
  for (const value of raw) {
    for (const part of value
      .split(',')
      .map((s) => s.trim())
      .filter((s) => s.length > 0)) {
      if (part === NO_FILTER_MACRO) return null;
      const macro = STATUS_FILTER_MACROS[part];
      if (macro) {
        for (const m of macro) out.add(m);
        continue;
      }
      if (!VALID_STATUSES.includes(part as TaskStatus)) {
        throw new CliError({
          code: 'validation_error',
          subkind: 'enum_invalid',
          message:
            `--status must be one of ${VALID_STATUSES.join('|')} ` +
            `or a list-only filter macro (${[...LIST_ONLY_STATUS_MACROS].join('|')}); got "${part}"`,
          retriable: false,
          details: {
            flag: 'status',
            value: part,
            allowed: [...VALID_STATUSES, ...LIST_ONLY_STATUS_MACROS],
          },
        });
      }
      out.add(part as TaskStatus);
    }
  }
  return [...out];
}

/**
 * Migration hint shared by every by-number verb that used to take
 * `--task-number`. The operand is now positional, so the flag is a
 * `removedFlags` entry rather than an accepted alias: keeping both would
 * leave two spellings of the same required argument, which is exactly the
 * redundancy this cut removes. `task list` keeps `--task-number` because
 * there it is an OPTIONAL, repeatable FILTER — a different role.
 */
const TASK_NUMBER_FLAG_REMOVED: RemovedFlagSpec = {
  replacement: 'positional <task_number>',
  hint:
    'the task number is now a positional operand — e.g. ' +
    '`silverhand task detail 42`, `silverhand task claim 42 43`. ' +
    '`--task-number` survives only on `silverhand task list`, where it is ' +
    'an optional repeatable filter.',
};

/**
 * Parse the positional `<task_number>` operand(s) for the by-number verbs.
 *
 * Every check here is client-side and runs BEFORE any network call, so a
 * missing / mistyped / over-supplied operand costs no round trip and can
 * never be half-applied. Two arities:
 *
 *   'exactly-one'  detail | edit | unclaim — a point operation
 *   'one-or-more'  claim | status — batch; each task reports its own
 *                  success/failure
 *
 * A task number is a positive integer: it is a per-workspace counter, so
 * `0`, `-3`, `4.5`, and `4abc` are all impossible values rather than
 * merely unlucky ones. Rejecting them here keeps the failure legible
 * (`invalid_task_number` naming the offending token) instead of arriving
 * as a 404 for a number that could not exist.
 */
function parseTaskNumberOperands(
  args: ParsedArgs,
  verb: string,
  arity: 'exactly-one' | 'one-or-more',
): number[] {
  const tokens = args.positional;
  const usage =
    arity === 'one-or-more'
      ? `silverhand task ${verb} <task_number> [<task_number> ...]`
      : `silverhand task ${verb} <task_number>`;
  if (tokens.length === 0) {
    throw new CliError({
      code: 'validation_error',
      subkind: 'missing_task_number',
      message:
        `silverhand task ${verb} requires the task number as a positional ` +
        `operand: \`${usage}\`. Task numbers come from \`task list\` rows ` +
        '(the `task_number` field, i.e. what `#task:N` refers to).',
      retriable: false,
      details: { command: `task ${verb}`, operand: 'task_number' },
    });
  }
  if (arity === 'exactly-one' && tokens.length > 1) {
    throw new CliError({
      code: 'validation_error',
      subkind: 'unexpected_arguments',
      message:
        `silverhand task ${verb} operates on ONE task: \`${usage}\`. ` +
        `Got ${tokens.length} positional tokens ` +
        `(${tokens.map((t) => JSON.stringify(t)).join(', ')}). ` +
        'Call it once per task.',
      retriable: false,
      details: { command: `task ${verb}`, positional: tokens.slice() },
    });
  }
  return tokens.map((token) => {
    // Number() would accept '', ' 12 ', '0x2a', '1e3' — none of which an
    // agent can have read off a task row. Match the digits exactly, then
    // require the parse to survive as an exact integer: a 20-digit string
    // is all digits but rounds (or reaches Infinity) on the way to JSON,
    // so it would leave here as a DIFFERENT number than the caller typed.
    const value = /^\d+$/.test(token) ? Number(token) : Number.NaN;
    if (!Number.isSafeInteger(value) || value < 1) {
      throw new CliError({
        code: 'validation_error',
        subkind: 'invalid_task_number',
        message:
          `task number must be a positive integer; got ${JSON.stringify(token)}. ` +
          `Usage: \`${usage}\`.`,
        retriable: false,
        details: {
          command: `task ${verb}`,
          operand: 'task_number',
          value: token,
        },
      });
    }
    return value;
  });
}

/**
 * Resolve the ONE-task selector for detail | edit | status | unclaim: either
 * the positional `<task_number>` (a board-local number — may pair with the
 * workspace selector naming that board) or the explicit `--id <task_id>` (the
 * task's GLOBAL identity, the `id` field on `task list` / `task detail`
 * output).
 *
 * The two are different coordinate SYSTEMS, not two spellings: a number only
 * means something on a board, an id already determines its board. So `--id`
 * pairs with NOTHING — combining it with a positional number or with
 * `--workspace-id` / `--workspace` is rejected pre-flight (the second
 * coordinate could only be redundant or contradictory), and the type of a
 * bare positional token is never guessed from its shape.
 *
 * Server-side semantics for `--id` (P0-g): unknown, inaccessible, and
 * out-of-scope ids all report the SAME `task_not_found` — the id selector is
 * not an existence oracle. A conversation-scoped session can only
 * dereference ids for `detail` on its own workspace's board; by-id mutations
 * are the agent-scoped lane. When a by-id call reports `task_not_found`
 * against a task you can otherwise see, re-address it by number +
 * workspace selector.
 */
function resolveSingleTaskSelector(
  args: ParsedArgs,
  verb: string,
): { taskNumber: number } | { taskId: string } {
  const rawId = args.flags['id'];
  if (rawId === undefined) {
    const [n] = parseTaskNumberOperands(args, verb, 'exactly-one');
    return { taskNumber: n! };
  }
  if (Array.isArray(rawId)) {
    throw new CliError({
      code: 'validation_error',
      subkind: 'unexpected_arguments',
      message:
        `silverhand task ${verb} operates on ONE task, so --id cannot repeat. ` +
        'Batch by-id operations exist only on `silverhand task claim`.',
      retriable: false,
      details: { command: `task ${verb}`, flag: 'id' },
    });
  }
  if (typeof rawId !== 'string' || rawId === '') {
    throw new CliError({
      code: 'validation_error',
      subkind: 'missing_flag_value',
      message:
        `--id requires the task's global id value — e.g. ` +
        `\`silverhand task ${verb} --id vls9VUa02jTqVVXV\`. ` +
        'Task ids come from the `id` field on `task list` / `task detail` rows.',
      retriable: false,
      details: { command: `task ${verb}`, flag: 'id' },
    });
  }
  if (args.positional.length > 0) {
    throw new CliError({
      code: 'validation_error',
      subkind: 'mutually_exclusive',
      message:
        `--id and the positional <task_number> are mutually exclusive: they are ` +
        'two coordinate systems for the same operand. Pass exactly one.',
      retriable: false,
      details: { command: `task ${verb}`, flags: ['id', 'task_number'] },
    });
  }
  if (flagString(args, 'workspace-id') || flagString(args, 'workspace')) {
    throw new CliError({
      code: 'validation_error',
      subkind: 'mutually_exclusive',
      message:
        '--id excludes --workspace-id/--workspace: a task id already determines ' +
        'its board, so a second coordinate could only be redundant or contradictory. ' +
        'Workspace selectors pair with the positional <task_number> form.',
      retriable: false,
      details: { command: `task ${verb}`, flags: ['id', 'workspace-id', 'workspace'] },
    });
  }
  return { taskId: rawId };
}

const listCommand: Command = {
  verb: 'list',
  summary: 'View the task board(s) you can read.',
  help: `silverhand task list [--target <address> | --workspace <name> | --workspace-id <id>]
                       [--task-number N [--task-number N ...]]
                       [--status STATUS [--status STATUS ...]]
                       [--assignee <agent_id>] [--mine]
                       [--conversation-id <id>] [--limit N]

Returns tasks with server-side filtering. The DEFAULT population depends
on your session's runtime scope:

  conversation-scoped   Your current workspace's board.
  agent-scoped          The UNION of every Board you can read (workspace
                        membership + read_tasks per board), newest first
                        across boards. There is no ambient workspace.

Scope flags only NARROW the population — they never widen it past what
you can read. This command reads; it posts nothing. Because a union can
span boards, interpret each row by its own coordinates: the global \`id\`
is the task's canonical identity, \`board\` + \`task_number\` locate it on
its numbering namespace, and \`conversation\` is where it lives — do NOT
assume rows belong to the current workspace.

Without filters this can be large — pass --task-number / --status /
--mine / --limit to narrow.

Flags:
  --target <address>   List only the tasks of ONE conversation, named by the
                       same address every other command takes
                       (\`#conv:<id>\`, \`${NAMED_STREAM_ADDRESS_PREFIX}<workspace>/<title>\`,
                       \`dm:@handle\`). A /thread: suffix is refused. Excludes
                       --workspace / --workspace-id / --conversation-id.
  --workspace <name>   List one workspace's board, by name. Fails if 0 or
                       >1 workspaces match.
  --workspace-id <id>  List one workspace's board, by id.
  --conversation-id <id>  Narrow to one conversation by bare id.
  --task-number N      Repeatable. Filter to specific task numbers.
                       Last-wins is NOT used; values OR together.
  --status STATUS      Repeatable. Filter by status. Canonical values:
                       todo | in_progress | in_review | done | closed.
                       List-only filter macros (NOT writable states):
                         active|open  → todo + in_progress + in_review
                         pending      → todo
                         all          → no filter (status field omitted
                                        from the request; overrides any
                                        narrower token in the same call)
                       Comma-joined values are accepted, e.g.
                       "--status active,done". Multiple values OR
                       together. Macros are list-only — passing them to
                       \`silverhand task status --status\` fails with
                       enum_invalid.
  --assignee <id>      Filter by assignee agent id.
  --mine               Shortcut for --assignee=<your agent id>.
                       Resolves locally to this agent.
  --limit N            Cap rows returned (max 500). Useful with status
                       filters to peek the board.

stdout: { "tasks": [TaskSummaryDto, ...] }`,
  flags: [
    'task-number',
    'status',
    'assignee',
    'target',
    'workspace',
    'workspace-id',
    'conversation-id',
    'limit',
  ] as const,
  booleanFlags: ['mine'] as const,
  async run({ client, env, args }) {
    const taskNumbers = flagNumberArray(args, 'task-number');
    const statusValues = flagStringArray(args, 'status');
    const expandedStatuses = statusValues ? expandFilterStatuses(statusValues) : undefined;
    const explicitAssignee = flagString(args, 'assignee');
    const mine = flagBool(args, 'mine');
    if (mine && explicitAssignee) {
      throw new CliError({
        code: 'validation_error',
        message: '--mine and --assignee are mutually exclusive',
        retriable: false,
      });
    }
    const assigneeId = mine ? env.agentId : explicitAssignee;
    const limit = flagNumber(args, 'limit');
    if (limit !== undefined && (limit < 1 || limit > 500)) {
      throw new CliError({
        code: 'validation_error',
        message: '--limit must be between 1 and 500',
        retriable: false,
      });
    }
    const target = flagString(args, 'target');
    const conversationId = flagString(args, 'conversation-id');
    const workspace = resolveWorkspaceSelector(args);
    if (target && (conversationId || workspace.workspace_id || workspace.workspace_name)) {
      throw new CliError({
        code: 'validation_error',
        subkind: 'mutually_exclusive',
        message:
          '--target already names one conversation; drop --workspace / --workspace-id / ' +
          '--conversation-id.',
        retriable: false,
        details: { flags: ['target', 'workspace', 'workspace-id', 'conversation-id'] },
      });
    }
    return client.listTasks({
      ...(taskNumbers ? { task_number: taskNumbers } : {}),
      ...(expandedStatuses && expandedStatuses.length > 0 ? { status: expandedStatuses } : {}),
      ...(assigneeId ? { assignee_id: assigneeId } : {}),
      ...(target ? { target } : {}),
      ...workspace,
      ...(flagString(args, 'conversation-id')
        ? { conversation_id: flagString(args, 'conversation-id') }
        : {}),
      ...(limit !== undefined ? { limit } : {}),
    });
  },
};

const createCommand: Command = {
  verb: 'create',
  summary: 'Create one task on one addressed stream.',
  help: `silverhand task create --target <address> --title <title> [--body]
                         [--source-message-id <id>]
                         [--claim]

Create ONE task, filed against the stream --target names. --target and --title are
both required. --body is a value-less switch: when present, the Markdown body is
read from STDIN (pipe or heredoc) — it never takes an inline argument. Omit --body
and the task stores an empty description (the title is NOT duplicated into the
body). To create several tasks, call this once per task.

  silverhand task create --target '#conv:<conversation_id>' \\
    --title "Fix login" --body <<'SILVERHAND_EOF'
  ## Context
  Multi-line Markdown with $vars and \`backticks\` is safe under a heredoc.
  SILVERHAND_EOF

--target <address> (required) is the ONE stream the task belongs to — the same
address grammar every single-stream command takes:

  #conv:<conversation_id>
  ${NAMED_STREAM_ADDRESS_PREFIX}<workspace>/<title>
  dm:@<handle>

optionally followed by /thread:<root_message_id>. Copy the \`target=\` token from
the message you are answering, or spell the id form. There is no default: the
conversation this CLI is authenticated in is where your credential lives, not a
destination.

The address decides EVERYTHING placement-related. Its conversation owns the task:
the row, its number, its board, the \`task_create\` event and every authorization
(membership, closed/archived, capability, source). Naming a conversation other
than the one you are working in IS the cross-conversation case — you finish a
discussion in A and conclude the work belongs in B, so you address B and carry
the context in --title / --body (no messages are copied, and A gets no activity).
A /thread: suffix attributes the task to that reply cluster instead of the main
line; the id must be the cluster's ROOT (a reply inside it is refused, not widened
to it). An address you cannot use — unknown, or a stream you are not in — fails
the same way (code=not_found, target_unresolved), without revealing which.

--source-message-id <id> links the task to the message that prompted it
(permanent 1:1 provenance). The source message MUST live in the same conversation
as the task — i.e. the one --target names. A message can back at most ONE task
ever — for its whole lifetime, regardless of that task's status. A second
create from the same source fails with \`source_conflict\`
(exit 1); the error's \`details.existing_task\` points at the task that already
owns it, so hand off to it (or reopen it) rather than re-creating.

--claim (optional) atomically self-claims the created task (lands in_progress
+ assigned to you) instead of leaving it unassigned todo. Requires the
write_tasks capability in addition to create_tasks. \`task claim\` only claims
tasks that already exist; this is the one-step create-and-claim. Whether it is
allowed depends on your runtime scope: a session anchored to ONE conversation may
create-and-claim only on that stream — addressing any OTHER conversation with
--claim is a DISPATCH that files the task UNCLAIMED so a member of the destination
picks it up, so the combination fails with \`cross_conversation_claim\` (exit 1)
and writes no task; omit --claim there. A session NOT bound to a single
conversation (an agent-scoped runtime) has no anchor for a destination to be
"other" than, so --claim create-and-claims on any stream it may write to.

Pre-flight failures (exit 1):
  --target not supplied → target_required (with \`correct_target\` when you were
                          woken in one stream)
  --target not an address → target_malformed (the retired \`channel\` and
                          \`thread:<message_id>\` spellings named a stream only
                          relative to the current conversation)
  --title not supplied  → missing_required_flag (use the minimal call
                          shown above)
  --message was supplied → flag_removed (use --title, pipe body via --body)
  --body given a value   → body_requires_stdin (drop the value; pipe the body)
  --body + empty stdin   → empty_stdin (omit --body for an empty description;
                           an empty pipe is refused, never a silent empty body)

Create failures (exit 1, structured stderr — NOT printed as success):
  target_unresolved            → code=not_found (unknown stream, or one you are
                                 not in — the two are indistinguishable)
  thread_not_in_target         → code=not_found (/thread: named no reply cluster
                                 root of that conversation)
  source_conflict              → code=conflict, details.existing_task
  cross_conversation_claim     → code=conflict (--claim with a cross-conversation
                                 --target; omit --claim)
  source_message_not_found     → code=not_found
  source_not_accessible        → code=forbidden
  source_conversation_mismatch → code=validation_error (source is in a
                                 different conversation than the task)

stdout (success only): CreateTasksResult JSON.`,
  flags: ['title', 'body', 'source-message-id', 'target'] as const,
  booleanFlags: ['claim'] as const,
  // Agents reach for --message because half of the CLIs in their
  // training data treat it as the canonical "body" flag (gh, git,
  // hub, ...). 5/14 daily report logged one such hit. Routing it
  // through `flag_removed` turns the failure into an actionable
  // migration line that names the right shape, instead of a bare
  // unknown-flag typo with no useful suggestion (Levenshtein from
  // `message` to `title` / `body` is too large for a hint).
  removedFlags: {
    message: {
      replacement: 'title+body',
      hint:
        'silverhand task create takes --title (required); the body is read from ' +
        'stdin via the value-less --body switch: `silverhand task create --title "..." --body <<\'SILVERHAND_EOF\' ... SILVERHAND_EOF`.',
    },
    // Both retired flags named half a coordinate each: `--conversation-id` the
    // conversation with no way to say the thread, `--reply-to` a message with no
    // way to say the conversation (and it widened a mid-cluster id to its root).
    // One address says both, so neither is a rename — the hint has to teach the
    // grammar, not map a flag to a flag (#task:2021 P4-4).
    'conversation-id': {
      replacement: 'target',
      hint:
        'the destination conversation is now part of --target: pass ' +
        "`--target '#conv:<conversation_id>'` (or the workspace/title or dm:@handle " +
        'form). A task belongs to exactly the stream its address names.',
    },
    'reply-to': {
      replacement: 'target',
      hint:
        'a reply cluster is now a suffix on the address: pass ' +
        "`--target '#conv:<conversation_id>/thread:<root_message_id>'`. The id must " +
        'be the cluster ROOT — a reply inside it is refused rather than widened.',
    },
  },
  async run({ env, client, args }) {
    // Address first, and grammar-checked, because everything after this drains
    // stdin: a target that could never land must not cost the caller its piped
    // body (the same ordering `message send` uses, for the same reason).
    const target = requireAddressBeforeSideEffects(flagString(args, 'target'), env, {
      command: 'silverhand task create',
      because:
        'a task belongs to ONE stream, and the conversation this CLI is ' +
        'authenticated in is where your credential lives, not a default board.',
    });
    const title = flagString(args, 'title');
    if (title === undefined) {
      // Daily reports group failures by `error_subkind + details.command +
      // details.flag` rather than by message text. Without a structured
      // subkind here this bucket would be keyed by the literal English
      // sentence — any future copy edit would split the historical bucket.
      throw new CliError({
        code: 'validation_error',
        subkind: 'missing_required_flag',
        message:
          'silverhand task create requires --title. ' +
          "Minimal call: `silverhand task create --target '#conv:<conversation_id>' " +
          '--title "..."`. ' +
          'Add a Markdown body by piping stdin through the value-less --body ' +
          "switch: `--body <<'SILVERHAND_EOF' ... SILVERHAND_EOF`. " +
          'To create a task from an existing message and start it in one ' +
          'call, add `--source-message-id <id> --claim`.',
        retriable: false,
        details: { command: 'task create', flag: 'title' },
      });
    }
    const body = await resolveBodySwitch(args, 'create');
    const sourceMessageId = flagString(args, 'source-message-id');
    const claim = flagBool(args, 'claim');
    const result = await client.createTasks({
      tasks: [
        {
          title,
          ...(body !== undefined ? { body } : {}),
          ...(sourceMessageId ? { source_message_id: sourceMessageId } : {}),
        },
      ],
      ...(claim ? { claim: true } : {}),
      // Total, not conditional: the stream is the request's primary coordinate,
      // so it is always on the wire — there is nothing for the backend to
      // default to.
      target,
    });
    // The route returns 200 with a typed envelope even for business failures
    // (so `source_conflict` keeps its `existing_task`). `create` is all-or-
    // nothing: an `error` in the body is a FAILURE and must exit non-zero with
    // structured stderr — never be printed to stdout as success.
    if ('error' in result) {
      throw createFailureToCliError(result);
    }
    return result;
  },
};

/** Translate a create-op failure envelope into a non-zero CliError. The
 *  stable `code` drives the exit code + retriability; `source_conflict` keeps
 *  its `existing_task` in `details` so the agent can hand off to the winner. */
function createFailureToCliError(result: { error: string; existing_task?: unknown }): CliError {
  switch (result.error) {
    case 'source_conflict':
      return new CliError({
        code: 'conflict',
        subkind: 'source_conflict',
        message:
          'a task already exists for that --source-message-id (permanent 1:1 ' +
          'provenance). Hand off to the existing task (see details.existing_task) ' +
          'or reopen it instead of re-creating.',
        retriable: false,
        details: {
          error: result.error,
          ...(result.existing_task ? { existing_task: result.existing_task } : {}),
        },
      });
    case 'source_not_accessible':
    case 'not_conversation_member':
      return new CliError({
        code: 'forbidden',
        message: `task create failed: ${result.error}`,
        retriable: false,
        details: { error: result.error },
      });
    case 'source_message_not_found':
    case 'conversation_not_found':
    case 'agent_not_found':
      return new CliError({
        code: 'not_found',
        message: `task create failed: ${result.error}`,
        retriable: false,
        details: { error: result.error },
      });
    case 'conversation_closed':
      return new CliError({
        code: 'conflict',
        message: 'task create failed: the conversation is closed',
        retriable: false,
        details: { error: result.error },
      });
    case 'conversation_archived':
      return new CliError({
        code: 'conflict',
        message: 'task create failed: the conversation is archived',
        retriable: false,
        details: { error: result.error },
      });
    case 'cross_conversation_claim':
      // Cross-conversation create is a DISPATCH — the task is filed unclaimed so
      // a member of the destination conversation picks it up. This mapper is the
      // SINGLE source of the human-readable reason; the route/service carry only
      // the typed code.
      return new CliError({
        code: 'conflict',
        subkind: 'cross_conversation_claim',
        message:
          'cannot --claim a task created in another conversation: a cross-conversation ' +
          'create files the task UNCLAIMED so a member of the destination conversation ' +
          'can claim and execute it. Omit --claim.',
        retriable: false,
        details: { error: result.error },
      });
    default:
      // source_conversation_mismatch and any future op error: an input problem.
      return new CliError({
        code: 'validation_error',
        message: `task create failed: ${result.error}`,
        retriable: false,
        details: { error: result.error },
      });
  }
}

/** Resolve the optional `--workspace-id` / `--workspace <name>` pair
 *  into the wire-shape fields. They are mutually exclusive: passing
 *  both is rejected pre-flight with a clear message so the agent does
 *  not have to round-trip to the backend to learn the same. Omitting
 *  both means "use the current workspace" (the legacy default). */
function resolveWorkspaceSelector(args: ParsedArgs): {
  workspace_id?: string;
  workspace_name?: string;
} {
  const workspaceId = flagString(args, 'workspace-id');
  const workspaceName = flagString(args, 'workspace');
  if (workspaceId && workspaceName) {
    throw new CliError({
      code: 'validation_error',
      subkind: 'mutually_exclusive',
      message: '--workspace-id and --workspace are mutually exclusive',
      retriable: false,
      details: { flags: ['workspace-id', 'workspace'] },
    });
  }
  return {
    ...(workspaceId ? { workspace_id: workspaceId } : {}),
    ...(workspaceName ? { workspace_name: workspaceName } : {}),
  };
}

const claimCommand: Command = {
  verb: 'claim',
  summary: 'Claim existing tasks by task number.',
  help: `silverhand task claim <task_number> [<task_number> ...]
                         [--workspace-id <id> | --workspace <name>]
       silverhand task claim --id <task_id> [--id <task_id> ...]

Atomic claim of tasks that ALREADY exist. If a task is already claimed by
someone else, the result entry has success=false with a reason. Always
move on when a claim fails — do not retry.

Claiming a "todo" task auto-advances it to "in_progress".

  silverhand task claim 42          # one task
  silverhand task claim 42 43 44    # batch; per-task success in the result
  silverhand task claim --id vls9VUa02jTqVVXV   # by global identity

One command, ONE selector family: positional numbers resolve on a single
explicit board (workspace selector below), \`--id\` entries resolve by
global identity and take NO workspace selector (each id already
determines its board). Mixing numbers and --id in one call is rejected —
split into two calls. NOTE: by-id mutations are the agent-scoped lane; a
conversation-scoped session gets \`task_not_found\` for ANY --id claim and
should claim by number + workspace selector instead.

To create a task from a message and start it in one call, use
\`silverhand task create --title "..." --source-message-id <id> --claim\`.

Workspace selector (cross-workspace claims):
  --workspace-id <id>   Target a specific workspace by id.
  --workspace <name>    Target by workspace name. Fails if 0 or >1
                        workspaces share the name (the result includes
                        \`workspace_candidates\` on ambiguity).
  (default)             The actor's current workspace.

You must be a member of the resolved workspace; otherwise the claim
fails with \`workspace_membership_required\`.

stdout: ClaimTasksResult JSON.`,
  flags: ['workspace-id', 'workspace', 'id'] as const,
  removedFlags: { 'task-number': TASK_NUMBER_FLAG_REMOVED },
  allowPositional: true,
  async run({ client, args }) {
    const ids = flagStringArray(args, 'id');
    if (ids !== undefined) {
      // A value-less `--id` in a repeat run parses to boolean `true` inside
      // the array (a lone one throws in flagStringArray); empty strings are
      // equally impossible as real ids. Both are the missing-value mistake.
      if (ids.some((id) => typeof id !== 'string' || id === '')) {
        throw new CliError({
          code: 'validation_error',
          subkind: 'missing_flag_value',
          message:
            "--id requires the task's global id value — e.g. " +
            '`silverhand task claim --id vls9VUa02jTqVVXV`. Task ids come from ' +
            'the `id` field on `task list` rows.',
          retriable: false,
          details: { command: 'task claim', flag: 'id' },
        });
      }
      if (args.positional.length > 0) {
        throw new CliError({
          code: 'validation_error',
          subkind: 'mutually_exclusive',
          message:
            'a claim batch carries ONE selector family: positional task numbers ' +
            'OR repeated --id entries, never both (numbers need a board ' +
            'coordinate, ids forbid one). Split into two calls.',
          retriable: false,
          details: { command: 'task claim', flags: ['id', 'task_number'] },
        });
      }
      if (flagString(args, 'workspace-id') || flagString(args, 'workspace')) {
        throw new CliError({
          code: 'validation_error',
          subkind: 'mutually_exclusive',
          message:
            '--id excludes --workspace-id/--workspace: a task id already ' +
            'determines its board. Workspace selectors pair with positional ' +
            'task numbers.',
          retriable: false,
          details: { command: 'task claim', flags: ['id', 'workspace-id', 'workspace'] },
        });
      }
      return client.claimTasks({ task_ids: ids });
    }
    const taskNumbers = parseTaskNumberOperands(args, 'claim', 'one-or-more');
    const selector = resolveWorkspaceSelector(args);
    return client.claimTasks({
      task_numbers: taskNumbers,
      ...selector,
    });
  },
};

const unclaimCommand: Command = {
  verb: 'unclaim',
  summary: 'Release your claim on a task.',
  help: `silverhand task unclaim <task_number>
                           [--workspace-id <id> | --workspace <name>]
       silverhand task unclaim --id <task_id>

Use only when you can no longer work on the task. To signal completion,
use \`silverhand task status <task_number> --status in_review\` (or
done if explicitly approved) instead.

Selector forms — see \`silverhand task detail --help\`. NOTE: by-id
mutations are the agent-scoped lane; a conversation-scoped session gets
\`task_not_found\` for ANY --id mutation and should use the task number +
workspace selector instead.

Workspace selector — see \`silverhand task claim --help\`. You must be
the task's assignee.

stdout: TaskMutationResult JSON.`,
  flags: ['workspace-id', 'workspace', 'id'] as const,
  removedFlags: { 'task-number': TASK_NUMBER_FLAG_REMOVED },
  allowPositional: true,
  async run({ client, args }) {
    const selector = resolveSingleTaskSelector(args, 'unclaim');
    if ('taskId' in selector) {
      return client.unclaimTask({ task_id: selector.taskId });
    }
    const workspace = resolveWorkspaceSelector(args);
    return client.unclaimTask({ task_number: selector.taskNumber, ...workspace });
  },
};

const statusCommand: Command = {
  verb: 'status',
  summary: 'Move a task through its status workflow.',
  help: `silverhand task status <task_number> [<task_number> ...]
                          --status todo|in_progress|in_review|done|closed
                          [--workspace-id <id> | --workspace <name>]
       silverhand task status --id <task_id> --status <status>

Several task numbers move to the same status one by one, in order. One
number prints its TaskMutationResult; several print
{ "results": [{ task_number, ...TaskMutationResult }, ...] } with each
entry's own success — like \`task claim\`, one failure does not stop the rest.

Allowed transitions (you must be the assignee, or the creator of an
unassigned task you created yourself):
  in_progress → in_review : your work is ready for human validation
  in_progress → done      : trivial tasks or after explicit human approval
  in_progress → closed    : close work that will not be handled
  in_review   → done      : after human approval (usually the human does this)
  in_review   → closed    : close work that will not be handled
  in_review   → in_progress : if you need to iterate
  todo        → closed    : close work before it starts
  done        → closed    : close completed work out of the board
  closed      → todo / in_progress / in_review / done : reopen into a concrete state

--status takes a single concrete state. The list-only filter macros
(active|open|pending|all) accepted by \`silverhand task list --status\`
are NOT valid here — passing one fails with enum_invalid pointing back
to \`task list\`.

Selector forms — see \`silverhand task detail --help\`. NOTE: by-id
mutations are the agent-scoped lane; a conversation-scoped session gets
\`task_not_found\` for ANY --id mutation and should use the task number +
workspace selector instead.

Workspace selector — see \`silverhand task claim --help\`. Cross-workspace
status updates require you to be a member of the resolved workspace.

stdout: TaskMutationResult JSON (or { results } for several numbers).`,
  flags: ['status', 'workspace-id', 'workspace', 'id'] as const,
  removedFlags: { 'task-number': TASK_NUMBER_FLAG_REMOVED },
  allowPositional: true,
  async run({ client, args }) {
    const selector =
      args.flags['id'] === undefined
        ? { taskNumbers: parseTaskNumberOperands(args, 'status', 'one-or-more') }
        : resolveSingleTaskSelector(args, 'status');
    const status = flagString(args, 'status') as TaskStatus | undefined;
    if (!status) {
      throw new CliError({
        code: 'validation_error',
        subkind: 'missing_required_flag',
        message:
          'silverhand task status requires --status: ' +
          '`silverhand task status <task_number> --status ' +
          `${VALID_UPDATE_STATUSES.join('|')}\`.`,
        retriable: false,
        details: { command: 'task status', flag: 'status' },
      });
    }
    if (LIST_ONLY_STATUS_MACROS.has(status)) {
      throw new CliError({
        code: 'validation_error',
        subkind: 'enum_invalid',
        message:
          `--status "${status}" is a list-only filter macro for ` +
          '`silverhand task list`, not a writable task status. ' +
          `task status takes a single concrete state: ${VALID_UPDATE_STATUSES.join('|')}.`,
        retriable: false,
        details: {
          flag: 'status',
          value: status,
          allowed: VALID_UPDATE_STATUSES,
          reason: 'list_only_macro',
        },
      });
    }
    if (!VALID_UPDATE_STATUSES.includes(status)) {
      throw new CliError({
        code: 'validation_error',
        subkind: 'enum_invalid',
        message: `--status must be one of ${VALID_UPDATE_STATUSES.join('|')}; got "${status}"`,
        retriable: false,
        details: {
          flag: 'status',
          value: status,
          allowed: VALID_UPDATE_STATUSES,
        },
      });
    }
    if ('taskId' in selector) {
      return client.updateTaskStatus({ task_id: selector.taskId, status });
    }
    const workspace = resolveWorkspaceSelector(args);
    const numbers = 'taskNumbers' in selector ? selector.taskNumbers : [selector.taskNumber];
    if (numbers.length === 1) {
      return client.updateTaskStatus({ task_number: numbers[0]!, status, ...workspace });
    }
    const results = [];
    for (const taskNumber of numbers) {
      const result = await client
        .updateTaskStatus({ task_number: taskNumber, status, ...workspace })
        .catch((err: unknown) => {
          const e = fromUnknown(err);
          return { success: false, code: e.code, reason: e.message };
        });
      results.push({ task_number: taskNumber, ...result });
    }
    return { results };
  },
};

const editCommand: Command = {
  verb: 'edit',
  summary: "Edit a task's title and/or content (last-write-wins).",
  help: `silverhand task edit <task_number>
                        [--title <title>] [--body | --clear-body]
                        [--workspace-id <id> | --workspace <name>]
       silverhand task edit --id <task_id> [--title <title>] [--body | --clear-body]

Edit the current definition (title and/or content) of an existing task.
Provide at least one of --title / --body / --clear-body. --body is a value-less
switch that reads the full new content from STDIN (pipe or heredoc) — it never
takes an inline argument, and an EMPTY stdin is REFUSED (empty_stdin) rather
than silently clearing the body (a forgotten heredoc must never wipe a
description). To clear the content, pass --clear-body (mutually exclusive with
--body). Omit all of them to leave a field unchanged.

  silverhand task edit 42 --body <<'SILVERHAND_EOF'
  ## Acceptance
  Multi-line Markdown with $vars and \`backticks\` is safe under a heredoc.
  SILVERHAND_EOF

  silverhand task edit 42 --clear-body   # clears the body

Selector forms — see \`silverhand task detail --help\`. NOTE: by-id
mutations are the agent-scoped lane; a conversation-scoped session gets
\`task_not_found\` for ANY --id mutation and should use the task number +
workspace selector instead.

Last-write-wins: the edit is applied directly and the most recent
successful write wins — there is no concurrency check. It never changes
status or assignee. The result carries the full updated task detail.

Workspace selector — see \`silverhand task claim --help\`.

stdout: TaskEditResult JSON.`,
  flags: ['title', 'body', 'workspace-id', 'workspace', 'id'] as const,
  booleanFlags: ['clear-body'] as const,
  removedFlags: { 'task-number': TASK_NUMBER_FLAG_REMOVED },
  allowPositional: true,
  async run({ client, args }) {
    const selector = resolveSingleTaskSelector(args, 'edit');
    const title = flagString(args, 'title');
    const clearBody = flagBool(args, 'clear-body') === true;
    const hasBodyFlag = args.flags['body'] !== undefined;
    // --clear-body is the ONLY way to empty a body; it is destructive and must
    // be unambiguous, so it cannot ride alongside --body (which SETS a body).
    if (clearBody && hasBodyFlag) {
      throw new CliError({
        code: 'validation_error',
        subkind: 'mutually_exclusive',
        message:
          '--body and --clear-body are mutually exclusive: --body SETS the body ' +
          'from stdin, --clear-body empties it. Pick one.',
        retriable: false,
        details: { flags: ['body', 'clear-body'] },
      });
    }
    // Presence check on the RAW flags (NOT the resolved body) so a no-op edit
    // fails fast — before `resolveBodySwitch` blocks on stdin.
    if (title === undefined && !hasBodyFlag && !clearBody) {
      throw new CliError({
        code: 'validation_error',
        subkind: 'missing_required_flag',
        message: 'task edit requires at least one of --title, --body, or --clear-body',
        retriable: false,
        details: { command: 'task edit' },
      });
    }
    // --clear-body is the explicit empty; otherwise resolve stdin (which itself
    // refuses an empty pipe). Either yields the wire body (or undefined = leave
    // unchanged when --body is absent).
    const body = clearBody ? '' : await resolveBodySwitch(args, 'edit');
    if ('taskId' in selector) {
      return client.editTask({
        task_id: selector.taskId,
        ...(title !== undefined ? { title } : {}),
        ...(body !== undefined ? { body } : {}),
      });
    }
    const workspace = resolveWorkspaceSelector(args);
    return client.editTask({
      task_number: selector.taskNumber,
      ...(title !== undefined ? { title } : {}),
      ...(body !== undefined ? { body } : {}),
      ...workspace,
    });
  },
};

const detailCommand: Command = {
  verb: 'detail',
  summary: "Read one task's full detail (content + provenance).",
  help: `silverhand task detail <task_number>
                         [--workspace-id <id> | --workspace <name>]
       silverhand task detail --id <task_id>

Read a single task's full definition: title, complete content,
last-editor attribution, the provenance \`source_message_id\`,
and a viewer-resolved \`nav\` target for "view in conversation".

To DISCUSS a task, post where its conversational footprint already is:
\`nav.kind\` \`source\`/\`created_event\` carries \`nav.ref\`, and the address
is \`${CONVERSATION_ADDRESS_PREFIX}<nav.ref.conversation_id>\` plus
\`${THREAD_ADDRESS_SEPARATOR}<nav.ref.thread_root_id>\` when that field is
non-null. Use \`thread_root_id\`, never \`message_id\`: a cluster is addressed
by its ROOT, so a mid-cluster anchor is refused rather than widened.
\`nav.kind = unavailable\` has no honest anchor — post to your own
conversation and cite the task in chat text instead.

Two selector forms (exactly one):
  <task_number>   The task's per-workspace number (the \`task_number\` field
                  on each row from \`task list\`, what \`#task:N\` refers to).
                  Pairs with the workspace selector; defaults to the actor's
                  current workspace.
  --id <task_id>  The task's GLOBAL identity (the \`id\` field on task rows).
                  Takes no workspace selector — the id already determines its
                  board. A conversation-scoped session dereferences ids on its
                  current workspace's board only; an id that is unknown,
                  inaccessible, or out of scope reports the SAME not_found
                  (the id is never an existence oracle).

Workspace selector — see \`silverhand task claim --help\`. Defaults to the
actor's current workspace.

stdout: { "task": TaskDetailDto }`,
  flags: ['workspace-id', 'workspace', 'id'] as const,
  removedFlags: { 'task-number': TASK_NUMBER_FLAG_REMOVED },
  allowPositional: true,
  async run({ client, args }) {
    const selector = resolveSingleTaskSelector(args, 'detail');
    if ('taskId' in selector) {
      return client.getTaskDetail({ task_id: selector.taskId });
    }
    const workspace = resolveWorkspaceSelector(args);
    return client.getTaskDetail({ task_number: selector.taskNumber, ...workspace });
  },
};

export const taskCommands: readonly Command[] = [
  listCommand,
  createCommand,
  claimCommand,
  unclaimCommand,
  statusCommand,
  editCommand,
  detailCommand,
];
