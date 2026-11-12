import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { toClaudeMcpConfig } from '@silverhand/shared';
import { type Driver, type DriverIO, type DriverSpawnContext, type ParsedEvent } from './index.js';
import { isQoderSdkFrame, QoderSdkSession } from './qoder-sdk.js';
import {
  classifyStreamJsonInitProgress,
  encodeStreamJsonUserMessage,
  parseStreamJsonLine,
} from './stream-json.js';
import { versionAtLeast } from './version-gate.js';

/**
 * How a Qoder process is driven, fixed per process generation from the CLI
 * version (fails closed to `headless` when unknown):
 *  - `headless`: stream-json, no steer. 1.0.48 accepts `priority:"next"`
 *    but never honours it (#task:1556); the rest below 1.1.17 is untested.
 *  - `steer` (>= 1.1.17, #task:1907): `priority:"next"` steers at the next
 *    tool boundary and `command_lifecycle` echoes our payload uuid.
 *  - `sdk` (>= 1.1.81, #task:2953): headless stream-json stopped emitting
 *    `command_lifecycle`, so the private `--sdk` daemon wire is used.
 */
type QoderMode = 'headless' | 'steer' | 'sdk';
const STEER_MIN_VERSION = '1.1.17';
const SDK_MIN_VERSION = '1.1.81';

function modeFor(version: string | null): QoderMode {
  if (versionAtLeast(version, SDK_MIN_VERSION)) return 'sdk';
  if (versionAtLeast(version, STEER_MIN_VERSION)) return 'steer';
  return 'headless';
}

/**
 * Qoder tools that clash with Silverhand's own contracts and must be
 * denied so the agent can't run a parallel, invisible control plane:
 *  - plan mode (Silverhand has no plan/exit-plan affordance);
 *  - cron / scheduling (Silverhand owns schedules via the CLI);
 *  - goals (Silverhand owns work tracking via tasks);
 *  - AskUserQuestion (headless runs have no one to answer; 1.1.81 exposes it).
 * Mirrors the Claude driver's disallow rationale, extended for Qoder's
 * native goal tools (`{Create,Get,Update}Goal`), which Claude lacks.
 * Tool names verified from qoder's `system/init.tools` list (v1.0.37).
 */
const DISALLOWED_TOOLS = [
  'EnterPlanMode',
  'ExitPlanMode',
  'CronCreate',
  'CronList',
  'CronDelete',
  'CreateGoal',
  'GetGoal',
  'UpdateGoal',
  'AskUserQuestion',
].join(',');

function queueStatusEvent(line: string): ParsedEvent | null {
  if (!line.includes('"model_queue_status"')) return null;

  let frame: Record<string, unknown>;
  try {
    const parsed = JSON.parse(line) as unknown;
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return null;
    }
    frame = parsed as Record<string, unknown>;
  } catch {
    return null;
  }

  if (frame.type !== 'system' || frame.subtype !== 'model_queue_status') {
    return null;
  }

  if (frame.status === 'ready') {
    return {
      kind: 'api_response',
      status: 'ready',
      message: 'Qoder model is available; resuming request',
    };
  }
  if (frame.status !== 'queued') return null;

  const details: string[] = [];
  if (
    typeof frame.queue_count === 'number' &&
    Number.isInteger(frame.queue_count) &&
    frame.queue_count >= 0
  ) {
    details.push(`queue size ${frame.queue_count}`);
  }
  if (
    typeof frame.wait_time_ms === 'number' &&
    Number.isFinite(frame.wait_time_ms) &&
    frame.wait_time_ms >= 0
  ) {
    const seconds = Math.round(frame.wait_time_ms / 1000);
    const minutes = Math.floor(seconds / 60);
    const remainingSeconds = seconds % 60;
    details.push(`estimated wait ${minutes > 0 ? `${minutes}m ` : ''}${remainingSeconds}s`);
  }

  const suffix = details.length > 0 ? ` (${details.join(', ')})` : '';
  return {
    kind: 'api_response',
    status: 'waiting',
    message: `Qoder model request queued${suffix}`,
  };
}

/**
 * Qoder CLI driver (`qodercli`). Speaks Claude's bidirectional stream-json
 * (withholds `system/init` until the first user turn, resumes via
 * `--resume`), so it reuses the shared parser with no supervisor branch.
 *
 * Divergences from Claude:
 *  - No `--verbose` / `--allow-dangerously-skip-permissions` (Qoder exits on
 *    unknown flags); `bypass_permissions` is snake_case.
 *  - No prompt-file flag: the standing prompt is passed inline (~54KB fits).
 *  - No verified interrupt frame: the supervisor falls back to signals.
 *  - Inherits the user's qoder config/hooks/skills on purpose (shiki).
 *  - Headless resume failure goes to stderr, so it recovers through the init
 *    watchdog rather than `session_not_found`.
 */
