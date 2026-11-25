/**
 * `silverhand message` — send, history, search.
 */
import type { AgentSendStaleResolutionReason, MessageOptions, Reference } from '@silverhand/shared';
import {
  MessageOptionsSchema,
  NAMED_STREAM_ADDRESS_PREFIX,
  conversationAddressOf,
  MAX_MESSAGE_CONTENT_LABEL,
  messageContentExceedsLimit,
} from '@silverhand/shared';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  HostClient,
  HostClientError,
  type AgentMessageSendParams,
  type AgentStaleReceiveParams,
  type AgentStaleReceiveResult,
  type AgentStaleResolveParams,
} from '@silverhand/host';
import { CliError, mapHttpError } from '../errors.js';
import type { BackendClient } from '../client.js';
import type { CliEnv } from '../env.js';
import { flagBool, flagNumber, flagString, flagStringArray, type ParsedArgs } from '../parse.js';
import { decodeStdinUtf8, readAllStdin } from '../stdin.js';
import { reachHelpParagraphSection } from '../reach-help.js';
import { requireAddressBeforeSideEffects, requireTargetFlag } from '../target-flag.js';
import type { Command, FacetGroup } from './index.js';

const sendCommand: Command = {
  verb: 'send',
  summary: 'Send a message to one addressed stream.',
  help: `silverhand message send --target <address>
                          [--attachment-id <id> ...]
                          [--option <id>=<label> ... [--multi]]
                          [--stale-resolution unaffected|revised]
                          [--anyway]

Reads the message body from STDIN and sends it to the stream named by
\`--target\`. A message must include text or at least one attachment.
Pipe text via stdin; attachment-only sends may omit stdin and pass
\`--attachment-id <id>\`.

\`--target\` is REQUIRED and says where the message goes. There is no
"here": the conversation this CLI is authenticated in is where your
credential lives, not a default destination — one runtime serves many
streams, so an omitted target would mean whichever one the reader
assumed. To post to the stream you were woken in, copy the \`target=\`
token from the message header you are answering.

${reachHelpParagraphSection('message_send')}The old \`--content <text>\` flag was removed because shell quoting
of a value containing \`"\`, backticks, or \`$\` silently truncated
the body before reaching this CLI.

Two recommended call shapes:

  # short body
  printf "%s" "hello" | silverhand message send --target '#conv:QVI9x6vU_K1a9oN9'

  # multi-line / contains quotes / contains $/backticks
  silverhand message send --target '#conv:QVI9x6vU_K1a9oN9' <<'SILVERHAND_EOF'
  Any text. Quotes, backticks, $vars, multi-line markdown — all safe.
  SILVERHAND_EOF

  # attachment-only
  silverhand message send --target '#conv:QVI9x6vU_K1a9oN9' --attachment-id att_123

  # into the reply cluster rooted at message QVI9x6vU_K1a9oN9
  printf "%s" "in-thread answer" | silverhand message send \\
    --target '#conv:QVI9x6vU_K1a9oN9/thread:Rp9mXk2fLb7QwZ0t'

  # by name, and to a direct pair
  silverhand message send --target '#silverhand/UI%2FUX'
  silverhand message send --target 'dm:@shiki'

The single-quoted heredoc delimiter (\`<<'SILVERHAND_EOF'\`) disables
shell expansion of \`$\`, backticks, and \`\\\`. The body is read as
raw bytes, decoded as UTF-8, and posted to the backend with **no
normalization** — no BOM stripping, no CRLF→LF, no trim.

Bodies larger than 64KiB (UTF-8 bytes) are automatically uploaded as a
\`text/plain\` attachment; the visible body becomes a short pointer
line. The backend rejects oversized raw content with 413.

Flags:
  --target <address>    REQUIRED. The stream this message goes to. Three
                        spellings, and quote it — a leading \`#\` starts a
                        shell comment:
                          #conv:<conversation_id>
                                      the form every stream always has.
                          #<workspace>/<title>
                                      the readable coordinate; a space in a
                                      name is %20, a slash is %2F.
                          dm:@<handle>
                                      your direct pair with that principal.
                                      Sending to the main line opens the pair
                                      if it does not exist yet. Agent endpoints
                                      must use agent scope. Conversation-scoped
                                      agents must communicate in a shared channel;
                                      existing DM history remains read-only.
                        Append \`/thread:<root_message_id>\` to any of them to
                        post into a reply cluster. A cluster is addressed by
                        its ROOT — the id under \`thread_root_id\`, or the
                        message id itself when you are answering a main-line
                        message. Answering a reply with that reply's own id
                        names a stream that does not exist (threads do not
                        nest) and is refused.
  --attachment-id <id>  Optional, repeatable. Attachment ids returned by
                        \`silverhand file upload\`. All ids must belong
                        to the same channel as the addressed stream.
  --option <id>=<label> Optional, repeatable (2-10). Attach answer options
                        readers can pick from. Use them only when the
                        question has a few clear, mutually comparable
                        alternatives; ask open questions in plain text.
                        <id> is lowercase [a-z0-9_-], up to 16 chars, unique
                        in the message. Readers can always answer with free
                        "other" text, so do not add a catch-all option.
                        Each answer posts a system event that mentions you.
  --multi               With --option: readers may pick several options.
                        Default is single choice.
  --stale-resolution    Optional. After a message_stale conflict, explain
                        why this retry is being sent:
                        "unaffected" means the original draft still applies;
                        "revised" means newer messages changed the draft.
                        Missing reason never blocks sending; it is logged as
                        unattributed telemetry when a pending stale event exists.
  --anyway              Bypass this send's stale-message freshness guard.
                        Use only after inspecting a message_stale conflict,
                        or when the message must be posted despite stale risk.

Pre-flight failures (exit 1):
  --target was omitted          → target_required (when you were woken in one
                                   stream, the reply names it as
                                   \`correct_target\`; with no ambient stream
                                   there is nothing to suggest and none is given)
  --target is not an address    → target_malformed (fix the spelling; nothing
                                   was looked up and nothing was sent)
  --content was supplied         → flag_removed (use stdin instead)
  --text / --message was supplied → flag_removed (use stdin instead;
                                    these were never accepted but are
                                    intercepted with a stdin template)
  stdin is a TTY and no attachment → stdin_is_tty (no body provided)
  stdin is empty and no attachment → empty_stdin
  stdin is not UTF-8             → invalid_utf8 (re-encode upstream)

stdout: { "message_id": "<id>", "message_seq": <seq>, "thread_root_id": <id|null> }

\`unresolved_references\` is added to that object ONLY when some reference in
the body addressed nobody — the handle names no participant of the conversation
the message landed in. The send still succeeded and everyone it did address
received it; fix the spelling (or add the member) and send again if that
reference mattered. No reason is given per token, and its absence means
nothing was reported, not that every reference landed.`,
  flags: ['target', 'attachment-id', 'option', 'stale-resolution'] as const,
  booleanFlags: ['anyway', 'multi'],
  removedFlags: {
    'reply-to': {
      replacement: '--target',
      hint:
        'a reply is addressed like any other send, by the stream it lands in: ' +
        "`--target '#conv:<conversation_id>/thread:<root_message_id>'`. The root " +
        'is the cluster id (`thread_root_id`), not the message you are answering.',
    },
    attachment: {
      replacement: '--attachment-id',
      hint:
        'use --attachment-id <id> (repeatable) — the value is an attachment ' +
        'id, and the -id spelling is its canonical name here',
    },
    content: {
      replacement: 'stdin',
      hint:
        'pipe the body via stdin instead — e.g. ' +
        "`silverhand message send <<'SILVERHAND_EOF' ... SILVERHAND_EOF` " +
        'or `printf "%s" "..." | silverhand message send`',
    },
    // `--text` / `--message` were never accepted, but agents reach for
    // them by default (every other CLI in their training data uses one
    // of these names). Routing them through the `flag_removed` hint
    // path turns the failure into an actionable migration line that
    // shows the heredoc template — much better than a generic
    // unknown-flag typo suggestion.
    text: {
      replacement: 'stdin',
      hint:
        'silverhand message send only accepts the body on stdin. ' +
        'Use a heredoc — e.g. ' +
        "`silverhand message send <<'SILVERHAND_EOF' ... SILVERHAND_EOF` " +
        'or `printf "%s" "..." | silverhand message send`',
    },
    message: {
      replacement: 'stdin',
      hint:
        'silverhand message send only accepts the body on stdin. ' +
        'Use a heredoc — e.g. ' +
        "`silverhand message send <<'SILVERHAND_EOF' ... SILVERHAND_EOF` " +
        'or `printf "%s" "..." | silverhand message send`',
    },
  },
  async run({ env, client, args }) {
    let attachmentIds = flagStringArray(args, 'attachment-id');
    const hasAttachments = (attachmentIds?.length ?? 0) > 0;
    let content = '';

    // Validate ALL flags BEFORE touching stdin. `readAllStdin()` below
    // blocks until the pipe closes; if an invalid flag (e.g. a bad
    // --stale-resolution value) were only rejected afterwards, an
    // inherited never-closing stdin pipe would hang the process forever
    // instead of failing fast with a typed error.
    const target = requireAddressBeforeSideEffects(flagString(args, 'target'), env, {
      command: 'silverhand message send',
      because:
        'the conversation this CLI is authenticated in is where your ' +
        'credential lives, not a default destination.',
    });
    const anyway = flagBool(args, 'anyway') ?? false;
    const options = parseOptionFlags(args);
    const staleResolution = parseSendStaleResolution(flagString(args, 'stale-resolution'));
    if (anyway && staleResolution !== undefined) {
      throw new CliError({
        code: 'validation_error',
        message:
          '--anyway already records stale resolution as force_send; ' +
          'do not also pass --stale-resolution',
        retriable: false,
      });
    }

    // Pre-flight: a TTY on stdin means the agent forgot to pipe a body.
    // Failing fast (rather than blocking on a read that will never
    // arrive) keeps the agent's exit path predictable. Attachment-only
    // sends are the one intentional no-stdin path.
    if (process.stdin.isTTY && !hasAttachments) {
      throw new CliError({
        code: 'validation_error',
        subkind: 'stdin_is_tty',
        message:
          'silverhand message send reads the body from stdin, but stdin is a TTY. ' +
          'Pipe the body — e.g. `printf "%s" "hello" | silverhand message send` ' +
          "or `silverhand message send <<'SILVERHAND_EOF' ... SILVERHAND_EOF`.",
        retriable: false,
      });
    }

    if (!process.stdin.isTTY) {
      // Read raw bytes; do NOT setEncoding so any UTF-8 surrogate pairs
      // / multi-byte sequences cross chunk boundaries cleanly.
      const buffer = await readAllStdin();

      if (buffer.byteLength === 0 && !hasAttachments) {
        throw new CliError({
          code: 'validation_error',
          subkind: 'empty_stdin',
          message:
            'silverhand message send received 0 bytes on stdin. ' +
            'Pipe a non-empty body, or attach at least one file.',
          retriable: false,
        });
      }

      // UTF-8 decode + zero normalization. Strict round-trip: any byte
      // sequence that is not valid UTF-8 is rejected (never silently
      // substituted with U+FFFD), so the bytes the backend/DB stores always
      // equal what the agent piped. No BOM strip, no CR/LF translation, no
      // trim — see `decodeStdinUtf8`.
      content = decodeStdinUtf8(buffer, 'message body');

      // Oversized bodies never enter the message pipeline (they would be
      // rejected by the backend's content byte cap, and multi-MB text
      // pins the desktop renderer). Convert to a text attachment
      // automatically and keep a short pointer line as the visible body.
      if (messageContentExceedsLimit(content)) {
        const uploaded = await uploadBodyAsAttachment(client, buffer, target);
        attachmentIds = [...(attachmentIds ?? []), uploaded.id];
        content =
          `Sent the long body as a text attachment (${uploaded.filename}, ` +
          `${formatByteSize(uploaded.size)}; the body is over ${MAX_MESSAGE_CONTENT_LABEL})`;
      }
    }

    const daemonInput = {
      runtime_scope: runtimeIntentScope(env),
      agent_id: env.agentId,
      content,
      target,
      ...(attachmentIds && attachmentIds.length > 0 ? { attachment_ids: attachmentIds } : {}),
      ...(options ? { options } : {}),
      ...(anyway ? { freshness_bypass: true } : {}),
      ...(staleResolution ? { stale_resolution_reason: staleResolution } : {}),
    };
    if (env.daemonSocketPath) {
      return await sendViaDaemon(env.daemonSocketPath, daemonInput);
    }

    // Migration fallback for non-daemon/manual CLI contexts. This path has
    // no daemon runtime scope, so freshness is evaluated by the backend from
    // the caller's own delivery state alone; there is no sidecar cursor to
    // carry.
    return await client.sendMessage({
      content,
      target,
      ...(anyway ? { freshness_bypass: true } : {}),
      ...(attachmentIds && attachmentIds.length > 0 ? { attachment_ids: attachmentIds } : {}),
      ...(options ? { options } : {}),
    });
  },
};

