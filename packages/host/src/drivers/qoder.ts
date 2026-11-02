import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { toClaudeMcpConfig } from '@silverhand/shared';
import { type Driver, type DriverIO, type DriverSpawnContext, type ParsedEvent } from './index.js';
import {
  classifyStreamJsonInitProgress,
  encodeStreamJsonUserMessage,
  parseStreamJsonLine,
} from './stream-json.js';
import { versionAtLeast } from './version-gate.js';

/**
 * Verified enable floor for `qodercli`'s `priority:"next"` steer — NOT a
 * claim about the first release that shipped it.
 *
 * What is actually proven: on 1.1.17, `priority:"next"` is a
 * non-destructive mid-turn steer that the runtime drains at the next tool
 * boundary of the SAME turn, and echoes through `command_lifecycle` frames
 * keyed by our payload uuid (#task:1907). Separately, 1.0.48 accepts the
 * field but its headless handler never forwards/honours it (#task:1556) —
 * that is the ONE build with negative evidence. Everything strictly between
 * those two, and every unparseable version, is simply untested; shiki chose
 * 1.1.17 as the conservative floor.
 *
 * So below the floor we treat the capability as UNPROVEN and fail closed:
 * neither the field nor the steer profile. If a lower release is ever
 * measured to honour it, the floor may drop — with that evidence attached.
 */
const STEER_MIN_VERSION = '1.1.17';

/**
 * From 1.1.81 plain headless stream-json no longer emits `command_lifecycle`,
 * so `started` never arrives and admission stalls (#task:2953). The private
 * `--sdk` daemon wire still emits it, so these builds are driven through it.
 */
