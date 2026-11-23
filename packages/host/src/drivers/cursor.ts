import { readFileSync } from 'node:fs';

import { type Driver, type DriverSpawnContext, type ParsedEvent } from './index.js';
import { claudeTool } from './tools.js';

/**
 * Cursor CLI (`cursor-agent --print`) driver.
 *
 * Reference: behaviour mirrored from `@slock-ai/daemon` (cursor.ts).
 *
 * Cursor differs from claude/codex in two big ways:
 *
 * 1. **One-shot.** `cursor-agent --print` processes a single positional
 *    prompt then exits — there's no persistent stdin loop. Therefore
 *    `oneShot = true`, `releaseProfile = 'one-shot'`. ConversationManager
 *    drains the inbox synchronously *before* spawn (into
 *    `spawnCtx.initialPrompt`) and respawns the process when more
 *    messages arrive.
 *
 * 2. **No `--system-prompt` flag.** The standing instructions must be
 *    rolled into the positional prompt argument. We read the
 *    pre-written `systemPromptPath`, prepend it to `initialPrompt`,
 *    and pass the combined string as the final argv. (This is the same
 *    trick slock uses, just packaged differently.)
 */
export class CursorDriver implements Driver {
  readonly kind = 'cursor' as const;
  readonly binary = 'cursor-agent';
  readonly kickoffMode = 'self_inits' as const;
  readonly releaseProfile = 'one-shot' as const;
  // #task:2210: cursor is one-shot — the batch is drained into the argv
  // kickoff before spawn, so its release *is* the spawn write. shiki accepts
  // write-success as the settlement point for such non-steer drivers; there
  // is no correlated lifecycle frame to gate on and #2210 leaves cursor's
  // behavior unchanged.
  readonly admissionPolicy = 'write-success' as const;
  readonly oneShot = true;
  readonly supportsNativeStandingPrompt = false;
  readonly initLivenessSilenceMs = 30 * 1000;
  readonly initPreProtocolCapMs = 5 * 60 * 1000;
  // 60s post-protocol budget. Aligned with codex / claude drivers so PR-A
  // post-protocol watchdog timing is uniform across runtimes; cursor
  // oneShot spawns rarely hit this path but we keep parity for triage.
  readonly initPostProtocolMs = 60 * 1000;

  buildArgs(ctx: DriverSpawnContext): string[] {
    const args = ['--print', '--output-format', 'stream-json', '--yolo', '--trust'];
    if (ctx.model) args.push('--model', ctx.model);
    if (ctx.cliSessionId) args.push('--resume', ctx.cliSessionId);

    args.push(buildKickoffPrompt(ctx));
    return args;
  }

  applyEnv(baseEnv: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
    return { ...baseEnv, FORCE_COLOR: '0', NO_COLOR: '1' };
  }

  encodeStdinMessage(_text: string, _cliSessionId: string | null): string | null {
    // Cursor doesn't accept stdin notifications. ConversationManager treats
    // null as "queue to inbox; respawn for next kickoff".
    return null;
  }

  encodeInterruptControl(): string | null {
    // No protocol-level cancel; ConversationManager falls through to SIGTERM.
    return null;
  }

  /**
   * `cursor-agent --output-format stream-json` mirrors claude's frame
   * shapes closely (system/init, assistant blocks, result). Ported from
   * slock with minor cleanup.
   */
  parseLine(line: string): ParsedEvent[] {
    const trimmed = line.trim();
    if (!trimmed) return [];

    let event: any;
    try {
      event = JSON.parse(trimmed);
    } catch {
      return [{ kind: 'unknown', raw: trimmed }];
    }

    const events: ParsedEvent[] = [];

    switch (event.type) {
      case 'system':
        if (event.subtype === 'init' && typeof event.session_id === 'string') {
          events.push({ kind: 'session_init', sessionId: event.session_id });
        } else if (event.subtype === 'status' && event.status === 'compacting') {
          events.push({ kind: 'compaction_started' });
        } else if (event.subtype === 'compact_boundary') {
          events.push({ kind: 'compaction_finished' });
        }
        break;

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

      case 'result': {
        const subtype = typeof event.subtype === 'string' ? event.subtype : 'success';
        if (subtype !== 'success' || event.is_error) {
          const parts: string[] = [];
          if (Array.isArray(event.errors)) {
            for (const e of event.errors) {
              if (typeof e === 'string' && e.trim()) parts.push(e.trim());
            }
          }
          if (typeof event.result === 'string' && event.result.trim()) {
            parts.push(event.result.trim());
          }
          events.push({
            kind: 'error',
            message: parts.join(' | ') || 'Execution failed',
          });
        }
        events.push({
          kind: 'turn_end',
          sessionId: typeof event.session_id === 'string' ? event.session_id : undefined,
        });
        break;
      }

      default:
        break;
    }

    return events;
  }

  classifyInitProgress(line: string): 'protocol_ready' | 'session_ready' | null {
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
}

/** Combine the standing prompt (file) with the inbox-derived kickoff
 *  text. cursor-agent requires SOME prompt as the trailing argv; if no
 *  inbox content, we use a default "stand by" nudge. */
function buildKickoffPrompt(ctx: DriverSpawnContext): string {
  const standingPrompt = readFileSync(ctx.systemPromptPath, 'utf-8');
  const initial =
    ctx.initialPrompt.trim() ||
    'You are now active in this channel. Read your MEMORY.md and stand by for further instructions.';
  return `${standingPrompt}\n\n---\n\n${initial}`;
}