const staleResolveCommand: Command = {
  verb: 'stale-resolve',
  summary: 'Report that a stale draft no longer needs to be sent.',
  help: `silverhand message stale-resolve --stale-event-id <id> --resolution obsolete

Reports an observability-only resolution for one message_stale conflict
without sending a message. Copy details.stale_event_id from that conflict.
Use this when the newer messages make the draft unnecessary.

Flags:
  --stale-event-id <id>  Required. Exact stale draft object to resolve.
  --resolution obsolete  Required. Records that the stale draft is obsolete.

stdout: { "resolved": true } or { "resolved": false, "reason": "no_pending_stale_event" }`,
  flags: ['stale-event-id', 'resolution'] as const,
  removedFlags: {
    'event-id': {
      replacement: 'stale-event-id',
      hint:
        'copy details.stale_event_id from the message_stale response and pass ' +
        '--stale-event-id <id>',
    },
  },
  async run({ env, args }) {
    const staleEventId = flagString(args, 'stale-event-id');
    if (!staleEventId) {
      throw new CliError({
        code: 'validation_error',
        message: '--stale-event-id is required',
        retriable: false,
      });
    }
    const resolution = flagString(args, 'resolution');
    if (resolution !== 'obsolete') {
      throw new CliError({
        code: 'validation_error',
        message: '--resolution must be obsolete',
        retriable: false,
      });
    }
    if (env.daemonSocketPath) {
      return await resolveStaleViaDaemon(env.daemonSocketPath, {
        runtime_scope: runtimeIntentScope(env),
        agent_id: env.agentId,
        stale_event_id: staleEventId,
        resolution_reason: 'obsolete',
      });
    }
    // Direct (no-daemon) fallback: the pending stale event is daemon-held
    // state, so without a daemon there is nothing to resolve. Reporting
    // that is honest — the alternative would be asking the agent to carry
    // an id it has no reliable way to know.
    return { resolved: false, reason: 'no_pending_stale_event' };
  },
};

