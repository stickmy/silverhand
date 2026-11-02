/**
 * RuntimeHandle — per-(conversation, agent) execution-plane state
 * owned by `Supervisor`.
 *
 * 3a scope: spawn / output / hard-stop / exit. 3b adds parse-fanout
 * state (line buffer + per-handle AgentRunState), startup-delivery
 * step-0 gating, and the `initObserved` flag the delivery gate keys off.
 * 3c-b adds the init-watchdog timers, retry/cooldown bookkeeping,
 * and the compaction watchdog. The lifecycle-lock lives on the
 * supervisor (`lifecycleTails: Map<key, Promise<void>>`) rather than
 * the handle so that operations on a (conv, agent) with no handle yet
 * (e.g. a fresh `start`) still queue correctly.
 *
 * Consumers reach the handle only through `Supervisor` methods.
 */
import type { ChildProcess } from 'node:child_process';
import type { AgentRunState } from '@silverhand/shared';
import type { Driver } from '../../drivers/index.js';
import type { RuntimeDelivery, RuntimeScope, RuntimeStartParams } from '../../protocol.js';

/** Tier name for the 3-tier init watchdog. Matches CM's
 *  `InitTimeoutReason` so the wire `runtime_init_timeout.timeout_reason`
 *  can carry these values through unmodified. */
export type InitTimeoutReason = 'silence' | 'pre_protocol_cap' | 'post_protocol';

export type RuntimePhase = 'spawning' | 'startup_pending' | 'ready' | 'draining' | 'exited';

/**
 * Step-0 startup-delivery gating state. Presence of this object is
 * the SINGLE source of truth for "this runtime has a startup batch
 * still in flight" — `null` means no batch pending (whether because
 * `start()` was called without a startup batch (pure lifecycle spawn),
 * or because the batch settled and the supervisor cleared this field).
 *
 * Cindy `_JjyK8UrpcmYA5Iu` + `wuiwdLKrLFyGip_s`: keep this a
 * separate, explicit field (not derived from a `Set<number>` of
 * delivery ids) so the state machine bit and the idempotency-ids
 * cannot drift. Settle order is fixed:
 *   1. stdin write resolves (success or failure determined)
 *   2. emit `runtime_delivery_consumed` OR `_failed`
 *   3. set `startupDelivery = null`
 *   4. only NOW can `maybeEmitReadyForDrain` fire
 * This prevents a stray `runtime_deliver` from sneaking past the gate
 * between "consumed/failed event sent" and "pending cleared".
 */
export interface StartupDeliveryState {
  /** Origin target for presentation and ack. NOT a freshness key: staleness is
   * a whole-runtime backend property (#task:2095), so nothing here is per
   * stream. Not part of AgentSlot runtime identity. */
  readonly conversationId: string;
  /** Host-resolved absolute action directory for this origin target. */
  readonly actionWorkdir: string;
  readonly deliveryIds: readonly number[];
  /**
   * Per-delivery entries for the batch, one per id in `deliveryIds`. The
   * daemon reads exactly two things off them: `delivery_id` (dedup) and
   * `presentation` (novel-remainder rebuild); the `delivered_up_to_seq`
   * sidecar it persists is `max(seq)` over them (#task:2106 removed the wire
   * scalar, so this array is the SOLE source). There is no stream axis and no
   * observed cursor here — #task:2095 deleted per-stream freshness and
   * #task:2097 deleted the wire fields that described it.
   *
   * Required (#task:2106): the sole production sender always populates it and
   * the hello equality gate admits one protocol generation only, so there is
   * no omitting peer. The entry set MUST match `deliveryIds` exactly — the
   * supervisor hard-rejects a drift at start time.
   */
  readonly deliveries: readonly RuntimeDelivery[];
  /**
   * Option A v2 (Cindy `UMGu36zoTDVuKWXi`): the unencoded text the
   * daemon will encode + write at consume time using its in-process
   * `handle.driver.encodeStdinMessage(text, handle.cliSessionId)`.
   * Held on the gate (not in a separate field) so there is exactly
   * ONE source of truth for the pending batch: clearing the gate
   * also drops the text. Stateful drivers (codex) consume only after
   * `driver.readyToEncode()` flips true; stateless drivers (claude)
   * consume immediately.
   */
  readonly text: string;
}

