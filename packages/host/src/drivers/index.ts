/**
 * Host-side runtime Driver registry.
 *
 * PR-2 step 2 commit 2 — Cindy `vqDWkLFF6pAeIE5t` plan: runtime
 * drivers move to host (execution plane), prompt assembly stays in
 * backend. The split rule is:
 *
 *  - **Driver = how to spawn the runtime, what to write on stdin, how
 *    to parse stdout, when init is considered ready.** Lives here.
 *  - **Prompt = what text to put in the system prompt before spawn.**
 *    Lives in backend (`drivers/platform-prompt.ts` +
 *    `drivers/index.ts:buildSystemPromptFor`).
 *
 * Drivers no longer implement `buildSystemPrompt`. Backend calls the
 * driver to spawn AFTER it has assembled the prompt text and written
 * it to `spawnCtx.systemPromptPath`. This keeps the host package free
 * of `@silverhand/backend` / `better-sqlite3` dependencies (enforced
 * by `tests/dependency-guard.test.ts`).
 */
import type { McpConfig, ParsedEvent, RuntimeKind, ToolOutcome } from '@silverhand/shared';

export type { ParsedEvent, ToolOutcome };

/**
 * Spawn-time context handed to a driver's `buildArgs` / `applyEnv`.
 * Backend builds this after the system prompt was assembled and the
 * temp files were written.
 */
export interface DriverSpawnContext {
  agentId: string;
  agentName: string;
  workspacePath: string;
  cliSessionId: string | null; // when resuming
  model: string | null;
  reasoningEffort: string | null;
  /** Codex service tier id. NULL = let codex use its own
   *  default. Other drivers ignore. */
  serviceTier: string | null;
  /** Path to the materialized `system-prompt.md`. Drivers that accept a
   *  CLI flag for it (claude) point at this; drivers that don't (codex,
   *  cursor) read the file's contents into their own protocol payload. */
  systemPromptPath: string;
  /** Inbox content drained synchronously *before* spawn. Used by
   *  `oneShot` drivers (cursor) which can't accept stdin notifications
   *  and must receive their kickoff message via argv. Streaming drivers
   *  (claude, codex) ignore this and let the supervisor post-drain via
   *  stdin after `session_init`. */
  initialPrompt: string;
  /** Validated additive profile layer for runtimes using a session protocol. */
  profileMcp?: McpConfig;
}

/** Stdin writer + spawn context handed to drivers via `bindIO`. Codex
 *  uses it to drive its JSON-RPC handshake right after spawn. */
export interface DriverIO {
  /** Append `\n` and write one line to the agent's stdin. */
  writeLine(line: string): Promise<void>;
  /** Host-local negative acknowledgment for one encoded payload. Only an
   * explicit, request-correlated runtime rejection may invoke this; silence,
   * transport success, and generic errors are not rejection evidence. */
  onInputRejected?(payloadUuid: string): void;
  spawnCtx: DriverSpawnContext;
}