const staleReceiveCommand: Command = {
  verb: 'stale-receive',
  summary: 'After message_stale: pull the pending deferred delivery batch.',
  help: `silverhand message stale-receive --stale-event-id <id> [--wait <seconds>]

After a send failed with \`message_stale\`, pull the delivery batch the
daemon is already holding for you (mid-turn deferred queue) as this
command's output, instead of waiting for it to arrive as your next
turn's input. Message bodies are returned exactly once: a batch this
command returns will NOT be delivered again at the next turn boundary,
and vice versa.

Copy details.stale_event_id from the conflict. AgentSlot can hold stale
drafts for several streams; no "latest" event is inferred.

The command only reads what the daemon already holds — it never fetches
from server history. If nothing is held yet it waits (bounded) for the
next delivery to arrive.

Outcomes (stdout JSON, discriminated by "outcome"):
  batch_queue_drained      A batch was returned AND it emptied the
                           daemon's deferred queue (nothing more pending).
                           Process it, then re-send with
                           \`--stale-resolution unaffected|revised\`.
  batch_more_pending       A batch was returned but the deferred queue
                           still holds a remainder; pull again for the rest.
  no_pending_stale         Nothing is pending.
  stale_event_superseded   A newer conflict replaced the id you named.
                           Retry with the current_stale_event_id in this
                           result.
  normal_delivery_in_progress
                           The covering content already entered the
                           normal input path; it arrives as regular
                           pending input, not through this command.
  timeout                  Nothing was obtained in the wait window.
                           Nothing changed; you may retry.

Flags:
  --stale-event-id <id>  Required. Exact stale draft object to receive for.
  --wait <seconds>   Max seconds to wait for a batch to arrive.
                     Default 20, max 120. 0 checks once without waiting.`,
  flags: ['stale-event-id', 'wait'] as const,
  async run({ env, args }) {
    const staleEventId = flagString(args, 'stale-event-id');
    if (!staleEventId) {
      throw new CliError({
        code: 'validation_error',
        message: '--stale-event-id is required',
        retriable: false,
      });
    }
    const waitSeconds = flagNumber(args, 'wait') ?? 20;
    if (!Number.isFinite(waitSeconds) || waitSeconds < 0 || waitSeconds > 120) {
      throw new CliError({
        code: 'validation_error',
        message: '--wait must be between 0 and 120 seconds',
        retriable: false,
      });
    }
    if (!env.daemonSocketPath) {
      throw new CliError({
        code: 'validation_error',
        message:
          'message stale-receive requires the silverhand daemon (no daemon socket in this environment)',
        retriable: false,
      });
    }
    const waitMs = Math.round(waitSeconds * 1000);
    return await staleReceiveViaDaemon(env.daemonSocketPath, {
      runtime_scope: runtimeIntentScope(env),
      agent_id: env.agentId,
      stale_event_id: staleEventId,
      wait_ms: waitMs,
    });
  },
};

