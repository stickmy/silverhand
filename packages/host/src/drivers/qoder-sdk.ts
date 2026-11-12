import type { DriverIO, ParsedEvent } from './index.js';

const DAEMON_PROTOCOL = 2;
const REQUIRED_DAEMON_CAPABILITY = 'multi_session_v2';
const REQUIRED_SESSION_CAPABILITY = 'msg_lifecycle_v1';

type Phase = 'daemon' | 'open' | 'session' | 'ready' | 'failed';

/**
 * Qoder's private `--sdk` daemon wire (#task:2957): `daemon.initialize` →
 * `daemon.open_session` (worker args = the headless argv) → session
 * `initialize`, then `session_init`. Session output is plain stream-json and
 * stays with the driver. Any handshake or daemon failure is fatal and shuts
 * the daemon down.
 */
export class QoderSdkSession {
  private phase: Phase = 'daemon';
  /** Route for user frames, bound by `daemon.open_session`. */
  private routeId: string | null = null;

  constructor(
    private readonly io: DriverIO,
    private readonly workerArgs: () => string[],
  ) {
    this.control('daemon', {
      type: 'daemon.initialize',
      daemon_protocol_version: DAEMON_PROTOCOL,
      sdk_version: 'silverhand',
      capabilities: [REQUIRED_DAEMON_CAPABILITY],
      auth: { type: 'qodercli' },
    });
  }

  /** Set once the session is open; worker output before that is not ours. */
  get sessionId(): string | null {
    return this.routeId;
  }

  get ready(): boolean {
    return this.phase === 'ready';
  }

  /** Events for a daemon frame, or null when the line is session output. */
  parseLine(line: string): ParsedEvent[] | null {
    const frame = sdkFrame(line);
    return frame ? this.onFrame(frame) : null;
  }

  private onFrame(frame: SdkFrame): ParsedEvent[] {
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
    if (response.request_id !== this.phase) return [];
    if (response.subtype !== 'success') {
      const detail = `${response.code ?? 'error'}: ${String(response.error ?? '').slice(0, 512)}`;
      return this.fail(
        'qoder_sdk_rejected',
        `Qoder SDK ${response.request_id} rejected (${detail})`,
      );
    }
    const result = isRecord(response.response) ? response.response : {};
    if (this.phase === 'daemon') {
      if (
        result.daemon_protocol_version !== DAEMON_PROTOCOL ||
        !hasCapability(result, REQUIRED_DAEMON_CAPABILITY)
      ) {
        return this.fail('qoder_sdk_unsupported', 'Qoder SDK daemon protocol is unsupported.');
      }
      this.phase = 'open';
      this.control('open', {
        type: 'daemon.open_session',
        cwd: this.io.spawnCtx.workspacePath,
        worker_args: this.workerArgs(),
      });
      return [];
    }
    if (this.phase === 'open') {
      const sessionId = typeof frame.session_id === 'string' ? frame.session_id : '';
      const resumeId = this.io.spawnCtx.cliSessionId;
      if (!sessionId || (resumeId && sessionId !== resumeId)) {
        return this.fail('qoder_sdk_session_mismatch', 'Qoder SDK opened an unexpected session.');
      }
      this.routeId = sessionId;
      this.phase = 'session';
      this.control('session', { type: 'initialize' }, sessionId);
      return [];
    }
    if (this.phase === 'session') {
      if (!hasCapability(result, REQUIRED_SESSION_CAPABILITY)) {
        return this.fail('qoder_sdk_unsupported', 'Qoder SDK session has no input lifecycle.');
      }
      this.phase = 'ready';
      return [{ kind: 'session_init', sessionId: this.routeId! }];
    }
    return [];
  }

  private fail(code: string, message: string): ParsedEvent[] {
    // Terminal for this process. Ask the daemon to exit; the supervisor
    // reclaims it if it does not, and the exit path settles pending input.
    if (this.phase === 'failed') return [];
    this.phase = 'failed';
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
    void this.io.writeLine(frame).catch(() => {
      this.phase = 'failed';
    });
  }
}

/** Daemon-level frames; everything else is the session's stream-json. */
export function isQoderSdkFrame(line: string): boolean {
  return sdkFrame(line) !== null;
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