export class QoderDriver implements Driver {
  readonly kind = 'qoder' as const;
  readonly binary = 'qodercli';
  readonly oneShot = false;
  readonly supportsNativeStandingPrompt = true;
  readonly initLivenessSilenceMs = 30 * 1000;
  readonly initPreProtocolCapMs = 5 * 60 * 1000;
  readonly initPostProtocolMs = 60 * 1000;

  /** Pinned per process generation; never re-resolved mid-session. */
  private mode: QoderMode = 'headless';
  private sdk: QoderSdkSession | null = null;

  bindRuntimeVersion(version: string | null): void {
    this.mode = modeFor(version);
  }

  get kickoffMode(): 'self_inits' | 'requires_first_message' {
    return this.mode === 'sdk' ? 'self_inits' : 'requires_first_message';
  }

  get releaseProfile(): 'tool-boundary-steer' | 'next-turn-batch' {
    return this.mode === 'headless' ? 'next-turn-batch' : 'tool-boundary-steer';
  }

  /** Steer modes prove admission per payload via `command_lifecycle`; headless
   *  has no such frame and settles on write (#task:2210, accepted by shiki). */
  get admissionPolicy(): 'lifecycle-start' | 'write-success' {
    return this.mode === 'headless' ? 'write-success' : 'lifecycle-start';
  }

  buildArgs(ctx: DriverSpawnContext): string[] {
    return this.mode === 'sdk' ? ['--sdk'] : workerArgs(ctx);
  }

  bindIO(io: DriverIO): void {
    if (this.mode === 'sdk') this.sdk = new QoderSdkSession(io, () => workerArgs(io.spawnCtx));
  }

  readyToEncode(): boolean {
    return this.mode !== 'sdk' || this.sdk?.ready === true;
  }

  applyEnv(baseEnv: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
    return { ...baseEnv };
  }

  encodeStdinMessage(
    content: string,
    cliSessionId: string | null,
    opts?: { payloadUuid?: string },
  ): string | null {
    switch (this.mode) {
      case 'headless':
        return encodeStreamJsonUserMessage(content, cliSessionId);
      case 'steer':
        return encodeStreamJsonUserMessage(content, cliSessionId, opts?.payloadUuid, {
          priority: 'next',
        });
      case 'sdk':
        if (!this.readyToEncode()) return null;
        return encodeStreamJsonUserMessage(content, this.sdk!.sessionId, opts?.payloadUuid, {
          priority: 'next',
        });
    }
  }

  encodeInterruptControl(): string | null {
    return null;
  }

  parseLine(line: string): ParsedEvent[] {
    if (this.mode !== 'sdk') return parseOutput(line);
    const daemon = this.sdk?.parseLine(line);
    if (daemon) return daemon;
    // The SDK session is announced by its `initialize` response; the worker's
    // per-turn `system/init` frames are not session evidence.
    return this.sdk?.sessionId
      ? parseOutput(line).filter((event) => event.kind !== 'session_init')
      : [];
  }

  classifyInitProgress(line: string): 'protocol_ready' | 'session_ready' | null {
    if (this.mode === 'sdk') return isQoderSdkFrame(line) ? 'protocol_ready' : null;
    return classifyStreamJsonInitProgress(line);
  }
}

/** Headless argv; in SDK mode these become `daemon.open_session` worker args,
 *  the only args that reach the session (profile MCP included). */
function workerArgs(ctx: DriverSpawnContext): string[] {
  const standingPrompt = readFileSync(ctx.systemPromptPath, 'utf-8');
  const args = [
    '--dangerously-skip-permissions',
    '--permission-mode',
    'bypass_permissions',
    '--output-format',
    'stream-json',
    '--input-format',
    'stream-json',
    '--disallowed-tools',
    DISALLOWED_TOOLS,
    '--append-system-prompt',
    standingPrompt,
  ];
  if (ctx.model) {
    args.push('--model', ctx.model);
  }
  if (ctx.reasoningEffort) {
    args.push('--reasoning-effort', ctx.reasoningEffort);
  }
  if (ctx.cliSessionId) {
    args.push('--resume', ctx.cliSessionId);
  }
  const mcp = ctx.profileMcp ? toClaudeMcpConfig(ctx.profileMcp, 'profile') : null;
  if (mcp) {
    const path = join(dirname(ctx.systemPromptPath), 'profile-mcp.json');
    writeFileSync(path, JSON.stringify(mcp, null, 2), { mode: 0o600 });
    args.push('--mcp-config', path);
  }
  return args;
}

function parseOutput(line: string): ParsedEvent[] {
  const queueStatus = queueStatusEvent(line);
  return queueStatus ? [queueStatus] : parseStreamJsonLine(line, { runtimeLabel: 'Qoder' });
}