const reactCommand: Command = {
  verb: 'react',
  summary: 'Add or remove your emoji reaction on a message.',
  help: `silverhand message react --message-id <id> --emoji <emoji> [--remove]

Add or remove your reaction on a message. Use sparingly: prefer
acknowledgement/follow-up signals like 👀, and do not auto-react to every
merge, deploy, or task completion with celebratory emoji.

A reaction is a lightweight signal, not a message: it allocates no seq,
never advances the conversation's freshness, and does not wake anyone.
Idempotent — re-adding a reaction you already placed, or removing one you
never placed, returns \`changed: false\`. React on any message you can
see in the current conversation (channel or thread); system messages
cannot be reacted to.

Flags:
  --message-id <id>  Required. The message to react to (from an incoming
                     header's msg_id, history, or a send result).
  --emoji <emoji>    Required. A single emoji (max 16 chars, no whitespace).
  --remove           Remove your reaction instead of adding it.

stdout: { "changed": <bool>, "emoji": "<emoji>", "count": <n> }`,
  flags: ['message-id', 'emoji'] as const,
  booleanFlags: ['remove'],
  async run({ client, args }) {
    const messageId = flagString(args, 'message-id');
    if (!messageId) {
      throw new CliError({
        code: 'validation_error',
        message: '--message-id is required',
        retriable: false,
      });
    }
    const emoji = flagString(args, 'emoji');
    if (!emoji) {
      throw new CliError({
        code: 'validation_error',
        message: '--emoji is required',
        retriable: false,
      });
    }
    const remove = flagBool(args, 'remove') ?? false;
    return await client.reactToMessage({
      message_id: messageId,
      emoji,
      ...(remove ? { remove: true } : {}),
    });
  },
};

