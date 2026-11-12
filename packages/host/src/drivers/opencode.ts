import { SILVERHAND_OPENCODE_AGENT } from '@silverhand/shared';
import { AcpClient, type AcpPermissionPolicy, type AcpRuntimeAdapter } from './acp.js';
import { type Driver, type DriverIO, type DriverSpawnContext, type ParsedEvent } from './index.js';
import { versionAtLeast } from './version-gate.js';

/** First release whose ACP server handles `_session/steering`. */
const STEERING_MIN_VERSION = '2.2.7';

const OPENCODE_PERMISSION_POLICY: AcpPermissionPolicy = {
  // Match the rest of Silverhand's agent runtimes: the runtime process is
  // trusted with machine access, while Silverhand's own capability buckets stay
  // the authorization boundary.
  shouldAllow: () => true,
};

class OpenCodeAcpAdapter implements AcpRuntimeAdapter {
  readonly runtimeLabel = 'OpenCode';
  readonly permissionPolicy = OPENCODE_PERMISSION_POLICY;

  buildInitializeParams(_ctx: DriverSpawnContext): Record<string, unknown> {
    return {
      protocolVersion: 1,
      clientCapabilities: {
        fs: {
          readTextFile: false,
          writeTextFile: false,
        },
        terminal: false,
      },
      clientInfo: {
        name: 'silverhand',
        version: '1.0.0',
      },
    };
  }

  buildSessionParams(ctx: DriverSpawnContext): Record<string, unknown> {
    return {
      cwd: ctx.workspacePath,
      mcpServers: [],
      ...(ctx.cliSessionId ? { sessionId: ctx.cliSessionId } : {}),
    };
  }

  buildSessionConfigRequests(
    ctx: DriverSpawnContext,
    sessionId: string,
  ): Array<Record<string, unknown>> {
    const requests: Array<Record<string, unknown>> = [
      {
        sessionId,
        configId: 'mode',
        value: SILVERHAND_OPENCODE_AGENT,
      },
    ];
    if (ctx.model) {
      requests.push({
        sessionId,
        configId: 'model',
        value: ctx.model,
      });
    }
    return requests;
  }
}

/** Busy input is injected into the running turn instead of waiting for it. */
class OpenCodeSteeringAcpAdapter extends OpenCodeAcpAdapter {
  buildBusyRequest(sessionId: string, text: string) {
    return {
      method: '_session/steering',
      params: { sessionId, prompt: [{ type: 'text', text }] },
    };
  }

  busyRequestNeedsPrompt(result: unknown): boolean {
    return (result as { outcome?: unknown } | null)?.outcome !== 'injected';
  }
}

/**
 * OpenCode ACP driver.
 *
 * OpenCode is a long-lived ACP v1 JSON-RPC/stdio runtime (`opencode acp`).
 * The supervisor still sees the ordinary Driver contract; ACP handshake,
 * session/prompt lifecycle, permission requests, and turn-boundary gating
 * live inside the composed AcpClient.
 */
export class OpenCodeDriver implements Driver {
  readonly kind = 'opencode' as const;
  readonly binary = 'opencode';
  readonly kickoffMode = 'self_inits' as const;
  // #task:2210: ACP has no per-payload admission frame, so a successful write
  // settles. A steer declined because its turn just ended is redelivered best
  // effort; input settled this way can still be lost if the process exits.
  readonly admissionPolicy = 'write-success' as const;
  readonly oneShot = false;
  readonly supportsNativeStandingPrompt = true;
  readonly initLivenessSilenceMs = 30 * 1000;
  readonly initPreProtocolCapMs = 5 * 60 * 1000;
  readonly initPostProtocolMs = 60 * 1000;

  /** Pinned per process generation by `bindRuntimeVersion`; fails closed. */
  private steering = false;
  private acp = new AcpClient(new OpenCodeAcpAdapter());

  /** `prompt-boundary`: one active `session/prompt` at a time; input waits for
   *  the turn boundary (a second prompt would be queued and merged). */
  get releaseProfile(): 'native-steer' | 'prompt-boundary' {
    return this.steering ? 'native-steer' : 'prompt-boundary';
  }

  bindRuntimeVersion(version: string | null): void {
    this.steering = versionAtLeast(version, STEERING_MIN_VERSION);
    this.acp = new AcpClient(
      this.steering ? new OpenCodeSteeringAcpAdapter() : new OpenCodeAcpAdapter(),
    );
  }

  // The working directory comes from the spawn cwd and the ACP session `cwd`;
  // opencode 2.x removed `acp --cwd` and exits on it.
  buildArgs(_ctx: DriverSpawnContext): string[] {
    return ['acp'];
  }

  applyEnv(baseEnv: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
    return { ...baseEnv, NO_COLOR: '1' };
  }

  bindIO(io: DriverIO): void {
    this.acp.bindIO(io);
  }

  encodeStdinMessage(content: string, _cliSessionId: string | null): string | null {
    return this.acp.encodeStdinMessage(content);
  }

  readyToEncode(): boolean {
    return this.acp.readyToEncode();
  }

  encodeInterruptControl(): string | null {
    return this.acp.encodeInterruptControl();
  }

  parseLine(line: string): ParsedEvent[] {
    return this.acp.parseLine(line);
  }

  classifyInitProgress(line: string): 'protocol_ready' | 'session_ready' | null {
    return this.acp.classifyInitProgress(line);
  }
}