/**
 * A batch the daemon accepted into its per-handle mid-turn queue
 * (#task:1417 Stage 3, §11.4). Distinct from the single `startupDelivery`
 * slot: the queue holds batches that arrived at a ready handle which could
 * not take the write *right now* — mid-turn (`gated` driver) or behind a
 * write already in flight. Each is flushed through the SAME
 * `consumeStartupDelivery` machine at the next safe boundary.
 *
 * The DB rows stay `in_flight` (owner=conn) the whole time — a `deferred`
 * disposition is a custody transfer to this queue, NOT an ack. On the
 * owning connection's drop the entry is discarded (owner-scoped cleanup,
 * §11.4.1) with no `consumed`/`failed`, because nothing was written and the
 * backend rolls the rows back to pending on its own disconnect handler.
 *
 * LOAD-BEARING CUSTODY CONTRACT (applies to every present or future
 * release trigger, including a CLI pull):
 *   - The backend inbox lease is the sole durable authority; this queue is
 *     the only daemon-owned set eligible for alternate release arbitration.
 *   - A consumer may take only the scheduler's next bounded FIFO prefix.
 *     It must preserve ids, owner, text, and the per-delivery entries verbatim;
 *     it may not select from the middle by seq or split an opaque batch.
 *   - Auto-flush and any alternate consumer must choose one release winner
 *     under the same per-handle lifecycle lock. Waiting never holds the lock
 *     and does not itself take custody. While a selected prefix is reserved
 *     but its handoff is unresolved, no later prefix may be released.
 *   - Custody settles only after the chosen transport's concrete handoff
 *     proof point. Before that point, timeout/cancel/failure changes neither
 *     custody nor the published scalar; after it, emit one consumed settlement
 *     and write the `delivered_up_to_seq` sidecar from this batch's own
 *     `max(seq)` (a per-slot diagnostic/crash-recovery scalar, not a freshness
 *     or per-stream coverage boundary — see `writeLastDeliveredSeq`).
 *
 * `startupDelivery` is already release-in-progress and is therefore never
 * eligible to be reclaimed by an alternate consumer.
 */
export interface DeferredEntry {
  readonly conversationId: string;
  readonly actionWorkdir: string;
  readonly deliveryIds: readonly number[];
  readonly deliveries: readonly RuntimeDelivery[];
  readonly text: string;
  /** Custody owner is per id, independent of this presentation/ack group. */
  readonly owners: ReadonlyMap<number, string>;
}

export type ActivitySnippetKind = 'thinking' | 'text';

export interface ActivitySnippetBuffer {
  runId: string;
  kind: ActivitySnippetKind;
  text: string;
}

/** Bound for `RuntimeHandle.inputPayloadUuids` (#task:1557). Payloads
 *  whose lifecycle never echoes back (non-claude runtimes, dropped
 *  frames) would otherwise accumulate for the handle's lifetime. */
export const INPUT_PAYLOAD_UUID_CAP = 200;

/** Signal-only stale contract: the 409 exposes NO bodies and NO seq — under
 *  the #task:2095 RuntimeScope model staleness is "this runtime has an
 *  undelivered inbox row", not a per-stream sequence position. The daemon
 *  remembers only the event id (to correlate the agent's resolution) and the
 *  conversation the rejected send TARGETED. */
export interface PendingStaleEvent {
  id: string;
  /** The conversation the rejected send aimed at, from the 409's
   *  `target_conversation_id` (#task:2095 F17). Fed VERBATIM to the
   *  `stale-resolve` POST as its authorization subject — the daemon holds no
   *  address-resolution authority and never re-derives it from a memo. */
  targetConversationId: string;
}

/**
 * Short-lived pull-in-flight reservation (#task:1614; contract
 * #task:1606): the ONE new custody state the stale-receive pull adds.
 * Created under the lifecycle lock the instant the pull splices its
 * FIFO prefix out of `deferredQueue`; resolved exactly once by the
 * response transport handoff (`consumed` + the `delivered_up_to_seq` scalar
 * write on success, FIFO restore in place on pre-proof failure). While it is
 * non-null NO
 * later prefix may be released — `deliver()`'s admit arm and
 * `flushDeferred` both hold, preserving presentation FIFO.
 *
 * Reconnect visibility (review P1-1): a re-submit of a RESERVED id-set
 * (backend rolled the rows back on the old owner's disconnect and
 * re-drove them under a new connection) must NOT enqueue a second copy —
 * `enqueueDeferred` dedups against these entries and rebinds the
 * matching entry's owner in place, mirroring the queue rebind path.
 * `batch` is therefore an element-replaceable array (binding readonly).
 */
