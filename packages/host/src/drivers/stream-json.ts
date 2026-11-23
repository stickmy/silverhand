import { type ParsedEvent } from './index.js';
import { claudeTool } from './tools.js';

/**
 * Shared Claude-shaped `stream-json` protocol helpers.
 *
 * Both the Claude Code driver and the Qoder driver speak the *same*
 * bidirectional `stream-json` wire shape (verified empirically against
 * `qodercli 1.0.37`): `system/init(session_id)`, assistant
 * `thinking/text/tool_use` blocks, user `tool_result` blocks, and a
 * terminal `result` frame. That shared shape is a real protocol, not
 * coincidental duplication — so we factor it into pure functions both
 * drivers call, rather than a base-class hierarchy. Composition keeps
 * each driver free to diverge (spawn args, error taxonomy, tool
 * disallow-lists, native-vs-forked frames) without an inheritance
 * contract binding them together.
 *
 * These functions are pure: no driver state, safe to call per line.
 */

/** Optional per-driver classifier for a terminal `result`-frame error
 *  message — lets a driver tag e.g. `session_not_found` as fatal. */
export type StreamJsonErrorClassifier = (message: string) => { code?: string; fatal?: boolean };

/** Encode one user turn for the runtime's stdin (the kickoff/notify
 *  message). Identical across Claude-shaped runtimes. `payloadUuid`
 *  (Claude, #task:1557) stamps the frame's `uuid` so the runtime's
 *  `command_lifecycle` frames can be correlated back to the payload;
 *  callers omit it for a runtime (or a CLI version) whose lifecycle echo
 *  is not established — the qoder driver only stamps it at/above its
 *  verified enable floor 1.1.17 (#task:1909). */
export function encodeStreamJsonUserMessage(
  content: string,
  cliSessionId: string | null,
  payloadUuid?: string,
  extra?: { priority?: 'next' },
): string {
  return JSON.stringify({
    type: 'user',
    message: {
      role: 'user',
      content: [{ type: 'text', text: content }],
    },
    ...(cliSessionId ? { session_id: cliSessionId } : {}),
    ...(payloadUuid ? { uuid: payloadUuid } : {}),
    ...(extra?.priority ? { priority: extra.priority } : {}),
  });
}

/** Parse one stdout line into zero or more `ParsedEvent`s. Unknown but
 *  well-formed JSON (including `system` frames the runtime emits that we
 *  don't model, e.g. Qoder's `hook_started`/`hook_response`) is dropped
 *  silently. Malformed JSON surfaces as an `unknown` raw event. */