const answerCommand: Command = {
  verb: 'answer',
  summary: "Answer a message's options.",
  help: `silverhand message answer --message-id <id>
                            [--option <id> ...] [--other <text>]
       silverhand message answer --message-id <id> --withdraw

Answer a message that carries options (shown under its body as
\`[Options (single choice) ...]\` with \`<id>: <label>\` lines). Pass the
option ids you pick, free text with --other, or both (multi choice only).
A single-choice answer takes exactly one --option or --other.

Answering again replaces your answer; the last answer wins. Each answer
posts a system event in the question's stream that mentions its author.
You cannot answer your own question. --withdraw clears your own current
answer (no_answer when unanswered, not_answerer when someone else answered).

Flags:
  --message-id <id>  Required. The question message.
  --option <id>      Repeatable. An option id from the question.
  --other <text>     Free-text answer when no option fits.
  --withdraw         Clear your answer instead of answering.

stdout: { "message_id", "answer": { "by", "option_ids", "other", "answered_at" },
          "event_message_id" }; with --withdraw: { "message_id", "event_message_id" }`,
  flags: ['message-id', 'option', 'other'] as const,
  booleanFlags: ['withdraw'],
  async run({ client, args }) {
    const messageId = requireMessageId(args);
    if (flagBool(args, 'withdraw') ?? false) {
      if (
        flagStringArray(args, 'option') !== undefined ||
        flagString(args, 'other') !== undefined
      ) {
        throw new CliError({
          code: 'validation_error',
          message: '--withdraw takes no --option or --other',
          retriable: false,
        });
      }
      return await client.withdrawMessageAnswer({ message_id: messageId });
    }
    const optionIds = flagStringArray(args, 'option') ?? [];
    const other = flagString(args, 'other');
    if (optionIds.length === 0 && (other === undefined || other.trim() === '')) {
      throw new CliError({
        code: 'validation_error',
        message: 'pass at least one --option <id> or --other <text>',
        retriable: false,
      });
    }
    return await client.answerMessage({
      message_id: messageId,
      ...(optionIds.length > 0 ? { option_ids: optionIds } : {}),
      ...(other !== undefined && other.trim() !== '' ? { other } : {}),
    });
  },
};

const optionsUpdateCommand: Command = {
  verb: 'update',
  summary: 'Replace the options of your own unanswered message.',
  help: `silverhand message options update --message-id <id>
                                    --option <id>=<label> ... [--multi]

Replace the whole option list of a message you sent. Allowed only while
nobody has answered it; once answered this fails with options_answered —
send a new message instead. Same --option rules as \`message send\`.

Flags:
  --message-id <id>      Required. Your message that carries options.
  --option <id>=<label>  Required, repeatable (2-10). The new options.
  --multi                Readers may pick several options.

stdout: { "message_id", "options": { "mode", "items": [{ "id", "label" }] } }`,
  flags: ['message-id', 'option'] as const,
  booleanFlags: ['multi'],
  async run({ client, args }) {
    const messageId = requireMessageId(args);
    const options = parseOptionFlags(args);
    if (!options) {
      throw new CliError({
        code: 'validation_error',
        message: 'pass the new options with --option <id>=<label> (repeatable)',
        retriable: false,
      });
    }
    return await client.updateMessageOptions({ message_id: messageId, options });
  },
};

export const messageFacets: readonly FacetGroup[] = [
  {
    facet: 'options',
    summary: 'Replace the answer options of your own unanswered message.',
    commands: [optionsUpdateCommand],
  },
];

function requireMessageId(args: ParsedArgs): string {
  const messageId = flagString(args, 'message-id');
  if (!messageId) {
    throw new CliError({
      code: 'validation_error',
      message: '--message-id is required',
      retriable: false,
    });
  }
  return messageId;
}

/** `--option <id>=<label>` (repeatable) + `--multi` → validated options. */
function parseOptionFlags(args: ParsedArgs): MessageOptions | undefined {
  const raw = flagStringArray(args, 'option');
  const multi = flagBool(args, 'multi') ?? false;
  if (raw === undefined || raw.length === 0) {
    if (multi) {
      throw new CliError({
        code: 'validation_error',
        message: '--multi needs options; pass --option <id>=<label> (repeatable)',
        retriable: false,
      });
    }
    return undefined;
  }
  const items = raw.map((entry) => {
    const split = entry.indexOf('=');
    if (split <= 0) {
      throw new CliError({
        code: 'validation_error',
        message: `--option expects <id>=<label>, got ${JSON.stringify(entry)}`,
        retriable: false,
      });
    }
    return { id: entry.slice(0, split), label: entry.slice(split + 1) };
  });
  const parsed = MessageOptionsSchema.safeParse({ mode: multi ? 'multi' : 'single', items });
  if (!parsed.success) {
    throw new CliError({
      code: 'validation_error',
      message:
        'invalid --option list: 2-10 options, ids lowercase [a-z0-9_-] up to 16 chars and ' +
        'unique, labels non-empty up to 200 chars',
      retriable: false,
    });
  }
  return parsed.data;
}