const SDK_MIN_VERSION = '1.1.81';
const SDK_DAEMON_PROTOCOL = 2;
const SDK_REQUIRED_DAEMON_CAPABILITY = 'multi_session_v2';
const SDK_REQUIRED_SESSION_CAPABILITY = 'msg_lifecycle_v1';

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
 * Qoder CLI driver (`qodercli`).
 *
 * Qoder speaks the same bidirectional `stream-json` protocol as Claude
 * Code (empirically verified against `qodercli 1.0.37`): it withholds
 * `system/init` until the first real stdin user turn, is long-lived
 * (not one-shot), queues follow-up turns, and resumes context across
 * processes via `--resume <session_id>`. So it slots into the exact
 * same host-supervision boundary as Claude — `requires_first_message`
 * kickoff + boundary-gated release — with **no supervisor-level runtime
 * branch**, and reuses the shared `stream-json` parser.
 *
 * Release profile is the one capability that varies with the CLI version
 * (#task:1909): at/above the verified enable floor `1.1.17`,
 * `priority:"next"` is honoured and `command_lifecycle` comes back, so
 * those builds get Claude's `tool-boundary-steer`; anything below the
 * floor — or unparseable — is treated as unproven and stays on
 * `next-turn-batch` (see {@link STEER_MIN_VERSION} for what is actually
 * measured vs merely untested). The version comes from the detection the
 * spawn path already performs and is bound once per process generation via
 * `bindRuntimeVersion` — the driver never probes and never re-resolves.
 *
 * Deliberate divergences from the Claude driver:
 *  - No `--verbose` / `--allow-dangerously-skip-permissions`: Qoder's
 *    arg parser doesn't recognize them and prints usage + exits.
 *  - `--permission-mode bypass_permissions` (snake_case) per Qoder's
 *    documented choices (Claude uses `bypassPermissions`).
 *  - Standing prompt goes via `--append-system-prompt <text>`: Qoder
 *    has no `--append-system-prompt-file`, so we read the materialized
 *    `systemPromptPath` and pass its contents inline (same approach the
 *    codex/cursor drivers already take for runtimes without a file
 *    flag). Verified: the ~54KB platform prompt passes via argv.
 *  - `encodeInterruptControl` returns null: Qoder's control-request
 *    protocol is unverified, so we let the supervisor fall through to
 *    SIGTERM/SIGKILL rather than send a frame Qoder might not honor.
 *
 * Operational notes:
 *  - Qoder intentionally inherits the developer machine's normal qoder
 *    config/hooks/skills by default (visible as `hook_started` /
 *    `hook_response` frames + auto-triggered skills). shiki explicitly
 *    chose this over a hermetic config root so existing user habits carry
 *    into Silverhand. A future hermetic mode would be a product decision,
 *    not a driver-default cleanup.
 *  - Profile MCP arrives as `ctx.profileMcp` and is written next to the
 *    standing prompt as a Claude-shaped `--mcp-config <file>` worker arg:
 *    in SDK mode only `daemon.open_session` worker args reach the session.
 *  - Headless resume-failure surfaces on stderr (not a stdout `result`
 *    frame), so it can't be classified here as `session_not_found`.
 *    Behaviour then matches Claude's invalid-resume path anyway: no
 *    `session_init` → init watchdog → retry respawn.
 *
 * SDK mode (>= {@link SDK_MIN_VERSION}): the process runs `--sdk`; `bindIO`
 * drives `daemon.initialize` → `daemon.open_session` (worker args = the
 * headless argv) → session `initialize`, and only then announces
 * `session_init` / `readyToEncode`. User frames carry the session route;
 * output is the same stream-json. Any handshake/daemon failure is fatal and
 * shuts the daemon down.
 */
export class QoderDriver implements Driver {
  readonly kind = 'qoder' as const;
  readonly binary = 'qodercli';
  readonly oneShot = false;
  readonly supportsNativeStandingPrompt = true;
  readonly initLivenessSilenceMs = 30 * 1000;
  readonly initPreProtocolCapMs = 5 * 60 * 1000;
  readonly initPostProtocolMs = 60 * 1000;

  /** Resolved once by `bindRuntimeVersion`; false until then so a driver
   *  instance nobody bound behaves like an old CLI (fail closed). */
  private steerCapable = false;
  private sdk = false;
  private io?: DriverIO;
  /** SDK route, bound by `daemon.open_session`. */
  private sdkSessionId: string | null = null;
  private sdkPhase: 'idle' | 'daemon' | 'open' | 'session' | 'ready' | 'failed' = 'idle';

  get kickoffMode(): 'self_inits' | 'requires_first_message' {
    return this.sdk ? 'self_inits' : 'requires_first_message';
  }

  /**
   * Version-derived, pinned to this instance = to the child process
   * generation (`getDriver` mints one driver per spawn). Reading it is
   * safe at any boundary decision; re-resolving per turn would let a
   * mid-session CLI upgrade change how an already-running child is
   * driven, which is exactly the bug the pin prevents.
   */
  get releaseProfile(): 'tool-boundary-steer' | 'next-turn-batch' {
    return this.steerCapable ? 'tool-boundary-steer' : 'next-turn-batch';
  }

  /**
   * #task:2210: settlement policy is version-bound on the same evidence
   * axis as {@link releaseProfile}. At/above the floor, `priority:"next"`
   * echoes through `command_lifecycle` keyed by our payload uuid, so
   * `input_lifecycle{state:'started'}` is a real per-payload proof
   * (`lifecycle-start`).
   *
   * Below the floor there is no per-payload lifecycle frame and no uuid is
   * stamped (see `encodeStdinMessage`). shiki explicitly accepts the weaker
   * `write-success` settlement there: the existing `next-turn-batch` release
   * already keeps the write inside a safe window, and #2210 preserves that
   * driver's current behavior byte-for-byte (no new deferral, no fixture
   * experiment). Pinned to the process generation via `bindRuntimeVersion`,
   * never re-resolved per turn.
   */
  get admissionPolicy(): 'lifecycle-start' | 'write-success' {
    return this.steerCapable ? 'lifecycle-start' : 'write-success';
  }

  bindRuntimeVersion(version: string | null): void {
    this.steerCapable = versionAtLeast(version, STEER_MIN_VERSION);
    this.sdk = versionAtLeast(version, SDK_MIN_VERSION);
  }

  buildArgs(ctx: DriverSpawnContext): string[] {
    return this.sdk ? ['--sdk'] : this.workerArgs(ctx);
  }

  private workerArgs(ctx: DriverSpawnContext): string[] {
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

  bindIO(io: DriverIO): void {
    if (!this.sdk) return;
    this.io = io;
    this.sdkPhase = 'daemon';
    this.control('daemon', {
      type: 'daemon.initialize',
      daemon_protocol_version: SDK_DAEMON_PROTOCOL,
      sdk_version: 'silverhand',
      capabilities: [SDK_REQUIRED_DAEMON_CAPABILITY],
      auth: { type: 'qodercli' },
    });
  }

  readyToEncode(): boolean {
    return !this.sdk || this.sdkPhase === 'ready';
  }

  applyEnv(baseEnv: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
    return { ...baseEnv };
  }

  encodeStdinMessage(
    content: string,
    cliSessionId: string | null,
    opts?: { payloadUuid?: string },
  ): string | null {
    if (this.sdk) {
      if (!this.readyToEncode()) return null;
      return encodeStreamJsonUserMessage(content, this.sdkSessionId, opts?.payloadUuid, {
        priority: 'next',
      });
    }
    if (!this.steerCapable) {
      // Below the enable floor the capability is unproven (1.0.48 accepts
      // `priority` but its headless handler never honours it, #task:1556;
      // the rest is untested), and no per-payload `command_lifecycle` is
      // known to come back. Stamping either field would be noise at best
      // and a false capability claim at worst. Keep the frame minimal.
      return encodeStreamJsonUserMessage(content, cliSessionId);
    }
    // 1.1.17+: state the tool-boundary-steer intent explicitly, and carry
    // the supervisor's payload uuid so the runtime's `command_lifecycle`
    // frames correlate back to this batch — `started` is what promotes the
    // batch from pending-admission to model-seen (`input_lifecycle`).
    return encodeStreamJsonUserMessage(content, cliSessionId, opts?.payloadUuid, {
      priority: 'next',
    });
  }

  encodeInterruptControl(): string | null {
    return null;
  }

  parseLine(line: string): ParsedEvent[] {
    if (this.sdk) {
      const frame = sdkFrame(line);
      if (frame) return this.parseSdkFrame(frame);
      return this.sdkSessionId ? this.parseStreamJson(line) : [];
    }
    return this.parseStreamJson(line);
  }

  private parseStreamJson(line: string): ParsedEvent[] {
    const queueStatus = queueStatusEvent(line);
    if (queueStatus) return [queueStatus];
    // In SDK mode the session is announced by the `initialize` response, so
    // the worker's own per-turn `system/init` frames are not session evidence.
    return parseStreamJsonLine(line, { runtimeLabel: 'Qoder' }).filter(
      (event) => !this.sdk || event.kind !== 'session_init',
    );
  }

  private parseSdkFrame(frame: SdkFrame): ParsedEvent[] {
    if (frame.type === 'keep_alive') return [];
    if (frame.type === 'protocol_error') {
      return this.fail('qoder_sdk_protocol_error', `Qoder SDK protocol error: ${frame.code}`);
    }
    if (frame.type === 'status_event') {
      return this.fail('qoder_sdk_session_failed', 'Qoder SDK session failed.');
    }
    const response = frame.response;
    // Only the response to the request in flight carries control; anything
    // else (unknown, duplicate, late) is ignored.
    if (response.request_id !== this.sdkPhase) return [];
    if (response.subtype !== 'success') {
      const detail = `${response.code ?? 'error'}: ${String(response.error ?? '').slice(0, 512)}`;
      return this.fail(
        'qoder_sdk_rejected',
        `Qoder SDK ${response.request_id} rejected (${detail})`,
      );
    }
    const result = isRecord(response.response) ? response.response : {};
    if (this.sdkPhase === 'daemon') {
      if (
        result.daemon_protocol_version !== SDK_DAEMON_PROTOCOL ||
        !hasCapability(result, SDK_REQUIRED_DAEMON_CAPABILITY)
      ) {
        return this.fail('qoder_sdk_unsupported', 'Qoder SDK daemon protocol is unsupported.');
      }
      const ctx = this.io!.spawnCtx;
      this.sdkPhase = 'open';
      this.control('open', {
        type: 'daemon.open_session',
        cwd: ctx.workspacePath,
        worker_args: this.workerArgs(ctx),
      });
      return [];
    }
    if (this.sdkPhase === 'open') {
      const sessionId = typeof frame.session_id === 'string' ? frame.session_id : '';
      const resumeId = this.io!.spawnCtx.cliSessionId;
      if (!sessionId || (resumeId && sessionId !== resumeId)) {
        return this.fail('qoder_sdk_session_mismatch', 'Qoder SDK opened an unexpected session.');
      }
      this.sdkSessionId = sessionId;
      this.sdkPhase = 'session';
      this.control('session', { type: 'initialize' }, sessionId);
      return [];
    }
    if (this.sdkPhase === 'session') {
      if (!hasCapability(result, SDK_REQUIRED_SESSION_CAPABILITY)) {
        return this.fail('qoder_sdk_unsupported', 'Qoder SDK session has no input lifecycle.');
      }
      this.sdkPhase = 'ready';
      return [{ kind: 'session_init', sessionId: this.sdkSessionId! }];
    }
    return [];
  }

  private fail(code: string, message: string): ParsedEvent[] {
    // Terminal for this process. Ask the daemon to exit; the supervisor
    // reclaims it if it does not, and the exit path settles pending input.
    if (this.sdkPhase === 'failed') return [];
    this.sdkPhase = 'failed';
    this.control('shutdown', { type: 'daemon.shutdown' });
    return [{ kind: 'error', code, message, fatal: true }];
  }

  private control(requestId: string, request: Record<string, unknown>, sessionId?: string): void {
    const frame = JSON.stringify({
      type: 'control_request',
      request_id: requestId,
      request,
      ...(sessionId ? { session_id: sessionId } : {}),
    });
    // No separate error report: a failed handshake write is bounded by the init
    // watchdog, a failed shutdown write by the supervisor's post-fatal reclaim.
    void this.io!.writeLine(frame).catch(() => {
      this.sdkPhase = 'failed';
    });
  }

  classifyInitProgress(line: string): 'protocol_ready' | 'session_ready' | null {
    if (this.sdk) return sdkFrame(line) ? 'protocol_ready' : null;
    return classifyStreamJsonInitProgress(line);
  }
}

type SdkFrame =
  | { type: 'keep_alive' }
  | { type: 'protocol_error'; code: string }
  | { type: 'status_event' }
  | {
      type: 'control_response';
      session_id?: unknown;
      response: {
        subtype: string;
        request_id: string;
        response?: unknown;
        error?: unknown;
        code?: string;
      };
    };

/** Daemon-level frames; everything else is the session's stream-json. */
function sdkFrame(line: string): SdkFrame | null {
  let frame: unknown;
  try {
    frame = JSON.parse(line);
  } catch {
    return null;
  }
  if (!isRecord(frame)) return null;
  switch (frame.type) {
    case 'keep_alive':
      return { type: 'keep_alive' };
    case 'protocol_error':
      return { type: 'protocol_error', code: String(frame.code ?? 'unknown') };
    case 'status_event':
      return frame.status === 'daemon.session_updated' ? { type: 'status_event' } : null;
    case 'control_response': {
      const response = frame.response;
      if (!isRecord(response) || typeof response.request_id !== 'string') return null;
      return {
        type: 'control_response',
        session_id: frame.session_id,
        response: {
          subtype: String(response.subtype),
          request_id: response.request_id,
          response: response.response,
          error: response.error,
          code: typeof response.code === 'string' ? response.code : undefined,
        },
      };
    }
    default:
      return null;
  }
}

function hasCapability(result: Record<string, unknown>, capability: string): boolean {
  return Array.isArray(result.capabilities) && result.capabilities.includes(capability);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}