export function parseStreamJsonLine(
  line: string,
  opts: {
    classifyError?: StreamJsonErrorClassifier;
    /** Human label for the runtime, used only in api-error snippets
     *  (e.g. "Claude API gateway unreachable"). Defaults to "API". */
    runtimeLabel?: string;
  } = {},
): ParsedEvent[] {
  const trimmed = line.trim();
  if (!trimmed) return [];

  let event: any;
  try {
    event = JSON.parse(trimmed);
  } catch {
    return [{ kind: 'unknown', raw: trimmed }];
  }

  const events: ParsedEvent[] = [];
  const classifyError = opts.classifyError;

  switch (event.type) {
    case 'system': {
      if (event.subtype === 'init' && typeof event.session_id === 'string') {
        events.push({ kind: 'session_init', sessionId: event.session_id });
      }
      if (event.subtype === 'api_error') {
        events.push(parseStreamJsonApiError(event, opts.runtimeLabel));
      }
      if (event.subtype === 'status' && event.status === 'compacting') {
        events.push({ kind: 'compaction_started' });
      }
      if (event.subtype === 'compact_boundary') {
        events.push({ kind: 'compaction_finished' });
      }
      break;
    }

    case 'assistant': {
      const content = event.message?.content;
      if (Array.isArray(content)) {
        for (const block of content) {
          if (block?.type === 'thinking' && typeof block.thinking === 'string') {
            events.push({ kind: 'thinking', text: block.thinking });
          } else if (block?.type === 'text' && typeof block.text === 'string') {
            events.push({ kind: 'text', text: block.text });
          } else if (block?.type === 'tool_use') {
            events.push({
              kind: 'tool_call',
              name: typeof block.name === 'string' ? block.name : 'unknown_tool',
              input: block.input,
              tool: claudeTool(block.name, block.input),
              toolCallId: typeof block.id === 'string' ? block.id : undefined,
            });
          }
        }
      }
      break;
    }

    case 'user': {
      const content = event.message?.content;
      if (Array.isArray(content)) {
        for (const block of content) {
          if (block?.type === 'tool_result') {
            // `is_error` is Claude's only outcome bit. Only an explicit
            // boolean is trusted: `true` → failed, `false` → success; a
            // missing/non-boolean field is NOT proven success, so it
            // downgrades to the neutral `completed`.
            const outcome =
              block.is_error === true
                ? 'failed'
                : block.is_error === false
                  ? 'success'
                  : 'completed';
            events.push({
              kind: 'tool_output',
              name: typeof block.name === 'string' ? block.name : undefined,
              toolCallId: typeof block.tool_use_id === 'string' ? block.tool_use_id : undefined,
              outcome,
            });
          }
        }
      }
      break;
    }

    case 'result': {
      const subtype = typeof event.subtype === 'string' ? event.subtype : 'success';
      const stopReason = typeof event.stop_reason === 'string' ? event.stop_reason : null;
      const errorIfNotBudget = (fallback: string) => {
        if (stopReason !== 'max_tokens') {
          const detail = collectResultError(event) || fallback;
          events.push({
            kind: 'error',
            message: detail,
            ...(classifyError ? classifyError(detail) : {}),
          });
        }
      };
      switch (subtype) {
        case 'success':
          if (event.is_error) errorIfNotBudget('Execution failed');
          break;
        case 'error_during_execution':
          errorIfNotBudget('Execution failed');
          break;
        case 'error_max_budget_usd':
          events.push({
            kind: 'error',
            message: collectResultError(event) || 'Budget limit exceeded',
          });
          break;
        case 'error_max_turns':
          events.push({
            kind: 'error',
            message: collectResultError(event) || 'Max turns exceeded',
          });
          break;
        case 'error_max_structured_output_retries':
          events.push({
            kind: 'error',
            message: collectResultError(event) || 'Structured output retries exceeded',
          });
          break;
      }
      events.push({
        kind: 'turn_end',
        sessionId: typeof event.session_id === 'string' ? event.session_id : undefined,
      });
      break;
    }

    case 'command_lifecycle': {
      // Claude emits per-input lifecycle frames keyed by the stdin
      // frame's `uuid` (observed as `command_uuid`): queued → started →
      // completed. `started` is the strongest per-payload admission
      // evidence available (#task:1556/#task:1557). Other states (or
      // absent uuid) are dropped like any unmodeled frame.
      const commandUuid =
        typeof event.command_uuid === 'string' && event.command_uuid.length > 0
          ? event.command_uuid
          : null;
      const state =
        event.state === 'queued' || event.state === 'started' || event.state === 'completed'
          ? event.state
          : null;
      if (commandUuid && state) {
        events.push({ kind: 'input_lifecycle', commandUuid, state });
      }
      break;
    }

    default:
      // unknown but well-formed JSON — discard silently
      break;
  }

  return events;
}

/** Pure init-watchdog progress classifier. Any well-formed JSON object
 *  is `protocol_ready`; a `system/init` with a `session_id` is
 *  `session_ready`. `session_ready` implies `protocol_ready`. */
export function classifyStreamJsonInitProgress(
  line: string,
): 'protocol_ready' | 'session_ready' | null {
  const trimmed = line.trim();
  if (!trimmed) return null;

  let event: any;
  try {
    event = JSON.parse(trimmed);
  } catch {
    return null;
  }

  if (
    event?.type === 'system' &&
    event.subtype === 'init' &&
    typeof event.session_id === 'string'
  ) {
    return 'session_ready';
  }
  return event && typeof event === 'object' ? 'protocol_ready' : null;
}

function parseStreamJsonApiError(event: any, runtimeLabel?: string): ParsedEvent {
  const causeCode =
    typeof event.error?.cause?.code === 'string'
      ? event.error.cause.code
      : typeof event.cause?.code === 'string'
        ? event.cause.code
        : null;
  const retryAttempt =
    typeof event.retryAttempt === 'number' && Number.isFinite(event.retryAttempt)
      ? event.retryAttempt
      : null;
  const code = causeCode === 'ConnectionRefused' ? 'api_connection_refused' : 'api_error';
  const prefix = runtimeLabel ? `${runtimeLabel} API` : 'API';
  const base =
    causeCode === 'ConnectionRefused'
      ? `${prefix} gateway unreachable; retrying`
      : `${prefix} request failed; retrying`;
  return {
    kind: 'error',
    code,
    message: retryAttempt !== null ? `${base} (attempt ${retryAttempt})` : base,
  };
}

function collectResultError(event: any): string {
  const parts: string[] = [];
  if (Array.isArray(event.errors)) {
    for (const e of event.errors) {
      if (typeof e === 'string' && e.trim()) parts.push(e.trim());
    }
  }
  if (typeof event.result === 'string' && event.result.trim()) {
    parts.push(event.result.trim());
  }
  return parts.join(' | ');
}