const historyCommand: Command = {
  verb: 'history',
  summary: 'Read one addressed stream; omit the anchor for the latest window.',
  help: `silverhand message history --target <address>
                             [--before-seq N | --after-seq N | --around <id>]
                             [--include-content] [--no-include-system]
                             [--limit N]

Read a window of messages from ONE stream. With no anchor, returns the latest
window in ascending seq order. Pass at most one of --before-seq, --after-seq,
or --around.

Flags:
  --target <address>    Required. WHICH stream to read — the same absolute
                        address the send door takes, in the same three forms:
                          #conv:<conversation_id>
                          ${NAMED_STREAM_ADDRESS_PREFIX}<workspace>/<title>
                          dm:@<handle>
                        Append \`/thread:<root_message_id>\` to read a reply
                        cluster instead of the main line. There is no "here":
                        the conversation this CLI is authenticated in is where
                        your credential lives, not a stream it reads by default.
  (no anchor)          Read the latest window.
  --before-seq N        Read messages with seq < N.
  --after-seq N         Read messages with seq > N.
  --around <message_id> Read a window centered on this message id.
  --include-content     Return full content. Default is snippets only.
  --no-include-system   Skip system messages. Default keeps them.
  --limit N             Max messages. Default 50, max 100.

Reading a reply cluster (a \`/thread:<root_message_id>\` suffix) is a pull:
gated by parent-channel visibility, it does NOT make you a follower, but
it advances your observed boundary for that cluster so a later
\`message send\` to the same address is judged fresh against what you just
read. The suffix must name the cluster ROOT — the id under
\`thread_root_id\` — exactly as on the send door; a mid-cluster reply id
names no stream and is refused rather than quietly widened to its cluster.

A read never creates anything: \`dm:@<handle>\` finds an existing pair and
does not open one, so a mistyped handle stays an error instead of becoming
a permanent empty conversation.

stdout: full AgentReadHistoryDto JSON.`,
  flags: ['before-seq', 'after-seq', 'around', 'target', 'limit'] as const,
  booleanFlags: ['include-content', 'include-system'],
  async run({ env, client, args }) {
    const target = requireTargetFlag(flagString(args, 'target'), env, {
      command: 'silverhand message history',
      because:
        'a read acts on one stream, and the conversation this CLI is ' +
        'authenticated in is not a default subject.',
    });
    const before = flagNumber(args, 'before-seq');
    const after = flagNumber(args, 'after-seq');
    const around = flagString(args, 'around');
    const anchorCount = [before, after, around].filter((v) => v !== undefined).length;
    if (anchorCount > 1) {
      throw new CliError({
        code: 'validation_error',
        message:
          'message history accepts at most one of --before-seq, --after-seq, or --around; omit all to read the latest window',
        retriable: false,
      });
    }
    const includeContent = flagBool(args, 'include-content') ?? false;
    const includeSystem = flagBool(args, 'include-system') ?? true;
    const limit = flagNumber(args, 'limit') ?? 50;
    const dto = await client.readHistory({
      ...(before !== undefined ? { before_seq: before } : {}),
      ...(after !== undefined ? { after_seq: after } : {}),
      ...(around !== undefined ? { around_message_id: around } : {}),
      target,
      include_content: includeContent,
      include_system: includeSystem,
      limit,
    });

    return dto;
  },
};

const searchCommand: Command = {
  verb: 'search',
  summary: 'Search visible message text.',
  help: `silverhand message search <query>
silverhand message search (--q <text> | --query <text>)
                            [--current-workspace | --workspace-id <id>
                             | --conversation-id <id>]
                            [--no-include-system] [--limit N]

Full-text search over messages this agent can see.

The range is whichever selector you pass — there is no separate mode
flag, and at most one selector may appear. Presence is the signal, so a
selector with no range (\`--no-current-workspace\`, or an id flag whose
value came out empty) is refused rather than quietly ignored.

Flags:
  <query>              Positional query, equivalent to --q. Quote
                       multi-word queries.
  --q <text>           Search query. Mutually exclusive with --query and
                       positional <query>. 1..200 chars.
  --query <text>       Alias for --q. Same length limit. Passing more than
                       one query form fails with mutually_exclusive — pick
                       one form per call.
  (no selector)        Only the current channel/thread. This is the default.
  --current-workspace  Every conv in THIS workspace where you are a current
                       member — no need to look up the workspace id first.
  --workspace-id <id>  Same, for another workspace you can access.
  --conversation-id <id>  One specific conversation; from "channel list".
  --no-include-system  Skip system messages.
  --limit N            Max hits. Default 50, max 100.

stdout: AgentSearchMessagesDto JSON. Its \`scope\` field echoes where the
search landed (current|workspace|conversation) — output only; you never
pass it in.`,
  flags: ['q', 'query', 'workspace-id', 'conversation-id', 'limit'] as const,
  booleanFlags: ['include-system', 'current-workspace'],
  removedFlags: {
    scope: {
      replacement: 'selector flags',
      hint:
        'the range is now the selector itself: omit everything for the ' +
        'current conversation, or pass one of --current-workspace, ' +
        '--workspace-id <id>, --conversation-id <id>',
    },
    'channel-id': {
      replacement: '--conversation-id',
      hint: 'use --conversation-id <id> (same value domain, one name)',
    },
  },
  allowPositional: true,
  async run({ client, args }) {
    const qShort = flagString(args, 'q');
    const qLong = flagString(args, 'query');
    const positionalQuery = parseSearchPositionalQuery(args.positional);
    const queryForms = [
      qShort !== undefined ? '--q' : null,
      qLong !== undefined ? '--query' : null,
      positionalQuery !== undefined ? '<query>' : null,
    ].filter((v): v is string => v !== null);
    if (queryForms.length > 1) {
      throw new CliError({
        code: 'validation_error',
        subkind: 'mutually_exclusive',
        message:
          '--q, --query, and positional <query> are mutually exclusive (they are aliases). ' +
          'Pass exactly one.',
        retriable: false,
        details: { query_forms: queryForms },
      });
    }
    const q = qShort ?? qLong ?? positionalQuery;
    if (!q) {
      throw new CliError({
        code: 'validation_error',
        message: 'search query is required: pass positional <query>, --q, or --query',
        retriable: false,
      });
    }
    // Selector-only: PRESENCE picks the range, and no selector at all means
    // the calling conversation. A selector that was named but carries no
    // range is refused, never dropped: `--no-current-workspace` and
    // `--conversation-id "$CID"` with an unset CID would otherwise answer a
    // deliberately wider question with the calling conversation — exactly the
    // silent narrowing this shape exists to remove. The wire agrees
    // (`current_workspace: false` and a blank id are both 400 there).
    const currentWorkspaceFlag = flagBool(args, 'current-workspace');
    if (currentWorkspaceFlag === false) {
      throw emptySelectorError(
        '--no-current-workspace',
        'negating it names no range. Omit every selector to search the ' +
          'current conversation, or pass one selector.',
      );
    }
    const currentWorkspace = currentWorkspaceFlag === true;
    const workspaceId = flagString(args, 'workspace-id');
    const conversationId = flagString(args, 'conversation-id');
    for (const [flag, value] of [
      ['--workspace-id', workspaceId],
      ['--conversation-id', conversationId],
    ] as const) {
      if (value !== undefined && value.trim() === '') {
        throw emptySelectorError(
          flag,
          'it was passed with an empty value — usually an unset shell ' +
            'variable. Supply the id, or omit the flag to search the ' +
            'current conversation.',
        );
      }
    }
    const selectors = [
      currentWorkspace ? '--current-workspace' : null,
      workspaceId !== undefined ? '--workspace-id' : null,
      conversationId !== undefined ? '--conversation-id' : null,
    ].filter((v): v is string => v !== null);
    if (selectors.length > 1) {
      throw new CliError({
        code: 'validation_error',
        subkind: 'mutually_exclusive',
        message:
          '--current-workspace, --workspace-id, and --conversation-id are ' +
          'mutually exclusive: each one names the whole search range. Pass ' +
          'one, or none to search the current conversation.',
        retriable: false,
        details: { selectors },
      });
    }
    const includeSystem = flagBool(args, 'include-system') ?? true;
    const limit = flagNumber(args, 'limit') ?? 50;
    return client.searchMessages({
      q,
      include_system: includeSystem,
      limit,
      ...(currentWorkspace ? { current_workspace: true as const } : {}),
      ...(workspaceId !== undefined ? { workspace_id: workspaceId } : {}),
      ...(conversationId !== undefined ? { conversation_id: conversationId } : {}),
    });
  },
};