export interface PullReservation {
  readonly staleEventId: string;
  /** The spliced entries, verbatim, for a pre-proof FIFO restore.
   *  Elements may be owner-rebound by `enqueueDeferred` (see above). */
  readonly batch: DeferredEntry[];
  /** The combined batch (same `combineDeferredBatch` product the flush
   *  would have written) — the settlement's delivery_ids/coverage
   *  source and the response's verbatim text. */
  readonly combined: StartupDeliveryState;
  /**
   * Resolves once `settleStaleReceiveHandoff` has fully run (either
   * arm). This is the ONLY caller-visible happens-before for
   * settlement: the response frame can reach the peer before the
   * server's write callback runs in this process (the kernel gives no
   * cross-process ordering), so anything that must read
   * post-settlement state — the send freshness barrier — awaits THIS,
   * never the write callback's scheduling position (review P1-2 v2).
   */
  readonly settled: Promise<void>;
  /** Resolves {@link settled}; called exactly once by
   *  `settleStaleReceiveHandoff` after its locked section completes. */
  readonly markSettled: () => void;
}

/** Why a registered waiter's wait promise resolved. `arrival` prompts a
 *  locked re-evaluation; `cancelled` (the owning control connection
 *  closed) aborts the call — zero custody either way. */
export type PullWakeReason = 'arrival' | 'cancelled';

/**
 * Registered stale-receive waiter (#task:1614). At most one per handle;
 * bound to the exact stale event AND the exact call (`token`) — a
 * concurrent second pull is refused, and its cleanup can never clear a
 * waiter it does not own (review P1-3). `owner` is the issuing control
 * connection's delivery-owner token: that connection's close cancels
 * the waiter (review P1-4) — cancel-before-take only unregisters, it
 * never touches custody. Registration/removal happen under the
 * lifecycle lock; the WAIT itself never holds the lock.
 */
export interface PullWaiter {
  readonly staleEventId: string;
  /** Unique per stale-receive CALL, minted by the supervisor. */
  readonly token: number;
  /** Delivery-owner token of the issuing control connection. */
  readonly owner: string;
  readonly wake: (reason: PullWakeReason) => void;
}

