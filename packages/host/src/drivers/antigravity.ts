import { readFileSync } from 'node:fs';
import { toAcpMcpServers } from '@silverhand/shared';
import { AcpClient, type AcpRuntimeAdapter } from './acp.js';
import type { Driver, DriverIO, DriverSpawnContext } from './index.js';

function record(value: unknown): Record<string, any> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, any>)
    : {};
}

function configValue(result: Record<string, any>, id: string): unknown {
  const options = Array.isArray(result.configOptions) ? result.configOptions : [];
  return options.find((option: any) => option?.id === id)?.currentValue;
}

// agy reports model failures (region, quota, auth) as a message chunk, e.g.
// "⚠ FAILED_PRECONDITION (code 400): User location is not supported ...".
const MESSAGE_ERROR = /^\s*⚠\s*([A-Z_]+ \(code \d{3}\): [^\n]+?)\s*$/u;

export class AntigravityAcpAdapter implements AcpRuntimeAdapter {
  readonly runtimeLabel = 'Antigravity';
  readonly permissionPolicy = { shouldAllow: () => true };
  readonly suppressPreSessionUpdates = true;
  readonly busyPromptJoinsTurn = true;

  buildInitializeParams(): Record<string, unknown> {
    return {
      protocolVersion: 1,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false },
      clientInfo: { name: 'silverhand', version: '1.0.0' },
    };
  }

  sessionMethod(ctx: DriverSpawnContext, result: unknown): string {
    const capabilities = record(record(result).agentCapabilities);
    if (record(capabilities._meta).antigravity?.systemPrompt?.append !== true) {
      throw new Error('Antigravity does not accept a system prompt. Update Antigravity CLI.');
    }
    if (ctx.cliSessionId && !record(capabilities.sessionCapabilities).resume) {
      throw new Error('Antigravity does not support restoring this session.');
    }
    return ctx.cliSessionId ? 'session/resume' : 'session/new';
  }

  buildSessionParams(ctx: DriverSpawnContext): Record<string, unknown> {
    return {
      cwd: ctx.workspacePath,
      mcpServers: toAcpMcpServers(ctx.profileMcp ?? { mcpServers: {} }, 'profile'),
      ...(ctx.cliSessionId ? { sessionId: ctx.cliSessionId } : {}),
      _meta: {
        antigravity: { systemPrompt: { append: readFileSync(ctx.systemPromptPath, 'utf8') } },
      },
    };
  }

  // Thinking levels belong to the selected model, so the model is set first.
  buildSessionConfigRequests(ctx: DriverSpawnContext, sessionId: string) {
    return [
      ...(ctx.model ? [{ sessionId, configId: 'model', value: ctx.model }] : []),
      ...(ctx.reasoningEffort
        ? [{ sessionId, configId: 'thought_level', value: ctx.reasoningEffort }]
        : []),
    ];
  }

  validateSession(ctx: DriverSpawnContext, value: unknown): string | null {
    const result = record(value);
    if (ctx.model && configValue(result, 'model') !== ctx.model) {
      return 'Antigravity did not select the requested model.';
    }
    if (ctx.reasoningEffort && configValue(result, 'thought_level') !== ctx.reasoningEffort) {
      return 'Antigravity did not apply the requested thinking level for this model.';
    }
    return null;
  }

  // agy's tool frames carry an ACP `kind` and PascalCase args.
  toolCall(update: Record<string, any>): { name: string; input: unknown } | null {
    const input = record(update.rawInput);
    const text = (key: string) => (typeof input[key] === 'string' ? input[key] : undefined);
    const path = text('Path');
    if (update.kind === 'execute' && text('CommandLine') !== undefined) {
      return { name: 'Bash', input: { command: text('CommandLine') } };
    }
    if (update.kind === 'read' && path) return { name: 'Read', input: { file_path: path } };
    if (update.kind === 'edit' && path && text('Content') !== undefined) {
      return { name: 'Write', input: { file_path: path, content: text('Content') } };
    }
    if (update.kind === 'edit' && path && text('Target') !== undefined) {
      return {
        name: 'Edit',
        input: {
          file_path: path,
          old_string: text('Target'),
          new_string: text('Replacement') ?? '',
        },
      };
    }
    if (update.kind === 'search' && text('query') !== undefined) {
      return { name: 'WebSearch', input: { query: text('query') } };
    }
    return null;
  }

  messageTextError(text: string): string | null {
    return MESSAGE_ERROR.exec(text)?.[1] ?? null;
  }
}

/** Antigravity CLI over ACP (`agy acp`); login is the user's own agy session. */
export class AntigravityDriver implements Driver {
  readonly kind = 'antigravity' as const;
  readonly binary = 'agy';
  readonly kickoffMode = 'self_inits' as const;
  readonly releaseProfile = 'native-steer' as const;
  readonly admissionPolicy = 'write-success' as const;
  readonly oneShot = false;
  readonly supportsNativeStandingPrompt = true;
  readonly initLivenessSilenceMs = 30_000;
  readonly initPreProtocolCapMs = 300_000;
  readonly initPostProtocolMs = 60_000;
  // 1.5.0 is the first release where a mid-turn prompt joins the running turn.
  readonly minimumVersion = '1.5.0';
  private readonly acp = new AcpClient(new AntigravityAcpAdapter());

  buildArgs(): string[] {
    return ['acp', '--dangerously-skip-permissions'];
  }

  // Under SSH agy switches to a separate file token store, away from the user's login.
  applyEnv(env: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
    const { SSH_CLIENT: _c, SSH_CONNECTION: _n, SSH_TTY: _t, ...rest } = env;
    return { ...rest, NO_COLOR: '1' };
  }
  bindIO(io: DriverIO): void {
    this.acp.bindIO(io);
  }
  readyToEncode(): boolean {
    return this.acp.readyToEncode();
  }
  encodeStdinMessage(content: string): string | null {
    return this.acp.encodeStdinMessage(content);
  }
  encodeInterruptControl(): string | null {
    return this.acp.encodeInterruptControl();
  }
  parseLine(line: string) {
    return this.acp.parseLine(line);
  }
  classifyInitProgress(line: string) {
    return this.acp.classifyInitProgress(line);
  }
}