/** A search selector was spelled but names no range (`--no-current-workspace`,
 *  or an id flag whose value came out empty). Refusing beats dropping it: the
 *  fallback range would be NARROWER than the caller asked for, and a narrower
 *  answer looks like a real answer. */
function emptySelectorError(flag: string, why: string): CliError {
  return new CliError({
    code: 'validation_error',
    subkind: 'empty_selector',
    message:
      `${flag} does not select a search range: ${why} ` +
      'Selectors: --current-workspace | --workspace-id <id> | --conversation-id <id>.',
    retriable: false,
    details: { selector: flag },
  });
}

function runtimeIntentScope(
  env: CliEnv,
): { kind: 'agent' } | { kind: 'conversation'; conversation_id: string } {
  return env.slot === 'agent'
    ? { kind: 'agent' }
    : { kind: 'conversation', conversation_id: env.conversationId };
}

function parseSearchPositionalQuery(positional: readonly string[]): string | undefined {
  if (positional.length === 0) return undefined;
  if (positional.length === 1) return positional[0];
  throw new CliError({
    code: 'validation_error',
    subkind: 'unexpected_arguments',
    message:
      'message search accepts at most one positional <query>. ' +
      'Quote multi-word queries, e.g. `silverhand message search "hello world"`.',
    retriable: false,
    details: { positional },
  });
}

export const messageCommands: readonly Command[] = [
  sendCommand,
  staleResolveCommand,
  staleReceiveCommand,
  reactCommand,
  answerCommand,
  historyCommand,
  searchCommand,
];

function parseSendStaleResolution(
  raw: string | undefined,
): AgentSendStaleResolutionReason | undefined {
  if (raw === undefined) return undefined;
  if (raw === 'unaffected' || raw === 'revised') return raw;
  throw new CliError({
    code: 'validation_error',
    message: '--stale-resolution must be unaffected or revised',
    retriable: false,
  });
}

async function sendViaDaemon(
  socketPath: string,
  input: AgentMessageSendParams,
): Promise<{
  message_id: string;
  message_seq?: number;
  thread_root_id?: string | null;
  unresolved_references?: readonly Reference[];
}> {
  const host = new HostClient({ socketPath });
  try {
    await host.connect();
    requireFreshnessAuthority(host);
    const result = await host.agentMessageSend(input);
    if (!result.ok) {
      throw mapHttpError(result.status, result.body_text);
    }
    return {
      message_id: result.message_id,
      ...(result.message_seq !== undefined ? { message_seq: result.message_seq } : {}),
      ...(result.thread_root_id !== undefined ? { thread_root_id: result.thread_root_id } : {}),
      // Reported only when the daemon reported it: the send SUCCEEDED, and on
      // stdout the field's absence means "nothing to say about your
      // references", never "all of them landed" (#task:2021 P4-5-b).
      ...(result.unresolved_references !== undefined
        ? { unresolved_references: result.unresolved_references }
        : {}),
    };
  } catch (err) {
    if (err instanceof HostClientError) {
      throw new CliError({
        code:
          err.code === 'disconnected' || err.code === 'timeout'
            ? 'upstream_unavailable'
            : 'internal',
        message: `silverhand daemon send failed: ${err.message}`,
        retriable: err.code === 'disconnected' || err.code === 'timeout',
      });
    }
    throw err;
  } finally {
    await host.close().catch(() => undefined);
  }
}