export interface RuntimeHandle {
  /** Stable session identity. Never reconstructed from the current target. */
  readonly runtimeScope: RuntimeScope;
  /** Stable runtime cwd selected by the same scope authority. */
  readonly cwd: string;
  /** Current/focus delivery target. Mutable only for agent-scoped sessions. */
  conversationId: string;
  readonly agentId: string;
  /** Backend-issued runtime custody generation for this exact child. */
  readonly runtimeGeneration: number;
  /** OS process id. Stable for the lifetime of the handle; `null` is
   *  not legal — if we don't have a pid the handle never gets
   *  created. */
  readonly pid: number;
  /** Wall-clock ms at spawn. Combined with `pid` to derive a stable
   *  `runId` for `runtime_state` events. */
  readonly startedAt: number;
  /** Live driver instance (per-spawn factory output). Holds any
   *  per-handle protocol counters codex/cursor accumulate. */
  readonly driver: Driver;
  /** The actual child process. Owned by the handle; cleanup releases
   *  stdio streams + removes listeners. */
  readonly child: ChildProcess;
  readonly lastDeliveredSeqPath: string;
  /** True once an admitted input batch is expected to be driving the
   *  current model action. Cleared on turn_end/runtime resolution.
   *  This covers the pre-first-output window where runState is still
   *  null but the model is already thinking. */
  inputTurnActive: boolean;
  /** #task:1558 Commit B (`next-turn-batch`, docs/runtime-input-design.md
   *  §3.4): set the moment a payload is written to the runtime, cleared
   *  only by the parsed terminal `turn_end` (or runtime resolution).
   *  Covers the write→first-activity gap where `inputTurnActive` alone
   *  would let a second payload slip in as another future turn. */
  nextTurnReserved: boolean;
  /** Wall-clock ms when `nextTurnReserved` was last armed; null while not
   *  reserved. Diagnostics only (§3.4 fail-closed follow-up): a lost
   *  `turn_end` keeps the reservation — and the deferred queue — held
   *  forever BY DESIGN (never write without a proven boundary); this
   *  timestamp lets the defer path emit a stall diagnostic so an operator
   *  can see the held state instead of silent starvation. */
  nextTurnReservedAt: number | null;
  /** One-shot latch so the stall diagnostic logs once per reservation,
   *  not once per deferred delivery. Reset when the reservation clears. */
  reservationStallWarned: boolean;
  /** Pending stall-check timer (#task:1563 Cindy P1-1). Armed when an
   *  entry defers under a held reservation so the diagnostic fires on
   *  TIMEOUT — a non-empty queue with no further inflow must still get
   *  its warn. Cleared on turn_end / re-arm / resolution / exit;
   *  unref'd; fires at most the warn, never a write. */
  reservationStallTimer: NodeJS.Timeout | null;
  /** Monotonic count of parsed terminal `turn_end`s. Snapshot before a
   *  stdin write and compare after: a write completion may only arm
   *  `nextTurnReserved` if no terminal crossed the await window —
   *  otherwise a fast runtime's already-finished turn would be
   *  overwritten back into "reserved" and the queue held forever
   *  (#1558 final-gate P1-1). */
  turnEndCount: number;
  /** The single stale-send event this runtime is holding, or `null`. At most
   *  ONE is pending per runtime: the carrier is the handle, which already IS
   *  the runtime scope (`runtimeScope` above), so there is no per-stream key.
   *  Armed when a send is rejected with `message_stale`; cleared when the agent
   *  resolves it or any later send commits. */
  pendingStaleEvent: PendingStaleEvent | null;
  /** Absolute per-origin action directories whose existence was verified for
   * this live aggregate. Avoids turning every delivery into an async fs gap;
   * a new origin is still ensured before daemon custody mutates. */
  ensuredActionWorkdirs: Set<string>;
  /** #task:1557 Phase 1 instrument: payload uuid → the delivery ids that
   *  payload carried, written at encode time and read/pruned when the
   *  runtime echoes `command_lifecycle` frames (claude). Observation
   *  only — never drives run-state or freshness in Phase 1. Bounded:
   *  oldest entries are dropped past `INPUT_PAYLOAD_UUID_CAP`. */
  inputPayloadUuids: Map<
    string,
    {
      deliveryIds: number[];
      mintedAt: number;
    }
  >;
  /** Current execution phase. Updated in-place by Supervisor as
   *  the process moves through its lifecycle. */
  phase: RuntimePhase;
  /** Output flow-control flag. `true` while `pauseOutput()` is in
   *  effect; raw stdout/stderr chunks are dropped silently while
   *  paused (no buffer-and-replay in 3a — 3b will revisit). */
  outputPaused: boolean;
  /** Final exit code observed via the child's `exit` event, or `null`
   *  if the process exited via signal without a code. Populated when
   *  phase transitions to `'exited'`; before then it is `null`. */
  exitCode: number | null;
  /** Whether the most recent transition to `draining` was caused by
   *  an intentional stop() / clearSession() / stopAll() request. Drives
   *  the `intentional` field on `runtime_exit`. Set true on stop;
   *  defaults to false (crash interpretation). */
  intentionalExit: boolean;
  /** Pending SIGTERM → SIGKILL timer, if a hard stop is in progress.
   *  Cleared when the child exits or the timer fires. */
  sigkillTimer: NodeJS.Timeout | null;
  /** Bounded reclaim after a post-init fatal error. */
  fatalReclaimTimer: NodeJS.Timeout | null;

  // ── 3b additions ─────────────────────────────────────────────────