export interface Driver {
  readonly kind: RuntimeKind;
  /** Bare command name; the host supervisor resolves the actual binary path. */
  readonly binary: string;
  /**
   * How the runtime reaches its first session-ready event:
   *  - `self_inits`: spawn can complete protocol/session init without user stdin.
   *  - `requires_first_message`: init is tied to the first real stdin user turn.
   *    Empty-inbox spawns may be marked active with a real pid while
   *    `initObserved=false`; the host supervisor defers the init watchdog
   *    until it actually writes a user message to stdin.
   *
   * A future runtime can opt into `requires_first_message` without adding a
   * supervisor-level runtime branch only if its driver keeps the boundary
   * intact: `encodeStdinMessage` must send the real kickoff user turn, and
   * `parseLine` must emit a reliable first `session_init` once the runtime
   * has actually accepted that turn. If either side is missing, implement
   * it in the driver first; otherwise the supervisor cannot know when a
   * pre-init inbox row is safe to mark delivered.
   */
  readonly kickoffMode: 'self_inits' | 'requires_first_message';
  /** Release profile (docs/runtime-input-design.md §3.2, #task:1558):
   *  how the daemon releases live input to this runtime and what counts
   *  as admission. The legacy three-value `busyDeliveryMode` was removed
   *  in Commit C — release scheduling dispatches on this profile only.
   *
   *  Read this from the LIVE per-spawn driver instance. A driver whose
   *  protocol capability depends on the installed CLI version resolves
   *  the value in `bindRuntimeVersion` (qoder, #task:1909), so it is
   *  constant per process generation but not constant per driver class.
   *  Backend holds a static instance for cosmetic/diagnostic use only
   *  (`DriverDeliveryView`); release scheduling is daemon-owned. */
  readonly releaseProfile:
    | 'native-steer'
    | 'tool-boundary-steer'
    | 'next-turn-batch'
    | 'prompt-boundary'
    | 'one-shot';
  /**
   * WHEN a written payload is settled as delivered — the axis orthogonal
   * to `releaseProfile` (#task:2210). `releaseProfile` answers *when the
   * daemon may write* input; `admissionPolicy` answers *what observable
   * event flips the inbox row from queued to delivered* (i.e. lets the
   * daemon emit `runtime_delivery_consumed`, the sole trigger that sets
   * `delivered_at`).
   *
   * History: #task:1561 established a per-payload lifecycle gate for codex
   * (the `item/started userMessage` frame keyed by our payload uuid)
   * precisely because the earlier transport-level ack "let already-generated
   * sends pass" (countgame round 2). #task:2095 retired the observed-cursor
   * and degraded that gate to pure observation — the supervisor fell back to
   * treating a successful stdin write (or first `session_init`) as consumed
   * for ALL stream-json drivers, re-opening the same countgame class this
   * ticket fixes.
   *
   * shiki's ratified settlement policy has exactly TWO observable behaviors —
   * release and settlement stay independent (`releaseProfile` decides the safe
   * write window; `admissionPolicy` decides only the settle point):
   *  - `lifecycle-start`: settle after the runtime echoes the per-payload
   *    `input_lifecycle{state:'started'}` whose `commandUuid` is our minted
   *    `payloadUuid` — real evidence the payload became precedent context for
   *    the next generation. This is the fix #2210 restores. Applies ONLY to
   *    drivers with an explicit steer + correlated admission frame: claude,
   *    codex, qoder≥1.1.17.
   *  - `write-success`: settle the moment the driver's existing releaseProfile
   *    has released and the stdin write succeeds. This is NOT a re-assertion
   *    that a transport ack is strong proof — it is shiki's explicit product
   *    trade-off for fallback / non-steer drivers where no correlated lifecycle
   *    frame exists (opencode: ACP prompt-boundary release then write; cursor:
   *    one-shot argv; qoder<1.1.17: existing next-turn-batch release). The
   *    releaseProfile gate is what keeps the write inside a safe window; this
   *    policy adds no new deferral and preserves each such driver's current
   *    behavior byte-for-byte.
   *
   * No `turn-terminal`/`unavailable` values: shiki declined to defer any
   * current driver to its turn terminal and declined to pre-build concepts for
   * hypothetical future protocols.
   *
   * Version-bound like `releaseProfile`: a driver whose protocol capability
   * depends on the CLI version resolves this in `bindRuntimeVersion`
   * (qoder), so it is constant per process generation but not per class.
   */
  readonly admissionPolicy: 'lifecycle-start' | 'write-success';
  /** True if the runtime exits after a single turn and must be
   *  respawned for each new kickoff (cursor-agent --print). False for
   *  long-lived stream-json runtimes (claude, codex). */
  readonly oneShot: boolean;
  /** True if the runtime supports a long-lived "standing" system prompt
   *  across turns (Claude does; some don't). Informational. */
  readonly supportsNativeStandingPrompt: boolean;
  /** Pre-protocol silence window. Raw stdout/stderr bytes reset this. */
  readonly initLivenessSilenceMs: number;
  /** Absolute cap before the first complete protocol frame is observed. */
  readonly initPreProtocolCapMs: number;
  /** Window from first protocol frame to session-ready init. */
  readonly initPostProtocolMs: number;

