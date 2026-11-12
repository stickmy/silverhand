/**
 * Supervisor — production execution-plane owner for silverhand-daemon.
 *
 * It owns one runtime child process per `(conversation, agent)`:
 * spawn args/env, stdout/stderr pumping, parsed-event fanout,
 * stdin delivery, lifecycle locks, init retry/cooldown, compaction
 * watchdogs, and run-state projection. Dispatcher/server code sees
 * only the `SupervisorContract`; this concrete class is wired by
 * `entry.ts` in production and instantiated directly by focused
 * execution-plane tests.
 */
import { spawn as nodeSpawn, type ChildProcess, type SpawnOptions } from 'node:child_process';
import { setTimeout as setTimer } from 'node:timers';
import { randomUUID } from 'node:crypto';
import { mkdir, writeFile } from 'node:fs/promises';
import {
  AUTH_TOKEN_HEADER,
  AgentSendMessageOutputSchema,
  buildSystemPromptFor,
  composeRuntimeDeliveryPresentation,
  type AgentActivitySnippetKind,
  type AgentRunState,
  internalPaths,
  type ParsedEvent,
  ParsedEventSchema,
  type RuntimeKind,
  SILVERHAND_AGENT_ID_ENV,
  SILVERHAND_AUTH_TOKEN_ENV,
  SILVERHAND_CONVERSATION_ID_ENV,
  SILVERHAND_SERVER_URL_ENV,
  type RuntimeDeliveryPresentation,
  runtimeCatalogEntry,
} from '@silverhand/shared';
import { compareToMinimum, parsedVersion } from '../../drivers/version-gate.js';

import {
  getDriver as getDefaultDriver,
  type Driver,
  type DriverSpawnContext,
} from '../../drivers/index.js';
import { detectRuntimes, platformBinaryCandidates } from '../../runtimes.js';
import {
  describeSpawnError,
  redactSpawnArgs,
  summarizeSpawnEnv,
  summarizeSpawnPath,
  summarizeSilverhandEnvPresence,
} from '../spawn-diagnostics.js';
import {
  encodeFrame,
  type AgentMessageSendParams,
  type AgentRuntimeIntentScope,
  type AgentMessageSendResult,
  type AgentStaleReceiveParams,
  type AgentStaleReceiveResult,
  type AgentStaleResolveParams,
  type AgentStaleResolveResult,
  type EventFrame,
  type RuntimeClearSessionParams,
  type RuntimeClearSessionResult,
  type RuntimeCloseActivityLogParams,
  type RuntimeCloseActivityLogResult,
  type RuntimeOutputPauseParams,
  type RuntimeDeliverParams,
  type RuntimeScope,
  type RuntimeDeliverResult,
  type RuntimeOutputPauseResult,
  type RuntimeOutputResumeParams,
  type RuntimeOutputResumeResult,
  type RuntimeStartParams,
  type RuntimeStartResult,
  type RuntimeStateDto,
  type RuntimeStatusParams,
  type RuntimeStatusResult,
  type RuntimeStopAllParams,
  type RuntimeStopAllResult,
  type RuntimeStopParams,
  type RuntimeStopResult,
  type RuntimeDelivery,
} from '../../protocol.js';
import type { DaemonLogger } from '../logger.js';
import { buildPromptContextFromRuntimeStartOptions } from '../prompt-context.js';
import type { ResolvedStartOptions, RuntimeStartResolveDeps } from '../runtime-start-resolve.js';
import {
  isResolvedAgentStartOptions,
  resolveRuntimeActionWorkdir,
  resolveRuntimeStartPaths,
  RuntimeStartResolveError,
} from '../runtime-start-resolve.js';
import { bundleCwdUsable } from '../agent-materialize/index.js';
import { unlinkRuntimePidfile, writeRuntimePidfile } from '../runtime-pidfiles.js';
import { buildDaemonSpawnEnv } from '../spawn-env.js';
import { readDaemonProxyConfig } from '../proxy-config.js';
import { prepareSpawnFiles, type SilverhandCliLauncher } from '../spawn-prep.js';
import { buildProfileMcpSpawnArgs } from '../mcp-spawn.js';
import { chooseSilverhandCliLauncher } from '../silverhand-cli-launcher.js';
import {
  AgentCustodySettlementTimeoutError,
  AgentDeletingError,
  AgentHomeMutationCoordinator,
  AgentQuiesceFailedError,
} from '../agent-home-mutation-coordinator.js';
import type { DeliveryOwnerContext, SupervisorContract } from '../supervisor-contract.js';
import { DaemonTypedError } from '../typed-error.js';
import type {
  ActivitySnippetKind,
  DeferredEntry,
  InitTimeoutReason,
  PendingStaleEvent,
  PullReservation,
  PullWakeReason,
  RuntimeHandle,
  RuntimePhase,
  StartupDeliveryState,
} from './handle.js';
import { INPUT_PAYLOAD_UUID_CAP } from './handle.js';
import { combineGatedBatch, SEND_MESSAGE_TOOL } from './gated-combine.js';

// ── Configurable timing knobs ──────────────────────────────────────
//
// These match CM's defaults (`packages/backend/src/conversation-
// manager.ts` `stopSigtermMs` / `stopSigkillMs`). Pulling them out
// as constructor options keeps tests fast (typically override to
// 50ms + 50ms) without re-validating the same cascade behavior at
// production timings.

/** Default time we wait after SIGTERM before escalating to SIGKILL. */
export const DEFAULT_SIGTERM_TO_SIGKILL_MS = 5_000;
/** Default upper bound on the SIGKILL wait before we mark the handle
 *  exited regardless of the OS's response. Real `kill -9` lands
 *  within ms; this is a safety budget for stuck zombies. */
export const DEFAULT_SIGKILL_TIMEOUT_MS = 10_000;

// ── Bounded deferred-batch snapshot (doc I4/§3.3, #1558 final-gate P1-2) ──
//
// A drain-all flush must not compose an unbounded stdin payload: cap the
// prefix by entry count and total UTF-8 text bytes; the remainder stays
// queued IN ORDER for the next boundary. Initial values — mechanism
// first, tuning from real distributions later (doc §9).
export const DEFERRED_BATCH_MAX_ENTRIES = 20;
export const DEFERRED_BATCH_MAX_TEXT_BYTES = 256 * 1024;

// ── 3c-b init watchdog + retry/cooldown + compaction knobs ─────────
//
// These mirror CM's defaults (`packages/backend/src/conversation-
// manager.ts` lines 57-66) byte-for-byte. The override precedence
// inside `initTimeoutFor` is locked by Cindy (`-pdNvRYOWip5e9yA`):
//
//   opts.initWatchdogMs (global override across all 3 tiers)
//     > opts.init<Tier>Ms                         (per-tier override)
//       > driver.<tier>                           (driver opinion)
//         > DEFAULT_INIT_<TIER>_MS                (this constant)
//
// `opts.initWatchdogMs` exists for the "I want fast tests" knob CM
// already exposes (one number, applies to all three timers); when it's
// set the per-tier values are deliberately ignored.

export const DEFAULT_INIT_LIVENESS_SILENCE_MS = 30 * 1000;
export const DEFAULT_INIT_PRE_PROTOCOL_CAP_MS = 5 * 60 * 1000;
export const DEFAULT_INIT_POST_PROTOCOL_MS = 30 * 1000;
export const DEFAULT_INIT_TIMEOUT_MAX_RETRIES = 1;
export const DEFAULT_INIT_FAILURE_COOLDOWN_THRESHOLD = 3;
export const DEFAULT_INIT_FAILURE_COOLDOWN_MS = 5 * 60 * 1000;
/** Watchdog window from `compaction_started` to `compaction_finished`.
 *  Past this the daemon emits `runtime_compaction_stale` and stacks
 *  `compactionStale: true` onto the run state — but does NOT kill
 *  the child or change phase. Matches CM's `COMPACTION_STALE_MS`. */
export const DEFAULT_COMPACTION_STALE_MS = 5 * 60 * 1000;
const ACTIVITY_SNIPPET_MAX_CHARS = 200;
const ACTIVITY_SNIPPET_BUFFER_MAX_CHARS = ACTIVITY_SNIPPET_MAX_CHARS * 4;
const SEND_MESSAGE_TOOL_NAME = 'silverhand message send';

type InitTimeoutTier = 'initLivenessSilenceMs' | 'initPreProtocolCapMs' | 'initPostProtocolMs';

/** Per-(conv,agent) init-failure counter + cooldown clock. Sticky
 *  across attempts within the same incident window — cleared on a
 *  successful init or on the cooldown clock fully elapsing without
 *  the entry being touched. */
interface InitRetryState {
  failures: number;
  cooldownUntil: number;
}

/**
 * What the spawn path needs from runtime detection: the binary to exec
 * plus the version detection already probed for it (#task:1909).
 *
 * The version rides along because detection probes it anyway — spawn
 * already awaits `detectRuntimes()` to find the binary, and the probe
 * result was previously discarded. Drivers whose protocol capability
 * depends on the CLI release (qoder steer) resolve it from this value
 * via `bindRuntimeVersion`, so no spawn-time parsing and no extra
 * subprocess is introduced. `null` = detection could not read a
 * version; every consumer must fail closed.
 */
export interface ResolvedRuntime {
  binary: string;
  version: string | null;
}

export interface SupervisorOptions {
  /** Optional composition-supplied owner. If omitted Supervisor creates one;
   *  Dispatcher must derive this exact instance through the public accessor. */
  agentHomeMutationCoordinator?: AgentHomeMutationCoordinator;
  /** Sink for outgoing events; wired by `runDaemon` to the per-socket
   *  broadcaster (with output backpressure applied per §2.2.2). */
  emit: (event: EventFrame) => void;
  /** Daemon-side structured logger (§3.4). Used for spawn / signal /
   *  exit lifecycle lines. */
  logger: DaemonLogger;
  /**
   * D4.e-6 — host path resolution deps for `runtime_start`. The wire
   * options are SEMANTIC-only (no `path`/`home_dir`); the supervisor
   * resolves every absolute host path via these deps just before building
   * the prompt context + spawn cwd — `regular` workspaces to their
   * deterministic agent-centric bundle coordinate
   * (`resolveAgentWorkspaceBundleDir`), `agent_home` via `resolveAgentDir`
   * (relocation-v1 §7 task 4: no registry establishment gate). The active
   * workspace's cwd is then HARD-ENSURED by `ensureRuntimeStartWorkspaceCwd`
   * (mkdir -p + usable-dir check) — process admission that fails closed only
   * when the OS cannot spawn a child there; a missing/half-seeded L2 never
   * blocks. Path resolution itself still fails closed (typed
   * `RuntimeStartResolveError`) for a foreign/corrupt `agent_home` ref or an
   * unsafe id segment. Required (not optional) so a missing wire is a typed
   * decision, never a silent path-resolution gap — same philosophy as
   * `supervisorFactory` being required-but-nullable.
   */
  runtimeStartResolveDeps: RuntimeStartResolveDeps;
  /** Absolute path to the daemon control socket. Spawned agents use
   *  this to route freshness-sensitive side effects through the daemon
   *  rather than computing freshness in the short-lived CLI. */
  daemonSocketPath?: string | null;
  /** Optional injection for tests:
   *   - `driverFor` lets tests substitute a fake `Driver` so they can
   *     drive `Supervisor` against a controlled binary;
   *   - `resolveRuntime` lets tests pin the binary path (and the probed
   *     version) without touching PATH;
   *   - `spawnFn` lets tests fully stub `child_process.spawn`.
   *
   *  Production wiring leaves them undefined → default
   *  `getDriver(kind)` + `detectRuntimes` + node's spawn.
   */
  driverFor?: (kind: RuntimeKind) => Driver;
  resolveRuntime?: (kind: RuntimeKind, fallback: string) => Promise<ResolvedRuntime | null>;
  spawnFn?: (binary: string, args: ReadonlyArray<string>, options: SpawnOptions) => ChildProcess;
  /** Time supplier override; lets tests pin `started_at` / event
   *  timestamps. */
  now?: () => number;
  sigtermToSigkillMs?: number;
  sigkillTimeoutMs?: number;
  deletionCustodySettleTimeoutMs?: number;
  /** #task:1563 test seam: override the next-turn reservation stall warn
   *  threshold (default {@link NEXT_TURN_RESERVATION_STALL_WARN_MS}). */
  reservationStallWarnMs?: number;
  /** Daemon-owned runtime pidfile context. Production `runDaemon`
   *  supplies both fields; unit tests can leave `runtimePidRootDir`
   *  null/undefined to disable filesystem pidfiles. */
  daemonInstanceId?: string;
  runtimePidRootDir?: string | null;
  proxyConfigPath?: string | null | (() => string | null);
  /**
   * Host-local launcher for the `silverhand` CLI shim exposed to runtimes.
   * Standalone backend cannot know packaged/host-local paths, so the
   * supervisor replaces only the backend's fallback `{command:'silverhand'}`
   * launcher with this local value. Embedded backends still pass an explicit
   * launcher and are left unchanged.
   */
  localSilverhandCliLauncher?: SilverhandCliLauncher | null;
  /**
   * Daemon-local current control-plane origin. In standalone remote-only
   * mode this comes from the active outbound attachment (Desktop profile /
   * daemon config), not from backend runtime_start. Embedded mode leaves it
   * null and uses the backend-provided finalized local URL.
   */
  controlPlaneServerUrl?: (() => string | null) | null;
  // ── 3c-b init watchdog + retry/cooldown + compaction knobs ──────
  //
  // Override precedence is fixed (Cindy `-pdNvRYOWip5e9yA`):
  //   opts.initWatchdogMs > opts.init<Tier>Ms > driver.<tier> > DEFAULT_*
  // See `initTimeoutFor` for the implementation. Constructor knobs
  // are optional so production wiring can omit them and inherit the
  // CM-aligned defaults verbatim.
  /** Global override for all 3 init timers (silence, pre-protocol
   *  cap, post-protocol). When set, per-tier overrides are ignored
   *  per the precedence table above. Maps 1:1 onto CM's
   *  `ConversationManager.initWatchdogMs`. */
  initWatchdogMs?: number | null;
  initLivenessSilenceMs?: number;
  initPreProtocolCapMs?: number;
  initPostProtocolMs?: number;
  initMaxRetries?: number;
  initFailureCooldownThreshold?: number;
  initFailureCooldownMs?: number;
  compactionStaleMs?: number;
  /**
   * Test/debug escape hatch only. Production leaves this false so raw
   * child stdout/stderr never crosses daemon→backend. Structured
   * parsed/state events are the supported runtime projection surface.
   */
  emitRawRuntimeOutput?: boolean;
}

/** Composite key for `Map<key, RuntimeHandle>` — null byte separator
 *  matches `FakeSupervisor` so log diffs across the two
 *  implementations stay clean. */
function key(conversationId: string, agentId: string): string {
  return runtimeScopeKey({
    kind: 'conversation',
    conversation_id: conversationId,
    agent_id: agentId,
  });
}

function runtimeScopeKey(scope: RuntimeScope): string {
  return scope.kind === 'agent'
    ? `agent\x00${scope.agent_id}`
    : `conversation\x00${scope.conversation_id}\x00${scope.agent_id}`;
}

/** Delivery admission is the focus boundary for an AgentSlot. Keep the
 * handle's outer origin and an already-materialized AgentRunState coordinate
 * atomic so no frame can route to B while carrying A in its payload. */
function bindRuntimeFocus(handle: RuntimeHandle, conversationId: string): void {
  handle.conversationId = conversationId;
  if (handle.runState && handle.runState.conversationId !== conversationId) {
    handle.runState = {
      ...handle.runState,
      conversationId,
    };
  }
}

/** Safe-boundary predicate for the daemon mid-turn queue (#task:1417
 *  Stage 3, §11.3). Byte-for-byte the coordinator's retired `isSafeBoundary`:
 *  no outstanding tool uses and not compacting. The daemon already tracks
 *  `handle.busy` in `updateRunStateFromParsed`, so this reads local truth. */
/** Commit A of #task:1558 (docs/runtime-input-design.md §3.2.1): the
 *  supervisor dispatches on the driver's declared release profile, never
 *  on runtime kind. Behavior-preserving mapping for now — the boundary-
 *  gated set reproduces the legacy boundary-gated
 *  semantics; Commit B gives `next-turn-batch` its own turn_end release,
 *  and Commit C deletes the legacy predicate entirely. */
function profileUsesBoundaryGate(handle: RuntimeHandle): boolean {
  switch (handle.driver.releaseProfile) {
    case 'tool-boundary-steer':
    case 'next-turn-batch':
    case 'prompt-boundary':
      return true;
    case 'native-steer':
    case 'one-shot':
      return false;
    default:
      // Fail closed (doc §3.2.1): a malformed or not-yet-wired profile
      // must never fall through to an implicit write. Throwing keeps
      // custody at the backend (the deliver command fails, rows stay
      // un-acked).
      throw new Error(
        `unknown releaseProfile ${String(
          (handle.driver as { releaseProfile?: unknown }).releaseProfile,
        )} — refusing live input (fail closed)`,
      );
  }
}

/** Per-profile release boundary (#task:1558 Commits B/B').
 *  - `next-turn-batch` (qoder, #1556: one stdin payload = one queued
 *    future turn): releases only when no turn is active AND no write
 *    reservation is pending — transport-safe tool boundaries are NOT
 *    its release boundary.
 *  - `tool-boundary-steer` (B' A/B-validated, notes claude-ab-results):
 *    immediate write — the runtime itself admits at its next tool
 *    boundary (we send an explicit `priority:"next"`). The single
 *    `!compacting` term is a MIGRATION-PERIOD CONSERVATIVE HOLD for the
 *    one scenario the experiments did not cover — it is not an
 *    evidence-backed guard and must not be re-shared across profiles;
 *    delete or harden it once compaction-time write evidence exists.
 *  - `prompt-boundary` (ACP, Commit C): the protocol's own prompt
 *    readiness (`driverReadyToEncode`) is the release boundary.
 *  - The unified transport-safe predicate (`handleAtSafeBoundary`) was
 *    deleted in Commit C — no profile consumes it. */

function strategyBoundaryOpen(handle: RuntimeHandle): boolean {
  switch (handle.driver.releaseProfile) {
    case 'next-turn-batch':
      return handle.nextTurnReserved !== true && handle.inputTurnActive !== true;
    case 'tool-boundary-steer':
      // Migration-period conservative hold (docs §3.2): compaction is
      // the one scenario the B' experiments did not cover.
      return handle.busy.compacting !== true;
    case 'prompt-boundary':
      // ACP's protocol-owned readiness: one active session/prompt at a
      // time, surfaced by the driver itself.
      return driverReadyToEncode(handle.driver);
    case 'native-steer':
    case 'one-shot':
      // Always open here — the write-in-flight slot and the oneShot
      // spawn path own their serialization.
      return true;
    default:
      // Fail closed (doc §3.2.1) — see profileUsesBoundaryGate.
      throw new Error(
        `unknown releaseProfile ${String(
          (handle.driver as { releaseProfile?: unknown }).releaseProfile,
        )} — refusing live input (fail closed)`,
      );
  }
}

/** Conservative per-entry byte budget for the combined payload's wrapping
 *  overhead: `combineGatedBatch`'s '\n\n---\n\n' separators plus the
 *  header/footer instruction lines and JSON string escaping headroom.
 *  Generous by design — the cap protects boundedness, not precision. */
const DEFERRED_BATCH_PER_ENTRY_OVERHEAD_BYTES = 512;

/** #task:1563 (doc §3.4): a `next-turn-batch` reservation armed at write
 *  time is cleared only by a parsed terminal `turn_end`; if that terminal
 *  is lost the boundary stays closed and every delivery defers forever.
 *  The hold IS the fail-closed contract — this threshold only decides when
 *  to surface the stall diagnostically, never when to force a write.
 *  120s is far past any real qoder turn-start latency. */
const NEXT_TURN_RESERVATION_STALL_WARN_MS = 120 * 1000;

/** Bounded snapshot of the deferred queue (doc I4/§3.3): take the longest
 *  prefix within the entry/byte caps (always at least one entry so a
 *  single oversized entry still drains), leave the remainder queued in
 *  order for the next boundary. Byte accounting includes a conservative
 *  per-entry wrapping budget so the FINAL combined payload — not just
 *  the raw texts — stays bounded. */
function spliceBoundedDeferredPrefix(queue: DeferredEntry[]): DeferredEntry[] {
  let take = 0;
  let bytes = 0;
  const conversationId = queue[0]?.conversationId;
  for (const entry of queue) {
    if (take > 0 && entry.conversationId !== conversationId) break;
    const entryBytes =
      Buffer.byteLength(entry.text, 'utf8') + DEFERRED_BATCH_PER_ENTRY_OVERHEAD_BYTES;
    if (
      take > 0 &&
      (take >= DEFERRED_BATCH_MAX_ENTRIES || bytes + entryBytes > DEFERRED_BATCH_MAX_TEXT_BYTES)
    ) {
      break;
    }
    take += 1;
    bytes += entryBytes;
  }
  return queue.splice(0, Math.max(1, take));
}

/** The `delivered_up_to_seq` sidecar value for a batch: `max(seq)` over its
 *  per-delivery entries. Since #task:2106 made `deliveries` required and
 *  non-empty across the whole chain, this is the SINGLE derivation of that
 *  scalar — there is no stored field and no wire scalar to drift from it. */
function deliveredUpToSeqOf(deliveries: readonly RuntimeDelivery[]): number {
  return Math.max(...deliveries.map((delivery) => delivery.seq));
}

/** Fold a drained deferred batch into a single `StartupDeliveryState` for the
 *  shared consume path (#task:1417 Stage 3, §11.4). `gated` multi-entry
 *  batches combine their texts into one turn (`combineGatedBatch`); a single
 *  entry (always the case for `direct` FIFO, or a lone gated entry) keeps its
 *  text verbatim. delivery_ids concatenate so the one `consumed` covers the
 *  whole batch; the per-delivery entries (required since #task:2106) flatten
 *  across the batch. */
function combineDeferredBatch(
  batch: readonly DeferredEntry[],
  gated: boolean,
): StartupDeliveryState {
  const conversationId = batch[0]?.conversationId;
  if (!conversationId || batch.some((entry) => entry.conversationId !== conversationId)) {
    throw new Error('cannot combine delivery presentation across conversation targets');
  }
  const deliveryIds = batch.flatMap((e) => [...e.deliveryIds]);
  const texts = batch.map((e) => e.text);
  // `batch` is non-empty by contract (flushDeferred only calls this after
  // checking the queue has entries), so `texts[0]` is always present; the
  // `?? ''` satisfies the compiler's index-widening without masking a real
  // empty text.
  const text =
    gated && texts.length > 1 ? combineGatedBatch(texts, SEND_MESSAGE_TOOL) : (texts[0] ?? '');
  const deliveries = batch.flatMap((e) => [...e.deliveries]);
  return {
    conversationId,
    actionWorkdir: batch[0]!.actionWorkdir,
    deliveryIds,
    deliveries,
    text,
  };
}

/**
 * Internal spawn input for {@link Supervisor.startUnlocked}. Stage 2c made the
 * wire `runtime_start` command pure lifecycle ({@link RuntimeStartParams} — no
 * delivery), so the ONLY source of a spawn-time startup batch is
 * `runtime_deliver`'s SPAWN arm, which passes one here. A plain
 * `RuntimeStartParams` (from `start()` / retry respawn) is assignable — its
 * `startup_delivery` is simply absent. This shape is daemon-internal and never
 * crosses the wire.
 */
type StartUnlockedParams = RuntimeStartParams & {
  startup_delivery?: {
    conversation_id: string;
    action_workdir: string;
    delivery_ids: number[];
    deliveries: RuntimeDelivery[];
    text: string;
  };
};

