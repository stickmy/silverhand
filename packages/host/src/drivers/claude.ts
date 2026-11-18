import { randomUUID } from 'node:crypto';
import { type Driver, type DriverSpawnContext, type ParsedEvent } from './index.js';
import {
  classifyStreamJsonInitProgress,
  encodeStreamJsonUserMessage,
  parseStreamJsonLine,
} from './stream-json.js';

const DISALLOWED_TOOLS = [
  'EnterPlanMode',
  'ExitPlanMode',
  'ScheduleWakeup',
  'CronCreate',
  'CronList',
  'CronDelete',
].join(',');

/**
 * Claude Code driver.
 *
 * Reference: behaviour mirrored from `@slock-ai/daemon` (claude.ts) plus
 * the canonical `control_request` interrupt format extracted from the
 * Claude Code binary itself.
 *
 * Notes:
 * - Stream-json bidirectional: we both consume claude's output and write
 *   user messages on stdin in the same JSON-line format.
 * - We pass the standing system prompt via `--append-system-prompt-file`
 *   (file path) rather than `--append-system-prompt <text>`, since the
 *   synthesized §4.7 platform prompt easily exceeds argv length limits.
 *   `--append-system-prompt-file` *appends* our platform manual onto
 *   Claude Code's built-in system prompt (tool usage, CLAUDE.md handling,
 *   etc.) rather than replacing it — this keeps Claude's native coding
 *   affordances intact and aligns Claude with the Codex driver, whose
 *   `developerInstructions` already layers on top of Codex's base prompt.
 * - We delete `CLAUDECODE` from the env to keep nested invocations from
 *   tripping Claude's "I'm running inside Claude Code" branch.
 */
/** Model used when the agent stores none. */
export const CLAUDE_DEFAULT_MODEL = 'opus';

export class ClaudeDriver implements Driver {
  readonly kind = 'claude' as const;
  readonly binary = 'claude';
  readonly kickoffMode = 'requires_first_message' as const;
  readonly releaseProfile = 'tool-boundary-steer' as const;
  // #task:2210: claude echoes our payload uuid on `command_lifecycle`
  // frames; `input_lifecycle{state:'started'}` is the per-payload proof
  // the turn actually consumed this batch. Settle on it.
  readonly admissionPolicy = 'lifecycle-start' as const;
  readonly oneShot = false;
  readonly supportsNativeStandingPrompt = true;
  readonly initLivenessSilenceMs = 30 * 1000;
  readonly initPreProtocolCapMs = 5 * 60 * 1000;
  // 60s post-protocol budget. Aligned with codex driver — the post-protocol
  // window measures time from first well-formed JSON frame to `system.init`,
  // and we want all runtimes to share a single recovery budget so PR-A
  // failure-mode triage (`reason=post_protocol`) doesn't depend on driver.
  readonly initPostProtocolMs = 60 * 1000;

  buildArgs(ctx: DriverSpawnContext): string[] {
    const args = [
      '--dangerously-skip-permissions',
      '--allow-dangerously-skip-permissions',
      '--verbose',
      '--permission-mode',
      'bypassPermissions',
      '--output-format',
      'stream-json',
      '--input-format',
      'stream-json',
      '--model',
      ctx.model || CLAUDE_DEFAULT_MODEL,
      '--disallowed-tools',
      DISALLOWED_TOOLS,
      '--append-system-prompt-file',
      ctx.systemPromptPath,
    ];
    if (ctx.reasoningEffort) args.push('--effort', ctx.reasoningEffort);
    if (ctx.cliSessionId) {
      args.push('--resume', ctx.cliSessionId);
    }
    return args;
  }

  applyEnv(baseEnv: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
    const env = { ...baseEnv };
    delete env.CLAUDECODE;
    return env;
  }

  encodeStdinMessage(
    content: string,
    cliSessionId: string | null,
    opts?: { payloadUuid?: string },
  ): string {
    // #task:1558 B' (product decision, thread 2026-07-20): send an
    // explicit `priority:"next"` on every user frame. The A/B runs
    // showed identical behavior with and without it on 2.1.210, but
    // the explicit field states our tool-boundary-steer intent and
    // guards against a future default-behavior change; first-frame
    // priority on an idle session was probed and behaves normally.
    // Undocumented-field maintenance cost is tracked in the design doc.
    return encodeStreamJsonUserMessage(content, cliSessionId, opts?.payloadUuid, {
      priority: 'next',
    });
  }

  encodeInterruptControl(): string {
    return JSON.stringify({
      type: 'control_request',
      request_id: randomUUID(),
      request: { subtype: 'interrupt' },
    });
  }

  parseLine(line: string): ParsedEvent[] {
    return parseStreamJsonLine(line, {
      runtimeLabel: 'Claude',
      classifyError: classifyClaudeError,
    });
  }

  classifyInitProgress(line: string): 'protocol_ready' | 'session_ready' | null {
    return classifyStreamJsonInitProgress(line);
  }
}

function classifyClaudeError(message: string): { code?: string; fatal?: boolean } {
  if (
    message.includes('No conversation found with session ID:') ||
    message.includes('--resume requires a valid session ID')
  ) {
    return { code: 'session_not_found', fatal: true };
  }
  return {};
}