  buildArgs(ctx: DriverSpawnContext): string[];
  /** Returns a derived env: copy of base with driver-specific tweaks
   *  (set / delete). Implementations must not mutate the input. */
  applyEnv(baseEnv: NodeJS.ProcessEnv): NodeJS.ProcessEnv;
  /** Encode one notification message for the agent's stdin. Returns
   *  null when the runtime doesn't accept stdin notifications (oneShot
   *  drivers) — the host supervisor treats null as "queue to inbox instead".
   *  `opts.payloadUuid` (#task:1557): a stable id the supervisor minted
   *  for this payload; drivers whose runtime echoes per-input lifecycle
   *  frames (claude) stamp it on the wire frame so those frames correlate
   *  back to the payload. Drivers without that protocol ignore it. */
  encodeStdinMessage(
    content: string,
    cliSessionId: string | null,
    opts?: { payloadUuid?: string },
  ): string | null;
  /** Encode a clean-cancel control message. Null = no protocol-level
   *  interrupt; the host supervisor falls through to SIGTERM/SIGKILL. */
  encodeInterruptControl(): string | null;
  parseLine(line: string): ParsedEvent[];
  /** Pure line classifier for init watchdog progress. Must not mutate
   *  driver protocol state; `session_ready` implies `protocol_ready`. */
  classifyInitProgress(line: string): 'protocol_ready' | 'session_ready' | null;

  /** Optional: called once after spawn, before the supervisor's first
   *  drainInbox. Lets stateful drivers (codex) start their JSON-RPC
   *  handshake. No-op for stateless drivers. */
  bindIO?(io: DriverIO): void;

  /**
   * Optional: hand the driver the version of the binary this spawn
   * resolved (#task:1909), so a driver whose protocol capabilities
   * differ across CLI releases can pick its behaviour once.
   *
   * Contract:
   *  - Called exactly once per spawn, immediately after the binary is
   *    resolved and BEFORE anything reads `releaseProfile` or encodes a
   *    stdin frame. The value is whatever runtime detection already
   *    probed (`RuntimeInfo.version`) — implementations must NOT run
   *    their own probe.
   *  - `null` means detection could not read a version. Implementations
   *    MUST fail closed (keep the capability the oldest supported CLI
   *    has), and must also fail closed when never called at all.
   *  - The resolved capability is pinned to this driver instance, i.e.
   *    to the process generation: driver instances are per-spawn
   *    (`getDriver`), so upgrading the CLI mid-session cannot change how
   *    the already-running child is driven. Never re-resolve per turn.
   */
  bindRuntimeVersion?(version: string | null): void;

  /** Oldest supported CLI version. The supervisor refuses a spawn only when the
   *  probed version parses as older; an unknown version still spawns. */
  readonly minimumVersion?: string;

  /**
   * Whether the driver can produce a non-null `encodeStdinMessage`
   * result *right now*. Option A v2 (Cindy `UMGu36zoTDVuKWXi`): the
   * daemon's `ready_for_drain` gate keys off this so codex doesn't
   * fire `runtime_ready_for_drain` before `thread/started` returns
   * a usable `threadId`.
   *
   * Default `true` (stateless drivers like claude are always ready
   * to encode). Stateful drivers MUST override:
   *   - codex: `() => this.threadId !== null`
   *   - any future driver with handshake state should mirror that.
   *
   * Why this is a method, not a boolean field: drivers track state
   * across `parseLine` mutations; a getter style lets the daemon
   * re-check on every `maybeEmitReadyForDrain` call without the
   * driver needing to push events at the supervisor.
   */
  readyToEncode?(): boolean;
}

import { ClaudeDriver } from './claude.js';
import { CodexDriver } from './codex.js';
import { CursorDriver } from './cursor.js';
import { OpenCodeDriver } from './opencode.js';
import { PiDriver } from './pi.js';
import { GrokDriver } from './grok.js';
import { AntigravityDriver } from './antigravity.js';
import { QoderDriver } from './qoder.js';

/** Per-spawn factory: codex/cursor hold per-handle state (threadId,
 *  protocol counters, …) so we can't share a singleton across handles. */
const FACTORIES: Partial<Record<RuntimeKind, () => Driver>> = {
  claude: () => new ClaudeDriver(),
  codex: () => new CodexDriver(),
  cursor: () => new CursorDriver(),
  opencode: () => new OpenCodeDriver(),
  qoder: () => new QoderDriver(),
  pi: () => new PiDriver(),
  grok: () => new GrokDriver(),
  antigravity: () => new AntigravityDriver(),
};

/** Returns a fresh driver instance for `kind`, or throws. */
export function getDriver(kind: RuntimeKind): Driver {
  const f = FACTORIES[kind];
  if (!f) throw new Error(`no driver registered for runtime "${kind}"`);
  return f();
}

export function hasDriver(kind: RuntimeKind): boolean {
  return FACTORIES[kind] !== undefined;
}