function parseObjectJson(text: string): Record<string, unknown> | null {
  try {
    const value = JSON.parse(text) as unknown;
    return value && typeof value === 'object' && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

function extractStaleConflict(bodyText: string): PendingStaleEvent | null {
  const parsed = parseObjectJson(bodyText);
  const error = parsed?.['error'];
  if (!error || typeof error !== 'object' || Array.isArray(error)) return null;
  const details = (error as Record<string, unknown>)['details'];
  if (!details || typeof details !== 'object' || Array.isArray(details)) {
    return null;
  }
  const d = details as Record<string, unknown>;
  if (d['subkind'] !== 'message_stale') return null;
  const id = d['stale_event_id'];
  // The conversation the rejected send TARGETED (#task:2095 F17), surfaced on
  // the 409 symmetrically with the success response's `conversation_id`. The
  // daemon holds no address-resolution authority — for a name-shaped address it
  // cannot re-derive this, and fabricating one is forbidden — so it MUST come
  // from the backend or the send has no `stale-resolve` authorization anchor.
  // Under the #task:2095 RuntimeScope model the 409 carries NO seq: staleness
  // is "this runtime has an undelivered inbox row", not a per-stream position.
  // Fail CLOSED: absent/blank id or target ⇒ no armed event.
  const targetConversationIdRaw = d['target_conversation_id'];
  if (
    typeof id !== 'string' ||
    id.length === 0 ||
    typeof targetConversationIdRaw !== 'string' ||
    targetConversationIdRaw.length === 0
  ) {
    return null;
  }
  return { id, targetConversationId: targetConversationIdRaw };
}

// ── agent_stale_receive (#task:1614) ────────────────────────────────

/** Bounds for the deferred-pull wait window. Waiting never holds the
 *  lifecycle lock and never takes custody, so the cap only bounds how
 *  long a CLI call can sit registered as the handle's waiter. */
export const STALE_RECEIVE_DEFAULT_WAIT_MS = 20_000;
export const STALE_RECEIVE_MAX_WAIT_MS = 120_000;
/** A deletion attempt never invents a custody outcome. It may wait only this
 * long for the ingress-owned response handoff to settle an already-reserved
 * stale batch before returning retryable `quiesce_failed`. */
export const DEFAULT_DELETION_CUSTODY_SETTLE_TIMEOUT_MS = 10_000;

/** Typed non-custody arbitration failure for `agent_stale_receive`
 *  (taxonomy: `AgentStaleReceiveErrorCodeSchema` in protocol.ts). */
export class AgentStaleReceiveError extends DaemonTypedError {
  constructor(
    readonly code:
      | 'pull_already_active'
      | 'stale_event_replaced'
      | 'runtime_replaced'
      | 'handoff_unsupported',
    message: string,
  ) {
    super(message);
  }
}

/** Locked single-step evaluation product for the stale-receive loop. */
type StaleReceiveStep =
  | { kind: 'result'; result: AgentStaleReceiveResult }
  | { kind: 'cancelled' }
  | { kind: 'wait'; wakePromise: Promise<PullWakeReason> };

async function safeReadResponseText(res: Response): Promise<string> {
  try {
    return await res.text();
  } catch {
    return '';
  }
}

export class Supervisor implements SupervisorContract {
  private shutdownPromise: Promise<void> | null = null;
  private shuttingDown = false;
  private readonly handles = new Map<string, RuntimeHandle>();
  private readonly emit: (event: EventFrame) => void;
  private readonly logger: DaemonLogger;
  private readonly runtimeStartResolveDeps: RuntimeStartResolveDeps;
  private readonly driverFor: (kind: RuntimeKind) => Driver;
  private readonly resolveRuntime: (
    kind: RuntimeKind,
    fallback: string,
  ) => Promise<ResolvedRuntime | null>;
  private readonly spawnFn: NonNullable<SupervisorOptions['spawnFn']>;
  private readonly now: () => number;
  private readonly sigtermToSigkillMs: number;
  private readonly sigkillTimeoutMs: number;
  private readonly deletionCustodySettleTimeoutMs: number;
  private readonly reservationStallWarnMs: number;
  private readonly daemonInstanceId: string | null;
  private readonly runtimePidRootDir: string | null;
  private readonly proxyConfigPath: () => string | null;
  private readonly localSilverhandCliLauncher: SilverhandCliLauncher | null;
  private readonly controlPlaneServerUrl: () => string | null;
  private readonly daemonSocketPath: string | null;
  private readonly agentHomeCoordinator: AgentHomeMutationCoordinator;

  // ── 3c-b state ─────────────────────────────────────────────────
  /** Sticky per-RuntimeScope init failure counter + cooldown clock.
   *  Survives across watchdog-triggered respawns within the same
   *  incident window. Cleared by a successful `session_init`. */
  private readonly initRetryStates = new Map<string, InitRetryState>();
  /** Monotonic stale-receive CALL token source (#task:1614 review
   *  P1-3): each pull call mints one so waiter cleanup is keyed to the
   *  exact call, never to the (shared) stale event id. */
  private pullWaiterSeq = 0;
  /** Live stale-receive calls per control-connection owner (#task:1614
   *  review round-3): cancellation must cover the window BEFORE the
   *  waiter is registered — a call can be queued behind the lifecycle
   *  lock when its connection closes, and a scan of the current
   *  `pullWaiter` alone would miss it (the dead call would then
   *  register and squat the slot for the full wait window). Call-scoped
   *  only: entries are added at call entry and removed in its
   *  `finally`; no persistent state. */
  private readonly activePullCallsByOwner = new Map<string, Set<{ cancelled: boolean }>>();
  /** Unsettled stale-receive custody, indexed independently of the current
   * runtime handle. A handle may exit or be replaced after reserving a batch;
   * deletion must still wait for the old response handoff proof. */
  private readonly pullReservationsByAgent = new Map<string, Set<PullReservation>>();
  /** Per-RuntimeScope lifecycle serialization (Cindy plan v2 issue
   *  #6 + tightening `ixAq6GPTn4fVSpCF`). Each entry is the tail
   *  promise for the *most recent* lifecycle operation on that key;
   *  new ops queue behind it. delivery intentionally DOES NOT
   *  participate — it gates via `handle.startupDelivery` / `phase`
   *  instead. */
  private readonly lifecycleTails = new Map<string, Promise<void>>();
  private readonly initWatchdogMs: number | null;
  private readonly initLivenessSilenceMs?: number;
  private readonly initPreProtocolCapMs?: number;
  private readonly initPostProtocolMs?: number;
  private readonly initMaxRetries: number;
  private readonly initFailureCooldownThreshold: number;
  private readonly initFailureCooldownMs: number;
  private readonly compactionStaleMs: number;
  private readonly emitRawRuntimeOutput: boolean;
  /** Wall-clock at daemon/supervisor construction — logged with every spawn
   *  so a runtime failure can be tied to the daemon process instance that
   *  produced it (see #task:1420 spawn diagnostics). */
  private readonly daemonStartedAt: number;

  constructor(opts: SupervisorOptions) {
    this.emit = opts.emit;
    this.logger = opts.logger;
    this.runtimeStartResolveDeps = opts.runtimeStartResolveDeps;
    this.driverFor = opts.driverFor ?? getDefaultDriver;
    this.resolveRuntime = opts.resolveRuntime ?? defaultResolveRuntime;
    this.spawnFn = opts.spawnFn ?? nodeSpawn;
    this.now = opts.now ?? Date.now;
    this.sigtermToSigkillMs = opts.sigtermToSigkillMs ?? DEFAULT_SIGTERM_TO_SIGKILL_MS;
    this.sigkillTimeoutMs = opts.sigkillTimeoutMs ?? DEFAULT_SIGKILL_TIMEOUT_MS;
    this.deletionCustodySettleTimeoutMs =
      opts.deletionCustodySettleTimeoutMs ?? DEFAULT_DELETION_CUSTODY_SETTLE_TIMEOUT_MS;
    this.reservationStallWarnMs =
      opts.reservationStallWarnMs ?? NEXT_TURN_RESERVATION_STALL_WARN_MS;
    this.daemonInstanceId = opts.daemonInstanceId ?? null;
    this.runtimePidRootDir = opts.runtimePidRootDir ?? null;
    const proxyConfigPath = opts.proxyConfigPath ?? null;
    this.proxyConfigPath =
      typeof proxyConfigPath === 'function' ? proxyConfigPath : () => proxyConfigPath;
    this.localSilverhandCliLauncher = opts.localSilverhandCliLauncher ?? null;
    this.controlPlaneServerUrl = opts.controlPlaneServerUrl ?? (() => null);
    this.daemonSocketPath = opts.daemonSocketPath ?? null;
    this.agentHomeCoordinator =
      opts.agentHomeMutationCoordinator ?? new AgentHomeMutationCoordinator();
    this.initWatchdogMs = opts.initWatchdogMs ?? null;
    this.initLivenessSilenceMs = opts.initLivenessSilenceMs;
    this.initPreProtocolCapMs = opts.initPreProtocolCapMs;
    this.initPostProtocolMs = opts.initPostProtocolMs;
    this.initMaxRetries = opts.initMaxRetries ?? DEFAULT_INIT_TIMEOUT_MAX_RETRIES;
    this.initFailureCooldownThreshold =
      opts.initFailureCooldownThreshold ?? DEFAULT_INIT_FAILURE_COOLDOWN_THRESHOLD;
    this.initFailureCooldownMs = opts.initFailureCooldownMs ?? DEFAULT_INIT_FAILURE_COOLDOWN_MS;
    this.compactionStaleMs = opts.compactionStaleMs ?? DEFAULT_COMPACTION_STALE_MS;
    this.emitRawRuntimeOutput = opts.emitRawRuntimeOutput ?? false;
    this.daemonStartedAt = this.now();
  }

  /** #task:1557 / #task:2210: mint a stable uuid for one encoded stdin
   *  payload and remember which delivery ids it carried. The
   *  `lifecycle-start` drivers (claude, codex, qoder≥1.1.17) echo it back on
   *  their `command_lifecycle` / `input_lifecycle` frames, where a matching
   *  `started` is the per-payload admission proof; `write-success` drivers
   *  (opencode, cursor, qoder<1.1.17) don't correlate it. Bounded FIFO. */
  private mintInputPayloadUuid(handle: RuntimeHandle, deliveryIds: readonly number[]): string {
    const uuid = randomUUID();
    handle.inputPayloadUuids.set(uuid, {
      deliveryIds: [...deliveryIds],
      mintedAt: Date.now(),
    });
    while (handle.inputPayloadUuids.size > INPUT_PAYLOAD_UUID_CAP) {
      const oldest = handle.inputPayloadUuids.keys().next().value;
      if (oldest === undefined) break;
      handle.inputPayloadUuids.delete(oldest);
    }
    return uuid;
  }

  /** Hard-reject a batch whose per-delivery entry id-set does not match its
   *  `delivery_ids` — text and per-delivery presentation must not drift. A
   *  batch without `deliveries[]` (backend-side optional; test/non-populating
   *  callers) is allowed and validated as a no-op. Throws on mismatch (a
   *  backend contract bug): the delivery is left un-acked so the failure is
   *  loud. */
  private assertDeliveryMetadataConsistent(
    conversationId: string,
    agentId: string,
    deliveryIds: readonly number[],
    deliveries: readonly RuntimeDelivery[],
    context: string,
  ): void {
    const idSet = new Set(deliveryIds);
    const metaSet = new Set(deliveries.map((d) => d.delivery_id));
    let mismatch = idSet.size !== metaSet.size;
    if (!mismatch) {
      for (const id of idSet) {
        if (!metaSet.has(id)) {
          mismatch = true;
          break;
        }
      }
    }
    if (mismatch) {
      this.logger.error(
        {
          kind: 'process',
          action: 'delivery_metadata_mismatch',
          conversationId,
          agent_id: agentId,
          context,
          delivery_ids: [...idSet],
          metadata_ids: [...metaSet],
        },
        'delivery metadata id-set does not match delivery_ids (backend bug)',
      );
      throw new Error(
        `delivery metadata mismatch in ${context}: ` +
          `delivery_ids=[${[...idSet].join(',')}] ` +
          `deliveries=[${[...metaSet].join(',')}]`,
      );
    }
  }

  private getHandleForAgentIntent(scope: AgentRuntimeIntentScope, agentId: string): RuntimeHandle {
    const runtimeScope: RuntimeScope =
      scope.kind === 'agent'
        ? { kind: 'agent', agent_id: agentId }
        : {
            kind: 'conversation',
            conversation_id: scope.conversation_id,
            agent_id: agentId,
          };
    const handle = this.handles.get(runtimeScopeKey(runtimeScope));
    if (!handle || handle.phase === 'exited') {
      throw new Error(
        `agent side-effect requested for non-running runtime ${runtimeScopeKey(runtimeScope)}`,
      );
    }
    return handle;
  }

  private async backendPost(handle: RuntimeHandle, path: string, body: unknown): Promise<Response> {
    const serverUrl = this.controlPlaneServerUrl();
    if (!serverUrl) {
      throw new Error('agent side-effect requested before daemon has a control-plane server URL');
    }
    return fetch(`${serverUrl}${path}`, {
      method: 'POST',
      headers: {
        [AUTH_TOKEN_HEADER]: `Bearer ${handle.spawnParams.options.env_policy.silverhand.auth_token}`,
        'X-Agent-Hub-Caller': 'silverhand-daemon',
        'Content-Type': 'application/json',
      },
      body: JSON.stringify(body),
    });
  }

  async agentMessageSend(params: AgentMessageSendParams): Promise<AgentMessageSendResult> {
    const handle = this.getHandleForAgentIntent(params.runtime_scope, params.agent_id);
    // P1-2 v2 (#task:1614): settlement-before-send ordering barrier.
    // The response frame can reach the CLI BEFORE this process runs the
    // write callback (the kernel gives no cross-process ordering), so a
    // resend reacting to `batch_queue_drained` may arrive while the
    // settlement has not even been queued yet — lock-chain position
    // proves nothing here. The reservation's `settled` promise is the
    // explicit happens-before: it resolves only after
    // `settleStaleReceiveHandoff` finished (settled + `delivered_up_to_seq`
    // scalar written on a proven handoff, FIFO restored otherwise), and the
    // write callback
    // is guaranteed to fire eventually (flush, error, or destroy), so
    // this wait is bounded by transport liveness, never by scheduling
    // luck. Loop: a new reservation may exist by the time one resolves.
    // The lock is NOT held here.
    while (handle.pullReservation !== null) {
      await handle.pullReservation.settled;
    }
    // #task:2095: freshness is now a RuntimeScope-wide EXISTS guard the backend
    // evaluates from `target` alone (it re-resolves the address it must resolve
    // to insert anyway). There is no per-stream observed cursor for the daemon
    // to stamp a `seen_up_to_seq`/`asserted_stream` boundary from, so the send
    // carries none — the only freshness field it forwards is the human-typed
    // `--anyway` bypass. `pendingStale` is the ONE event that may be armed for
    // this runtime (the handle IS the scope); a retry attaches its id so the
    // backend can pair the send with the conflict the agent digested.
    const pendingStale = handle.pendingStaleEvent;
    // Activity-row coordinate, snapshotted BEFORE the HTTP await (#task:2099,
    // Cindy's review point 1). `handle.conversationId` is the focus target and
    // is mutable for an agent-scoped session: a delivery that lands while the
    // send is in flight rewrites it, and `agentMessageSend` holds no lifecycle
    // lock. Reading it after the await would file the refusal under whichever
    // conversation happened to win the race — precisely the cross-conversation
    // case this ticket exists to get right. The snapshot is the conversation the
    // command was issued from, which is also the key its sibling `tool_call` row
    // was written under.
    const activityConversationId = handle.conversationId;
    const body: Record<string, unknown> = {
      content: params.content,
      ...(params.attachment_ids ? { attachment_ids: params.attachment_ids } : {}),
      // Forwarded VERBATIM: the address the agent spelled is the request, and a
      // daemon that re-formatted it would be a second grammar between the two
      // authorities (the CLI's `--target` and the backend's resolver).
      target: params.target,
      ...(params.freshness_bypass ? { freshness_bypass: true } : {}),
      ...(pendingStale
        ? {
            stale_event_id: pendingStale.id,
            ...(params.freshness_bypass
              ? { stale_resolution_reason: 'force_send' }
              : params.stale_resolution_reason
                ? { stale_resolution_reason: params.stale_resolution_reason }
                : {}),
          }
        : {}),
    };
    const res = await this.backendPost(
      handle,
      internalPaths.agentSendConversationMessage(params.agent_id),
      body,
    );
    if (!res.ok) {
      const bodyText = await safeReadResponseText(res);
      const stale = res.status === 409 ? extractStaleConflict(bodyText) : null;
      if (stale) {
        // Signal-only: the 409 exposes NO bodies and NO seq. At most ONE event
        // is pending per runtime (the handle IS the RuntimeScope), so a later
        // 409 REPLACES this one (#task:2095 F18): the superseded id can no
        // longer be resolved — the daemon holds only the live event's
        // `targetConversationId` and fabricating one is forbidden — so
        // `stale-receive`/`stale-resolve` answer `stale_event_superseded` with
        // the live id rather than a false `no_pending_stale`.
        handle.pendingStaleEvent = {
          id: stale.id,
          targetConversationId: stale.targetConversationId,
        };
        // The ONE authoritative "a send of mine was refused as stale" fact
        // (#task:2099). Emitted per refusal — a replacement 409 is a second
        // refusal and gets its own row — and only from this branch, so a
        // non-stale rejection or an unparseable 409 produces none. `parsed`
        // events are the driver's; this one is the daemon relaying the backend's
        // verdict, hence its own frame. No ids on it: see the schema arm.
        this.emit({
          event: 'runtime_message_stale',
          runtime_scope: handle.runtimeScope,
          conversation_id: activityConversationId,
          agent_id: handle.agentId,
          runtime_generation: handle.runtimeGeneration,
          ts: this.now(),
        });
      } else if (pendingStale) {
        // A non-stale rejection (or a 409 this daemon could not parse) of a send
        // that carried an armed draft clears it: the draft is no longer the live
        // conflict.
        handle.pendingStaleEvent = null;
      }
      return { ok: false, status: res.status, body_text: bodyText };
    }
    const json = (await res.json()) as Record<string, unknown>;
    const messageId = json['message_id'];
    const messageSeq = json['message_seq'];
    if (typeof messageId !== 'string' || messageId.length === 0) {
      throw new Error('backend send-message response missing message_id');
    }
    // WHERE the words actually landed, per the resolver — the one authority for
    // what a name denoted at insert time, forwarded to the CLI so a caller can
    // report which stream the message landed in (P4-4).
    const landedConversationId = json['conversation_id'];
    if (typeof landedConversationId !== 'string' || landedConversationId.length === 0) {
      throw new Error('backend send-message response missing conversation_id');
    }
    const resolvedRoot = typeof json['thread_root_id'] === 'string' ? json['thread_root_id'] : null;
    // The send committed, so whatever conflict was armed for this runtime is now
    // resolved.
    handle.pendingStaleEvent = null;
    // Sender-only feedback, FORWARDED and never derived (#task:2021 P4-5-b):
    // the daemon holds no participation authority, so it validates the shape
    // against the backend's own output contract and hands the answer through
    // unchanged. A field we cannot read is DROPPED, not repaired: the CLI must
    // never learn "every reference landed" from a response the daemon failed to
    // parse, and the send itself succeeded either way.
    const unresolvedReferences = AgentSendMessageOutputSchema.shape.unresolved_references.safeParse(
      json['unresolved_references'],
    );
    if (!unresolvedReferences.success) {
      // The field is `.optional()`, so an ABSENT key parses successfully: a
      // failure here means the backend sent something we cannot read. Dropping
      // it quietly would make protocol/shape drift indistinguishable from "every
      // reference landed" — the one reading a daemon must never invent. So: one
      // NAMED warn, synchronous, diagnostic-only. The send stays `ok: true` and
      // the actor still gets no field, because a committed send is not undone by
      // a diagnostic. Only the zod issue SHAPE (code + path) is logged: the body
      // is untrusted input, and a log line is not the place to echo it.
      this.logger.warn(
        {
          kind: 'process',
          action: 'send_unresolved_references_shape_mismatch',
          conversationId: handle.conversationId,
          agent_id: params.agent_id,
          issues: unresolvedReferences.error.issues.map((issue) => ({
            code: issue.code,
            path: issue.path.join('.'),
          })),
        },
        'backend send response carried an unreadable unresolved_references field; omitted from the sender answer',
      );
    }
    return {
      ok: true,
      message_id: messageId,
      ...(typeof messageSeq === 'number' ? { message_seq: messageSeq } : {}),
      conversation_id: landedConversationId,
      thread_root_id: resolvedRoot,
      ...(unresolvedReferences.success && unresolvedReferences.data
        ? { unresolved_references: unresolvedReferences.data }
        : {}),
    };
  }

  async agentStaleResolve(params: AgentStaleResolveParams): Promise<AgentStaleResolveResult> {
    const handle = this.getHandleForAgentIntent(params.runtime_scope, params.agent_id);
    const pending = handle.pendingStaleEvent;
    if (!pending) {
      return { ok: true, resolved: false, reason: 'no_pending_stale_event' };
    }
    if (pending.id !== params.stale_event_id) {
      // #task:2095 F18: a later 409 REPLACED the event the agent is resolving.
      // Only ONE is held per runtime, so the superseded id is gone — report the
      // LIVE id rather than a false `no_pending_stale_event` so the agent
      // re-anchors onto the id that is actually pending. (Distinct from
      // stale-receive's mid-wait `stale_event_replaced`, which fires while a
      // pull is already parked; this one is the pre-flight entry check.)
      return {
        ok: true,
        resolved: false,
        reason: 'stale_event_superseded',
        current_stale_event_id: pending.id,
      };
    }
    const res = await this.backendPost(
      handle,
      internalPaths.agentResolveStaleMessage(params.agent_id),
      {
        // The backend requires the conversation as its authorization subject
        // (#task:2095 F17); fed VERBATIM from the 409's `target_conversation_id`
        // — the daemon holds no address-resolution authority to re-derive it.
        conversation_id: pending.targetConversationId,
        stale_event_id: pending.id,
        resolution_reason: params.resolution_reason,
      },
    );
    if (!res.ok) {
      return {
        ok: false,
        status: res.status,
        body_text: await safeReadResponseText(res),
      };
    }
    handle.pendingStaleEvent = null;
    return { ok: true, resolved: true };
  }

  // ── agent_stale_receive (#task:1614; custody contract #task:1606) ──

  /**
   * Deferred-pull second consumer. Everything custody-relevant happens
   * in `evaluateStaleReceiveLocked` under the per-handle lifecycle lock;
   * this method only owns the bind-once entry checks, the (lock-free)
   * bounded wait, and waiter cleanup. Custody settles exclusively in
   * `settleStaleReceiveHandoff`, driven by the ingress's response
   * transport handoff — never here.
   */
  async agentStaleReceive(
    params: AgentStaleReceiveParams,
    ctx: DeliveryOwnerContext,
  ): Promise<AgentStaleReceiveResult> {
    if (ctx.supportsResponseHandoff !== true) {
      // Fail closed BEFORE any custody decision: without a transport
      // handoff proof point the reservation could never settle and the
      // deferred queue would be held forever.
      throw new AgentStaleReceiveError(
        'handoff_unsupported',
        'agent_stale_receive requires an ingress that proves response transport handoff (control socket only)',
      );
    }
    const handle = this.getHandleForAgentIntent(params.runtime_scope, params.agent_id);
    const pending = handle.pendingStaleEvent;
    if (!pending) {
      return { ok: true, outcome: 'no_pending_stale' };
    }
    if (pending.id !== params.stale_event_id) {
      // #task:2095 F18 (pre-wait): a later 409 REPLACED the event before this
      // pull even parked. Report the LIVE id — symmetric with stale-resolve's
      // `stale_event_superseded` — so the agent re-anchors rather than reading
      // a false `no_pending_stale`. Distinct from the mid-wait
      // `stale_event_replaced` thrown below, which fires only after a wait was
      // already registered against the now-gone id.
      return {
        ok: true,
        outcome: 'stale_event_superseded',
        current_stale_event_id: pending.id,
      };
    }
    const staleEventId = pending.id;
    const waitMs = Math.min(
      params.wait_ms ?? STALE_RECEIVE_DEFAULT_WAIT_MS,
      STALE_RECEIVE_MAX_WAIT_MS,
    );
    const k = runtimeScopeKey(handle.runtimeScope);
    // Unique CALL identity (review P1-3): waiter registration/cleanup is
    // keyed by this token, so a rejected concurrent call's cleanup can
    // never clear a waiter it does not own.
    const waiterToken = ++this.pullWaiterSeq;
    // Call-scoped cancellation bit (review round-3): registered BEFORE
    // any lock wait so a connection close can cancel this call even
    // while it is still queued behind the lifecycle lock — i.e. before
    // any waiter exists to scan.
    const pullCall = { cancelled: false };
    let ownerCalls = this.activePullCallsByOwner.get(ctx.owner);
    if (!ownerCalls) {
      ownerCalls = new Set();
      this.activePullCallsByOwner.set(ctx.owner, ownerCalls);
    }
    ownerCalls.add(pullCall);
    const startedAt = this.now();
    const deadline = startedAt + waitMs;
    const cancelledOutcome = (): AgentStaleReceiveResult => ({
      ok: true,
      outcome: 'timeout',
      stale_event_id: staleEventId,
      waited_ms: Math.max(0, this.now() - startedAt),
    });
    try {
      for (;;) {
        const step = await this.withLifecycleLock(k, async () =>
          this.evaluateStaleReceiveLocked(k, handle, staleEventId, waiterToken, pullCall, ctx),
        );
        if (step.kind === 'cancelled') return cancelledOutcome();
        if (step.kind === 'result') return step.result;
        const remaining = deadline - this.now();
        if (remaining <= 0) {
          return {
            ok: true,
            outcome: 'timeout',
            stale_event_id: staleEventId,
            waited_ms: Math.max(0, this.now() - startedAt),
          };
        }
        // The WAIT itself: outside the lock, zero custody. `arrival` is
        // only a re-evaluation prompt; `cancelled` (the issuing control
        // connection closed — review P1-4) aborts the call with the
        // timeout arm's exact statement: nothing was obtained, custody
        // and the stale event are unchanged. Timer expiry loops back
        // into the deadline check above.
        const woke = await Promise.race([
          step.wakePromise,
          new Promise<'timer'>((resolve) => {
            const timer = setTimeout(() => resolve('timer'), remaining);
            timer.unref?.();
          }),
        ]);
        if (woke === 'cancelled') return cancelledOutcome();
        await this.clearOwnPullWaiter(k, handle, waiterToken);
      }
    } finally {
      ownerCalls.delete(pullCall);
      if (ownerCalls.size === 0) {
        this.activePullCallsByOwner.delete(ctx.owner);
      }
      await this.clearOwnPullWaiter(k, handle, waiterToken);
    }
  }

  /**
   * One locked arbitration step. Ordering is the contract's:
   *   1. binding validity (live handle, same stale event) — typed errors;
   *   2. take the next bounded FIFO prefix when — and only when — the
   *      queue is non-empty, no release is in progress
   *      (`startupDelivery === null`), and no reservation is unresolved;
   *   3. `normal_delivery_in_progress` when a release is already underway
   *      (`startupDelivery !== null`) — the covering content entered
   *      admission on its own path;
   *   4. otherwise register the waiter (still under the lock) and wait.
   * Queue-empty / timeout are never admission evidence (#task:1606).
   * #task:2095 F12①: the stale event's identity never participates in any
   * of these decisions — the retired per-stream observed-cursor coverage
   * check (`already_admitted`) is gone.
   */
  private evaluateStaleReceiveLocked(
    k: string,
    handle: RuntimeHandle,
    staleEventId: string,
    waiterToken: number,
    pullCall: { cancelled: boolean },
    ctx: DeliveryOwnerContext,
  ): StaleReceiveStep {
    // Round-3 pre-registration cancel: the issuing connection may have
    // closed while this call was queued behind the lifecycle lock. Bail
    // BEFORE any custody read or waiter registration — a dead call must
    // never take a batch or squat the waiter slot.
    if (pullCall.cancelled) return { kind: 'cancelled' };
    const live = this.handles.get(k);
    if (live !== handle || handle.phase === 'exited' || handle.phase === 'draining') {
      throw new AgentStaleReceiveError(
        'runtime_replaced',
        'runtime handle exited or was replaced while the pull was waiting',
      );
    }
    const stale = live.pendingStaleEvent;
    if (!stale) {
      // Resolved while we waited (e.g. a parallel send succeeded).
      return { kind: 'result', result: { ok: true, outcome: 'no_pending_stale' } };
    }
    if (stale.id !== staleEventId) {
      throw new AgentStaleReceiveError(
        'stale_event_replaced',
        'pending stale event changed while the pull was waiting; re-issue against the new event',
      );
    }
    // Dispatcher admission can precede a purge fence, so the definitive
    // TOCTOU check belongs under the same lifecycle lock as the queue splice.
    // Once fenced, no new pull reservation may be formed.
    if (this.agentHomeCoordinator.isFenced(live.agentId)) {
      throw new AgentDeletingError(live.agentId);
    }
    if (live.pullWaiter !== null || live.pullReservation !== null) {
      // One waiter/reservation per handle. Our own waiter is always
      // cleared before re-evaluating, so anything present here belongs
      // to a concurrent call.
      throw new AgentStaleReceiveError(
        'pull_already_active',
        'another stale-receive pull is already active for this runtime',
      );
    }
    if (live.deferredQueue.length > 0 && live.startupDelivery === null && live.phase === 'ready') {
      // TAKE — the single release-winner moment. Identical selection to
      // `flushDeferred` (same bounded FIFO prefix, same combine), so the
      // pull returns exactly what the next auto-flush would have written.
      const gated = profileUsesBoundaryGate(live);
      const batch = gated
        ? spliceBoundedDeferredPrefix(live.deferredQueue)
        : live.deferredQueue.splice(0, 1);
      const combined = combineDeferredBatch(batch, gated);
      let markSettled!: () => void;
      const settled = new Promise<void>((resolve) => {
        markSettled = resolve;
      });
      const reservation = {
        staleEventId,
        batch,
        combined,
        settled,
        markSettled,
      } satisfies PullReservation;
      live.pullReservation = reservation;
      let agentReservations = this.pullReservationsByAgent.get(live.agentId);
      if (!agentReservations) {
        agentReservations = new Set();
        this.pullReservationsByAgent.set(live.agentId, agentReservations);
      }
      agentReservations.add(reservation);
      // Custody settles only at the response transport handoff; the
      // ingress invokes this exactly once (#task:1614 proof point).
      ctx.responseHandoff = (handoffOk) => {
        void this.settleStaleReceiveHandoff(k, handle, handoffOk);
      };
      // #task:2095 F12: the outcome is a property of the QUEUE after the
      // splice, not of any per-stream seq coverage — did this prefix drain
      // the deferred queue, or is more still pending?
      const queueDrained = live.deferredQueue.length === 0;
      this.logger.info(
        {
          kind: 'process',
          action: 'stale_receive_prefix_reserved',
          conversationId: live.conversationId,
          agent_id: live.agentId,
          stale_event_id: staleEventId,
          delivery_ids: [...combined.deliveryIds],
          queue_drained: queueDrained,
        },
        'stale-receive pull reserved the next deferred FIFO prefix',
      );
      return {
        kind: 'result',
        result: {
          ok: true,
          outcome: queueDrained ? 'batch_queue_drained' : 'batch_more_pending',
          stale_event_id: staleEventId,
          text: combined.text,
          delivery_ids: [...combined.deliveryIds],
        },
      };
    }
    if (live.startupDelivery !== null) {
      // A release is already underway for this runtime — the covering content
      // entered admission on its own path. #task:2095 F12①: the stale event's
      // identity plays no part; a live `startupDelivery` is itself the proof
      // the field-of-view is draining.
      return {
        kind: 'result',
        result: {
          ok: true,
          outcome: 'normal_delivery_in_progress',
          stale_event_id: staleEventId,
        },
      };
    }
    // Nothing takeable and nothing provable: register the waiter under
    // the lock; the wait itself happens outside it. The waiter carries
    // this call's token (P1-3) and the issuing connection's owner
    // (P1-4 cancel leg).
    const wakePromise = new Promise<PullWakeReason>((resolve) => {
      live.pullWaiter = {
        staleEventId,
        token: waiterToken,
        owner: ctx.owner,
        wake: resolve,
      };
    });
    return { kind: 'wait', wakePromise };
  }

  /**
   * Resolve the pull-in-flight reservation — the ONLY place it clears.
   * Runs under the lifecycle lock, exactly once per reservation
   * (double invocations no-op on the null check).
   *
   *   - handoff OK → the tool result reached the CLI transport:
   *     constructive admission. One `runtime_delivery_consumed` for the
   *     whole batch; the content is now in the CURRENT turn's context. The
   *     retired per-stream observed cursor no longer participates
   *     (#task:2095) — only the published `deliveredUpToSeq` scalar sidecar
   *     is written, for crash-recovery dedup. Post-handoff crashes follow
   *     the existing stdin-write at-most-once handoff-loss semantics.
   *   - handoff failed, handle live → pre-proof failure: restore the
   *     prefix at the queue FRONT (later arrivals queued behind keep
   *     FIFO), zero consumed, zero observed.
   *   - handoff failed, handle gone → the queue it would restore into
   *     was already failed out by exit cleanup; settle the batch failed
   *     so the backend rolls the rows back to pending (exactly one
   *     settlement either way — exit cleanup never touches the
   *     reservation).
   */
  private async settleStaleReceiveHandoff(
    k: string,
    handle: RuntimeHandle,
    handoffOk: boolean,
  ): Promise<void> {
    let settledReservation: PullReservation | null = null;
    try {
      await this.withLifecycleLock(k, async () => {
        const reservation = handle.pullReservation;
        if (!reservation) return;
        settledReservation = reservation;
        handle.pullReservation = null;
        const live =
          this.handles.get(k) === handle &&
          handle.phase !== 'exited' &&
          handle.phase !== 'draining';
        if (handoffOk) {
          bindRuntimeFocus(handle, reservation.combined.conversationId);
          this.emitStartupDeliveryConsumed(handle, reservation.combined.deliveryIds);
          // #task:2095: a pull feeds the CURRENT context and starts no turn;
          // the retired observed-cursor admission (which marked turn activity
          // for a stdin write) is gone, so there is nothing to advance and no
          // turn-activity bit to preserve here. Only the published
          // `deliveredUpToSeq` scalar sidecar is written, for crash-recovery
          // dedup.
          await this.writeLastDeliveredSeq(
            handle,
            deliveredUpToSeqOf(reservation.combined.deliveries),
          );
          this.logger.info(
            {
              kind: 'process',
              action: 'stale_receive_settled_consumed',
              conversationId: handle.conversationId,
              agent_id: handle.agentId,
              delivery_ids: [...reservation.combined.deliveryIds],
            },
            'stale-receive handoff proven; batch consumed and admitted',
          );
        } else if (live) {
          handle.deferredQueue.unshift(...reservation.batch);
          this.logger.warn(
            {
              kind: 'process',
              action: 'stale_receive_restored',
              conversationId: handle.conversationId,
              agent_id: handle.agentId,
              delivery_ids: [...reservation.combined.deliveryIds],
            },
            'stale-receive handoff failed before the proof point; prefix restored to the deferred queue in place',
          );
        } else {
          for (const entry of reservation.batch) {
            bindRuntimeFocus(handle, entry.conversationId);
            this.emitStartupDeliveryFailed(handle, entry.deliveryIds, 'child_exit');
          }
          this.logger.warn(
            {
              kind: 'process',
              action: 'stale_receive_failed_dead_handle',
              conversationId: handle.conversationId,
              agent_id: handle.agentId,
              delivery_ids: [...reservation.combined.deliveryIds],
            },
            'stale-receive handoff failed and the runtime is gone; batch settled failed for backend rollback',
          );
        }
        this.maybeScheduleDeferredFlush(handle);
      });
    } finally {
      // The caller-visible settlement happens-before (P1-2 v2): resolve
      // AFTER the locked section completed, unconditionally — a hung
      // `settled` promise would block the send barrier forever.
      const reservation = settledReservation as PullReservation | null;
      reservation?.markSettled();
      if (reservation) {
        const agentReservations = this.pullReservationsByAgent.get(handle.agentId);
        agentReservations?.delete(reservation);
        if (agentReservations?.size === 0) {
          this.pullReservationsByAgent.delete(handle.agentId);
        }
      }
    }
  }

  /** Remove this CALL's own waiter under the lock, keyed by the call
   *  token (P1-3) — idempotent, never touches another call's waiter. */
  private clearOwnPullWaiter(k: string, handle: RuntimeHandle, waiterToken: number): Promise<void> {
    return this.withLifecycleLock(k, async () => {
      if (handle.pullWaiter?.token === waiterToken) {
        handle.pullWaiter = null;
      }
    });
  }

  /** Prompt a registered stale-receive waiter to re-evaluate. Purely a
   *  wake — custody decisions happen only in the waiter's next locked
   *  evaluation step. */
  private wakePullWaiter(handle: RuntimeHandle): void {
    handle.pullWaiter?.wake('arrival');
  }

  // ── runtime_start ──────────────────────────────────────────────

  /**
   * Public start entry — funnels every semantic RuntimeScope start through
   * the lifecycle lock, then the cooldown gate, then the actual
   * spawn path. Cindy plan v2:
   *   - Lock granularity: per RuntimeScope, not global. Different
   *     keys spawn in parallel; same key serializes start↔stop↔
   *     clearSession↔internal-retry-respawn.
   *   - Cooldown gate scope: ONLY for fresh user-initiated starts
   *     (`initAttempt === 0`) AND only when no live handle exists.
   *     Idempotent return for an already-running handle is the
   *     success arm even if cooldown would otherwise apply, and
   *     daemon-internal retry respawns (attempt > 0) bypass the gate
   *     entirely so an in-flight retry can complete.
   */
  async start(params: RuntimeStartParams): Promise<RuntimeStartResult> {
    this.assertAgentHomeOpen(params.agent_id);
    return this.startWithLock(params, 0);
  }

  agentHomeMutationCoordinator(): AgentHomeMutationCoordinator {
    return this.agentHomeCoordinator;
  }

  private startWithLock(
    params: RuntimeStartParams,
    initAttempt: number,
  ): Promise<RuntimeStartResult> {
    const k = runtimeScopeKey(params.options.runtime_scope);
    return this.withLifecycleLock(k, () => this.startUnlocked(params, initAttempt));
  }

  private async startUnlocked(
    params: StartUnlockedParams,
    initAttempt: number,
  ): Promise<RuntimeStartResult> {
    if (this.shuttingDown) throw new Error('runtime_start: daemon is shutting down');
    this.assertAgentHomeOpen(params.agent_id);
    const k = runtimeScopeKey(params.options.runtime_scope);

    // Idempotent on RuntimeScope — second start while running
    // returns the existing pid + started_at, matching CM today.
    // A dead handle (phase === 'exited') is treated as gone; we
    // re-spawn. The repeat call's `startup_delivery` (if any) is
    // ignored: the original startup batch is the authoritative one
    // (start is not a delivery substitute). Callers wanting to push a
    // new batch after start must use `runtime_deliver`.
    //
    // v6 (#task:1417 Stage 2c): `runtime_start` is pure lifecycle — its
    // result is a bare `{started, pid, started_at}`, no `phase` /
    // `startup_delivery_accepted`. Delivery custody is transferred solely
    // by `runtime_deliver`'s sync disposition + async
    // `runtime_delivery_{consumed,failed}`; `startup_delivery` survives
    // only as the daemon-internal spawn-with-batch seam threaded here from
    // `deliver()`'s SPAWN arm.
    const existing = this.handles.get(k);
    if (existing && existing.phase !== 'exited') {
      if (existing.runtimeGeneration !== params.runtime_generation) {
        throw new Error(
          `runtime generation mismatch for ${params.conversation_id}/${params.agent_id}`,
        );
      }
      return {
        started: true,
        pid: existing.pid,
        started_at: existing.startedAt,
      };
    }
    if (existing && existing.phase === 'exited') {
      this.handles.delete(k);
    }

    // 3c-b cooldown gate (Cindy plan v2 issue #3). Fires only when:
    //   - this is a fresh user-initiated start (initAttempt === 0), AND
    //   - no live handle remains (idempotent success returned above).
    // Internal retry respawns (initAttempt > 0) bypass — the watchdog
    // already decided retry eligibility at the moment of failure;
    // re-gating here would deadlock a half-spawned retry against its
    // own cooldown clock.
    if (initAttempt === 0 && this.isInitCoolingDown(k)) {
      const state = this.initRetryStates.get(k);
      const cooldownUntil = state?.cooldownUntil ?? this.now();
      this.logger.info(
        {
          kind: 'process',
          action: 'start_blocked_by_cooldown',
          conversationId: params.conversation_id,
          agent_id: params.agent_id,
          cooldown_until: cooldownUntil,
        },
        'start refused: init failure cooldown active',
      );
      return {
        started: false,
        reason: 'init_cooldown',
        cooldown_until: cooldownUntil,
      };
    }

    const startOptions = params.options;
    const runtimeKind = startOptions.driver.kind;

    // D4.e-6 — resolve every host path from the semantic wire options
    // (deterministic bundle coordinate for `regular`, `resolveAgentDir` for
    // `agent_home`). Throws a typed `RuntimeStartResolveError` (→
    // `workspace_not_provisioned` / `registry_unavailable`) only for a
    // foreign/corrupt `agent_home` ref or an unsafe id segment. Done after the
    // idempotent + cooldown gates so an already-running handle never pays it,
    // and before any spawn work.
    const resolvedOptions = await resolveRuntimeStartPaths(
      startOptions,
      this.runtimeStartResolveDeps,
    );
    // shiki 2026-08-04: the session cwd is the ONLY hard-ensured runtime base.
    // Create + require a usable working dir before forking; L1/L2 CONTENT is
    // best-effort and never blocks the spawn.
    await ensureRuntimeScopeCwd(resolvedOptions, this.logger);

    const driver = this.driverFor(runtimeKind);
    const resolved = await this.resolveRuntime(runtimeKind, driver.binary);
    const label = runtimeCatalogEntry(runtimeKind).cliLabel;
    if (!resolved) {
      throw new RuntimeStartResolveError(
        'runtime_binary_missing',
        `${label} is not installed on this computer.`,
      );
    }
    if (driver.minimumVersion) {
      const verdict = compareToMinimum(resolved.version, driver.minimumVersion);
      if (verdict === 'below') {
        throw new RuntimeStartResolveError(
          'runtime_version_unsupported',
          `${label} ${parsedVersion(resolved.version)} is not supported. Update to ${driver.minimumVersion} or newer.`,
        );
      }
      if (verdict === 'unknown') {
        this.logger.warn(
          { runtime: runtimeKind, minimum: driver.minimumVersion },
          'runtime_start: CLI version unknown; starting without the version gate',
        );
      }
    }
    const binary = resolved.binary;
    // #task:1909 — pin version-derived driver capabilities (qoder's
    // release profile) to THIS process generation, before anything reads
    // `driver.releaseProfile` or encodes a stdin frame. Driver instances
    // are per-spawn, so a CLI upgrade mid-session can never change how the
    // child started here is driven.
    driver.bindRuntimeVersion?.(resolved.version);

    const promptCtx = await buildPromptContextFromRuntimeStartOptions(resolvedOptions);
    const systemPromptText = buildSystemPromptFor(runtimeKind, promptCtx);
    const wireSilverhandCli = startOptions.env_policy.silverhand_cli;
    const serverSilverhandCli: SilverhandCliLauncher | null = wireSilverhandCli
      ? {
          command: wireSilverhandCli.command,
          args: wireSilverhandCli.args,
          ...(wireSilverhandCli.env_vars ? { env: wireSilverhandCli.env_vars } : {}),
        }
      : null;
    const controlPlaneServerUrl = this.controlPlaneServerUrl();
    if (!controlPlaneServerUrl) {
      throw new Error(
        'runtime_start: no silverhand server URL supplied by server and no daemon-local control-plane URL is attached',
      );
    }
    const spawnPrep = await prepareSpawnFiles({
      runtimeScope: startOptions.runtime_scope,
      agentId: params.agent_id,
      systemPromptText,
      silverhandCli: chooseSilverhandCliLauncher({
        serverLauncher: serverSilverhandCli,
        localLauncher: this.localSilverhandCliLauncher,
      }),
      silverhandEnv: {
        ...(startOptions.runtime_scope.kind === 'conversation'
          ? { [SILVERHAND_CONVERSATION_ID_ENV]: params.conversation_id }
          : {}),
        [SILVERHAND_AGENT_ID_ENV]: params.agent_id,
        [SILVERHAND_SERVER_URL_ENV]: controlPlaneServerUrl,
        [SILVERHAND_AUTH_TOKEN_ENV]: startOptions.env_policy.silverhand.auth_token,
      },
      daemonSocketPath: this.daemonSocketPath ?? undefined,
    });

    const spawnCtx: DriverSpawnContext = {
      agentId: params.agent_id,
      agentName: startOptions.agent.name,
      workspacePath: resolvedOptions.cwd,
      cliSessionId: startOptions.driver.cli_session_id,
      model: startOptions.driver.model,
      reasoningEffort: startOptions.driver.reasoning_effort,
      fastMode: startOptions.driver.fast_mode,
      systemPromptPath: spawnPrep.systemPromptPath,
      initialPrompt: driver.oneShot && params.startup_delivery ? params.startup_delivery.text : '',
    };

    // Profile MCP (#task:1135): the subject's additive MCP layer. Keyed by
    // runtime, drops the whole layer (loud) on a bad file but never blocks
    // spawn. argv runtimes get flags appended after buildArgs; opencode
    // (#task:1405) gets `env`; runtimes that place it themselves read
    // `spawnCtx.profileMcp`, so it is set before buildArgs.
    const mcpSpawn = await buildProfileMcpSpawnArgs({
      runtimeKind,
      agentHomeDir: resolvedOptions.agent.home_dir,
      spawnDir: spawnPrep.dir,
      systemPromptPath: spawnPrep.systemPromptPath,
      agentId: params.agent_id,
      logger: this.logger,
    });
    spawnCtx.profileMcp = mcpSpawn.profileMcp;
    const args = driver.buildArgs(spawnCtx);
    args.push(...(mcpSpawn.args ?? []));
    const daemonEnv = buildDaemonSpawnEnv({
      driver,
      options: startOptions,
      conversationId: params.conversation_id,
      agentId: params.agent_id,
      daemonSocketPath: this.daemonSocketPath ?? undefined,
      lastDeliveredSeqPath: spawnPrep.lastDeliveredSeqPath,
      binDir: spawnPrep.binDir,
      controlPlaneServerUrl,
      proxyConfig: readDaemonProxyConfig(this.proxyConfigPath(), this.logger),
    });
    const env: NodeJS.ProcessEnv = {
      ...daemonEnv,
      // The profile-MCP env layer (opencode's OPENCODE_CONFIG_CONTENT) is
      // authoritative for its own keys. The Silverhand identity/control-plane
      // env is re-applied below because those four keys are the child CLI's
      // integrity boundary and must not be shadowed by any additive layer.
      ...(mcpSpawn.env ?? {}),
      ...(startOptions.runtime_scope.kind === 'conversation'
        ? {
            [SILVERHAND_CONVERSATION_ID_ENV]: daemonEnv[SILVERHAND_CONVERSATION_ID_ENV],
          }
        : {}),
      [SILVERHAND_AGENT_ID_ENV]: daemonEnv[SILVERHAND_AGENT_ID_ENV],
      [SILVERHAND_SERVER_URL_ENV]: daemonEnv[SILVERHAND_SERVER_URL_ENV],
      [SILVERHAND_AUTH_TOKEN_ENV]: daemonEnv[SILVERHAND_AUTH_TOKEN_ENV],
    };
    if (startOptions.runtime_scope.kind === 'agent') {
      delete env[SILVERHAND_CONVERSATION_ID_ENV];
    }
    const startedAt = this.now();

    // #task:1420 — pre-spawn diagnostics. Log-only: capture exactly what env
    // and which binary this daemon process is about to exec, so the next
    // runtime failure is legible without a repro. Secret-safe by construction
    // (see spawn-diagnostics.ts): no token/secret/cert body is emitted.
    this.logger.info(
      {
        kind: 'process',
        action: 'runtime_spawn_attempt',
        daemon_pid: process.pid,
        daemon_started_at: this.daemonStartedAt,
        conversationId: params.conversation_id,
        agent_id: params.agent_id,
        runtime: runtimeKind,
        binary,
        binary_source: platformBinaryCandidates(runtimeKind).includes(binary)
          ? 'platform_candidate'
          : 'path_or_detected',
        argv: redactSpawnArgs(args),
        cwd: spawnCtx.workspacePath,
        path_summary: summarizeSpawnPath(env.PATH),
        env_summary: summarizeSpawnEnv(env),
        silverhand_env_presence: {
          child: summarizeSilverhandEnvPresence(env, {
            conversationIdRequired: startOptions.runtime_scope.kind === 'conversation',
          }),
          shim: spawnPrep.silverhandShimEnvPresence,
        },
      },
      'spawning runtime child',
    );

    // Preparation above awaits filesystem/config work. Shutdown may have begun
    // before a handle existed; recheck at the synchronous spawn boundary.
    if (this.shuttingDown) throw new Error('runtime_start: daemon is shutting down');
    const child = this.spawnFn(binary, args, {
      cwd: spawnCtx.workspacePath,
      env,
      stdio: ['pipe', 'pipe', 'pipe'],
      // Keep the child attached; daemon shutdown explicitly drains handles.
      // A signal sent to the daemon PID alone is not a process-group signal.
    });

    // `spawn()` can return a ChildProcess with no pid and emit `error`
    // asynchronously. Register before checking pid so a single failed
    // runtime_start cannot become an unhandled EventEmitter error that
    // terminates the daemon.
    const earlyChildErrorListener = (err: Error): void => {
      const spawnError = describeSpawnError(err);
      this.logger.error(
        {
          kind: 'process',
          action: 'child_error_before_handle',
          daemon_pid: process.pid,
          daemon_started_at: this.daemonStartedAt,
          conversationId: params.conversation_id,
          agent_id: params.agent_id,
          runtime: runtimeKind,
          binary,
          binary_source: platformBinaryCandidates(runtimeKind).includes(binary)
            ? 'platform_candidate'
            : 'path_or_detected',
          workspace_path: spawnCtx.workspacePath,
          spawn_error: spawnError,
        },
        `runtime child failed to spawn before handle was registered: ${spawnError.readable}`,
      );
    };
    child.once('error', earlyChildErrorListener);

    const pid = child.pid;
    if (typeof pid !== 'number') {
      throw new Error(`runtime_start: child spawned without a pid for runtime "${runtimeKind}"`);
    }
    try {
      this.writeRuntimePidfile({
        runtimeScope: params.options.runtime_scope,
        pid,
        startedAt,
        runtime: runtimeKind,
        cmd: binary,
      });
    } catch (err) {
      try {
        child.kill('SIGKILL');
      } catch {
        /* already gone */
      }
      throw new Error(
        `runtime_start: failed to write runtime pidfile for "${params.conversation_id}/${params.agent_id}": ${(err as Error).message}`,
      );
    }

    // Step-0 mutex (Cindy `_JjyK8UrpcmYA5Iu`): if backend supplied a
    // startup_delivery, the runtime starts in `startup_pending` and
    // the gate stays closed until the supervisor settles the batch.
    // Without startup_delivery the gate is already open.
    if (params.startup_delivery) {
      this.assertDeliveryMetadataConsistent(
        params.conversation_id,
        params.agent_id,
        params.startup_delivery.delivery_ids,
        params.startup_delivery.deliveries,
        'startup_delivery',
      );
    }
    const startupBatch =
      params.startup_delivery && !driver.oneShot
        ? {
            conversationId: params.startup_delivery.conversation_id,
            actionWorkdir: params.startup_delivery.action_workdir,
            deliveryIds: [...params.startup_delivery.delivery_ids],
            deliveries: params.startup_delivery.deliveries,
            text: params.startup_delivery.text,
          }
        : null;

    const handle: RuntimeHandle = {
      runtimeScope: params.options.runtime_scope,
      cwd: resolvedOptions.cwd,
      conversationId: params.conversation_id,
      agentId: params.agent_id,
      runtimeGeneration: params.runtime_generation,
      pid,
      startedAt,
      driver,
      child,
      lastDeliveredSeqPath: spawnPrep.lastDeliveredSeqPath,
      inputTurnActive: driver.oneShot && !!params.startup_delivery,
      nextTurnReserved: false,
      nextTurnReservedAt: null,
      reservationStallWarned: false,
      reservationStallTimer: null,
      turnEndCount: 0,
      pendingStaleEvent: null,
      ensuredActionWorkdirs: new Set(
        params.startup_delivery ? [params.startup_delivery.action_workdir] : [],
      ),
      inputPayloadUuids: new Map(),
      phase: startupBatch ? 'startup_pending' : 'spawning',
      outputPaused: false,
      exitCode: null,
      intentionalExit: false,
      sigkillTimer: null,
      fatalReclaimTimer: null,
      // 3b additions
      startupDelivery: startupBatch,
      startupAwaitingInit: false,
      startupAdmission: null,
      stdoutEnded: false,
      startupGateExitReason: null,
      startupConsumeInFlight: false,
      readyForDrainEmitted: false,
      initObserved: false,
      cliSessionId: spawnCtx.cliSessionId,
      stdoutLineBuffer: '',
      runState: null,
      activitySnippetBuffer: null,
      busy: { outstandingToolUses: 0, compacting: false },
      deferredQueue: [],
      flushingDeferred: false,
      pullReservation: null,
      pullWaiter: null,
      // 3c-b additions
      // Strip `startup_delivery` from the cached params so an internal
      // retry respawn doesn't try to replay a payload the previous
      // attempt already consumed (or already failed). Backend re-pumps
      // pending inbox rows via `runtime_deliver` against the new handle.
      spawnParams: {
        conversation_id: params.conversation_id,
        agent_id: params.agent_id,
        runtime_generation: params.runtime_generation,
        options: params.options,
      },
      initWatchdogArmed: false,
      initStartedAt: startedAt,
      initProtocolObserved: false,
      initTimedOut: false,
      initTimeoutReason: null,
      initLivenessSilenceTimer: null,
      initPreProtocolCapTimer: null,
      initPostProtocolTimer: null,
      initAttempt,
      initRetryPlanned: false,
      compactionWatchdog: null,
    };
    this.handles.set(k, handle);

    this.logger.info(
      {
        kind: 'process',
        action: 'spawn',
        pid,
        conversationId: handle.conversationId,
        agent_id: handle.agentId,
        runtime: runtimeKind,
        // #task:1909 — the release profile can be version-derived, so the
        // spawn record is the only place the pair is observable. Backend's
        // static driver view logs the class default and may differ.
        runtime_version: resolved.version,
        release_profile: driver.releaseProfile,
        startup_delivery: startupBatch ? startupBatch.deliveryIds : null,
      },
      'spawned agent runtime',
    );

    // Initial phase emit — same contract as FakeSupervisor.
    this.emitState(handle);

    this.attachChildListeners(handle);
    child.off('error', earlyChildErrorListener);

    if (driver.oneShot && params.startup_delivery) {
      // oneShot drivers consume the startup batch at spawn time (no gate).
      // #task:2095: the retired per-stream observed admission is gone; the
      // scalar sidecar write below still records the last delivered seq
      // (dedup cursor only on a ConversationSlot; diagnostic on an AgentSlot —
      // see writeLastDeliveredSeq), and `inputTurnActive` was already set at
      // handle init (a oneShot spawn carrying a startup batch begins a turn).
      void this.writeLastDeliveredSeq(
        handle,
        deliveredUpToSeqOf(params.startup_delivery.deliveries),
      );
      this.emitStartupDeliveryConsumed(handle, params.startup_delivery.delivery_ids);
    }

    // 3c-b: driver-side init handshake. Codex needs `bindIO` to send
    // its JSON-RPC `initialize` → `thread/start` immediately after
    // spawn — without it the child never reaches `thread/started` and
    // every spawn hits the post-protocol watchdog. Drivers without a
    // handshake (claude, cursor) leave `bindIO` undefined.
    //
    // The writeLine wrapper (a) routes through `writeStdinPayload`
    // which is the single point of stdin writes (preserves the
    // backpressure / drain semantics) and (b) asserts the driver did
    // NOT include a trailing `\n` — `writeStdinPayload` always appends
    // one, so a driver that already terminates would emit a
    // double-newline frame. Cindy `-pdNvRYOWip5e9yA` flagged this as
    // the bindIO failure mode worth catching at the seam, not at the
    // child. Cindy `HyC05fC-FEs94uoZ` follow-up: throw SYNCHRONOUSLY
    // (not a rejected Promise) — drivers like Codex call
    // `void this.io?.writeLine(...)` which would swallow a rejection
    // into an unhandled rejection. A sync throw blows up the bound
    // callsite immediately and surfaces during driver-author testing.
    if (handle.driver.bindIO) {
      handle.driver.bindIO({
        writeLine: (line: string) => {
          if (line.endsWith('\n')) {
            throw new Error(
              'driver.writeLine: line must not end with "\\n" (writeStdinPayload appends the newline)',
            );
          }
          // A stopping runtime must not start new work from driver-initiated writes.
          if (handle.phase === 'draining' || handle.phase === 'exited') {
            return Promise.reject(new Error(`runtime is ${handle.phase}`));
          }
          return this.writeStdinPayload(handle, line);
        },
        spawnCtx,
        onInputRejected: (uuid) => this.observeInputRejected(handle, uuid),
      });
    }

    // 3c-b: 3-tier init watchdog. Two arming paths:
    //
    //   - `self_inits`: arm immediately at spawn. These runtimes own
    //     their startup protocol and should emit session-ready without
    //     any user stdin.
    //   - `requires_first_message`: defer until the FIRST successful
    //     stdin write (a `runtime_deliver` batch — at spawn or into a
    //     ready handle). Claude
    //     Code does not emit `system.init` merely because the process
    //     spawned; it emits only after accepting a real user turn. A
    //     spawn-time watchdog therefore kills legitimate idle Claude
    //     processes and retry respawns that are waiting for backend to
    //     replay the still-pending inbox row.
    if (handle.driver.kickoffMode !== 'requires_first_message') {
      this.startInitWatchdog(handle);
    }

    // Optimistic ready transition. The init watchdog is armed above
    // when required; we still mark the runtime `ready` after a single
    // tick so backend can observe a live process while protocol init
    // continues. The startup_delivery write below settles
    // asynchronously; until it settles, `maybeEmitReadyForDrain` is
    // gated by `handle.startupDelivery !== null` and will not emit.
    // (Cindy `_JjyK8UrpcmYA5Iu`: pending startup batch keeps
    // ready_for_drain bottled up; a concurrent `runtime_deliver` is
    // sync-refused `startup_pending` during the same window.)
    setImmediate(() => {
      if (handle.phase !== 'spawning' && handle.phase !== 'startup_pending') {
        return;
      }
      handle.phase = 'ready';
      this.emitState(handle);
      this.maybeEmitReadyForDrain(handle);
    });

    // Consume startup_delivery asynchronously — settle order (Cindy
    // `wuiwdLKrLFyGip_s` + Option A v2 `UMGu36zoTDVuKWXi`):
    //   1. wait until `driver.readyToEncode()` (stateless drivers like
    //      claude: immediate; stateful drivers like codex: after
    //      `thread/started`)
    //   2. encode raw text via `driver.encodeStdinMessage(text, cliSessionId)`
    //   3. write encoded line to stdin (or fail)
    //   4. emit `runtime_delivery_consumed` OR `_failed`
    //   5. clear `handle.startupDelivery = null`
    //   6. THEN re-evaluate ready_for_drain
    // We don't `await` here — start() must return promptly so backend
    // can move on; the settle continues on the next microtask.
    if (startupBatch) {
      void this.consumeStartupDelivery(handle);
    }

    return {
      started: true,
      pid,
      started_at: startedAt,
    };
  }

  private async consumeStartupDelivery(handle: RuntimeHandle): Promise<void> {
    // Option A v2 (Cindy `UMGu36zoTDVuKWXi`): startup batch carries
    // RAW TEXT on `handle.startupDelivery.text`. Daemon owns encode
    // because the driver instance with the right runtime state lives
    // here, not in backend. Two stateful pre-conditions, in order:
    //
    //   - exited/draining → fail with `child_exit` (same as before)
    //   - `!driver.readyToEncode()` → DEFER until init/handshake.
    //     For codex, `readyToEncode = threadId !== null`. The deferred
    //     consume kicks back in from `updateRunStateFromParsed` after
    //     `session_init` (claude) or `handleThreadReady` (codex) flips
    //     state. The gate stays closed across the defer so a concurrent
    //     `runtime_deliver` and `runtime_ready_for_drain` keep their
    //     blocking behavior.
    //   - encode returns null AFTER ready → fail with `driver_refused`
    //     (config/programming error; fail loud per Cindy
    //     `0G5uJBydO_4qt_2r`)
    //   - encode + write succeeds → settle as consumed (or, for the
    //     `requires_first_message + !initObserved` arm, mark
    //     `startupAwaitingInit` and let `session_init` settle —
    //     unchanged from 3b).
    //
    // After ANY settle, the gate clears via `clearStartupGate` (which
    // wipes both `startupDelivery` and `startupAwaitingInit`) and we
    // re-evaluate `ready_for_drain` exactly once.
    const batch = handle.startupDelivery;
    if (!batch) return; // gate already cleared by another path
    handle.startupConsumeInFlight = true;
    try {
      await this.consumeStartupDeliveryLatched(handle, batch);
    } finally {
      handle.startupConsumeInFlight = false;
    }
  }

  /** Body of {@link consumeStartupDelivery}; runs under the
   *  `startupConsumeInFlight` latch. */
  private async consumeStartupDeliveryLatched(
    handle: RuntimeHandle,
    batch: StartupDeliveryState,
  ): Promise<void> {
    // AgentSlot identity is conversation-free; presentation, ack and stream
    // freshness remain bound to the batch's origin target.
    bindRuntimeFocus(handle, batch.conversationId);
    const deliveryIds = batch.deliveryIds;

    if (handle.phase === 'exited' || handle.phase === 'draining') {
      this.emitStartupDeliveryFailed(handle, deliveryIds, 'child_exit');
      this.clearStartupGate(handle);
      this.maybeEmitReadyForDrain(handle);
      return;
    }

    const driverReady = driverReadyToEncode(handle.driver);
    if (!driverReady) {
      // Defer — driver not ready yet. session_init / handleThreadReady
      // will retry via `tryConsumeStartupDelivery(handle)`. NO emit, NO
      // gate clear; the batch stays pending. Log once per defer so the
      // operator can see the transition.
      this.logger.info(
        {
          kind: 'process',
          action: 'startup_delivery_defer_pre_ready',
          pid: handle.pid,
          conversationId: handle.conversationId,
          agent_id: handle.agentId,
          delivery_ids: [...deliveryIds],
        },
        'startup_delivery deferred: driver not ready to encode (e.g. codex pre-thread/started)',
      );
      return;
    }

    const payloadUuid = this.mintInputPayloadUuid(handle, deliveryIds);
    const encoded = handle.driver.encodeStdinMessage(batch.text, handle.cliSessionId, {
      payloadUuid,
    });
    if (encoded === null) {
      // Driver claims ready_to_encode but still refused. This is a
      // driver-author bug; fail loud per Cindy `0G5uJBydO_4qt_2r`.
      this.logger.warn(
        {
          kind: 'process',
          action: 'startup_delivery_driver_refused',
          pid: handle.pid,
          conversationId: handle.conversationId,
          agent_id: handle.agentId,
          delivery_ids: [...deliveryIds],
          text_bytes: batch.text.length,
        },
        'startup_delivery encode returned null after driver claimed ready (driver bug?)',
      );
      this.emitStartupDeliveryFailed(handle, deliveryIds, 'driver_refused');
      this.clearStartupGate(handle);
      this.maybeEmitReadyForDrain(handle);
      return;
    }

    // #task:2210: arm the `lifecycle-start` admission gate BEFORE the stdin
    // write. `dispatchParsedLine` runs synchronously from the stdout data
    // listener OUTSIDE this lifecycle lock, so a fast runtime that echoes
    // `input_lifecycle{state:'started'}` the instant it reads the payload can
    // be dispatched during the write's own await window (or the
    // `writeLastDeliveredSeq` await below) — i.e. BEFORE any post-write
    // bookkeeping. Arming here means such an early `started` is RECORDED
    // (`startupAdmission.startedObserved`) but does NOT settle: settlement is
    // the conjunction `startedObserved && writeConfirmed`, so an early proof
    // waits for the write's full success boundary and a write failure in
    // between settles `failed` only. The write-failure catch below resets the
    // whole gate via `clearStartupGate`.
    if (handle.driver.admissionPolicy === 'lifecycle-start') {
      handle.startupAdmission = {
        uuid: payloadUuid,
        startedObserved: false,
        writeConfirmed: false,
        rejected: false,
      };
    }

    try {
      // #task:2095: the retired per-stream observed-cursor admission is gone —
      // freshness is now the RuntimeScope-wide undelivered-inbox guard, so a
      // mid-turn write no longer needs a per-stream admit/queue decision. A
      // stdin write DOES start a turn, which drives two turn-lifecycle flags —
      // and they are NOT maintained the same way (F25):
      //   - `inputTurnActive` is a profile-INDEPENDENT lifecycle fact ("the
      //     turn this write started is still running"). It is set at handle
      //     init for any oneShot startup delivery (:1814) and cleared for
      //     EVERY profile on terminal/exit (:4232); the write path maintains
      //     it the same way — unconditionally, gated only by the snapshot
      //     below. It happens to have a reader ONLY in the `next-turn-batch`
      //     boundary today (:415), but its MEANING is not a function of
      //     release profile, so its three maintenance sites must stay
      //     consistent rather than couple this one bit to a single profile.
      //   - `nextTurnReserved` is a next-turn-batch-SPECIFIC concept (§3.4
      //     step 2 reservation), so its set stays gated on that profile.
      // Both are cleared by the parsed terminal `turn_end`. The scalar
      // `deliveredUpToSeq` sidecar is written afterward (see writeLastDeliveredSeq).
      //
      // Snapshot the turn-end count BEFORE `await writeStdinPayload`.
      // `dispatchParsedLine` runs synchronously from the stdout data listener
      // and is NOT scoped under this handle's lifecycle lock, so a `turn_end`
      // processed during the write's await window would flip
      // `inputTurnActive=false` and consume the terminal. Setting either flag
      // off a post-await read (unguarded) would then overwrite a finished turn
      // back into the boundary-closed state with no terminal left to reopen it
      // — holding the queue forever (#1558 final-gate P1-1). Both are therefore
      // guarded by the one `turnEndsBeforeWrite` snapshot below.
      const turnEndsBeforeWrite = handle.turnEndCount;
      await this.writeStdinPayload(handle, encoded);
      const turnStartedByThisWriteStillRunning = handle.turnEndCount === turnEndsBeforeWrite;
      if (turnStartedByThisWriteStillRunning) {
        // Profile-independent lifecycle fact (F25): set under the snapshot
        // guard ALONE, not gated on release profile, so this site stays
        // consistent with the profile-agnostic init (:1814) and clear (:4232).
        // If the terminal for this write already crossed the await window it
        // has been consumed; re-setting the bit true would close the
        // next-turn-batch boundary (:415) with no terminal left to reopen it,
        // starving the queue permanently (#1558 final-gate P1-1).
        handle.inputTurnActive = true;
      }
      if (
        handle.driver.releaseProfile === 'next-turn-batch' &&
        turnStartedByThisWriteStillRunning
      ) {
        // §3.4 step 2: reserve the next turn the instant the write
        // lands — do NOT wait for the runtime's first activity frame.
        // Same snapshot guard: if the terminal for this very write crossed
        // the await window (fast runtime), arming now would overwrite a
        // finished turn back into "reserved" and hold the queue forever
        // (#1558 final-gate P1-1).
        handle.nextTurnReserved = true;
        handle.nextTurnReservedAt = this.now();
        handle.reservationStallWarned = false;
        // Fresh reservation: drop any stall timer from the previous one
        // and re-schedule if a bounded-prefix remainder is still queued.
        this.clearReservationStallTimer(handle);
        this.scheduleReservationStallCheck(handle);
      }
      await this.writeLastDeliveredSeq(handle, deliveredUpToSeqOf(batch.deliveries));
    } catch (err) {
      this.logger.warn(
        {
          kind: 'process',
          action: 'startup_delivery_write_failed',
          pid: handle.pid,
          conversationId: handle.conversationId,
          agent_id: handle.agentId,
          delivery_ids: [...deliveryIds],
          error_message: (err as Error).message,
        },
        'startup_delivery stdin write failed',
      );
      this.emitStartupDeliveryFailed(handle, deliveryIds, 'write_failed');
      this.clearStartupGate(handle);
      this.maybeEmitReadyForDrain(handle);
      return;
    }

    // `requires_first_message` drivers reach session init only after
    // this first real user turn lands on stdin. Arm the init watchdog
    // from the write boundary, not from process spawn. For self-init
    // drivers the watchdog was already armed at spawn, so this is a
    // no-op.
    this.ensureInitWatchdogArmed(handle);

    // #task:2210: the stdin write reached its FULL success boundary (this
    // point is past `writeStdinPayload` AND `writeLastDeliveredSeq`, the same
    // boundary the write-success path settles on below — deliberately NOT
    // shortened). For `lifecycle-start` drivers record write-readiness and try
    // to settle: settlement is the conjunction `startedObserved &&
    // writeConfirmed`, so if the runtime already echoed a matching
    // `started` during the write (early proof) THIS is the second half and we
    // settle exactly once here; otherwise the gate stays closed and the
    // `input_lifecycle` handler settles when the (late) `started` arrives. A
    // successful stdin write ALONE never settles (#task:2095 anti-proof); a
    // child exit before settlement fails the still-pending gate back to
    // pending (consumer-death rollback). session_init is process-ready, never
    // this payload's admission. Precedes the requires_first_message/immediate
    // paths below.
    if (handle.driver.admissionPolicy === 'lifecycle-start') {
      if (handle.startupAdmission !== null) {
        handle.startupAdmission.writeConfirmed = true;
      }
      if (!this.settleLifecycleStartAdmissionIfReady(handle)) {
        this.logger.info(
          {
            kind: 'process',
            action: 'startup_delivery_awaiting_admission',
            pid: handle.pid,
            conversationId: handle.conversationId,
            agent_id: handle.agentId,
            delivery_ids: [...deliveryIds],
            command_uuid: payloadUuid,
          },
          'startup_delivery written; deferring consumed until input_lifecycle started',
        );
      }
      return;
    }

    // Write succeeded. For `requires_first_message` drivers (claude) we
    // must defer the consumed emit until `session_init` confirms the
    // runtime actually accepted the turn — `startupAwaitingInit`
    // marks this sub-state. The gate (`startupDelivery !== null`)
    // stays closed, so delivery → `startup_pending` and
    // `runtime_ready_for_drain` remain blocked until session_init.
    // For self-init drivers (kickoffMode !== 'requires_first_message'),
    // or when initObserved is already true (e.g. resume path), settle now.
    if (handle.driver.kickoffMode === 'requires_first_message' && !handle.initObserved) {
      handle.startupAwaitingInit = true;
      this.logger.info(
        {
          kind: 'process',
          action: 'startup_delivery_awaiting_init',
          pid: handle.pid,
          conversationId: handle.conversationId,
          agent_id: handle.agentId,
          delivery_ids: [...deliveryIds],
        },
        'startup_delivery written; deferring consumed until session_init',
      );
      return;
    }
    this.emitStartupDeliveryConsumed(handle, deliveryIds);
    this.clearStartupGate(handle);
    this.maybeEmitReadyForDrain(handle);
  }

  /** Re-attempt a deferred startup_delivery after a driver-state
   *  transition (claude session_init OR codex handleThreadReady).
   *  Idempotent: no-op when no gate is pending or when driver is
   *  still not ready. Kept as a separate entry so the run-state
   *  handler can call it without re-deriving "did we just become
   *  ready". */
  private tryConsumeStartupDelivery(handle: RuntimeHandle): void {
    if (!handle.startupDelivery) return;
    if (handle.startupAwaitingInit) return; // write done, awaiting init
    // #task:2210: write done, awaiting the lifecycle-start `started` proof.
    // Same rationale as the awaitingInit bail — the batch was already
    // written; re-consuming would write it to stdin twice. session_init
    // (which calls this) proves process-readiness, never this payload's
    // admission, so it must not re-drive the consume.
    if (handle.startupAdmission !== null) return;
    // #task:1563: a consume is mid-write (dispatchParsedLine runs outside
    // the lifecycle lock, so session_init can land inside that await
    // window). Re-entering would write the SAME batch to stdin twice —
    // duplicate exposure + double consumed. The in-flight consume settles
    // on its own (initObserved is already true by now).
    if (handle.startupConsumeInFlight) return;
    if (!driverReadyToEncode(handle.driver)) return;
    void this.consumeStartupDelivery(handle);
  }

  /**
   * #task:2210: settle a `lifecycle-start` batch ONLY on the conjunction of
   * both halves of `startupAdmission` — a matching `input_lifecycle{started}`
   * was observed (`startedObserved`) AND the stdin write reached its full
   * success boundary (`writeConfirmed`). Called by whichever half completes
   * second (the `input_lifecycle` handler for a late `started`; the consume
   * body's post-write continuation for an early one). Emits consumed → clears
   * the gate → re-evaluates ready_for_drain, the same order as every other
   * settle, and is idempotent via the gate-held guard (a second call after the
   * gate cleared is a no-op). Splitting settlement from either half is what
   * prevents an early `started` from settling before the write succeeds —
   * which would otherwise allow consumed+failed double-settle, a premature
   * deferred flush, and continuation on an already-cleared gate. Returns
   * whether it settled. A request-correlated rejection also settles, as a
   * terminal refusal, after the same write boundary. Observed admission takes
   * precedence over a contradictory rejection; RPC success is never proof. */
  private settleLifecycleStartAdmissionIfReady(handle: RuntimeHandle): boolean {
    const admission = handle.startupAdmission;
    if (
      admission === null ||
      handle.startupDelivery === null ||
      (!admission.startedObserved && !admission.rejected) ||
      !admission.writeConfirmed
    ) {
      return false;
    }

    const ids = handle.startupDelivery.deliveryIds;
    if (admission.rejected && !admission.startedObserved) {
      // Use the existing terminal refusal outcome, not a retriable transport
      // failure: repeating a rejected request automatically could loop forever.
      this.emitStartupDeliveryFailed(handle, ids, 'driver_refused');
      handle.inputPayloadUuids.delete(admission.uuid);
      this.clearStartupGate(handle);
      this.maybeEmitReadyForDrain(handle);
      return true;
    }

    this.logger.info(
      {
        kind: 'process',
        action: 'startup_delivery_admitted',
        pid: handle.pid,
        conversationId: handle.conversationId,
        agent_id: handle.agentId,
        delivery_ids: [...ids],
        command_uuid: admission.uuid,
      },
      'lifecycle-start admission: started proof + write success both confirmed; settling delivered',
    );
    this.emitStartupDeliveryConsumed(handle, ids);
    this.clearStartupGate(handle);
    this.maybeEmitReadyForDrain(handle);
    return true;
  }

  private observeInputRejected(handle: RuntimeHandle, uuid: string): void {
    // Post-exit stdout draining is admission-only; its EOF path owns rollback.
    if (handle.phase === 'exited' || handle.phase === 'draining') {
      return;
    }

    const admission = handle.startupAdmission;
    if (!admission || !handle.startupDelivery || admission.uuid !== uuid) {
      return;
    }

    // Admission already observed is stronger evidence than a contradictory
    // late error; never roll an exposed input back for another execution.
    if (admission.startedObserved) {
      return;
    }

    this.logger.warn(
      {
        kind: 'process',
        action: 'startup_delivery_rejected',
        pid: handle.pid,
        conversationId: handle.conversationId,
        agent_id: handle.agentId,
        command_uuid: uuid,
        delivery_ids: [...handle.startupDelivery.deliveryIds],
      },
      'runtime explicitly rejected the correlated input',
    );
    admission.rejected = true;

    // Like early admission, an early rejection must wait for the write to
    // finish; otherwise its continuation could settle or clear the next batch.
    this.settleLifecycleStartAdmissionIfReady(handle);
  }

  /**
   * Clear the step-0 gate. ALWAYS clears both `startupDelivery`
   * AND `startupAwaitingInit` in the same critical section so the
   * `(startupDelivery=null, startupAwaitingInit=true)` interleaving
   * is impossible (Cindy `GMbjEkGUHrB1BBMe`). Callers must emit
   * the appropriate `_consumed` / `_failed` BEFORE calling this so
   * the wire order stays "settle event → clear gate → maybe ready".
   */
  private clearStartupGate(handle: RuntimeHandle): void {
    handle.startupDelivery = null;
    handle.startupAwaitingInit = false;
    // #task:2210: the lifecycle-start admission gate clears with the delivery
    // in the SAME critical section, mirroring startupAwaitingInit, so no
    // `(startupDelivery=null, startupAdmission!=null)` interleaving can appear.
    // `stdoutEnded` is NOT reset here — it tracks the child's stdout channel
    // lifecycle, not the gate, and must survive gate settlement so a late
    // `onStdoutDrained` stays a no-op rather than re-finalizing.
    handle.startupAdmission = null;
    // Post-write re-drive (#task:1417 Stage 3, §11.4): the single delivery
    // slot just freed, so a queued deferred batch may now be admissible.
    // Guarded + lock-serialized inside; a no-op while a flush is active
    // (that path re-drives from its own `finally`).
    this.maybeScheduleDeferredFlush(handle);
    // A stale-receive waiter (#task:1614) re-evaluates too: the freed slot
    // may make the queue takeable, or the settled write's admission may
    // now prove its outcome. Fair race with the flush above — both sides
    // arbitrate under the same lifecycle lock, single winner.
    this.wakePullWaiter(handle);
  }

  /**
   * Emit the "batch consumed" ack for `handle`'s current startup batch.
   * Every startup batch now arrives via `runtime_deliver` (Stage 2c retired
   * `runtime_start`'s batch + `runtime_wake`), so this always settles onto
   * the unified `runtime_delivery_consumed`. It is `delivery_ids`-keyed and
   * is backend's `markDelivered` trigger — the ONLY thing that flips
   * `delivered_at`.
   */
  private emitStartupDeliveryConsumed(handle: RuntimeHandle, deliveryIds: readonly number[]): void {
    this.emit({
      event: 'runtime_delivery_consumed',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
      runtime_generation: handle.runtimeGeneration,
      delivery_ids: [...deliveryIds],
      ts: this.now(),
    });
  }

  /** Mirror of {@link emitStartupDeliveryConsumed} for the failure ack.
   *  Always the unified `runtime_delivery_failed`; backend rolls the batch
   *  back to pending (retriable reasons) or marks it failed
   *  (`driver_refused`: encoding refusal or explicit runtime rejection). */
  private emitStartupDeliveryFailed(
    handle: RuntimeHandle,
    deliveryIds: readonly number[],
    reason: 'child_exit' | 'write_failed' | 'driver_refused' | 'init_watchdog',
  ): void {
    this.emit({
      event: 'runtime_delivery_failed',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
      runtime_generation: handle.runtimeGeneration,
      delivery_ids: [...deliveryIds],
      reason,
      ts: this.now(),
    });
  }

  /** Settle daemon-held deferred custody exactly once before a hard stop.
   *  Emptying happens only after each canonical failure event is emitted, so a
   *  child that never exits cannot strand backend inbox rows in-flight. */
  private failDeferredQueue(handle: RuntimeHandle): void {
    if (handle.deferredQueue.length === 0) return;
    const doomed = handle.deferredQueue;
    handle.deferredQueue = [];
    for (const entry of doomed) {
      bindRuntimeFocus(handle, entry.conversationId);
      this.emitStartupDeliveryFailed(handle, entry.deliveryIds, 'child_exit');
    }
  }

  /**
   * Step-0 ready_for_drain gate. The single arbiter of when this
   * event fires; called from (a) start()'s setImmediate ready
   * transition, (b) every startup_delivery settle, and (c) the
   * session_init / handleThreadReady tryConsume path. Idempotent
   * via `handle.readyForDrainEmitted` so re-entry is harmless.
   *
   * Conditions (Cindy `wuiwdLKrLFyGip_s` + Option A v2
   * `UMGu36zoTDVuKWXi`):
   *   - phase === 'ready'                 (runtime is live)
   *   - startupDelivery === null          (no pending batch)
   *   - driver.readyToEncode()            (codex post-`thread/started`;
   *                                        stateless drivers always
   *                                        return true)
   *   - !readyForDrainEmitted             (one-shot per run)
   *   - !driver.oneShot                   (one-shot runtimes cannot live-drain)
   *
   * Explicitly NOT gated on `initObserved` — see Cindy plan v2.
   * `requires_first_message + empty-inbox` claude reaches `ready`
   * before `session_init`; we still want `runtime_ready_for_drain`
   * to fire so backend can drain its inbox (the first write is
   * what makes claude emit `session_init` in the first place).
   * For codex the equivalent gate IS `readyToEncode()` because
   * `encodeStdinMessage` returns null pre-`thread/started`.
   */
  private maybeEmitReadyForDrain(handle: RuntimeHandle): void {
    if (handle.readyForDrainEmitted) return;
    if (handle.phase !== 'ready') return;
    if (handle.driver.oneShot) return;
    if (handle.startupDelivery !== null) return;
    if (!driverReadyToEncode(handle.driver)) return;
    handle.readyForDrainEmitted = true;
    this.emit({
      event: 'runtime_ready_for_drain',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
      runtime_generation: handle.runtimeGeneration,
      ts: this.now(),
    });
  }

  // ── runtime_stop ───────────────────────────────────────────────

  async stop(params: RuntimeStopParams): Promise<RuntimeStopResult> {
    const k = runtimeScopeKey(params.runtime_scope);
    return this.withLifecycleLock(k, async () => {
      const handle = this.handles.get(k);
      if (!handle || handle.phase === 'exited') {
        return { mode: 'hard', stopped: false, exit_code: null };
      }
      if (handle.runtimeGeneration !== params.runtime_generation) {
        return { mode: 'hard', stopped: false, exit_code: null };
      }
      return this.hardStopHandle(handle);
    });
  }

  /**
   * Internal helper shared by `stop()` and `stopAll`.
   * Initiates the SIGTERM → SIGKILL cascade and resolves only AFTER
   * the child has actually exited (or the SIGKILL timeout fired).
   * The returned `exit_code` is whatever the OS surfaced via the
   * `'exit'` event; the cascade timer is cleared in the exit
   * handler.
   */
  private hardStopHandle(handle: RuntimeHandle): Promise<RuntimeStopResult> {
    handle.intentionalExit = true;
    handle.phase = 'draining';
    // A deletion fence (or any hard stop) makes queued custody impossible to
    // flush. Settle now rather than waiting for `exit`: SIGKILL itself has a
    // bounded timeout, and a non-exiting child must not strand rows forever.
    this.failDeferredQueue(handle);
    // Cindy `HyC05fC-FEs94uoZ` B2: a user-initiated hard stop must
    // cancel any in-flight init watchdog (and compaction watchdog) AT
    // DRAINING ENTRY, mirroring CM (`conversation-manager.ts`
    // lines 1020-1023, `clearInitWatchdog(h)` right after
    // `intentionallyStopped = true`). Without this, the timer would
    // keep ticking inside the SIGTERM grace window; if it fired before
    // the child actually exited, `failInitWatchdog` would set
    // `initTimedOut = true` and the exit handler's reason-mapping
    // would pick `'timeout'` over `'stopped'`, polluting terminal
    // state, retry/cooldown bookkeeping, and the startup_delivery
    // failed reason (`init_watchdog` instead of `child_exit`). Same
    // rationale applies to the compaction watchdog — a stale-emit
    // mid-shutdown is just noise on the wire and wastes a
    // run-state diff.
    this.clearInitWatchdog(handle);
    this.clearCompactionWatchdog(handle);
    this.emitState(handle);

    const exitPromise = new Promise<RuntimeStopResult>((resolve) => {
      const onExit = (code: number | null): void => {
        // The actual exit emit is handled by `attachChildListeners`;
        // we only resolve the stop() promise here.
        resolve({ mode: 'hard', stopped: true, exit_code: code });
      };
      // Listen on a dedicated channel — handle.child.once('exit') is
      // already bound by attachChildListeners (which emits the
      // runtime_exit event); we add a second listener whose only job
      // is to resolve this promise.
      handle.child.once('exit', onExit);

      // Safety net: if the child somehow never fires 'exit' inside
      // our SIGKILL window the dispatcher cannot hang waiting on us,
      // so we resolve — but Cindy `OQCA05pivtB8nSTC` B2: we must NOT
      // claim `stopped: true`. The child is still alive (or in an
      // unknown zombie state); pretending it was reaped would let the
      // caller mark cleanup done and free downstream resources while
      // the OS-level handle persists. The honest answer is
      // `stopped: false, exit_code: null` with the handle left in
      // `draining` — when/if the child eventually exits, the primary
      // `attachChildListeners.once('exit')` will still fire and emit
      // `runtime_exit`, and the second `onExit` listener resolving
      // again is a harmless no-op on an already-settled promise.
      const timeout = setTimer(() => {
        if (handle.phase !== 'exited') {
          this.logger.warn(
            {
              kind: 'process',
              action: 'sigkill_timeout',
              pid: handle.pid,
              conversationId: handle.conversationId,
              agent_id: handle.agentId,
              timeout_ms: this.sigkillTimeoutMs,
            },
            'child did not exit within sigkill timeout',
          );
          resolve({ mode: 'hard', stopped: false, exit_code: null });
        }
      }, this.sigkillTimeoutMs);
      timeout.unref();
    });

    // Send SIGTERM immediately; escalate to SIGKILL after the
    // configured grace period. This is the same cascade CM uses
    // (`stopSigtermMs` → `stopSigkillMs`).
    this.logger.info(
      {
        kind: 'process',
        action: 'sigterm',
        pid: handle.pid,
        conversationId: handle.conversationId,
        agent_id: handle.agentId,
      },
      'sending SIGTERM to runtime',
    );
    try {
      handle.child.kill('SIGTERM');
    } catch {
      // Process already gone — exit listener will resolve us.
    }
    handle.sigkillTimer = setTimer(() => {
      handle.sigkillTimer = null;
      if (handle.phase === 'exited') return;
      this.logger.warn(
        {
          kind: 'process',
          action: 'sigkill',
          pid: handle.pid,
          conversationId: handle.conversationId,
          agent_id: handle.agentId,
        },
        'escalating to SIGKILL after grace period',
      );
      try {
        handle.child.kill('SIGKILL');
      } catch {
        // Process already gone — exit listener will resolve us.
      }
    }, this.sigtermToSigkillMs);
    handle.sigkillTimer.unref();

    return exitPromise;
  }

  // ── runtime_deliver (Stage 3, #task:1417) ──────────────────────

  /**
   * Daemon-authoritative delivery — the single command that subsumes the
   * backend start-vs-wake fork. Backend claims the pending batch in-flight
   * then hands it here; the daemon, sole owner of runtime-lifecycle truth,
   * picks spawn / write / defer / refuse against the REAL handle rather than
   * the backend's desyncable `RunStateCache` (the #task:1416 bug class).
   * Canonical: docs/daemon-authoritative-delivery-v1.md §11.
   *
   * LOAD-BEARING (§11): the synchronous result transfers or refuses CUSTODY
   * only — it never acks the DB. `delivered_at` flips solely on the async
   * `runtime_delivery_consumed` this path ultimately emits (or rolls back on
   * `runtime_delivery_failed`). "Sent to daemon" ≠ ack. This holds for every
   * arm, including `deferred`.
   *
   * Four arms, all under the per-(conv,agent) lifecycle lock so a concurrent
   * stop / start / retry-respawn cannot race the handle read:
   *   - SPAWN — no live handle (absent or exited) → `startUnlocked` with the
   *     batch as its startup delivery. `spawning`, or `refused{cooldown}`
   *     when the init-failure cooldown gate is active.
   *   - REFUSE — draining (hard stop in flight) → `refused{draining}`; or a
   *     startup-phase / live-oneShot handle (still spawning/startup_pending,
   *     driver not ready to encode, or a oneShot that drains its inbox only
   *     at spawn) → `refused{startup_pending}`. These are DELIBERATELY not
   *     folded into the deferred queue (§11.2): each has a lossless DB-backed
   *     re-drive (`runtime_ready_for_drain`, or exit→respawn-with-batch), so
   *     volatile daemon custody would be strictly worse on crash-safety.
   *   - DEFER — a ready, encode-ready, non-oneShot handle that cannot take
   *     the write RIGHT NOW (§11.3): a `gated` driver caught mid-turn
   *     (boundary closed), or a prior write still in flight. Custody moves
   *     into the per-handle deferred queue keyed by the injected owner and is
   *     flushed at the next safe boundary / when the in-flight write clears.
   *     `deferred`.
   *   - ADMIT — a ready handle safe to write now → place the batch on the
   *     handle and drive it through the SAME `consumeStartupDelivery` machine
   *     the spawn path uses (init-proof deferral for `requires_first_message`
   *     drivers, encode, write, seq flush, consumed/failed emit). `admitted`
   *     the instant custody sits on the handle.
   */
  async deliver(
    params: RuntimeDeliverParams,
    ctx: DeliveryOwnerContext,
  ): Promise<RuntimeDeliverResult> {
    const k = runtimeScopeKey(params.spawn_options.runtime_scope);
    return this.withLifecycleLock(k, async () => {
      this.assertAgentHomeOpen(params.agent_id);
      // Fail loud on a drifted batch before any custody decision — the
      // same consistency contract the spawn-time startup batch enforces.
      this.assertDeliveryMetadataConsistent(
        params.conversation_id,
        params.agent_id,
        params.delivery_ids,
        params.deliveries,
        'runtime_deliver',
      );
      const actionWorkdir = resolveRuntimeActionWorkdir(
        params.spawn_options.runtime_scope,
        params.target,
        this.runtimeStartResolveDeps,
      );
      let handle = this.handles.get(k);
      if (!handle?.ensuredActionWorkdirs.has(actionWorkdir)) {
        await ensureRuntimeActionWorkdir(
          actionWorkdir,
          params.target,
          params.spawn_options.runtime_scope,
          this.logger,
        );
        // Child events are allowed to advance a handle while filesystem I/O
        // yields. Re-read before the custody decision; the lifecycle lock
        // still excludes competing commands.
        handle = this.handles.get(k);
        handle?.ensuredActionWorkdirs.add(actionWorkdir);
      }

      // ── SPAWN arm: no live handle ──────────────────────────────
      if (!handle || handle.phase === 'exited') {
        const result = await this.startUnlocked(
          {
            conversation_id: params.conversation_id,
            agent_id: params.agent_id,
            runtime_generation: params.runtime_generation,
            options: params.spawn_options,
            startup_delivery: {
              conversation_id: params.conversation_id,
              action_workdir: actionWorkdir,
              delivery_ids: [...params.delivery_ids],
              deliveries: params.deliveries,
              text: params.text,
            },
          },
          0,
        );
        if (result.started) {
          return {
            disposition: 'spawning',
            pid: result.pid,
            started_at: result.started_at,
          };
        }
        // The only non-started `startUnlocked` result is the init-failure
        // cooldown gate. Custody stays with backend (retriable).
        return {
          disposition: 'refused',
          reason: 'cooldown',
          cooldown_until: result.cooldown_until,
        };
      }

      if (handle.runtimeGeneration !== params.runtime_generation) {
        throw new Error(
          `runtime generation mismatch for ${params.conversation_id}/${params.agent_id}`,
        );
      }

      // Response-loss retries may EXPAND or shrink their batch while daemon
      // custody is already spread across startup release, stale-pull
      // reservation, and the deferred FIFO. Partition by individual delivery
      // id across that complete ownership union. Held ids are rebound to the
      // current connection owner in place; only novel ids enter the FIFO.
      // Any non-refused disposition below means daemon custody covers the
      // caller's whole batch, so backend keeps every row in-flight.
      const ownership = this.partitionDeliveryOwnership(handle, params, actionWorkdir, ctx.owner);
      if (ownership.heldCount > 0) {
        if (ownership.novel.length > 0) {
          handle.deferredQueue.push(...ownership.novel);
          this.wakePullWaiter(handle);
          this.scheduleReservationStallCheck(handle);
        }
        if (handle.phase === 'spawning' || handle.phase === 'startup_pending') {
          return {
            disposition: 'spawning',
            pid: handle.pid,
            started_at: handle.startedAt,
          };
        }
        if (ownership.novel.length > 0 || !ownership.heldInStartup) {
          return {
            disposition: 'deferred',
            pid: handle.pid,
            started_at: handle.startedAt,
          };
        }
        return {
          disposition: 'admitted',
          pid: handle.pid,
          started_at: handle.startedAt,
        };
      }

      // ── REFUSE arms: draining / startup-phase / live oneShot ────
      if (handle.phase === 'draining') {
        return { disposition: 'refused', reason: 'draining' };
      }
      if (
        handle.driver.oneShot ||
        handle.phase === 'spawning' ||
        handle.phase === 'startup_pending' ||
        (!driverReadyToEncode(handle.driver) && !handle.initObserved)
      ) {
        // Startup-phase or live oneShot — DELIBERATELY NOT folded into the
        // daemon deferred queue (§11.2). Each already has a lossless
        // DB-backed re-drive: `runtime_ready_for_drain` once the handle is
        // ready + its startup batch consumed (spawning/startup_pending/
        // pre-init not-ready-to-encode), or exit→respawn-with-batch for a
        // live oneShot. Taking volatile daemon custody here would be
        // strictly worse on crash-safety for zero benefit, so we refuse
        // and let the backend keep the rows pending and resubmit.
        //
        // POST-init `!readyToEncode` is NOT this arm (#task:1565): for a
        // prompt-boundary runtime the encoder goes un-ready on EVERY
        // active prompt, and `ready_for_drain` is once-per-run — refusing
        // here would starve the rows until an unrelated live delivery.
        // Those land in the DEFER arm below (boundary closed), custody in
        // the daemon queue, flushed when the prompt response reopens.
        return { disposition: 'refused', reason: 'startup_pending' };
      }

      // Here: phase === 'ready', not oneShot. The driver is ready to
      // encode OR (post-init, gated profile) its boundary is closed —
      // the defer arm below takes the closed-boundary case before the
      // admit arm can reach the encoder.
      const gated = profileUsesBoundaryGate(handle);
      const writeInFlight = handle.startupDelivery !== null;
      const boundaryOpen = strategyBoundaryOpen(handle);

      // ── DEFER arm: ready, but cannot take the write RIGHT NOW (§11.3) ──
      // Two custody-into-daemon-queue cases, both keep the rows in-flight:
      //   - `gated` driver caught mid-turn (boundary closed) — flushed at
      //     the next `turn_end`;
      //   - a prior write still in flight (the single `startupDelivery`
      //     slot is busy) — the consecutive-delivery window, re-driven
      //     after that write's consumed clears the slot.
      // A third hold (#task:1614): an unresolved pull-in-flight
      // reservation. Its prefix is EARLIER in presentation FIFO than this
      // arrival, so admitting now would release later content first —
      // defer until the reservation settles.
      // `deferred` is a custody transfer, NOT an ack — `delivered_at` still
      // moves only on the async `runtime_delivery_consumed` the eventual
      // flush emits.
      if (writeInFlight || handle.pullReservation !== null || (gated && !boundaryOpen)) {
        this.enqueueDeferred(handle, params, actionWorkdir, ctx.owner);
        this.scheduleReservationStallCheck(handle);
        return {
          disposition: 'deferred',
          pid: handle.pid,
          started_at: handle.startedAt,
        };
      }

      // ── ADMIT arm: ready handle, safe to write now ─────────────
      // Route the batch through the same consume machine the spawn path
      // uses (init-proof deferral, encode, write, admit, seq flush,
      // consumed/failed emit). Custody transfers the instant the batch
      // sits on the handle; the async ack settles `delivered_at`.
      handle.startupDelivery = {
        conversationId: params.conversation_id,
        actionWorkdir,
        deliveryIds: [...params.delivery_ids],
        deliveries: params.deliveries,
        text: params.text,
      };
      void this.consumeStartupDelivery(handle);
      return {
        disposition: 'admitted',
        pid: handle.pid,
        started_at: handle.startedAt,
      };
    });
  }

  /**
   * Owner-scoped deferred-queue cleanup (#task:1417 Stage 3, §11.4.1).
   * Called by an ingress when its connection drops (control-socket close,
   * local-outbound socket close, or outbound-WS close/reconnect/stop):
   * discard ONLY the deferred (unwritten) batches this owner enqueued,
   * across every handle, emitting NOTHING. The backend rolls the matching
   * in-flight rows back to `pending` on its own disconnect handler and
   * reconnect replay re-drives them, so a `consumed`/`failed` here would be
   * wrong (nothing was written) and a double-delivery risk. Never touches
   * another owner's entries; idempotent.
   */
  cleanupDeferredForOwner(owner: string): void {
    // Round-3: cancel this owner's live pull CALLS first — this covers
    // the pre-registration window (call queued behind the lifecycle
    // lock, no waiter to scan yet). The waiter leg below handles the
    // post-registration window; both are zero-custody.
    const ownerCalls = this.activePullCallsByOwner.get(owner);
    if (ownerCalls) {
      for (const call of ownerCalls) call.cancelled = true;
    }
    let discarded = 0;
    for (const handle of this.handles.values()) {
      // P1-4 cancel leg (#task:1614): a stale-receive waiter registered
      // by this connection dies with it — unregister and wake it as
      // `cancelled` so the call returns instead of squatting the
      // handle's single waiter slot for up to the full wait window.
      // Zero custody: a waiter holds nothing by contract. An unresolved
      // pull RESERVATION is deliberately NOT touched here — its response
      // may already be in flight and `settleStaleReceiveHandoff` owns
      // its exactly-once settlement; the re-driven rows dedup against
      // the reservation in `enqueueDeferred` (P1-1).
      const waiter = handle.pullWaiter;
      if (waiter && waiter.owner === owner) {
        handle.pullWaiter = null;
        waiter.wake('cancelled');
        this.logger.info(
          {
            kind: 'process',
            action: 'stale_receive_waiter_cancelled',
            conversationId: handle.conversationId,
            agent_id: handle.agentId,
            owner,
          },
          'stale-receive waiter cancelled: issuing control connection closed',
        );
      }
      if (handle.deferredQueue.length === 0) continue;
      const retained: DeferredEntry[] = [];
      for (const entry of handle.deferredQueue) {
        const remainingIds = entry.deliveryIds.filter((id) => entry.owners.get(id) !== owner);
        discarded += entry.deliveryIds.length - remainingIds.length;
        if (remainingIds.length > 0) {
          retained.push(this.subsetDeferredEntry(handle, entry, remainingIds));
        }
      }
      handle.deferredQueue = retained;
    }
    if (discarded > 0) {
      this.logger.info(
        { kind: 'process', action: 'deferred_owner_cleanup', owner, discarded },
        'discarded deferred deliveries for dropped connection owner',
      );
    }
  }

  /** Stall diagnostics for a `next-turn-batch` reservation (#task:1563,
   *  doc §3.4; Cindy P1-1: timeout-triggered, not inflow-triggered — a
   *  non-empty deferred queue with no further deliveries must still get
   *  its warn). Scheduled when an entry defers under a held reservation
   *  and on a re-arm that leaves a queue remainder. The timer only ever
   *  logs: it never writes and never releases the hold. One-shot per
   *  reservation via `reservationStallWarned`; latch and timer reset when
   *  the next write re-arms. */
  private scheduleReservationStallCheck(handle: RuntimeHandle): void {
    if (!handle.nextTurnReserved || handle.nextTurnReservedAt === null) return;
    if (handle.reservationStallWarned) return;
    if (handle.reservationStallTimer !== null) return;
    if (handle.deferredQueue.length === 0) return;
    const remaining = this.reservationStallWarnMs - (this.now() - handle.nextTurnReservedAt);
    if (remaining <= 0) {
      this.warnReservationStall(handle);
      return;
    }
    handle.reservationStallTimer = setTimeout(() => {
      handle.reservationStallTimer = null;
      // Re-check LIVE state: the handle may have been replaced, the
      // reservation released, or the queue drained since scheduling.
      const live = this.handles.get(runtimeScopeKey(handle.runtimeScope));
      if (live !== handle) return;
      if (!handle.nextTurnReserved || handle.reservationStallWarned) return;
      if (handle.deferredQueue.length === 0) return;
      this.warnReservationStall(handle);
    }, remaining);
    handle.reservationStallTimer.unref();
  }

  private warnReservationStall(handle: RuntimeHandle): void {
    handle.reservationStallWarned = true;
    this.logger.warn(
      {
        kind: 'process',
        action: 'next_turn_reservation_stalled',
        conversation_id: handle.conversationId,
        agent_id: handle.agentId,
        reservation_age_ms:
          handle.nextTurnReservedAt === null ? null : this.now() - handle.nextTurnReservedAt,
        deferred_queue_depth: handle.deferredQueue.length,
      },
      'next-turn reservation held past stall threshold; deliveries deferring until terminal turn_end (fail closed, no write)',
    );
  }

  private clearReservationStallTimer(handle: RuntimeHandle): void {
    if (handle.reservationStallTimer !== null) {
      clearTimeout(handle.reservationStallTimer);
      handle.reservationStallTimer = null;
    }
  }

  /** Enqueue one submitted presentation/ack group. Custody inside it is per id. */
  private enqueueDeferred(
    handle: RuntimeHandle,
    params: RuntimeDeliverParams,
    actionWorkdir: string,
    owner: string,
  ): void {
    handle.deferredQueue.push(
      this.deferredEntryFromParams(handle, params, params.delivery_ids, actionWorkdir, owner),
    );
    // A stale-receive waiter (#task:1614) may now have a takeable prefix.
    this.wakePullWaiter(handle);
  }

  /** Full daemon custody union, partitioned by individual delivery id. */
  private partitionDeliveryOwnership(
    handle: RuntimeHandle,
    params: RuntimeDeliverParams,
    actionWorkdir: string,
    owner: string,
  ): {
    heldCount: number;
    heldInStartup: boolean;
    novel: DeferredEntry[];
  } {
    const incoming = new Set(params.delivery_ids);
    const held = new Set<number>();
    let heldInStartup = false;
    for (const id of handle.startupDelivery?.deliveryIds ?? []) {
      held.add(id);
      if (incoming.has(id)) heldInStartup = true;
    }
    for (const entry of handle.pullReservation?.batch ?? []) {
      for (const id of entry.deliveryIds) held.add(id);
    }
    for (const entry of handle.deferredQueue) {
      for (const id of entry.deliveryIds) held.add(id);
    }

    // A partial overlap (some ids held, some novel) requires per-delivery
    // metadata for the novel remainder so the daemon can rebuild its
    // presentation. #task:2106 made `deliveries` always present, so this is a
    // metadata-completeness check on the novel subset, never a hard refusal.
    {
      const novelIds = params.delivery_ids.filter((id) => !held.has(id));
      if (novelIds.length > 0 && novelIds.length !== params.delivery_ids.length) {
        const novel = new Set(novelIds);
        this.requireDeliveryPresentations(
          params.deliveries.filter((delivery) => novel.has(delivery.delivery_id)),
        );
      }
    }

    const rebind = (
      entries: DeferredEntry[],
      action: 'stale_receive_reservation_rebind' | 'deferred_owner_rebind',
    ): void => {
      for (let index = 0; index < entries.length; index += 1) {
        const entry = entries[index]!;
        const owners = new Map(entry.owners);
        const rebound: Array<{ id: number; from: string }> = [];
        for (const id of entry.deliveryIds) {
          const from = owners.get(id);
          if (!incoming.has(id) || from === undefined || from === owner) continue;
          owners.set(id, owner);
          rebound.push({ id, from });
        }
        if (rebound.length === 0) continue;
        entries[index] = { ...entry, owners };
        this.logger.info(
          {
            kind: 'process',
            action,
            conversation_id: handle.conversationId,
            agent_id: handle.agentId,
            delivery_ids: rebound.map((item) => item.id),
            from_owners: [...new Set(rebound.map((item) => item.from))],
            to_owner: owner,
          },
          'rebound daemon-held delivery ids to current owner',
        );
      }
    };
    if (handle.pullReservation) {
      rebind(handle.pullReservation.batch, 'stale_receive_reservation_rebind');
    }
    rebind(handle.deferredQueue, 'deferred_owner_rebind');

    const novelIds = params.delivery_ids.filter((id) => !held.has(id));
    const novel =
      novelIds.length > 0
        ? [this.deferredEntryFromParams(handle, params, novelIds, actionWorkdir, owner)]
        : [];
    let heldCount = 0;
    for (const id of incoming) {
      if (held.has(id)) heldCount += 1;
    }
    return { heldCount, heldInStartup, novel };
  }

  /** Build one presentation/ack group for all or a selected novel subset. */
  private deferredEntryFromParams(
    handle: RuntimeHandle,
    params: RuntimeDeliverParams,
    deliveryIds: readonly number[],
    actionWorkdir: string,
    owner: string,
  ): DeferredEntry {
    const isWholeBatch =
      deliveryIds.length === params.delivery_ids.length &&
      deliveryIds.every((id, index) => id === params.delivery_ids[index]);
    const byId = new Map(params.deliveries.map((delivery) => [delivery.delivery_id, delivery]));
    const deliveries = deliveryIds.map((id) => {
      const delivery = byId.get(id);
      if (!delivery) {
        throw new Error(`runtime_deliver missing payload for delivery_id=${id}`);
      }
      return delivery;
    });
    return {
      conversationId: params.conversation_id,
      actionWorkdir,
      deliveryIds: [...deliveryIds],
      deliveries,
      text: isWholeBatch
        ? params.text
        : composeRuntimeDeliveryPresentation(
            this.requireDeliveryPresentations(deliveries),
            SEND_MESSAGE_TOOL,
          ),
      owners: new Map(deliveryIds.map((id) => [id, owner])),
    };
  }

  /** Filter a queued presentation group after owner cleanup, preserving one group. */
  private subsetDeferredEntry(
    handle: RuntimeHandle,
    entry: DeferredEntry,
    deliveryIds: readonly number[],
  ): DeferredEntry {
    if (deliveryIds.length === entry.deliveryIds.length) return entry;
    const selected = new Set(deliveryIds);
    const deliveries = entry.deliveries.filter((delivery) => selected.has(delivery.delivery_id));
    return {
      conversationId: entry.conversationId,
      actionWorkdir: entry.actionWorkdir,
      deliveryIds: [...deliveryIds],
      deliveries,
      text: composeRuntimeDeliveryPresentation(
        this.requireDeliveryPresentations(deliveries),
        SEND_MESSAGE_TOOL,
      ),
      owners: new Map(deliveryIds.map((id) => [id, entry.owners.get(id)!])),
    };
  }

  private requireDeliveryPresentations(
    deliveries: readonly RuntimeDelivery[],
  ): RuntimeDeliveryPresentation[] {
    return deliveries.map((delivery) => {
      if (!delivery.presentation) {
        throw new Error(
          `runtime_deliver partial ownership overlap requires presentation metadata for delivery_id=${delivery.delivery_id}`,
        );
      }
      return delivery.presentation;
    });
  }

  /**
   * Flush the daemon mid-turn queue at a safe boundary (#task:1417 Stage 3,
   * §11.4). Scheduled (never awaited by its caller) from the two triggers:
   * a boundary reopen in `updateRunStateFromParsed`, and a post-write
   * re-drive when a prior write settles. Takes the lifecycle lock so it
   * cannot race a concurrent `deliver`/`stop`/exit, and re-checks every
   * precondition against the LIVE handle before touching it.
   *
   * Split by driver mode (Cindy P1-2):
   *   - `gated`: drain the WHOLE queue, combine into ONE write → one
   *     `consumed` covering every delivery_id (one-turn semantics).
   *   - `direct`: flush ONE entry FIFO, no combine; the post-write re-drive
   *     picks up the next once this write's `consumed` clears the slot.
   *
   * Custody stays the daemon's until `consumeStartupDelivery` emits the
   * async `runtime_delivery_consumed`; the flush never acks the DB.
   *
   * This queue splice is the auto-push side of the single-release-winner
   * boundary documented on `DeferredEntry`. Any alternate consumer must
   * arbitrate under this same lifecycle lock, use this same profile-specific
   * bounded FIFO selection, and reserve the selected prefix until its own
   * transport handoff commits. While that reservation is unresolved it must
   * also block every later prefix from release, preserving presentation FIFO.
   * It must never steal `startupDelivery`, infer admission from an empty
   * queue/timeout, or hold this lock while waiting.
   * With no alternate consumer active, this function's behavior must remain
   * byte-for-byte identical.
   */
  private async flushDeferred(handle: RuntimeHandle): Promise<void> {
    const k = runtimeScopeKey(handle.runtimeScope);
    await this.withLifecycleLock(k, async () => {
      const live = this.handles.get(k);
      // Handle may have been replaced (respawn) or torn down since the
      // flush was scheduled — only act on the exact same live handle.
      if (!live || live !== handle) return;
      if (this.agentHomeCoordinator.isFenced(live.agentId)) return;
      if (live.flushingDeferred) return;
      if (live.deferredQueue.length === 0) return;
      if (live.phase !== 'ready' || !driverReadyToEncode(live.driver)) return;
      // A write is still in flight — the post-write re-drive will retry once
      // the slot clears. Never stomp a batch mid-consume.
      if (live.startupDelivery !== null) return;
      // An unresolved pull-in-flight reservation blocks every later
      // prefix (#task:1614): its content precedes this queue in
      // presentation FIFO. Settlement re-drives via
      // `maybeScheduleDeferredFlush`.
      if (live.pullReservation !== null) return;
      const gated = profileUsesBoundaryGate(live);
      // Still mid-turn for a gated driver — wait for the next boundary.
      if (gated && !strategyBoundaryOpen(live)) return;

      live.flushingDeferred = true;
      try {
        const batch = gated
          ? spliceBoundedDeferredPrefix(live.deferredQueue) // bounded prefix (I4)
          : live.deferredQueue.splice(0, 1); // FIFO one
        if (batch.length === 0) return;
        live.startupDelivery = combineDeferredBatch(batch, gated);
        await this.consumeStartupDelivery(live);
      } finally {
        live.flushingDeferred = false;
        // FIFO continuation for `direct`: the just-consumed entry cleared
        // the slot synchronously, so re-drive to pick up the next queued
        // entry. Guarded (no-op if the queue is empty or a write deferred
        // its ack via `requires_first_message`).
        this.maybeScheduleDeferredFlush(live);
      }
    });
  }

  /** Schedule a deferred-queue flush if there is anything to flush. Cheap
   *  fast-path guard so the hot parsed-event path doesn't spawn a lock
   *  acquisition on every event. The real preconditions are re-checked
   *  under the lock inside `flushDeferred`. */
  private maybeScheduleDeferredFlush(handle: RuntimeHandle): void {
    if (handle.deferredQueue.length === 0) return;
    if (handle.flushingDeferred) return;
    if (handle.pullReservation !== null) return;
    void this.flushDeferred(handle);
  }

  private async writeLastDeliveredSeq(
    handle: RuntimeHandle,
    deliveredUpToSeq: number,
  ): Promise<void> {
    if (!Number.isSafeInteger(deliveredUpToSeq) || deliveredUpToSeq <= 0) {
      return;
    }
    try {
      // #task:2095: both ConversationSlot and AgentSlot persist the
      // last-written `delivered_up_to_seq` as a bare scalar (`<seq>\n`). The
      // retired per-stream observed ledger (`{version:1,streams}`) had ZERO
      // readers — the daemon no longer keeps per-stream observed cursors — so
      // it collapses to this scalar. There is NO in-tree reader either way
      // (`cli/src/env.ts` forwards the path but never parses it); per-origin
      // ack authority lives on the `runtime_delivery_consumed` event stream,
      // never here.
      //
      // Whether this file is monotonic — hence usable for out-of-tree
      // crash-recovery dedup — depends on the slot (F24, do NOT conflate):
      //   - ConversationSlot: `seq` is per-conversation
      //     (`uniq_messages_conversation_seq`) and the slot IS one
      //     conversation, so successive writes only ever ascend — a valid
      //     per-origin dedup cursor.
      //   - AgentSlot: one write is always a single conversation
      //     (`spliceBoundedDeferredPrefix` breaks on a differing
      //     `conversationId`) but the slot spans many, so the file HOPS
      //     between seq spaces — it is the LAST value written, NOT a monotonic
      //     high-water. On an AgentSlot this value is DIAGNOSTIC ONLY and must
      //     NOT be used for dedup.
      await writeFile(handle.lastDeliveredSeqPath, `${deliveredUpToSeq}\n`, 'utf-8');
    } catch (err) {
      this.logger.warn(
        {
          kind: 'process',
          action: 'last_delivered_seq_write_failed',
          pid: handle.pid,
          conversationId: handle.conversationId,
          agent_id: handle.agentId,
          delivered_up_to_seq: deliveredUpToSeq,
          path: handle.lastDeliveredSeqPath,
          error_message: (err as Error).message,
        },
        'failed to update runtime last-delivered seq file',
      );
    }
  }

  // ── runtime_status ─────────────────────────────────────────────

  status(params: RuntimeStatusParams): RuntimeStatusResult {
    const out: RuntimeStateDto[] = [];
    for (const handle of this.handles.values()) {
      if (params.conversation_id && params.conversation_id !== handle.conversationId) {
        continue;
      }
      if (params.agent_id && params.agent_id !== handle.agentId) continue;
      out.push({
        runtime_scope: handle.runtimeScope,
        conversation_id: handle.conversationId,
        agent_id: handle.agentId,
        runtime_generation: handle.runtimeGeneration,
        pid: handle.phase === 'exited' ? null : handle.pid,
        phase: handle.phase,
        run_state: handle.runState,
        started_at: handle.startedAt,
      });
    }
    return { runtimes: out };
  }

  // ── runtime_clear_session ──────────────────────────────────────

  async clearSession(params: RuntimeClearSessionParams): Promise<RuntimeClearSessionResult> {
    // 3a semantics (per Cindy `OQCA05pivtB8nSTC` clarification note):
    // `cleared: true` here means **the host-side runtime was hard-
    // stopped** — the runtime handle is gone, so any subsequent
    // `runtime_deliver` would have to re-spawn. It does NOT mean the
    // backend-owned resume-state / cli_session_id projection has
    // been wiped; that is downstream of this call. Backend treats
    // `cleared: true` as a signal to nullify its own resume bookkeeping
    // (`agents.cli_session_id`) after this resolves. 3b will add the
    // explicit resume-state path on the host side (driver-mediated
    // `claude --resume` invalidation) and tighten this contract.
    //
    // 3c-b: runs under the per-RuntimeScope lifecycle lock so a
    // concurrent start / hardStop trigger serializes behind it. The
    // wrapped block does ALL the handle bookkeeping (read + hard
    // stop) inside the critical section so a race with start cannot
    // observe `clearSession` mid-stop.
    const k = runtimeScopeKey(params.runtime_scope);
    return this.withLifecycleLock(k, async () => {
      const handle = this.handles.get(k);
      if (!handle || handle.phase === 'exited') return { cleared: false };
      if (handle.runtimeGeneration !== params.runtime_generation) {
        return { cleared: false };
      }
      await this.hardStopHandle(handle);
      return { cleared: true };
    });
  }

  // ── runtime_stop_all ───────────────────────────────────────────

  shutdown(): Promise<void> {
    this.shuttingDown = true;
    this.shutdownPromise ??= this.stopAll({}).then(() => undefined);
    return this.shutdownPromise;
  }

  async stopAll(_params: RuntimeStopAllParams): Promise<RuntimeStopAllResult> {
    // 3c-b (Cindy tightening `ixAq6GPTn4fVSpCF`): fan out one
    // lifecycle-lock acquisition per RuntimeScope and wait on all
    // of them in parallel. Different keys spawn / stop independently,
    // so the natural shape is `Promise.all` over per-key entries —
    // a single global lock would queue every stop behind every other,
    // turning stopAll into a serial drain.
    //
    // Snapshot the keys BEFORE awaiting any lock so we don't iterate
    // a Map that's being mutated by concurrent `onChildExit` cleanups
    // or by another in-flight stop completing under one of our lock
    // sections.
    const livingKeys: string[] = [];
    for (const [k, handle] of this.handles) {
      if (handle.phase !== 'exited') livingKeys.push(k);
    }
    const count = livingKeys.length;
    await Promise.all(
      livingKeys.map((k) =>
        this.withLifecycleLock(k, async () => {
          const handle = this.handles.get(k);
          if (!handle || handle.phase === 'exited') return;
          await this.hardStopHandle(handle);
        }),
      ),
    );
    return { stopped_count: count };
  }

  /**
   * Agent-wide terminal deletion barrier. Every selected handle is stopped
   * under its existing lifecycle lock, and success requires the child's real
   * `exit` event rather than merely sending SIGKILL.
   */
  async fenceAndQuiesceAgent(agentId: string): Promise<{ quiesced_count: number }> {
    if (!this.agentHomeCoordinator.isFenced(agentId)) {
      throw new Error(
        `refusing to quiesce agent ${agentId} before the shared deletion fence is installed`,
      );
    }

    const livingKeys = [...this.handles.entries()]
      .filter(([, handle]) => handle.agentId === agentId && handle.phase !== 'exited')
      .map(([k]) => k);

    // A stale-receive reservation is custody already removed from the normal
    // deferred queue. Its response handoff is the only valid proof of whether
    // it was consumed or must be restored/failed. Wait outside every
    // lifecycle lock so settlement can acquire that same lock. Timeout never
    // fabricates an outcome and never starts rm: the coordinator retains its
    // terminal fence in `fenced_retryable` for a later purge attempt.
    const reservations = [...(this.pullReservationsByAgent.get(agentId) ?? [])];
    const custodySettled = await Promise.all(
      reservations.map((reservation) =>
        this.waitForDeletionCustodySettlement(agentId, reservation),
      ),
    );
    const unsettledCount = custodySettled.filter((settled) => !settled).length;
    if (unsettledCount > 0) {
      throw new AgentCustodySettlementTimeoutError(agentId, unsettledCount);
    }

    const outcomes = await Promise.all(
      livingKeys.map((k) =>
        this.withLifecycleLock(k, async (): Promise<boolean> => {
          const handle = this.handles.get(k);
          this.initRetryStates.delete(k);
          if (!handle || handle.agentId !== agentId || handle.phase === 'exited') {
            return true;
          }
          const result = await this.hardStopHandle(handle);
          return result.stopped;
        }),
      ),
    );
    const livingCount = outcomes.filter((stopped) => !stopped).length;
    if (livingCount > 0) {
      throw new AgentQuiesceFailedError(agentId, livingCount);
    }
    return { quiesced_count: livingKeys.length };
  }

  private async waitForDeletionCustodySettlement(
    agentId: string,
    reservation: PullReservation,
  ): Promise<boolean> {
    const settlement = reservation.settled;

    let timer: NodeJS.Timeout | null = null;
    const outcome = await Promise.race([
      settlement.then(() => 'settled' as const),
      new Promise<'timeout'>((resolve) => {
        timer = setTimer(() => resolve('timeout'), this.deletionCustodySettleTimeoutMs);
        timer.unref();
      }),
    ]);
    if (timer) clearTimeout(timer);
    if (outcome === 'settled') return true;

    this.logger.warn(
      {
        kind: 'process',
        action: 'deletion_custody_settlement_timeout',
        agent_id: agentId,
        delivery_ids: [...reservation.combined.deliveryIds],
        timeout_ms: this.deletionCustodySettleTimeoutMs,
      },
      'agent deletion could not prove stale-receive custody settlement before timeout',
    );
    return false;
  }

  // ── runtime_close_activity_log ─────────────────────────────────

  closeActivityLog(_params: RuntimeCloseActivityLogParams): RuntimeCloseActivityLogResult {
    // The activity-log JSONL writer is backend-owned (per §3.4 /
    // §4.3); host has nothing to flush. Returning flushed=true keeps
    // backend's intent loop honest — if anything is in flight inside
    // OutputBackpressure, the server-level recovery summary mechanism
    // is the right place to drain it.
    return { flushed: true };
  }

  // ── runtime_output_pause / runtime_output_resume ───────────────

  pauseOutput(params: RuntimeOutputPauseParams): RuntimeOutputPauseResult {
    const k = runtimeScopeKey(params.runtime_scope);
    const handle = this.handles.get(k);
    if (!handle || handle.phase === 'exited') return { paused: false };
    if (handle.outputPaused) return { paused: true };
    handle.outputPaused = true;
    this.emit({
      event: 'runtime_output_paused',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
    });
    return { paused: true };
  }

  resumeOutput(params: RuntimeOutputResumeParams): RuntimeOutputResumeResult {
    const k = runtimeScopeKey(params.runtime_scope);
    const handle = this.handles.get(k);
    if (!handle || handle.phase === 'exited') return { resumed: false };
    if (!handle.outputPaused) return { resumed: true };
    handle.outputPaused = false;
    this.emit({
      event: 'runtime_output_resumed',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
    });
    return { resumed: true };
  }

  // ── private: child lifecycle wiring ────────────────────────────

  private attachChildListeners(handle: RuntimeHandle): void {
    const { child } = handle;

    if (child.stdout) {
      child.stdout.on('data', (chunk: Buffer | string) => {
        this.onChildBytes(handle, 'stdout', chunk);
      });
      // #task:2210: the stdout stream reaching EOF (`end`) / `close` is the
      // authoritative "admission channel fully drained" signal — every byte
      // the child ever wrote has now been delivered and parsed. `exit` does
      // NOT imply it (buffered stdout can arrive after the process is gone), so
      // a still-pending lifecycle-start gate's failed/rollback is finalized
      // here, not at exit. Idempotent via the `stdoutEnded` flag in
      // `onStdoutDrained` (both events fire on a normal close).
      child.stdout.once('end', () => this.onStdoutDrained(handle));
      child.stdout.once('close', () => this.onStdoutDrained(handle));
    }
    if (child.stderr) {
      child.stderr.on('data', (chunk: Buffer | string) => {
        this.onChildBytes(handle, 'stderr', chunk);
      });
    }

    child.once('exit', (code, signal) => {
      this.onChildExit(handle, code, signal);
    });

    child.on('error', (err) => {
      // `error` fires for ENOENT / EACCES on spawn (rare here since
      // resolveBinary already verified existence) and for stdin write
      // failures (tolerated on the `runtime_deliver` write path). Log
      // for triage; the exit event still drives state transitions.
      this.logger.warn(
        {
          kind: 'process',
          action: 'child_error',
          pid: handle.pid,
          conversationId: handle.conversationId,
          agent_id: handle.agentId,
          error_message: (err as Error).message,
        },
        'child process emitted error',
      );
    });
  }

  private onChildBytes(
    handle: RuntimeHandle,
    stream: 'stdout' | 'stderr',
    chunk: Buffer | string,
  ): void {
    const text = typeof chunk === 'string' ? chunk : chunk.toString('utf-8');
    if (text.length === 0) return;

    // 3c-b: any byte from the child (stdout OR stderr) refreshes the
    // pre-protocol silence watchdog. Matches CM (`conversation-manager.ts`
    // lines 785-787, 796-797). Refresh is a no-op once `initProtocolObserved`
    // or `initObserved` has flipped — the silence tier is only active
    // pre-first-protocol-frame.
    this.refreshInitLiveness(handle);

    // Do NOT forward raw child bytes on the daemon→backend wire by default.
    //
    // The raw stream is unbounded: a single tool invocation can print
    // megabytes of stdout/stderr, and forwarding that as
    // `runtime_output` duplicates data that backend does not consume
    // for correctness. UI state, delivery gates, delivery ack, and DB
    // projection are all driven by bounded structured events
    // (`runtime_parsed_event` / `runtime_state`) below. Keeping raw
    // tool output daemon-local prevents large `cat`/`grep`/shell
    // results from filling the outbound socket buffer while preserving
    // all control-plane semantics. `runtime_output_pause/resume` stay
    // as protocol compatibility no-ops for older callers.
    //
    // `emitRawRuntimeOutput` exists only for targeted supervisor
    // tests/debugging of this legacy wire arm. Production wiring never
    // sets it.
    if (this.emitRawRuntimeOutput && !handle.outputPaused) {
      this.emit({
        event: 'runtime_output',
        runtime_scope: handle.runtimeScope,
        conversation_id: handle.conversationId,
        agent_id: handle.agentId,
        stream,
        chunk: text,
        ts: this.now(),
      });
    }

    // Parse fanout only feeds off stdout — stderr is treated as raw
    // diagnostic output (CM same). Even when output is paused, we
    // continue to feed parseLine because parsed events drive
    // host-internal AgentRunState and the delivery gate, which are
    // not droppable concerns. (`runtime_parsed_event` may still be
    // skipped at the broadcaster's discretion — but that's frame-level
    // backpressure, not pause semantics.)
    if (stream !== 'stdout') return;
    this.consumeStdoutForParse(handle, text);
  }

  /**
   * Line-buffer stdout and dispatch each complete line to
   * `driver.parseLine`. Mirrors CM's readline-based pump
   * (`conversation-manager.ts` ~line 789) but inlined here so the
   * supervisor owns the buffer.
   *
   * Lines that arrive partial across chunks are stitched via
   * `handle.stdoutLineBuffer`. A trailing partial fragment at child
   * exit IS flushed — `onChildExit` hands the leftover buffer to
   * `dispatchParsedLine` before flipping `phase='exited'`, matching
   * Node's `readline.createInterface` behavior of emitting a final
   * `line` when input ends without a newline (Cindy
   * `5nruk1IPaEHSab0T` B5). Without that flush the last complete-
   * but-unterminated JSON frame (typically the child's final
   * session_init / turn_end / error on quick exit) would silently
   * disappear from the parsed-event stream. The ONE exception is a
   * deferred lifecycle-start gate (#task:2210): there `onChildExit`
   * preserves the buffer so a `started` split across exit can re-stitch
   * at EOF, and `onStdoutDrained` drains that tail admission-only.
   */
  private consumeStdoutForParse(handle: RuntimeHandle, text: string): void {
    // #task:2210 exit-vs-stdout-drain: the guard is the stdout CHANNEL's
    // lifecycle, not the process's. Once the channel has truly closed
    // (`stdoutEnded`) nothing more can arrive, so stop. Between the child's
    // `exit` and that close, KEEP framing lines the SAME way — but ONLY while
    // a lifecycle-start admission gate is still pending, and route each line
    // through the admission-ONLY drain reader (`drainAdmissionProofFromLine`),
    // never the full `dispatchParsedLine`. That way a `started` that raced the
    // exit still settles the batch to consumed, while `turn_end` / tool /
    // session / error frames in the same window run NO run-state, wire, or
    // boundary side effects against an already-terminal run (Cindy
    // `CbJ59KTcL69Z6rJa` B). Once the gate settles/clears we bail exactly like
    // the old `phase==='exited'` guard.
    if (handle.stdoutEnded) return;
    const postExit = handle.phase === 'exited';
    if (postExit && handle.startupAdmission === null) return;
    const combined = handle.stdoutLineBuffer + text;
    const newlineIdx = combined.lastIndexOf('\n');
    if (newlineIdx === -1) {
      handle.stdoutLineBuffer = combined;
      return;
    }
    const completePart = combined.slice(0, newlineIdx);
    handle.stdoutLineBuffer = combined.slice(newlineIdx + 1);

    const lines = completePart.split('\n');
    for (const line of lines) {
      if (line.length === 0) continue;
      // Post-exit: stop the moment the admission gate settles/clears, so a
      // `started` that admits the batch is honored but a trailing frame in the
      // same chunk is not drained against an already-settled gate.
      if (postExit && handle.startupAdmission === null) break;
      if (postExit) {
        this.drainAdmissionProofFromLine(handle, line);
      } else {
        this.dispatchParsedLine(handle, line);
      }
    }
  }

  private dispatchParsedLine(handle: RuntimeHandle, line: string): void {
    // 3c-b: observe init-progress BEFORE parseLine so a driver's
    // classifyInitProgress (pure line classifier) can advance the
    // watchdog state machine even if parseLine returns no events for
    // this specific frame (drivers occasionally emit protocol
    // bookkeeping that maps to no ParsedEvent). Order matches CM
    // (`conversation-manager.ts` lines 789-794): observe → parse →
    // fanout.
    this.observeInitProgress(handle, line);

    // Boundary-reopen detection must bracket the PARSE itself, not only
    // the per-event busy transitions (#task:1565): a prompt-boundary
    // driver (ACP) settles its active prompt INSIDE parseLine — the
    // driver-owned `readyToEncode` flips true before any event is
    // dispatched, so the per-event edge check in
    // `updateRunStateFromParsed` reads "was already open" and never
    // fires. Snapshot the boundary before parsing and drive the queue
    // if the parse (or its events) reopened it. `maybeScheduleDeferredFlush`
    // is guarded: no-op when the queue is empty or a flush is running.
    const boundaryOpenBeforeParse = strategyBoundaryOpen(handle);

    let events: ParsedEvent[];
    try {
      events = handle.driver.parseLine(line);
    } catch (err) {
      this.logger.warn(
        {
          kind: 'process',
          action: 'parse_line_threw',
          pid: handle.pid,
          conversationId: handle.conversationId,
          agent_id: handle.agentId,
          error_message: (err as Error).message,
        },
        'driver.parseLine threw on stdout line',
      );
      return;
    }
    for (const ev of events) {
      // Validate before emitting so the daemon never puts an
      // ill-typed parsed event on the wire. Drivers shouldn't
      // produce them, but a future driver update could regress
      // without this guard. Defense in depth — backend re-validates
      // on its side too (Cindy `OQCA05pivtB8nSTC`: zod schema is
      // bilateral).
      const validated = ParsedEventSchema.safeParse(ev);
      if (!validated.success) {
        this.logger.warn(
          {
            kind: 'process',
            action: 'parse_event_invalid',
            pid: handle.pid,
            conversationId: handle.conversationId,
            agent_id: handle.agentId,
            issue: validated.error.issues[0]?.message ?? 'unknown',
          },
          'driver.parseLine produced event that fails ParsedEventSchema',
        );
        continue;
      }
      // RUN-STATE FIRST. Per Cindy `C2BZGDbQWfnq-bKJ`:
      // frame-too-large is a broadcast concern only — host-internal
      // AgentRunState derivation must happen even if the wire frame
      // is too large to emit. Run-state updates emit `runtime_state`
      // separately (non-droppable, payload-bounded).
      this.updateRunStateFromParsed(handle, validated.data);
      this.emitParsedEvent(handle, validated.data);
    }
    // Parse-bracketed reopen edge (#task:1565): the ACP prompt response
    // flipped `readyToEncode` inside parseLine, invisible to the
    // per-event edge check above. Idempotent with that check — the
    // flush scheduler no-ops on an empty queue or an active flush.
    if (!boundaryOpenBeforeParse && strategyBoundaryOpen(handle)) {
      this.maybeScheduleDeferredFlush(handle);
    }
  }

  /**
   * #task:2210 exit-vs-stdout-drain — POST-EXIT admission-only drain reader.
   *
   * After the child's `exit`, the run has already been finalized: `resolveRun`
   * ran, `runtime_exit` was emitted, `phase==='exited'`. The ONLY reason we
   * keep reading stdout until EOF/`close` is to catch a matching
   * `input_lifecycle{state:'started'}` that admits a still-pending
   * lifecycle-start gate (the runtime read the payload and echoed `started`,
   * then exited before its stdout chunk reached us). So this reader runs the
   * SAME framing + `driver.parseLine` + schema-validate as the live pump, but
   * per event it acts on ONLY that admission proof. Every other frame —
   * `turn_end`, tool/session/error, anything driving run-state, the
   * `runtime_parsed_event` wire, boundary/queue flush — is dropped silently
   * (Cindy `CbJ59KTcL69Z6rJa` B): honoring them would let an already-terminal
   * run resolve twice, rebuild run-state, publish ordinary events, or fire a
   * deferred write after `runtime_exit`. `observeInitProgress`,
   * `updateRunStateFromParsed`, `emitParsedEvent`, and the boundary reopen are
   * deliberately NOT called here.
   */
  private drainAdmissionProofFromLine(handle: RuntimeHandle, line: string): void {
    if (handle.startupAdmission === null || handle.startupDelivery === null) {
      return;
    }
    let events: ParsedEvent[];
    try {
      events = handle.driver.parseLine(line);
    } catch {
      // A parse throw in the post-exit window is inert: there is no wire to
      // fan out to and no run-state to advance. Match the live pump's
      // swallow-and-continue rather than surface a second failure path.
      return;
    }
    for (const ev of events) {
      const validated = ParsedEventSchema.safeParse(ev);
      if (!validated.success) continue;
      const parsed = validated.data;
      if (
        parsed.kind === 'input_lifecycle' &&
        parsed.state === 'started' &&
        handle.startupAdmission !== null &&
        handle.startupDelivery !== null &&
        handle.startupAdmission.uuid === parsed.commandUuid
      ) {
        this.logger.info(
          {
            kind: 'process',
            action: 'startup_admission_drained_post_exit',
            pid: handle.pid,
            conversationId: handle.conversationId,
            agent_id: handle.agentId,
            command_uuid: parsed.commandUuid,
          },
          'matching started drained after exit; settling admission before rollback',
        );
        handle.startupAdmission.startedObserved = true;
        this.settleLifecycleStartAdmissionIfReady(handle);
      }
    }
  }

  /**
   * Emit `runtime_parsed_event` for `parsed`. If the encoded frame
   * would exceed `MAX_FRAME_BYTES`, fall back to
   * `runtime_parsed_event_dropped` — a strict metadata-only frame
   * (Cindy `C2BZGDbQWfnq-bKJ`: NO payload, NO raw line, NO partial
   * fields). We pre-encode the candidate frame to measure its
   * actual wire size; the protocol layer's `encodeFrame` would
   * throw otherwise, which would drop the parsed event silently
   * (worse than honest reporting).
   */
  private emitParsedEvent(handle: RuntimeHandle, parsed: ParsedEvent): void {
    const candidate: EventFrame = {
      event: 'runtime_parsed_event',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
      runtime_generation: handle.runtimeGeneration,
      parsed,
      ts: this.now(),
    };
    let encoded: string | null = null;
    try {
      encoded = encodeFrame(candidate);
    } catch (err) {
      // `encodeFrame` throws on MAX_FRAME_BYTES violation. Fall
      // through to the dropped-frame path.
      this.logger.warn(
        {
          kind: 'process',
          action: 'parse_event_too_large',
          pid: handle.pid,
          conversationId: handle.conversationId,
          agent_id: handle.agentId,
          parsed_kind: parsed.kind,
          error_message: (err as Error).message,
        },
        'parsed event exceeds frame cap; emitting dropped marker',
      );
    }
    if (encoded !== null) {
      this.emit(candidate);
      return;
    }
    // Compute an honest byte count for the dropped marker. Use the
    // length the encoder would have produced (sans the payload-bearing
    // frame). We approximate via Buffer.byteLength of a probe encoding
    // — the dropped frame itself is tiny so it always fits.
    const probe = JSON.stringify({
      event: 'runtime_parsed_event',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
      parsed,
      ts: this.now(),
    });
    const encodedBytes = Buffer.byteLength(probe, 'utf-8') + 1; // + '\n'
    this.emit({
      event: 'runtime_parsed_event_dropped',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
      parsed_kind: parsed.kind,
      encoded_bytes: encodedBytes,
      reason: 'frame_too_large',
      ts: this.now(),
    });
  }

  /**
   * Mutate `handle.runState` based on `parsed`. The transition table
   * mirrors CM's `handleParsedEvent` switch + `recordTool*` /
   * `recordCompaction*` helpers (`conversation-manager.ts`
   * ~lines 1230, 1470-1516). Only the status-shaping subset lives
   * here in 3b; backend keeps owning the activity-snippet ring
   * buffer, tool-call counters, and SSE projection (those are UX,
   * not execution-plane).
   *
   * `session_init` is special: it flips `initObserved` (drives the
   * 3c delivery gate) and updates `cliSessionId`. No run-state change
   * here — run-state lifecycle starts when the agent actually does
   * something (text/thinking/tool_call/turn_end).
   */
  private updateRunStateFromParsed(handle: RuntimeHandle, parsed: ParsedEvent): void {
    if (parsed.kind === 'session_init') {
      if (!handle.initObserved) {
        handle.initObserved = true;
        handle.cliSessionId = parsed.sessionId;
        // 3c-b: stop every init watchdog timer now that init has
        // landed AND clear the RuntimeScope retry/cooldown bucket.
        // The cooldown counter is sticky across attempts within an
        // incident window; a successful init is the explicit reset.
        this.clearInitWatchdog(handle);
        this.clearInitRetryState(handle.runtimeScope);
        this.logger.info(
          {
            kind: 'process',
            action: 'session_init',
            pid: handle.pid,
            conversationId: handle.conversationId,
            agent_id: handle.agentId,
            cli_session_id: parsed.sessionId,
          },
          'agent: session_init received',
        );
        // Cindy `YW2BGWryWjfymPkU` B2: settle the deferred consumed
        // emit if `consumeStartupDelivery` parked the batch awaiting
        // init (claude path: write already done; just emit consumed).
        // Settle order — emit consumed FIRST, then clear gate, then
        // re-evaluate ready_for_drain — same as the non-deferred path
        // inside `consumeStartupDelivery`. The `startupDelivery !==
        // null` precondition guarantees we only fire for batches that
        // are still pending (no double-emit on a re-consumed race).
        if (handle.startupAwaitingInit && handle.startupDelivery) {
          const ids = handle.startupDelivery.deliveryIds;
          this.emitStartupDeliveryConsumed(handle, ids);
          this.clearStartupGate(handle);
          this.maybeEmitReadyForDrain(handle);
        }
        // Option A v2 (Cindy `UMGu36zoTDVuKWXi`): codex path. If
        // `consumeStartupDelivery` deferred earlier because
        // `driver.readyToEncode()` was false (codex pre-`thread/started`),
        // the gate is still pending without `startupAwaitingInit`.
        // session_init means the driver is now ready (codex
        // `handleThreadReady` sets `threadId` BEFORE pushing the
        // `session_init` parsed event), so the deferred consume can
        // proceed. Mutually exclusive with the awaitingInit branch
        // above — that one already cleared the gate.
        this.tryConsumeStartupDelivery(handle);
        // Also re-evaluate the drain gate: codex's `ready_for_drain`
        // is bottled until `driver.readyToEncode()` flips true, and
        // (for runs without startup_delivery) we need to fire it now
        // that the driver is ready.
        this.maybeEmitReadyForDrain(handle);
      }
      return;
    }

    // Boundary-reopen trigger for the daemon mid-turn queue (#task:1417
    // Stage 3, §11.4). Capture the safe-boundary state BEFORE the busy
    // transition; if this event reopens the boundary, flush any deferred
    // batch — symmetric to the retired coordinator's
    // `if (!wasSafe && nowSafe && pending.length) flush()`.
    const wasBoundaryOpen = strategyBoundaryOpen(handle);

    switch (parsed.kind) {
      case 'thinking':
        this.recordActivitySnippet(handle, 'thinking', parsed.text);
        break;
      case 'text':
        this.recordActivitySnippet(handle, 'text', parsed.text);
        break;
      case 'api_response': {
        // Normalized provider notice (#task:1582): a queued/rate-limited
        // provider holds the run in the dedicated `waiting` status (the
        // runtime is NOT inferring — calling it thinking was the #1553
        // mislabel); `ready` returns it to `thinking`; `notice` is a
        // display-only Activity row. The message rides the run-state
        // snippet as its own `api_response` snippet kind — a discrete
        // notice, never blended into the thinking/text coalescing
        // buffer. Display-state only, and only within its own lane:
        // `waiting` may not displace a more specific state (running tool /
        // compaction — same guard as tool_output's restore), `ready`
        // restores ONLY a hold that `waiting` itself created, and
        // `notice` never touches the run state at all. A notice that
        // cannot own the status leaves the run state untouched — it is
        // still persisted as a discrete activity row backend-side.
        const currentStatus = handle.runState?.status;
        if (parsed.status === 'waiting') {
          if (handle.busy.outstandingToolUses === 0 && !handle.busy.compacting) {
            this.transitionRunState(handle, {
              status: 'waiting',
              detail: null,
              activitySnippet: parsed.message,
              activitySnippetKind: 'api_response',
            });
          }
        } else if (parsed.status === 'ready' && currentStatus === 'waiting') {
          this.transitionRunState(handle, {
            status: 'thinking',
            detail: null,
            activitySnippet: parsed.message,
            activitySnippetKind: 'api_response',
          });
        }
        break;
      }
      case 'tool_call':
        handle.busy.outstandingToolUses += 1;
        const toolCallCounts =
          parsed.name === SEND_MESSAGE_TOOL_NAME
            ? handle.runState?.toolCallCounts
            : incrementToolCount(handle.runState?.toolCallCounts, parsed.name);
        this.transitionRunState(handle, {
          status: 'tool_running',
          detail: parsed.name,
          ...(toolCallCounts ? { toolCallCounts } : {}),
        });
        break;
      case 'tool_call_input':
        // #task:2101 — late parameters for a tool call already counted by its
        // `tool_call`. Run state is deliberately untouched: `outstandingToolUses`
        // and `toolCallCounts` were incremented once, at creation, and this frame
        // reports no new call. It exists only to refine that row's payload, so it
        // must not re-transition status either (the call may already have
        // finished by the time the params arrive).
        break;
      case 'tool_output':
        handle.busy.outstandingToolUses = Math.max(0, handle.busy.outstandingToolUses - 1);
        if (handle.busy.outstandingToolUses === 0 && !handle.busy.compacting) {
          this.transitionRunState(handle, {
            status: 'thinking',
            detail: null,
          });
        }
        break;
      case 'compaction_started':
        handle.busy.compacting = true;
        // 3c-b: arm the compaction watchdog. Cleared on
        // compaction_finished, child exit, or hard stop. Watchdog
        // fire path stacks `compactionStale: true` on the run state
        // and emits `runtime_compaction_stale` — but does NOT change
        // phase or kill the child (Cindy plan v2 issue #4).
        this.startCompactionWatchdog(handle);
        this.transitionRunState(handle, {
          compacting: true,
          compactingSince: this.now(),
          // Clear any prior `compactionStale` flag so the next stale
          // window starts fresh and the wire-visible diff fires
          // again if the new compaction also stalls.
          compactionStale: false,
        });
        break;
      case 'compaction_finished':
        handle.busy.compacting = false;
        this.clearCompactionWatchdog(handle);
        this.transitionRunState(handle, {
          compacting: false,
          compactingSince: null,
          compactionStale: false,
          ...(handle.busy.outstandingToolUses === 0 ? { status: 'thinking', detail: null } : {}),
        });
        break;
      case 'turn_end':
        handle.busy.outstandingToolUses = 0;
        handle.busy.compacting = false;
        handle.nextTurnReserved = false;
        handle.nextTurnReservedAt = null;
        handle.reservationStallWarned = false;
        this.clearReservationStallTimer(handle);
        handle.turnEndCount += 1;
        // Cindy `YW2BGWryWjfymPkU` B3: turn_end is a terminal
        // transition — go through `resolveRun` so the terminal
        // state lands, then `handle.runState` is cleared so the
        // next activity creates a fresh runId with no stale
        // terminalReason.
        this.resolveRun(handle, 'turn_ended');
        break;
      case 'input_lifecycle': {
        // Correlate the runtime's per-payload lifecycle back to the
        // delivery ids we stamped at encode time; log it always.
        const mapped = handle.inputPayloadUuids.get(parsed.commandUuid);
        this.logger.info(
          {
            kind: 'process',
            action: 'input_lifecycle',
            pid: handle.pid,
            conversationId: handle.conversationId,
            agent_id: handle.agentId,
            command_uuid: parsed.commandUuid,
            state: parsed.state,
            delivery_ids: mapped ? [...mapped.deliveryIds] : null,
            correlated: mapped !== undefined,
          },
          'runtime input lifecycle observed',
        );
        // #task:2210: for `lifecycle-start` drivers a matching `started` is
        // the ADMISSION proof (restoring the #task:1561 gate that #task:2095
        // degraded to observation), but it is only ONE half of the settlement
        // conjunction. A `started` whose `commandUuid` matches the batch the
        // gate is holding (`startupAdmission.uuid`) RECORDS the proof
        // (`startupAdmission.startedObserved`) and then attempts to settle; it
        // never emits/clears on its own. Guards keep it idempotent and
        // impossible to cross-admit:
        //   - gate must still be held (`startupAdmission !== null` &&
        //     `startupDelivery`) — a second `started`, or one after the batch
        //     already settled/failed, is a no-op;
        //   - the uuid must match exactly — a late frame from a PRIOR payload
        //     (different uuid) can never admit the current batch.
        // If the write has not yet reached its full success boundary (early
        // `started`), `settleLifecycleStartAdmissionIfReady` is a no-op here
        // and the write's post-write continuation settles once it confirms —
        // so an intervening write FAILURE settles `failed` only. A successful
        // stdin write alone NEVER records a `started`, so it can never settle.
        // #task:2210 exit-vs-drain: this handler has NO phase guard, so a
        // `started` that arrives on stdout AFTER the child's `exit` (but before
        // its stdout channel closes) still records here and settles consumed —
        // `onChildExit` deferred the failed/rollback precisely for this window.
        if (
          parsed.state === 'started' &&
          handle.startupAdmission !== null &&
          handle.startupDelivery !== null &&
          handle.startupAdmission.uuid === parsed.commandUuid
        ) {
          handle.startupAdmission.startedObserved = true;
          this.settleLifecycleStartAdmissionIfReady(handle);
        }
        // Prune the observation entry on completion (unchanged; the
        // admission gate above is driven off `startupAdmission.uuid`, not
        // this bounded observation map).
        if (parsed.state === 'completed') {
          handle.inputPayloadUuids.delete(parsed.commandUuid);
        }
        break;
      }
      case 'error':
        if (parsed.fatal === true && handle.initObserved) this.reclaimAfterFatal(handle);
        if (parsed.fatal !== true) {
          this.transitionRunState(handle, {
            status: 'thinking',
            detail: null,
            activitySnippet: parsed.message,
            activitySnippetKind: null,
          });
        }
        break;
      case 'unknown':
        // No-op; backend may decide what to do with it.
        break;
    }

    // If the busy transition just reopened a safe boundary, drive the
    // daemon mid-turn queue (#task:1417 Stage 3, §11.4). Guarded/no-op when
    // the queue is empty or a flush is already running.
    if (!wasBoundaryOpen && strategyBoundaryOpen(handle)) {
      this.maybeScheduleDeferredFlush(handle);
    }
  }

  /**
   * A fatal error after init means the process can no longer serve input, and
   * a runtime may stay alive after reporting it. Close delivery (draining, not
   * intentional), give it the stop grace to exit on its own, then signal it;
   * the normal exit path settles deliveries. Pre-init fatals are already
   * bounded by the init watchdog.
   */
  private reclaimAfterFatal(handle: RuntimeHandle): void {
    if (handle.fatalReclaimTimer || handle.phase === 'exited' || handle.phase === 'draining')
      return;
    handle.phase = 'draining';
    this.emitState(handle);
    const kill = (signal: NodeJS.Signals, next?: () => void) =>
      setTimer(() => {
        if (handle.phase === 'exited') return;
        try {
          handle.child.kill(signal);
        } catch {
          // Already gone; the exit listener settles.
        }
        next?.();
      }, this.sigtermToSigkillMs).unref();
    handle.fatalReclaimTimer = kill('SIGTERM', () => {
      handle.fatalReclaimTimer = kill('SIGKILL');
    });
  }

  /**
   * Apply a transition to `handle.runState`. Lazily creates the
   * state on first transition (so `runId` is stable from the
   * moment the agent starts emitting events). Patch semantics:
   *
   *   - `undefined` field        → leave existing value untouched
   *   - explicit `null` field    → clear (drop optional field)
   *   - any other value          → set
   *
   * Emits `runtime_state` only when something visible to the wire
   * changes (status, detail, snippet, tool counts, compacting,
   * terminalReason).
   */
  private transitionRunState(
    handle: RuntimeHandle,
    patch: {
      status?: AgentRunState['status'];
      detail?: string | null;
      terminalReason?: AgentRunState['terminalReason'];
      compacting?: boolean;
      compactingSince?: number | null;
      /** 3c-b: stacked stale flag from the compaction watchdog. Must
       *  be set in the same patch that DOES NOT touch `compacting` so
       *  the run-state keeps `compacting: true` alongside
       *  `compactionStale: true` (Cindy plan v2 issue #4). */
      compactionStale?: boolean;
      activitySnippet?: string | null;
      activitySnippetKind?: AgentActivitySnippetKind | null;
      toolCallCounts?: Record<string, number> | null;
    },
  ): void {
    const ts = this.now();
    if (!handle.runState) {
      const status = patch.status ?? 'thinking';
      handle.runState = {
        runId: randomUUID(),
        agentId: handle.agentId,
        conversationId: handle.conversationId,
        status,
        startedAt: ts,
        updatedAt: ts,
        lastActivityAt: ts,
        ...(patch.detail && patch.detail !== null ? { detail: patch.detail } : {}),
        ...(patch.terminalReason ? { terminalReason: patch.terminalReason } : {}),
        ...(patch.activitySnippet ? { activitySnippet: patch.activitySnippet } : {}),
        ...(patch.activitySnippet && patch.activitySnippetKind
          ? { activitySnippetKind: patch.activitySnippetKind }
          : {}),
        ...(patch.toolCallCounts ? { toolCallCounts: patch.toolCallCounts } : {}),
        ...(patch.compacting !== undefined ? { compacting: patch.compacting } : {}),
        ...(patch.compactingSince !== undefined && patch.compactingSince !== null
          ? { compactingSince: patch.compactingSince }
          : {}),
        ...(patch.compactionStale === true ? { compactionStale: true } : {}),
      };
      this.emitState(handle);
      return;
    }
    const prev = handle.runState;
    const next: AgentRunState = {
      ...prev,
      updatedAt: ts,
      lastActivityAt: ts,
    };
    if (patch.status !== undefined) next.status = patch.status;
    if (patch.detail === null) {
      delete next.detail;
    } else if (patch.detail !== undefined) {
      next.detail = patch.detail;
    }
    if (patch.terminalReason !== undefined) {
      next.terminalReason = patch.terminalReason;
    }
    if (patch.activitySnippet === null) {
      delete next.activitySnippet;
      delete next.activitySnippetKind;
    } else if (patch.activitySnippet !== undefined) {
      next.activitySnippet = patch.activitySnippet;
      if (patch.activitySnippetKind) {
        next.activitySnippetKind = patch.activitySnippetKind;
      } else {
        delete next.activitySnippetKind;
      }
    } else if (patch.activitySnippetKind === null) {
      delete next.activitySnippetKind;
    }
    if (patch.toolCallCounts === null) {
      delete next.toolCallCounts;
    } else if (patch.toolCallCounts !== undefined) {
      next.toolCallCounts = patch.toolCallCounts;
    }
    if (patch.compacting !== undefined) next.compacting = patch.compacting;
    if (patch.compactingSince === null) {
      delete next.compactingSince;
    } else if (patch.compactingSince !== undefined) {
      next.compactingSince = patch.compactingSince;
    }
    if (patch.compactionStale === true) {
      next.compactionStale = true;
    } else if (patch.compactionStale === false) {
      delete next.compactionStale;
    }
    handle.runState = next;
    // Emit when wire-visible field changed.
    const shouldEmit =
      prev.status !== next.status ||
      prev.detail !== next.detail ||
      prev.terminalReason !== next.terminalReason ||
      prev.activitySnippet !== next.activitySnippet ||
      prev.activitySnippetKind !== next.activitySnippetKind ||
      JSON.stringify(prev.toolCallCounts ?? {}) !== JSON.stringify(next.toolCallCounts ?? {}) ||
      prev.compacting !== next.compacting ||
      prev.compactionStale !== next.compactionStale;
    if (shouldEmit) this.emitState(handle);
  }

  private recordActivitySnippet(
    handle: RuntimeHandle,
    kind: ActivitySnippetKind,
    text: string,
  ): void {
    if (!handle.runState) {
      this.transitionRunState(handle, { status: 'thinking', detail: null });
    }
    const runId = handle.runState?.runId;
    if (!runId) return;
    const activitySnippet = this.appendActivitySnippetBuffer(handle, runId, kind, text);
    this.transitionRunState(
      handle,
      activitySnippet ? { activitySnippet, activitySnippetKind: kind } : { status: 'thinking' },
    );
  }

  private appendActivitySnippetBuffer(
    handle: RuntimeHandle,
    runId: string,
    kind: ActivitySnippetKind,
    text: string,
  ): string | undefined {
    const current = handle.activitySnippetBuffer;
    const nextText =
      current?.runId === runId && current.kind === kind ? `${current.text}${text}` : text;
    const boundedText =
      nextText.length <= ACTIVITY_SNIPPET_BUFFER_MAX_CHARS
        ? nextText
        : nextText.slice(-ACTIVITY_SNIPPET_BUFFER_MAX_CHARS);
    handle.activitySnippetBuffer = { runId, kind, text: boundedText };
    return trimActivitySnippet(boundedText);
  }

  private clearActivitySnippetBuffer(handle: RuntimeHandle, runId?: string): void {
    const current = handle.activitySnippetBuffer;
    if (!current) return;
    if (runId && current.runId !== runId) return;
    handle.activitySnippetBuffer = null;
  }

  /**
   * Terminal-resolve `handle.runState`. Three responsibilities,
   * matching CM's `resolveRunByKey` + `publishTerminalRunState`
   * (Cindy `YW2BGWryWjfymPkU` B3):
   *
   *   1. Map `reason` to the canonical terminal status. CM rule
   *      (`terminalStatus`): `replied | turn_ended | replaced` →
   *      `idle`; every other terminal reason
   *      (`stopped | crashed | timeout | conversation_closed`) →
   *      `failed`. Note `stopped` is FAILED, not idle — hard stop
   *      during tool_running surfaces as `{status:'failed',
   *      terminalReason:'stopped'}` so backend's RunStateCache and
   *      UI both project the correct "ended on a hard stop" arm.
   *   2. Emit ONE `runtime_state` carrying the terminal status +
   *      terminalReason. Uses the existing runState if present
   *      (preserves the runId so the consumer can correlate);
   *      synthesizes a phase-derived state otherwise so we always
   *      surface the terminal transition.
   *   3. Null out `handle.runState` + reset busy/compacting so the
   *      next observable activity (a fresh thinking/tool_call) gets
   *      a brand-new runId and no leaked terminalReason.
   */
  private resolveRun(
    handle: RuntimeHandle,
    reason: NonNullable<AgentRunState['terminalReason']>,
  ): void {
    const terminalStatus: AgentRunState['status'] =
      reason === 'replied' || reason === 'turn_ended' || reason === 'replaced' ? 'idle' : 'failed';
    const ts = this.now();
    const base: AgentRunState = handle.runState
      ? {
          ...handle.runState,
          status: terminalStatus,
          updatedAt: ts,
          lastActivityAt: ts,
          terminalReason: reason,
          // Clear transient activity fields on terminal emit so the
          // wire shape matches CM's terminal projection.
          compacting: false,
        }
      : {
          runId: `real-${handle.pid}-${handle.startedAt}`,
          agentId: handle.agentId,
          conversationId: handle.conversationId,
          status: terminalStatus,
          startedAt: handle.startedAt,
          updatedAt: ts,
          lastActivityAt: ts,
          terminalReason: reason,
        };
    this.emit({
      event: 'runtime_state',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
      runtime_generation: handle.runtimeGeneration,
      state: base,
      ts,
    });
    this.clearActivitySnippetBuffer(handle, base.runId);
    // Clear so the next activity starts a fresh run. busy is reset
    // here too — turn_end already does it, but exit/crash paths
    // benefit from the safety net.
    handle.runState = null;
    handle.busy.outstandingToolUses = 0;
    handle.busy.compacting = false;
    handle.inputTurnActive = false;
    handle.nextTurnReserved = false;
    handle.nextTurnReservedAt = null;
    handle.reservationStallWarned = false;
    this.clearReservationStallTimer(handle);
  }

  /**
   * Write `payload` to the child's stdin as a single NDJSON line.
   * The daemon's contract is "I write whatever bytes you give me +
   * a trailing newline" — `payload` is intentionally `unknown` so
   * either encoding shape is accepted:
   *
   *   - `string` → written verbatim + '\n'
   *   - object   → `JSON.stringify(payload) + '\n'`
   *
   * Both match CM's `writeLine` semantics — encoded line, single
   * trailing newline. Backpressure: if `stdin.write` returns false
   * we wait for `'drain'` before resolving (matches CM
   * `conversation-manager.ts` ~2322).
   */
  private writeStdinPayload(handle: RuntimeHandle, payload: unknown): Promise<void> {
    return new Promise((resolve, reject) => {
      const stdin = handle.child.stdin;
      if (!stdin || stdin.destroyed) {
        reject(new Error('child stdin is not writable'));
        return;
      }
      const line = typeof payload === 'string' ? payload : JSON.stringify(payload);
      const ok = stdin.write(line + '\n', (err) => (err ? reject(err) : resolve()));
      if (!ok) {
        stdin.once('drain', () => resolve());
      }
    });
  }

  private writeRuntimePidfile(params: {
    runtimeScope: RuntimeScope;
    pid: number;
    startedAt: number;
    runtime: RuntimeKind;
    cmd: string;
  }): void {
    if (!this.runtimePidRootDir || !this.daemonInstanceId) return;
    writeRuntimePidfile({
      rootDir: this.runtimePidRootDir,
      daemonInstanceId: this.daemonInstanceId,
      ...params,
    });
  }

  private unlinkRuntimePidfileForHandle(handle: RuntimeHandle): void {
    if (!this.runtimePidRootDir || !this.daemonInstanceId) return;
    try {
      unlinkRuntimePidfile({
        rootDir: this.runtimePidRootDir,
        daemonInstanceId: this.daemonInstanceId,
        runtimeScope: handle.runtimeScope,
      });
    } catch (err) {
      this.logger.warn(
        {
          kind: 'process',
          action: 'runtime_pidfile_unlink_failed',
          pid: handle.pid,
          conversationId: handle.conversationId,
          agent_id: handle.agentId,
          err: (err as Error).message,
        },
        'failed to unlink runtime pidfile during child finalization',
      );
    }
  }

  private onChildExit(
    handle: RuntimeHandle,
    code: number | null,
    signal: NodeJS.Signals | null,
  ): void {
    if (handle.phase === 'exited') return; // double-fire guard

    if (handle.sigkillTimer) {
      clearTimeout(handle.sigkillTimer);
      handle.sigkillTimer = null;
    }
    if (handle.fatalReclaimTimer) {
      clearTimeout(handle.fatalReclaimTimer);
      handle.fatalReclaimTimer = null;
    }
    // 3c-b: stop any pending init / compaction timers attached to
    // this handle BEFORE the retry-respawn branch fires, otherwise a
    // stray `failInitWatchdog` callback could race the new handle and
    // observe the old one mid-cleanup.
    this.clearInitWatchdog(handle);
    this.clearCompactionWatchdog(handle);

    // #task:2210 exit-vs-stdout-drain: decide the deferral BEFORE touching the
    // line buffer. A lifecycle-start gate that is still pending with the stdout
    // channel not yet closed must survive to EOF — because a matching `started`
    // can arrive split across chunks around the exit. If `onChildExit` flushed
    // the buffer now, a half-frame prefix already sitting in it would be parsed
    // as a (broken) complete line and cleared, so the suffix arriving after
    // exit could never be re-stitched → the gate would wrongly fail→rollback
    // (Cindy `CbJ59KTcL69Z6rJa` A). So when deferring we leave `stdoutLineBuffer`
    // intact; `onStdoutDrained` stitches and drains it at EOF.
    const deferLifecycleGate =
      handle.startupDelivery !== null &&
      handle.startupAdmission !== null &&
      !handle.stdoutEnded &&
      handle.child.stdout != null;

    // Cindy `5nruk1IPaEHSab0T` B5: Node's `readline.createInterface`
    // (which CM uses) emits a final `line` for input that ended
    // without a trailing `\n` (verified locally:
    // `Readable.from(['abc'])` → `line: 'abc'`). The supervisor's
    // chunk-buffered pump was discarding that tail — meaning the
    // last complete-but-unterminated JSON frame (typically the
    // child's final session_init / turn_end / error line on quick
    // exit) would silently vanish from the parsed-event stream.
    // Flush it BEFORE setting `phase='exited'`, because
    // `consumeStdoutForParse` early-returns on `phase==='exited'`
    // — but `dispatchParsedLine` itself has no phase guard, so we
    // hand the tail to it directly. Cleared so the double-fire guard
    // above stays correct on the (impossible-by-contract) re-entry.
    // Skipped entirely when deferring a lifecycle gate (see above): the
    // buffer is preserved for EOF reassembly instead.
    if (!deferLifecycleGate && handle.stdoutLineBuffer.length > 0) {
      const tail = handle.stdoutLineBuffer;
      handle.stdoutLineBuffer = '';
      this.dispatchParsedLine(handle, tail);
    }

    handle.phase = 'exited';
    handle.exitCode = code;
    this.unlinkRuntimePidfileForHandle(handle);

    const intentional = handle.intentionalExit;
    const ts = this.now();
    const k = runtimeScopeKey(handle.runtimeScope);

    // #task:1460: fold a pre-init failure into the SAME sticky
    // init-failure counter the watchdog uses, so a runtime that dies
    // before `session_init` (before, or without, the watchdog arming)
    // can't drive an unbounded backend re-drive loop (incident #1459).
    // Membership contract:
    //   - never reached `session_init` (`!initObserved`), is not the
    //     watchdog's own kill (`!initTimedOut`, already counted in
    //     `failInitWatchdog`), and is not a user/daemon stop
    //     (`!intentionalExit`);
    //   - a long-lived (non-oneShot) runtime counts on ANY such exit,
    //     even a clean code 0 — it never replied, so it is an init
    //     failure (Cindy: lifecycle, not exit code);
    //   - a oneShot (cursor `--print`) runtime's compatible completion is
    //     a clean code-0 exit, which is NOT a failure; but an ABNORMAL
    //     oneShot exit before init (non-zero code, or a signal such as an
    //     EDR SIGKILL) IS a failure — `oneShot` explains "exits after a
    //     successful turn", not "gets killed mid-startup".
    // The daemon only ACCOUNTS the failure here (bumps the counter,
    // engages cooldown at threshold); backend owns the actual respawn via
    // its pending-inbox re-drive (immediate, no backoff → this is bounded
    // retry + cooldown, not exponential/short backoff).
    const preInitFailure =
      !handle.initObserved &&
      !handle.initTimedOut &&
      !handle.intentionalExit &&
      (!handle.driver.oneShot || code !== 0 || signal !== null);
    if (preInitFailure) {
      this.recordInitFailure(handle);
    }

    this.logger.info(
      {
        kind: 'process',
        action: 'exit',
        pid: handle.pid,
        conversationId: handle.conversationId,
        agent_id: handle.agentId,
        exit_code: code,
        signal: signal ?? null,
        intentional,
        init_timed_out: handle.initTimedOut,
        init_retry_planned: handle.initRetryPlanned,
      },
      'child exited',
    );

    // If a startup batch was still pending at exit, settle it as
    // `child_exit` so backend's startup_delivery handler isn't left
    // hanging. (Cindy: failed settle must still clear the gate so
    // run-state / delivery gating stays coherent.) `clearStartupGate`
    // wipes both `startupDelivery` and `startupAwaitingInit` in the
    // same critical section so no `(null, true)` interleaving
    // appears.
    //
    // #task:2210 exit-vs-stdout-drain: for a lifecycle-start gate the failed
    // ROLLBACK must not run yet if the stdout admission channel is still open.
    // Node's `exit` does not guarantee stdout is drained: a runtime that read
    // the payload, echoed a matching `started`, then exited can have that
    // stdout chunk delivered AFTER `exit`. Emitting failed here would roll an
    // already-admitted row back to pending → the next runtime re-reads it →
    // product-level double delivery, violating the safety root (reopen pending
    // only once the old consumer can't read AND its admission proof is
    // drained). So we DEFER: record the reason and let `onStdoutDrained`
    // finalize the single failed once the channel truly closes with no
    // matching `started`. The gate stays held meanwhile, so a raced `started`
    // still settles it to consumed (see the `input_lifecycle` handler, which
    // has no phase guard, and `consumeStdoutForParse`, which keeps feeding a
    // pending admission gate post-exit). NOT time-inferred — purely EOF-driven.
    // The non-lifecycle-start paths (`startupAwaitingInit`, plain immediate)
    // keep settling at exit exactly as before.
    if (handle.startupDelivery !== null) {
      const reason: 'child_exit' | 'init_watchdog' = handle.initTimedOut
        ? 'init_watchdog'
        : 'child_exit';
      if (deferLifecycleGate) {
        handle.startupGateExitReason = reason;
      } else {
        this.emitStartupDeliveryFailed(handle, handle.startupDelivery.deliveryIds, reason);
        this.clearStartupGate(handle);
      }
    }

    // Any batches the daemon was holding in its mid-turn queue (#task:1417
    // Stage 3, §11.4/§11.6) die with the runtime — settle each as
    // `child_exit` so the backend rolls the rows back to pending
    // (oneShot respawn-with-batch or `ready_for_drain` replay re-drives).
    // No orphan: the DB rows were never acked. Emitted per-entry so each
    // owner's rollback is independent.
    this.failDeferredQueue(handle);
    this.clearReservationStallTimer(handle);
    // A waiting stale-receive pull (#task:1614) re-evaluates immediately
    // and observes `runtime_replaced` instead of running out its window.
    // An unresolved pull RESERVATION is deliberately NOT touched here:
    // its batch is no longer in the queue, and `settleStaleReceiveHandoff`
    // owns its exactly-once settlement (consumed on a proven handoff,
    // failed-for-rollback otherwise).
    this.wakePullWaiter(handle);

    // 3c-b retry branch (Cindy plan v2 issue #3 + override precedence
    // `-pdNvRYOWip5e9yA`): if the init watchdog killed the child AND
    // it decided retry was eligible, respawn under the lifecycle
    // lock with `initAttempt + 1`. Daemon-internal retry SKIPS the
    // start-time cooldown gate (`startWithLock` is the gated entry
    // — internalRespawnForRetry calls it WITHOUT bypassing because
    // `startUnlocked` does its own gate check that distinguishes
    // `initAttempt === 0` vs `> 0`).
    if (handle.initTimedOut && handle.initRetryPlanned && !handle.intentionalExit) {
      const newAttempt = handle.initAttempt + 1;
      this.emit({
        event: 'runtime_init_retrying',
        runtime_scope: handle.runtimeScope,
        conversation_id: handle.conversationId,
        agent_id: handle.agentId,
        runtime_generation: handle.runtimeGeneration,
        retry_attempt: newAttempt,
        max_retries: this.initMaxRetries,
        delay_ms: 0,
        ts: this.now(),
      });
      // Emit terminal runtime_state for the failed attempt and the
      // accompanying runtime_exit, then drop the handle and respawn.
      this.handles.delete(k);
      this.resolveRun(handle, 'timeout');
      this.emit({
        event: 'runtime_exit',
        runtime_scope: handle.runtimeScope,
        conversation_id: handle.conversationId,
        agent_id: handle.agentId,
        runtime_generation: handle.runtimeGeneration,
        code,
        intentional: false,
        ts,
      });
      void this.internalRespawnForRetry(handle.spawnParams, newAttempt);
      return;
    }

    // 3c-b cooldown branch: init failed, no retry. If a fresh
    // cooldown window engaged in this incident, surface it on the
    // wire so backend can project `agent_status crashed
    // reason=init_cooldown` for the now-dead handle. `cooldownUntil > now`
    // is the only "really cooling down right now" check (Cindy plan
    // v2 issue #2 — cooldown ≠ retry-exhausted; the two are
    // independent state). #task:1460: a pre-init exit accounted above
    // takes the exact same branch — it engages cooldown identically, so
    // backend sets `initFailureFlag` (suppressing the next respawn) and
    // the start-time cooldown gate refuses fresh starts. The
    // `runtime_init_cooldown` emit MUST precede the terminal
    // `runtime_state` / `runtime_exit` below so backend has the flag set
    // before it processes the exit.
    if (handle.initTimedOut || preInitFailure) {
      const state = this.initRetryStates.get(k);
      if (state && state.cooldownUntil > this.now()) {
        this.emit({
          event: 'runtime_init_cooldown',
          runtime_scope: handle.runtimeScope,
          conversation_id: handle.conversationId,
          agent_id: handle.agentId,
          runtime_generation: handle.runtimeGeneration,
          cooldown_until: state.cooldownUntil,
          attempts_exhausted: state.failures,
          ts: this.now(),
        });
      }
    }

    // Cindy `YW2BGWryWjfymPkU` B3 + `5nruk1IPaEHSab0T` B4: terminal
    // cleanup goes through `resolveRun`, which (a) maps the reason to
    // the correct terminal status per CM, (b) emits the terminal
    // `runtime_state`, and (c) clears `handle.runState` so the next
    // run starts with a fresh `runId` and no stale terminalReason.
    //
    // Reason mapping (matches CM `conversation-manager.ts` lines
    // 856-907):
    //   init watchdog killed it → `timeout` (precedence over intentional)
    //   intentional stop request → `stopped`
    //   pre-init failure (#task:1460) → `crashed`, even on code === 0:
    //     a long-lived runtime that dies before `session_init` did NOT
    //     reply, so it must not project `replied → idle`; and it is NOT a
    //     `timeout` (the watchdog never fired). A oneShot's CLEAN code-0
    //     exit is excluded from `preInitFailure`, so it still maps to
    //     `replied` below; an ABNORMAL oneShot pre-init exit is a
    //     `preInitFailure` and correctly lands here as `crashed`.
    //   clean exit (code === 0)  → `replied`  (oneShot/cursor critical)
    //   non-zero / signal exit   → `crashed`
    // `replied → idle`, `timeout|stopped|crashed → failed` per CM's
    // terminalStatus table (Cindy `GMbjEkGUHrB1BBMe`).
    const reason: NonNullable<AgentRunState['terminalReason']> = handle.initTimedOut
      ? 'timeout'
      : intentional
        ? 'stopped'
        : preInitFailure
          ? 'crashed'
          : code === 0
            ? 'replied'
            : 'crashed';
    this.resolveRun(handle, reason);
    this.emit({
      event: 'runtime_exit',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
      runtime_generation: handle.runtimeGeneration,
      code,
      intentional,
      ts,
    });
  }

  /**
   * #task:2210 exit-vs-stdout-drain finalizer. Fires when the child's stdout
   * admission channel reaches EOF (`end`) / `close` — the point at which every
   * byte the runtime ever wrote has been delivered and parsed. Idempotent: the
   * `stdoutEnded` flag makes the second of the `end`/`close` pair (and any
   * duplicate) a no-op, satisfying "close 后重复 exit/close 不二次结算".
   *
   * Two responsibilities, both scoped to a lifecycle-start gate that
   * `onChildExit` DEFERRED (`startupGateExitReason !== null`):
   *   1. Drain a final unterminated tail preserved across exit (`onChildExit`
   *      deliberately did NOT flush it) through the admission-only reader, so a
   *      matching `started` split across the exit boundary re-stitches here and
   *      still settles consumed — without any ordinary post-exit side effect.
   *   2. If the gate is STILL pending after the channel closed, no admission
   *      proof will ever arrive → emit the single failed/rollback with the
   *      reason captured at exit. If a raced `started` already settled it to
   *      consumed, `startupAdmission` is null here and we emit nothing — the
   *      exactly-once, EOF-not-timeout rollback the safety root requires.
   */
  private onStdoutDrained(handle: RuntimeHandle): void {
    if (handle.stdoutEnded) return;
    handle.stdoutEnded = true;
    if (handle.startupGateExitReason === null) {
      // Either no gate was deferred, or the channel drained before `exit`
      // (then `onChildExit` finalizes inline via the `stdoutEnded` branch).
      return;
    }
    if (handle.startupAdmission !== null && handle.stdoutLineBuffer.length > 0) {
      // Final unterminated tail: a matching `started` may have arrived split
      // across the exit and only completed here. Stitch it as ONE line (the
      // post-exit pump always leaves a newline-free remainder) and run it
      // through the SAME admission-only reader — never `dispatchParsedLine`, so
      // no ordinary run-state / wire / boundary effect fires after exit
      // (Cindy `CbJ59KTcL69Z6rJa` A+B).
      const tail = handle.stdoutLineBuffer;
      handle.stdoutLineBuffer = '';
      this.drainAdmissionProofFromLine(handle, tail);
    }
    if (handle.startupAdmission !== null && handle.startupDelivery !== null) {
      this.emitStartupDeliveryFailed(
        handle,
        handle.startupDelivery.deliveryIds,
        handle.startupGateExitReason,
      );
      this.clearStartupGate(handle);
    }
    handle.startupGateExitReason = null;
  }

  // ── private: state emitter ─────────────────────────────────────

  /**
   * Emit a `runtime_state` reflecting `handle`'s current phase /
   * run-state. Two layers:
   *
   *   1. Phase-derived (starting/idle) — used when there's no
   *      handle.runState yet. Synthesizes a stable runId from
   *      pid+startedAt so consecutive phase emits correlate.
   *   2. Run-state-derived — when 3b's parsed-event lifecycle has
   *      produced a real AgentRunState we project it as-is.
   *
   * NOTE (Cindy `YW2BGWryWjfymPkU` B3): this method does NOT carry
   * `terminalReason`. Every terminal transition (turn_end, hard
   * stop, crash) MUST go through
   * `resolveRun` instead, which (a) maps reason → terminal status
   * per CM and (b) clears `handle.runState` so the next run starts
   * fresh. Passing terminalReason through here would let a
   * tool_running state get a `terminalReason` patched on without
   * the status flip, which is exactly the bug B3 is fixing.
   */
  private emitState(handle: RuntimeHandle): void {
    const ts = this.now();
    const base: AgentRunState = handle.runState
      ? {
          ...handle.runState,
          updatedAt: ts,
        }
      : {
          runId: `real-${handle.pid}-${handle.startedAt}`,
          agentId: handle.agentId,
          conversationId: handle.conversationId,
          status: phaseToStatus(handle.phase),
          startedAt: handle.startedAt,
          updatedAt: ts,
          lastActivityAt: ts,
        };
    this.emit({
      event: 'runtime_state',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
      runtime_generation: handle.runtimeGeneration,
      state: base,
      ts,
    });
  }

  // ── 3c-b: init watchdog + retry/cooldown + lifecycle lock ──────

  /**
   * Override-resolution for one init watchdog tier. Cindy
   * `-pdNvRYOWip5e9yA` locked precedence:
   *
   *   1. `opts.initWatchdogMs` (global override; one number applies
   *      to all three tiers — matches CM `ConversationManager.initWatchdogMs`)
   *   2. `opts.init<Tier>Ms`   (per-tier override on the supervisor)
   *   3. `driver.<tier>`       (driver opinion — claude/codex/cursor
   *      each declare their own defaults)
   *   4. `DEFAULT_INIT_<TIER>_MS`
   *
   * The global override exists specifically for tests that want one
   * tiny number for all three timers without overriding the per-tier
   * fields one by one.
   */
  private initTimeoutFor(driver: Driver, tier: InitTimeoutTier, fallback: number): number {
    if (this.initWatchdogMs !== null) return this.initWatchdogMs;
    const overrideMap: Record<InitTimeoutTier, number | undefined> = {
      initLivenessSilenceMs: this.initLivenessSilenceMs,
      initPreProtocolCapMs: this.initPreProtocolCapMs,
      initPostProtocolMs: this.initPostProtocolMs,
    };
    const override = overrideMap[tier];
    if (override !== undefined) return override;
    return driver[tier] ?? fallback;
  }

  /** Arm both the silence-tier and the pre-protocol-cap timer for
   *  this handle. Idempotent: clears any prior timers first so retries
   *  / delivery-driven deferred arming don't pile up. */
  private startInitWatchdog(handle: RuntimeHandle): void {
    this.clearInitWatchdog(handle);
    handle.initWatchdogArmed = true;
    handle.initStartedAt = this.now();
    this.armInitLivenessSilenceWatchdog(handle);
    const timeoutMs = this.initTimeoutFor(
      handle.driver,
      'initPreProtocolCapMs',
      DEFAULT_INIT_PRE_PROTOCOL_CAP_MS,
    );
    handle.initPreProtocolCapTimer = setTimer(() => {
      handle.initPreProtocolCapTimer = null;
      this.failInitWatchdog(handle, 'pre_protocol_cap', timeoutMs);
    }, timeoutMs);
    handle.initPreProtocolCapTimer.unref();
  }

  /** Clear every init-watchdog timer attached to this handle. Called
   *  from clearSession, hard stop, child exit, AND when init lands. */
  private clearInitWatchdog(handle: RuntimeHandle): void {
    if (handle.initLivenessSilenceTimer) {
      clearTimeout(handle.initLivenessSilenceTimer);
      handle.initLivenessSilenceTimer = null;
    }
    if (handle.initPreProtocolCapTimer) {
      clearTimeout(handle.initPreProtocolCapTimer);
      handle.initPreProtocolCapTimer = null;
    }
    if (handle.initPostProtocolTimer) {
      clearTimeout(handle.initPostProtocolTimer);
      handle.initPostProtocolTimer = null;
    }
  }

  /** (Re-)arm the silence-tier timer. Called on watchdog start AND on
   *  every refresh (raw byte arrival pre-protocol). */
  private armInitLivenessSilenceWatchdog(handle: RuntimeHandle): void {
    if (handle.initLivenessSilenceTimer) {
      clearTimeout(handle.initLivenessSilenceTimer);
      handle.initLivenessSilenceTimer = null;
    }
    const timeoutMs = this.initTimeoutFor(
      handle.driver,
      'initLivenessSilenceMs',
      DEFAULT_INIT_LIVENESS_SILENCE_MS,
    );
    handle.initLivenessSilenceTimer = setTimer(() => {
      handle.initLivenessSilenceTimer = null;
      this.failInitWatchdog(handle, 'silence', timeoutMs);
    }, timeoutMs);
    handle.initLivenessSilenceTimer.unref();
  }

  /** Reset the silence-tier debounce on every raw byte from the
   *  child. No-op once protocol or session ready has been observed —
   *  silence is only meaningful pre-first-frame. */
  private refreshInitLiveness(handle: RuntimeHandle): void {
    if (!handle.initWatchdogArmed) return;
    if (handle.initObserved || handle.initProtocolObserved || handle.initTimedOut) return;
    this.armInitLivenessSilenceWatchdog(handle);
  }

  /** Driver's pure line classifier advances the watchdog state
   *  machine. `protocol_ready` cancels silence + pre-protocol cap and
   *  arms the post-protocol timer; `session_ready` cancels every
   *  timer outright (the parsed `session_init` branch then flips
   *  `initObserved` + clears the retry bucket). */
  private observeInitProgress(handle: RuntimeHandle, line: string): void {
    if (!handle.initWatchdogArmed) return;
    if (handle.initObserved || handle.initTimedOut) return;
    const progress = handle.driver.classifyInitProgress(line);
    if (!progress) return;
    this.markInitProtocolReady(handle);
    if (progress === 'session_ready') this.clearInitWatchdog(handle);
  }

  private markInitProtocolReady(handle: RuntimeHandle): void {
    if (handle.initProtocolObserved) return;
    handle.initProtocolObserved = true;
    // Cancel the pre-protocol-only timers; arm the post-protocol
    // timer to bound the silence between first protocol frame and
    // session ready.
    if (handle.initLivenessSilenceTimer) {
      clearTimeout(handle.initLivenessSilenceTimer);
      handle.initLivenessSilenceTimer = null;
    }
    if (handle.initPreProtocolCapTimer) {
      clearTimeout(handle.initPreProtocolCapTimer);
      handle.initPreProtocolCapTimer = null;
    }
    const timeoutMs = this.initTimeoutFor(
      handle.driver,
      'initPostProtocolMs',
      DEFAULT_INIT_POST_PROTOCOL_MS,
    );
    handle.initPostProtocolTimer = setTimer(() => {
      handle.initPostProtocolTimer = null;
      this.failInitWatchdog(handle, 'post_protocol', timeoutMs);
    }, timeoutMs);
    handle.initPostProtocolTimer.unref();
  }

  /** Deferred-init arming for `requires_first_message` drivers. Called
   *  from `consumeStartupDelivery` after a successful first stdin write.
   *  No-op if watchdog
   *  already armed, init already observed, or init already timed out. */
  private ensureInitWatchdogArmed(handle: RuntimeHandle): void {
    if (handle.initWatchdogArmed || handle.initObserved || handle.initTimedOut) return;
    this.startInitWatchdog(handle);
  }

  /**
   * Watchdog fired. Emit `runtime_init_timeout`, decide retry vs
   * cooldown, then SIGTERM → SIGKILL the child. The exit handler
   * picks up the retry branch from `handle.initRetryPlanned`.
   *
   * Idempotent: bails if the handle is already gone, already init'd,
   * or already marked init-timed-out. This matters because three
   * timers race the same flag; only the first to fire counts.
   */
  private failInitWatchdog(
    handle: RuntimeHandle,
    reason: InitTimeoutReason,
    timeoutMs: number,
  ): void {
    const k = runtimeScopeKey(handle.runtimeScope);
    if (!this.handles.has(k) || handle.initObserved || handle.initTimedOut) return;

    this.clearInitWatchdog(handle);
    handle.initTimedOut = true;
    handle.initTimeoutReason = reason;
    // Cindy `HyC05fC-FEs94uoZ` B3: flip to `draining` + emit state so
    // the delivery gate (`handle.phase === 'draining' → reason:'draining'`)
    // covers the SIGTERM→SIGKILL kill window. Without this, backend
    // could `runtime_deliver` between the timer firing and the child
    // actually exiting, and the daemon would write stdin + markDelivered
    // a message to a process that's already being killed. We do NOT
    // touch `intentionalExit` — the watchdog kill is daemon-initiated,
    // not a user stop, and the exit handler's reason mapping still
    // resolves to `'timeout'` (which takes precedence over both
    // `intentional` and `code === 0`). The retry-respawn branch in
    // `onChildExit` doesn't gate on phase, so this draining flip does
    // not block `internalRespawnForRetry`.
    handle.phase = 'draining';
    this.emitState(handle);
    // Account the failure (pure), then decide daemon-internal retry —
    // a watchdog-only concern: retry only when no cooldown is active,
    // attempts remain, and the exit wasn't user-intentional.
    const { cooldownUntilOnEvent } = this.recordInitFailure(handle);
    const retry =
      cooldownUntilOnEvent === null &&
      handle.initAttempt < this.initMaxRetries &&
      !handle.intentionalExit;
    handle.initRetryPlanned = retry;

    const elapsedMs = this.now() - handle.initStartedAt;
    this.logger.warn(
      {
        kind: 'process',
        action: 'init_watchdog_fired',
        pid: handle.pid,
        conversationId: handle.conversationId,
        agent_id: handle.agentId,
        timeout_reason: reason,
        elapsed_ms: elapsedMs,
        timeout_ms: timeoutMs,
        retry_attempt: handle.initAttempt,
        retrying: retry,
      },
      'init watchdog fired; killing child',
    );
    this.emit({
      event: 'runtime_init_timeout',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
      runtime_generation: handle.runtimeGeneration,
      timeout_reason: reason,
      elapsed_ms: elapsedMs,
      timeout_ms: timeoutMs,
      retrying: retry,
      retry_attempt: handle.initAttempt,
      max_retries: this.initMaxRetries,
      ...(cooldownUntilOnEvent !== null ? { cooldown_until: cooldownUntilOnEvent } : {}),
      ts: this.now(),
    });

    // SIGTERM → SIGKILL cascade. Use the supervisor's grace-period
    // knob so test runs stay snappy.
    try {
      handle.child.kill('SIGTERM');
    } catch {
      // Already gone — exit listener fires the cleanup either way.
    }
    setTimer(() => {
      if (!this.handles.has(k)) return;
      if (handle.phase === 'exited') return;
      try {
        handle.child.kill('SIGKILL');
      } catch {
        // Already gone — same.
      }
    }, this.sigtermToSigkillMs).unref();
  }

  /** Bump the sticky per-RuntimeScope init-failure counter and engage
   *  the cooldown clock when the threshold is crossed. PURE accounting —
   *  it deliberately does NOT decide daemon-internal retry; that is the
   *  watchdog's concern (`failInitWatchdog`), layered on top of this
   *  result, and it does not apply to a backend-driven pre-init respawn.
   *  Returns the new failure count plus, when a cooldown window is active
   *  right now, its deadline (Cindy plan v2 issue #2: cooldown vs
   *  retry-exhausted are independent state). */
  private recordInitFailure(handle: RuntimeHandle): {
    failures: number;
    cooldownUntilOnEvent: number | null;
  } {
    const k = runtimeScopeKey(handle.runtimeScope);
    const now = this.now();
    const state = this.initRetryStates.get(k) ?? { failures: 0, cooldownUntil: 0 };
    state.failures += 1;
    if (state.failures >= this.initFailureCooldownThreshold) {
      state.cooldownUntil = now + this.initFailureCooldownMs;
    }
    this.initRetryStates.set(k, state);
    const cooldownActive = state.cooldownUntil > now;
    return {
      failures: state.failures,
      cooldownUntilOnEvent: cooldownActive ? state.cooldownUntil : null,
    };
  }

  /** Wipe the failure counter + cooldown clock. Called on successful
   *  `session_init` AND when start_unlocked observes a no-op
   *  idempotent return — neither of those cases should keep an old
   *  incident window alive. */
  private clearInitRetryState(scope: RuntimeScope): void {
    this.initRetryStates.delete(runtimeScopeKey(scope));
  }

  /** Is the RuntimeScope currently inside an active cooldown window? */
  private isInitCoolingDown(k: string): boolean {
    const state = this.initRetryStates.get(k);
    return !!state && state.cooldownUntil > this.now();
  }

  /**
   * Daemon-internal retry respawn. Called from the child-exit handler
   * when `initRetryPlanned` was true. Goes through `startWithLock` so
   * the per-RuntimeScope lifecycle lock serializes the respawn
   * against any concurrent user-initiated start / stop / clearSession.
   * Errors are logged but not propagated — the retry already emitted
   * `runtime_init_retrying`; a failed respawn surfaces as the next
   * watchdog firing or a different `runtime_exit`.
   */
  private async internalRespawnForRetry(
    originalParams: RuntimeStartParams,
    newAttempt: number,
  ): Promise<void> {
    if (this.agentHomeCoordinator.isFenced(originalParams.agent_id)) {
      this.clearInitRetryState(originalParams.options.runtime_scope);
      this.logger.info(
        {
          kind: 'process',
          action: 'init_retry_respawn_fenced',
          conversationId: originalParams.conversation_id,
          agent_id: originalParams.agent_id,
        },
        'skipping internal retry respawn: agent deletion fence is installed',
      );
      return;
    }
    try {
      await this.startWithLock(originalParams, newAttempt);
    } catch (err) {
      this.logger.error(
        {
          kind: 'process',
          action: 'init_retry_respawn_failed',
          conversationId: originalParams.conversation_id,
          agent_id: originalParams.agent_id,
          retry_attempt: newAttempt,
          error_message: (err as Error).message,
        },
        'init watchdog retry respawn failed',
      );
    }
  }

  private assertAgentHomeOpen(agentId: string): void {
    if (this.agentHomeCoordinator.isFenced(agentId)) {
      throw new AgentDeletingError(agentId);
    }
  }

  /**
   * Per-RuntimeScope lifecycle serialization. Cindy plan v2 issue
   * #6 + tightening `ixAq6GPTn4fVSpCF`:
   *
   *   - Covers ONLY lifecycle mutations: start, hard stop, clearSession,
   *     internal retry respawn. delivery/output-pause go around.
   *   - Per-key, not global — different RuntimeScope operations parallelize.
   *   - Survives previous-op failure: a rejected predecessor must not
   *     wedge the queue. We swallow the previous tail's rejection at
   *     the await site.
   *
   * The map entry holds the *latest tail* so subsequent calls queue
   * behind every in-flight op for that key.
   */
  private async withLifecycleLock<T>(k: string, fn: () => Promise<T>): Promise<T> {
    const prev = this.lifecycleTails.get(k);
    let resolveTail!: () => void;
    const tail = new Promise<void>((r) => {
      resolveTail = r;
    });
    this.lifecycleTails.set(k, tail);
    try {
      if (prev) {
        // Tolerate previous-op failure — the queue keeps moving even
        // if an earlier start threw or a stop rejected.
        try {
          await prev;
        } catch {
          /* swallow — see comment */
        }
      }
      return await fn();
    } finally {
      resolveTail();
      // Only clear the map entry if we're still the tail. A faster
      // follow-up op may have already overwritten it with its own
      // tail; in that case the map should keep the newer reference.
      if (this.lifecycleTails.get(k) === tail) {
        this.lifecycleTails.delete(k);
      }
    }
  }

  // ── 3c-b: compaction watchdog ──────────────────────────────────

  /**
   * Arm the compaction watchdog. On fire: emit
   * `runtime_compaction_stale` AND stack `compactionStale: true` on
   * the run state (Cindy plan v2 issue #4 — observational only;
   * preserve `compacting: true`; do NOT kill the child or change
   * phase). Idempotent: clears any prior timer first.
   */
  private startCompactionWatchdog(handle: RuntimeHandle): void {
    this.clearCompactionWatchdog(handle);
    const startedAt = this.now();
    handle.compactionWatchdog = setTimer(() => {
      handle.compactionWatchdog = null;
      this.fireCompactionStale(handle, startedAt);
    }, this.compactionStaleMs);
    handle.compactionWatchdog.unref();
  }

  private clearCompactionWatchdog(handle: RuntimeHandle): void {
    if (handle.compactionWatchdog) {
      clearTimeout(handle.compactionWatchdog);
      handle.compactionWatchdog = null;
    }
  }

  private fireCompactionStale(handle: RuntimeHandle, startedAt: number): void {
    // Defense in depth: a finish that lands in the same tick as the
    // timer would clear the watchdog but we'd still be inside the
    // callback. Bail if compaction is no longer active.
    if (!handle.busy.compacting) return;
    if (handle.phase === 'exited' || handle.phase === 'draining') return;
    const elapsedMs = this.now() - startedAt;
    this.logger.warn(
      {
        kind: 'process',
        action: 'compaction_stale',
        pid: handle.pid,
        conversationId: handle.conversationId,
        agent_id: handle.agentId,
        elapsed_ms: elapsedMs,
      },
      'compaction has been running past the stale window with no finish event',
    );
    this.emit({
      event: 'runtime_compaction_stale',
      runtime_scope: handle.runtimeScope,
      conversation_id: handle.conversationId,
      agent_id: handle.agentId,
      elapsed_ms: elapsedMs,
      ts: this.now(),
    });
    // Stack stale onto the run state; explicitly do NOT touch
    // `compacting` — the UI must still show "compacting (stale)",
    // not "no longer compacting".
    this.transitionRunState(handle, { compactionStale: true });
  }
}

/** Phase → AgentRunState.status mapping used when no parsed-event
 *  run-state has been recorded yet. Matches the 3a contract. */
function phaseToStatus(phase: RuntimePhase): AgentRunState['status'] {
  switch (phase) {
    case 'spawning':
    case 'startup_pending':
      return 'starting';
    case 'ready':
    case 'draining':
    case 'exited':
      return 'idle';
  }
}

/**
 * Read `driver.readyToEncode()` with a default-true fallback for
 * drivers that don't implement the optional method. Stateless drivers
 * (claude) typically omit it; stateful drivers (codex) MUST implement
 * so the daemon's `ready_for_drain` gate, the `runtime_deliver`
 * `driver_not_ready` refuse arm, and the startup_delivery defer-pre-encode
 * path (Option A v2 — Cindy `UMGu36zoTDVuKWXi`) all key off the right state.
 *
 * Lifted to a module-level helper rather than an instance method so
 * `consumeStartupDelivery` / `tryConsumeStartupDelivery` /
 * `maybeEmitReadyForDrain` / `deliver` can call it
 * without re-establishing `this` binding.
 */
function driverReadyToEncode(driver: Driver): boolean {
  return driver.readyToEncode?.() ?? true;
}

async function ensureRuntimeActionWorkdir(
  workdir: string,
  target: RuntimeDeliverParams['target'],
  runtimeScope: RuntimeScope,
  logger: DaemonLogger,
): Promise<void> {
  try {
    await mkdir(workdir, { recursive: true });
  } catch (err) {
    logger.warn(
      {
        kind: 'process',
        action: 'runtime_action_workdir_unusable',
        runtime_scope: runtimeScope,
        target,
        resolved_path: workdir,
        reason: 'mkdir_failed',
        detail: err instanceof Error ? err.message : String(err),
      },
      'runtime delivery action workdir could not be created',
    );
    throw new RuntimeStartResolveError(
      'workspace_bundle_missing',
      `runtime delivery target ${target.conversation_id} has no usable action workdir on this computer`,
    );
  }
  if (await bundleCwdUsable(workdir)) return;
  logger.warn(
    {
      kind: 'process',
      action: 'runtime_action_workdir_unusable',
      runtime_scope: runtimeScope,
      target,
      resolved_path: workdir,
      reason: 'not_enterable',
    },
    'runtime delivery action workdir is not enterable',
  );
  throw new RuntimeStartResolveError(
    'workspace_bundle_missing',
    `runtime delivery target ${target.conversation_id} has no usable action workdir on this computer`,
  );
}

async function ensureRuntimeScopeCwd(
  options: ResolvedStartOptions,
  logger: DaemonLogger,
): Promise<void> {
  const workspace = isResolvedAgentStartOptions(options) ? null : options.workspace;
  const cwd = options.cwd;
  // shiki + Cindy 2026-08-04: the session cwd is the ONLY hard-ensured runtime
  // base; L1/L2 CONTENT is best-effort and never gates a spawn. This creates
  // the deterministic cwd (`mkdir -p`) and requires it to be a usable working
  // directory. It fails closed ONLY when the OS genuinely cannot spawn a child
  // there — the path is occupied by a non-directory file, or the dir cannot be
  // created/entered. A missing/half-seeded MEMORY.md is NOT a failure here; the
  // prompt simply omits the L2 section.
  //
  // relocation-v1 §7 task 3 (Cindy nail 1): the daemon→backend typed error is a
  // contract surface. The absolute cwd path is a host-local diagnostic — logged
  // HERE (host-only) but MUST NOT ride the wire `message`, which is forwarded
  // verbatim to the API/renderer. The wire message names only the semantic
  // workspace id; the concrete path never leaves the host.
  const logUnusable = (reason: string, detail?: string) => {
    logger.warn(
      {
        kind: 'process',
        action: 'runtime_start_workspace_cwd_unusable',
        reason,
        runtime_scope: options.runtime_scope,
        ...(workspace
          ? {
              workspace_id: workspace.id,
              workspace_name: workspace.name,
            }
          : {}),
        resolved_path: cwd,
        ...(detail !== undefined ? { detail } : {}),
      },
      'runtime_start: session cwd could not be ensured as a usable working directory',
    );
  };
  const cwdUnusable = () =>
    new RuntimeStartResolveError(
      'workspace_bundle_missing',
      workspace
        ? `runtime_start: session cwd for workspace ${workspace.id} ` +
            'could not be created or entered on this computer'
        : `runtime_start: agent-scoped session cwd for ${options.agent.id} ` +
            'could not be created or entered on this computer',
    );
  // Best-effort create — idempotent when the dir (or a parent chain) already
  // exists. A conflicting non-directory file at the path throws EEXIST/ENOTDIR
  // and is caught below as a hard admission failure.
  try {
    await mkdir(cwd, { recursive: true });
  } catch (err) {
    logUnusable('mkdir_failed', err instanceof Error ? err.message : String(err));
    throw cwdUnusable();
  }
  // Cwd-usability uses the SAME predicate as `list_workspaces.agent_dir`
  // (`bundleCwdUsable`: an existing directory this process can enter/traverse),
  // so list never reports a dir usable that this spawn would then choke on.
  if (await bundleCwdUsable(cwd)) return;
  logUnusable('not_enterable');
  throw cwdUnusable();
}

function incrementToolCount(
  counts: Record<string, number> | undefined,
  toolName: string,
): Record<string, number> {
  return {
    ...(counts ?? {}),
    [toolName]: (counts?.[toolName] ?? 0) + 1,
  };
}

function trimActivitySnippet(text: string): string | undefined {
  const normalized = text.replace(/\s+/g, ' ').trim();
  if (!normalized) return undefined;
  return normalized.length <= ACTIVITY_SNIPPET_MAX_CHARS
    ? normalized
    : normalized.slice(-ACTIVITY_SNIPPET_MAX_CHARS);
}

/**
 * Default runtime resolver — wraps `detectRuntimes()` from
 * `packages/host/src/runtimes.ts`. The detector has a short cache, but a
 * miss is force-refreshed so installing a runtime after daemon boot does not
 * require a daemon restart before spawn can find it.
 *
 * Returns the probed `version` alongside the binary (#task:1909). It costs
 * nothing extra: `detectRuntime` already runs `<bin> --version` to decide
 * `present` vs `present_unverified`, and this resolver already awaited that
 * catalog — it simply used to drop the version on the floor.
 */
async function detectedRuntimes(force = false): Promise<Map<RuntimeKind, ResolvedRuntime>> {
  const infos = await detectRuntimes({ force });
  const m = new Map<RuntimeKind, ResolvedRuntime>();
  for (const info of infos) {
    if (info.installed && info.binary) {
      m.set(info.kind, { binary: info.binary, version: info.version });
    }
  }
  return m;
}

async function defaultResolveRuntime(
  kind: RuntimeKind,
  _fallback: string,
): Promise<ResolvedRuntime | null> {
  const cached = await detectedRuntimes(false);
  const found = cached.get(kind);
  if (found) return found;
  const fresh = await detectedRuntimes(true);
  return fresh.get(kind) ?? null;
}

export type { RuntimeHandle, RuntimePhase };