  /** Step-0 startup-delivery gating; see `StartupDeliveryState`. */
  startupDelivery: StartupDeliveryState | null;
  /** Sub-relation of `startupDelivery !== null` (Cindy
   *  `GMbjEkGUHrB1BBMe`): when true, the stdin write succeeded but
   *  the daemon is holding the consumed-emit until the runtime
   *  confirms via `session_init` (mirrors CM's
   *  `initPendingInboxIds` defer-markDelivered path for
   *  `requires_first_message` drivers). NOT a second source of
   *  truth — `runtime_deliver` / `maybeEmitReadyForDrain` still gate
   *  ONLY on `startupDelivery !== null`. Cleared in the same
   *  critical section as `startupDelivery` via `clearStartupGate`,
   *  so the `(null, true)` interleaving is impossible. */
  startupAwaitingInit: boolean;
  /** #task:2210 lifecycle-start admission gate, for admission-proof drivers
   *  (claude / codex / qoder≥1.1.17). `null` means the handle is NOT in a
   *  lifecycle gate; a non-null value carries the whole gate state as one
   *  unit, so illegal combinations (a uuid without the flags, a `started`
   *  proof recorded while no gate is armed) are simply unrepresentable rather
   *  than held together by convention. Armed BEFORE the stdin write (a fast
   *  runtime can echo `started` the instant it reads the payload, and
   *  `dispatchParsedLine` runs OUTSIDE the lifecycle lock, so the gate must
   *  already be held before the write) and stays held until the batch settles.
   *
   *  Settlement is the CONJUNCTION `startedObserved && writeConfirmed`, never
   *  either half alone. A matching `started` only records the proof; a
   *  successful write only records write-readiness; whichever becomes true
   *  second settles exactly once. This is what keeps a transport-level
   *  stdin-write ack (the anti-proof #task:2095 regressed to) from ever
   *  settling, AND keeps an early `started` from settling before the write has
   *  actually succeeded (which would allow consumed+failed double-settle, a
   *  premature deferred-flush, and continuation on a cleared gate).
   *  `session_init` proves the process is ready but NEVER counts as this
   *  payload's admission.
   *
   *  Fields:
   *   - `uuid` — the `payloadUuid` the daemon minted for the held batch. The
   *     `input_lifecycle` handler records the proof ONLY on a `started` frame
   *     whose `commandUuid` matches this exactly; a stale/duplicate/unknown
   *     uuid is an idempotent no-op, so a late frame from a prior payload can
   *     never admit the current one.
   *   - `startedObserved` — first conjunction half; a matching `started` was
   *     seen. Recording it NEVER settles or clears on its own; an early
   *     `started` parks here so a subsequent write FAILURE settles `failed`
   *     only, zero consumed.
   *   - `writeConfirmed` — second half; the stdin write reached its FULL
   *     success boundary (the same boundary the write-success path settles on
   *     today — including `writeLastDeliveredSeq`), never a partial/early ack.
   *
   *  Sub-relation of `startupDelivery !== null` (the gate stays closed, so the
   *  batch cannot be re-consumed and a child exit before settlement fails it
   *  back to pending — the consumer-death rollback). Cleared with the gate in
   *  `clearStartupGate` so no `(startupDelivery=null, startupAdmission!=null)`
   *  interleaving appears. Mutually exclusive with `startupAwaitingInit`. */
  startupAdmission: {
    uuid: string;
    startedObserved: boolean;
    writeConfirmed: boolean;
    /** Explicit runtime refusal; settled after the write boundary. Never
     * inferred from a timeout, and never overrides observed admission. */
    rejected: boolean;
  } | null;
  /** #task:2210 the child's stdout is the admission channel. `true` once that
   *  stream reaches EOF (`'end'`) / `'close'`. Process `exit` does NOT imply
   *  it — Node delivers buffered stdout data up to the stream's own end, which
   *  can land AFTER `exit`. So the pending lifecycle-start gate's final
   *  failed/rollback is deferred until this flips (see `onStdoutDrained`); a
   *  matching `started` that raced the exit is still parsed and can settle
   *  consumed before then. Never time-inferred. */
  stdoutEnded: boolean;
  /** #task:2210 set by `onChildExit` when it defers a still-pending
   *  lifecycle-start gate's finalization to stdout drain, carrying the reason
   *  (`child_exit` / `init_watchdog`) computed at exit time. `onStdoutDrained`
   *  consumes it to emit the single failed/rollback once the channel is truly
   *  closed and no proof arrived; `null` otherwise. */
  startupGateExitReason: 'child_exit' | 'init_watchdog' | null;
  /** In-flight latch for `consumeStartupDelivery` (#task:1563). The
   *  consume awaits `writeStdinPayload`, and `dispatchParsedLine` runs
   *  synchronously OUTSIDE the lifecycle lock — a `session_init` parsed
   *  during that await window used to re-enter the consume via
   *  `tryConsumeStartupDelivery` (gate still non-null, awaitingInit not
   *  yet set) and write the SAME batch to stdin twice → duplicate
   *  exposure + double consumed. Set for the duration of one consume
   *  attempt; re-entries no-op while it holds. */
  startupConsumeInFlight: boolean;
  /** Whether `runtime_ready_for_drain` has been emitted for this run.
   *  Step-0 mutex (Cindy): we emit at most one per (conv,agent,run);
   *  re-spawn resets via a fresh handle. */
  readyForDrainEmitted: boolean;
  /** True once a parsed `session_init` event has been observed.
   *  Drives the delivery gate's `not_initialized` arm and the future
   *  3c init-watchdog cancellation. */
  initObserved: boolean;
  /** Cached cli session id from the latest `session_init`. Backend
   *  re-uses it on resume; daemon holds it so delivery can
   *  surface its presence (3c gating). */
  cliSessionId: string | null;
  /** Residue from line-splitting stdout chunks. The daemon hands
   *  driver.parseLine complete lines only; bytes not terminated by
   *  `\n` accumulate here until the next chunk arrives or the child
   *  exits (a half-line at exit is forwarded as a trailing parse). */
  stdoutLineBuffer: string;
  /** Per-handle AgentRunState. Initialized lazily when the first
   *  parsed event arrives (or at start()) so `status()` always has
   *  a meaningful `run_state` to project. */
  runState: AgentRunState | null;
  /** Rolling snippet buffer for visible agent activity text. Scoped
   *  by runId so a new run cannot inherit text from the previous one
   *  on the same (conversation, agent) handle. */
  activitySnippetBuffer: ActivitySnippetBuffer | null;
  /** Per-handle tool-call bookkeeping so `tool_output` knows when to
   *  drop status back to `thinking`. Mirrors CM's `handle.busy`. */
  busy: { outstandingToolUses: number; compacting: boolean };
  /** Daemon-held mid-turn delivery queue (#task:1417 Stage 3, §11.4).
   *  Batches accepted at a ready handle that could not take the write now
   *  (mid-turn `gated`, or a write already in flight). Flushed through
   *  `consumeStartupDelivery` at the next safe boundary — `gated` drains
   *  the whole queue combined into one write, `direct` flushes FIFO one at
   *  a time. Volatile by contract: discarded on the owning connection's
   *  drop (owner-scoped) and failed on runtime exit. */
  deferredQueue: DeferredEntry[];
  /** True while a `flushDeferred` is in flight for this handle. Prevents a
   *  second boundary-reopen / post-write cascade from racing a flush.
   *  Mirrors the backend coordinator's `flushing` guard. */
  flushingDeferred: boolean;
  /** Pull-in-flight reservation (#task:1614); see {@link PullReservation}.
   *  Non-null blocks BOTH `flushDeferred` and `deliver()`'s admit arm
   *  from releasing any later prefix. */
  pullReservation: PullReservation | null;
  /** Registered stale-receive waiter (#task:1614); see {@link PullWaiter}.
   *  At most one; its presence makes a second concurrent pull fail with
   *  `pull_already_active`. */
  pullWaiter: PullWaiter | null;