async function resolveStaleViaDaemon(
  socketPath: string,
  input: AgentStaleResolveParams,
): Promise<
  | { resolved: true }
  | { resolved: false; reason: 'no_pending_stale_event' }
  | {
      resolved: false;
      reason: 'stale_event_superseded';
      current_stale_event_id: string;
    }
> {
  const host = new HostClient({ socketPath });
  try {
    await host.connect();
    requireFreshnessAuthority(host);
    const result = await host.agentStaleResolve(input);
    if (!result.ok) {
      throw mapHttpError(result.status, result.body_text);
    }
    if (result.resolved) return { resolved: true };
    // #task:2095 F18: a later 409 replaced the named event. Surface the LIVE
    // id so the agent re-issues against it rather than reading a false
    // "nothing pending".
    return result.reason === 'stale_event_superseded'
      ? {
          resolved: false,
          reason: result.reason,
          current_stale_event_id: result.current_stale_event_id,
        }
      : { resolved: false, reason: result.reason };
  } catch (err) {
    if (err instanceof HostClientError) {
      throw new CliError({
        code:
          err.code === 'disconnected' || err.code === 'timeout'
            ? 'upstream_unavailable'
            : 'internal',
        message: `silverhand daemon stale-resolve failed: ${err.message}`,
        retriable: err.code === 'disconnected' || err.code === 'timeout',
      });
    }
    throw err;
  } finally {
    await host.close().catch(() => undefined);
  }
}

function requireFreshnessAuthority(host: HostClient): void {
  if (host.capabilities.has('agent.freshness_authority')) return;
  throw new CliError({
    code: 'internal',
    message:
      'silverhand daemon does not advertise agent.freshness_authority; restart the app/daemon from the same build as this CLI',
    retriable: false,
  });
}

/**
 * `agent_stale_receive` over the daemon socket (#task:1614). The client
 * request timeout is sized ABOVE the daemon-side wait window — the
 * daemon legitimately holds the response until a batch arrives or the
 * window elapses.
 */
async function staleReceiveViaDaemon(
  socketPath: string,
  input: AgentStaleReceiveParams,
): Promise<AgentStaleReceiveResult> {
  const host = new HostClient({
    socketPath,
    requestTimeoutMs: (input.wait_ms ?? 20_000) + 10_000,
  });
  try {
    await host.connect();
    if (!host.capabilities.has('agent.stale_receive.v1')) {
      throw new CliError({
        code: 'internal',
        message:
          'silverhand daemon does not advertise agent.stale_receive.v1; restart the app/daemon from the same build as this CLI',
        retriable: false,
      });
    }
    return await host.agentStaleReceive(input);
  } catch (err) {
    if (err instanceof HostClientError) {
      const transient = err.code === 'disconnected' || err.code === 'timeout';
      const arbitration =
        err.code === 'pull_already_active' ||
        err.code === 'stale_event_replaced' ||
        err.code === 'runtime_replaced';
      throw new CliError({
        code: transient ? 'upstream_unavailable' : arbitration ? 'conflict' : 'internal',
        ...(arbitration ? { subkind: err.code } : {}),
        message: `silverhand daemon stale-receive failed: ${err.message}`,
        retriable: transient,
      });
    }
    throw err;
  } finally {
    await host.close().catch(() => undefined);
  }
}

/** Spool an oversized stdin body to a temp file and upload it as a
 *  `text/plain` attachment. The temp file is best-effort removed; the
 *  attachment itself survives a later failed send and can be reused by
 *  passing its id to `--attachment-id` on the retry.
 *
 *  `target` is the SEND's address; the attachment is owned by its conversation,
 *  so the upload is addressed at the head (#task:2021 P4-4) — a body spooled on
 *  the way into a reply cluster belongs to the conversation that cluster is in,
 *  and the upload door refuses a suffix rather than dropping one. The `?? target`
 *  is not a fallback policy: the address was parsed before stdin was read, so the
 *  head is always spellable, and forwarding the raw token lets the resolver be
 *  the only thing that ever says otherwise. */
async function uploadBodyAsAttachment(
  client: BackendClient,
  body: Buffer,
  target: string,
): Promise<{ id: string; filename: string; size: number }> {
  const dir = await mkdtemp(join(tmpdir(), 'silverhand-body-'));
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const tmpPath = join(dir, `message-body-${stamp}.txt`);
  try {
    await writeFile(tmpPath, body);
    const att = await client.uploadAttachment({
      path: tmpPath,
      target: conversationAddressOf(target) ?? target,
      contentType: 'text/plain; charset=utf-8',
    });
    return { id: att.id, filename: att.filename, size: att.size };
  } finally {
    await rm(dir, { recursive: true, force: true }).catch(() => {});
  }
}

function formatByteSize(n: number): string {
  if (n >= 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${Math.ceil(n / 1024)} KB`;
}