  // ── 3c-b additions ───────────────────────────────────────────────

  /** Cached original `RuntimeStartParams` minus `startup_delivery`,
   *  used by `internalRespawnForRetry` so a watchdog-triggered
   *  re-spawn re-runs `start()` with the same `options` and the same
   *  (conv, agent). startup_delivery is intentionally NOT carried —
   *  the original payload was either consumed or failed before the
   *  timeout fired, and the daemon cannot reliably replay it. Backend's
   *  inbox path handles redelivery via `runtime_deliver` on the new
   *  handle. */
  spawnParams: RuntimeStartParams;

  /** Init watchdog state — three timers (silence / pre-protocol cap /
   *  post-protocol) plus the cross-cutting bookkeeping. `armed`
   *  protects against double-arming on the deferred-init path
   *  (claude empty-inbox: arm only when the first `runtime_deliver`
   *  actually writes). All
   *  three timers are independently cleared once `initObserved` flips
   *  (session_init parsed). */
  initWatchdogArmed: boolean;
  initStartedAt: number;
  initProtocolObserved: boolean;
  initTimedOut: boolean;
  initTimeoutReason: InitTimeoutReason | null;
  initLivenessSilenceTimer: NodeJS.Timeout | null;
  initPreProtocolCapTimer: NodeJS.Timeout | null;
  initPostProtocolTimer: NodeJS.Timeout | null;

  /** 0-indexed attempt counter for this (conv, agent). `0` = first
   *  user-initiated start; daemon-internal retry respawns bump it.
   *  The cooldown gate in `start()` only fires for `initAttempt === 0`
   *  per Cindy plan v2 issue #3. */
  initAttempt: number;
  /** Decided at the moment `failInitWatchdog` fires: whether the
   *  child-exit handler should respawn (`true`) or finalize as a
   *  terminal init failure (`false` → may also engage cooldown). */
  initRetryPlanned: boolean;

  /** Compaction watchdog. Set when `compaction_started` parsed event
   *  arrives; cleared on `compaction_finished` or on exit. When it
   *  fires we emit `runtime_compaction_stale` (observational only —
   *  does NOT kill the child) and stack `compactionStale: true` onto
   *  the runState while preserving `compacting: true` (Cindy plan
   *  v2 issue #4). */
  compactionWatchdog: NodeJS.Timeout | null;
}
