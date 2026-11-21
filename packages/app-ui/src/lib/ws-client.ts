import type { EventSocket } from "@silverhand/client";
import { getPlatform } from "@/platform";
import type {
  ClientFrame,
  EventFrame,
  ResumeFrame,
  ServerFrame,
  SubscribedFrame,
  SubscribeFrame,
  TransportEpoch,
  TransportSeq,
} from "@silverhand/shared";
import { advanceTransportSeq, parseConversationScope } from "@silverhand/shared";
import { getBackendInfo } from "./api";

const EVENT_WS_PATH = "/api/ws/events";
const RECONNECT_DELAYS_MS = [500, 1_000, 2_000, 4_000, 8_000, 30_000] as const;

export type WsClientStatus =
  | "idle"
  | "connecting"
  | "authenticating"
  | "connected"
  | "disconnected";

export type WsClientListener = (frame: ServerFrame) => void;
export type WsStatusListener = (status: WsClientStatus) => void;

export interface WsScopeSubscription {
  unsubscribe(): void;
}

interface ResumePosition {
  epoch: TransportEpoch;
  lastSeq: TransportSeq;
}

type EventWsDiagnosticKind =
  | "socket_open"
  | "socket_close"
  | "authenticated"
  | "subscribed"
  | "message_event"
  | "live_gap"
  | "resume_sent"
  | "snapshot_required"
  | "orphan_frame"
  | "markSatisfied_no_floor"
  | "satisfied_pending_overflow";

interface EventWsDiagnostic {
  kind: EventWsDiagnosticKind;
  scope?: string;
  eventKind?: string;
  seq?: number;
  lastSeq?: number;
  expectedSeq?: number;
  epoch?: string;
  reason?: string;
  code?: number;
  wasClean?: boolean;
}

/**
 * Upper bound on a scope's pending-satisfaction backlog (#1261). A satisfied-
 * but-non-contiguous set this deep means the gap-filling seq has (or will) age
 * out of the server replay buffer (also 200 entries) — the scope can then only
 * recover its floor via a `snapshot_required` re-cold-start. Past it the newest
 * seq is forgotten: the floor stays pinned below the gap either way, and a
 * forgotten seq is satisfied again when a resume replays it.
 */
const SATISFIED_PENDING_CAP = 200;

/**
 * @internal — exported for unit tests so they can construct a fresh
 * client without sharing state with the module-level `eventWsClient`
 * singleton. Production code should always use `eventWsClient` (or the
 * top-level `subscribe*` helpers).
 *
 * ──────────────────────── Transport cursor model ────────────────────────
 * Per scope the client tracks TWO water marks, not one (docs/event-ws-transport-v1.md
 * §4.1). They exist only to serve different jobs the old single cursor conflated:
 *
 * - `receivedPositions[scope]` — the highest seq contiguously RECEIVED on the
 *   socket. Advanced in `acceptEventFrame` on every in-order desired-scope
 *   frame. Serves wire-continuity / forward-gap detection ONLY. It is NOT a
 *   resume floor.
 * - `resumeFloorPositions[scope]` — the resume FLOOR (what `resume` replays from).
 *   For a CONTENT-GATED scope (`conversation:<id>`, the message stream) it is
 *   advanced ONLY by `markSatisfied` (a resume-safe disposition) or `commitSnapshotBaseline`
 *   (an apply-settled snapshot re-cold-start) — never by frame receipt. For every
 *   OTHER scope (viewer / workspace / app / `conversation-side:`) there is no
 *   content-apply seam, so the floor auto-advances with `received` (the pre-P2
 *   single-cursor behavior, preserved bit-for-bit).
 *
 * The two coincide for auto-advance scopes and while a content reader applies
 * every frame; they diverge when a content-gated scope receives a frame that
 * writes no warm content (no reader / off-window no-op) — `received` advances so
 * the stream keeps flowing and gaps stay detectable, the resume floor stays pinned so a
 * resume re-delivers the un-applied frame (idempotent) rather than skipping it.
 */
export class EventWsClient {
  private ws: EventSocket | null = null;
  private socketGeneration = 0;
  private status: WsClientStatus = "idle";
  private reconnectAttempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private shouldReconnect = true;
  private readonly frameListeners = new Set<WsClientListener>();
  private readonly statusListeners = new Set<WsStatusListener>();
  private readonly desiredScopes = new Map<string, number>();
  /** Wire-continuity / gap-detection high-water (see class docstring). */
  private readonly receivedPositions = new Map<string, ResumePosition>();
  /** Resume floor (see class docstring). The ONLY thing `resume` replays from. */
  private readonly resumeFloorPositions = new Map<string, ResumePosition>();
  /**
   * Per-scope pending-SATISFACTION queue (#1261) — the option-2 queue the
   * original strict-contiguous floor-advance note (see `markSatisfied` below)
   * deferred. Holds satisfied seqs that are not yet contiguous with the resume
   * floor, waiting for the gap below them to fill. Stamped with the epoch it
   * belongs to (a newer server epoch resets it). `markSatisfied` drains it into
   * `resumeFloorPositions` contiguously, so a cursor-inert frame (an off-window
   * vacuous-satisfy, or a derived-invalidate whose refetch settled) no longer
   * PINS the floor behind it. It is per-connection: cleared in
   * `clearConnectionState` (a reconnect replays from the floor and re-satisfies),
   * and per-scope on a floor reposition (`commitSnapshotBaseline` / `resume`).
   * Bounded-diagnostic on overflow.
   */
  private readonly satisfiedPending = new Map<
    string,
    { epoch: TransportEpoch; seqs: Set<TransportSeq> }
  >();
  /**
   * Server-stamped transport baseline for an in-flight `snapshot_required`
   * re-cold-start on a content-gated scope (§6). Recorded on receipt but NOT
   * committed to `resumeFloorPositions` until the ingestor signals its REST
   * reconcile settled via `commitSnapshotBaseline` (apply-gated). While present,
   * live frames for the scope are buffered (`pendingBuffers`) rather than
   * applied, exactly like cold-start.
   */
  private readonly pendingBaselines = new Map<string, ResumePosition>();
  /** Frames `> pendingBaselines[scope].lastSeq` held during a snapshot
   *  re-cold-start, drained in seq order once the baseline commits. */
  private readonly pendingBuffers = new Map<string, EventFrame[]>();
  private readonly confirmedScopes = new Set<string>();
  /**
   * Scopes with a forward-gap recovery in flight. When a live `event`
   * frame arrives beyond the next expected seq we hold the received-water,
   * ask the server to replay from the applied floor, and latch the scope here
   * so every further gapped frame coalesces into the one in-flight resume
   * instead of firing a redundant one. The latch drops as soon as the in-order
   * replay delivers the next contiguous seq (see `acceptEventFrame`).
   */
  private readonly recovering = new Set<string>();
  /**
   * Live `subscribeViewerScope()` handles. Each holds a deferred inner
   * subscription that materialises against the canonical
   * `viewer:{resolvedViewerId}` scope once the server's `authenticated`
   * frame arrives. Keeping the indirection out of `subscribe()` itself
   * means the position maps / `desiredScopes` / `confirmedScopes` only ever
   * see the canonical key, so `resubscribeDesiredScopes()` naturally
   * carries the viewer's last seq across reconnects.
   */
  private readonly viewerSubscribers = new Set<ViewerSubscriber>();
  /**
   * Server-resolved viewer id from the `authenticated` frame. Used by
   * `getResolvedViewerScope()` so callers can match incoming events
   * scoped to `viewer:{id}` without needing to know the id up front
   * (clients subscribe via the `viewer:self` alias and the server
   * canonicalizes both directions).
   */
  private resolvedViewerId: string | null = null;

  connect(): void {
    this.shouldReconnect = true;
    if (
      this.status === "connecting" ||
      this.status === "authenticating" ||
      this.status === "connected"
    ) {
      return;
    }
    if (this.ws && (this.ws.readyState === 1 || this.ws.readyState === 0)) {
      return;
    }
    void this.openSocket();
  }

  disconnect(): void {
    this.socketGeneration += 1;
    this.shouldReconnect = false;
    this.clearReconnectTimer();
    const ws = this.ws;
    this.ws = null;
    this.clearConnectionState();
    this.resolvedViewerId = null;
    // Detach viewer wrappers so the canonical `viewer:<id>` subscription
    // state — desiredScopes refcount + confirmed — is released. The
    // resume floor (`resumeFloorPositions`) and received-water are deliberately
    // retained so a later resume can replay; wrappers re-attach on the next
    // connect() → authenticated cycle if the wrapper handle is still live.
    for (const sub of Array.from(this.viewerSubscribers)) {
      sub.detach();
    }
    if (ws && ws.readyState !== 3) {
      ws.close(1000, "client_disconnect");
    }
    this.setStatus("disconnected");
  }

  subscribe(scope: string): WsScopeSubscription {
    const previousCount = this.desiredScopes.get(scope) ?? 0;
    this.desiredScopes.set(scope, previousCount + 1);
    this.connect();
    if (previousCount === 0 || (this.status === "connected" && !this.confirmedScopes.has(scope))) {
      this.sendSubscribe(scope);
    }

    return {
      unsubscribe: () => {
        const count = this.desiredScopes.get(scope) ?? 0;
        if (count <= 1) {
          this.desiredScopes.delete(scope);
          this.confirmedScopes.delete(scope);
          this.recovering.delete(scope);
          // A recovery in flight is abandoned (no consumer to reconcile).
          this.pendingBaselines.delete(scope);
          this.pendingBuffers.delete(scope);
          // Drop the pending-satisfaction backlog too (#1261, Cindy's blocker):
          // it is the temporary fact "these seqs' cache/display paths were
          // satisfied", not a durable floor. With no listener the caches behind
          // it may be GC'd / removeQueries'd / overwritten by a later
          // authoritative fetch, so the fact cannot survive the no-listener
          // window. A same-socket resubscribe resumes from the retained floor and
          // re-satisfies from the replay — carrying a stale entry over would let
          // the floor drain past a seq the replay has not re-satisfied this round
          // (same hazard as reconnect, just the same-socket path). Floor +
          // received are retained (below) for the #1251 replay.
          this.satisfiedPending.delete(scope);
          // Retain the resume floor (`resumeFloorPositions`) AND the received-water
          // — do NOT delete them. The retained floor is what lets the next
          // resubscribe resume and replay the no-listener gap (#task:1251); the
          // received-water lets gap detection continue seamlessly. Cost is a
          // scope string + a few primitives, negligible even at 10× the
          // conversations a session ever opens. A bounded LRU is deliberately
          // NOT used: a bare evict re-creates the no-listener gap (an evicted
          // scope resubscribes with no `resume`, the server has no reason to
          // emit `snapshot_required`, and the `subscribed` ack baselines to head
          // — silently skipping gap traffic). If memory ever forces a bound it
          // must carry a tombstone → forced snapshot, never a bare delete (see
          // docs/event-ws-transport-v1.md §4).
          this.sendIfConnected({ type: "unsubscribe", scope });
        } else {
          this.desiredScopes.set(scope, count - 1);
        }
      },
    };
  }

  /**
   * Send a `subscribe` for the scope and, when we hold a retained resume
   * floor for it, a paired `resume` from that floor (#task:1251). The resume
   * makes the server replay events published while no listener was
   * attached; without it the `subscribed` ack would re-baseline to head
   * and skip them. No-op until connected — a disconnected subscribe is
   * (re)issued, floor and all, by `resubscribeDesiredScopes()` on the
   * next `authenticated` frame.
   */
  private sendSubscribe(scope: string): void {
    if (!this.sendIfConnected({ type: "subscribe", scope })) return;
    const floor = this.resumeFloorPositions.get(scope);
    if (floor) this.sendResume({ [scope]: floor });
  }

  /**
   * Subscribe to the resolved viewer's private scope. Returns a handle
   * that immediately participates in refcounting on the canonical
   * `viewer:{id}` scope when the viewer id is already resolved, and
   * otherwise materialises on the next `authenticated` frame. The
   * caller never sees the alias key — it stays an over-the-wire detail
   * so the position maps / `desiredScopes` / `confirmedScopes` are
   * canonical-only and reconnect resume frames can carry the viewer
   * scope's last seq.
   */
  subscribeViewerScope(): WsScopeSubscription {
    const sub = new ViewerSubscriber(this);
    this.viewerSubscribers.add(sub);
    this.connect();
    sub.attach();
    return {
      unsubscribe: () => {
        sub.detach();
        this.viewerSubscribers.delete(sub);
      },
    };
  }

  resume(scope: string, position: ResumePosition): void {
    this.recovering.delete(scope);
    this.resumeFloorPositions.set(scope, position);
    this.receivedPositions.set(scope, position);
    // Floor forcibly repositioned: any pending-satisfaction backlog for the scope
    // is relative to the old floor and no longer meaningful (#1261).
    this.satisfiedPending.delete(scope);
    this.sendResume({ [scope]: position });
  }

  /**
   * Advance a CONTENT-GATED scope's resume floor once the ingestor has driven a
   * frame to a resume-safe DISPOSITION (Stage-2 #1255 §4.1 + #1261). "Satisfied"
   * generalizes "content-applied": a frame is satisfied when EVERY path by which
   * its content / message-derived projections could reach the screen has either
   * committed the frame's effect OR been forced through an authoritative fetch
   * (no warm displayable cache left). The ingestor computes that disposition
   * (per-frame `Promise.allSettled` over all active effects, all-fulfilled) and
   * calls this exactly once per satisfied frame. It MUST NOT call for a dispatch
   * still in flight, only partially applied, or one that leaves a warm inactive
   * cache displayable — that would re-open the #1251 / #1255 skip hole.
   *
   * The advance is a PENDING-SATISFACTION QUEUE (#1261 — the option-2 this note
   * previously deferred): the satisfied seq is recorded, then the floor drains
   * contiguously from `floor+1` through every already-satisfied seq. An
   * out-of-order satisfied seq (a later frame satisfied while an earlier seq is
   * still pending) is remembered and absorbed the moment the gap fills — so a
   * cursor-inert frame (an off-window vacuous-satisfy, or a derived-invalidate
   * whose refetch settled) no longer PINS the floor behind it. That is what makes
   * the (c) reply-count model healthy: every mainline-only reply is cursor-inert,
   * and without the queue each would pin the floor until snapshot.
   *
   * Two invariants are load-bearing and MUST NOT be relaxed:
   *  - NEVER ESTABLISHES a floor. A satisfied seq with no same-epoch floor
   *    (missing / stale-epoch) is dropped with a diagnostic; only the
   *    `subscribed` ack and `commitSnapshotBaseline` may establish one (they
   *    reflect the server head). Establishing here from an arbitrary satisfied
   *    seq would let the next resume start above un-received seqs, silently
   *    skipping them (the #1251 skip-unseen hole). The floor self-heals when the
   *    next ack / snapshot baselines it.
   *  - COMMIT-GATED, not dispatch-gated (initiated ≠ committed). The caller must
   *    have COMMITTED every effect before calling; this method trusts that and
   *    only sequences the floor. A seq at-or-below the floor is an idempotent
   *    no-op (a replay re-satisfy).
   *
   * Non-content-gated scopes have no content-apply seam and are ignored here
   * (their floor auto-advances with received in `acceptEventFrame`).
   */
  markSatisfied(scope: string, seq: TransportSeq, epoch: TransportEpoch): void {
    if (!isContentGated(scope)) return;
    const floor = this.resumeFloorPositions.get(scope);
    if (!floor || floor.epoch !== epoch) {
      recordEventWsDiagnostic({ kind: "markSatisfied_no_floor", scope, seq, epoch });
      return;
    }
    // At-or-below the floor: already covered (a replay re-satisfy) → idempotent.
    if (seq <= floor.lastSeq) return;

    // Record into the per-scope+epoch satisfied set, then drain contiguously
    // from the floor, absorbing any already-satisfied later seqs. Each step to
    // the next contiguous seq goes through `advanceTransportSeq` — the sole
    // legal +1 out for a TransportSeq — so the typed floor never decays to a
    // bare number arithmetic.
    const pending = this.satisfiedPendingFor(scope, epoch);
    pending.add(seq);
    let advancedTo = floor.lastSeq;
    let candidate = advanceTransportSeq(advancedTo);
    while (pending.delete(candidate)) {
      advancedTo = candidate;
      candidate = advanceTransportSeq(candidate);
    }
    if (advancedTo > floor.lastSeq) {
      this.resumeFloorPositions.set(scope, { epoch, lastSeq: advancedTo });
    }
    if (pending.size > SATISFIED_PENDING_CAP) {
      pending.delete(Math.max(...pending) as TransportSeq);
      recordEventWsDiagnostic({
        kind: "satisfied_pending_overflow",
        scope,
        seq,
        lastSeq: advancedTo,
        epoch,
      });
    }
  }

  /**
   * Return the per-scope satisfied set for `epoch`, resetting it if the stored
   * entry belongs to a superseded epoch (a fresh server epoch invalidates any
   * carried-over pending seqs).
   */
  private satisfiedPendingFor(scope: string, epoch: TransportEpoch): Set<TransportSeq> {
    const existing = this.satisfiedPending.get(scope);
    if (existing && existing.epoch === epoch) return existing.seqs;
    const seqs = new Set<TransportSeq>();
    this.satisfiedPending.set(scope, { epoch, seqs });
    return seqs;
  }

  /**
   * Commit an in-flight `snapshot_required` re-cold-start for a content-gated
   * scope (Stage-2 #1255 §6). The ingestor calls this once its REST reconcile
   * for every target in the scope has SETTLED (its `Promise.allSettled` join is
   * all-fulfilled) — the transport→ingestor bounded-context boundary: the
   * renderer signals "content committed", the transport translates that into
   * advancing the floor to the server-stamped transport seq it stashed in
   * `pendingBaselines`.
   *
   * Apply-gated and epoch-checked: a stale-epoch or no-pending signal is
   * discarded (a newer epoch superseded the recovery, or a double-signal). On a
   * match the floor + received-water jump to the pending baseline, and any live
   * frames buffered past it during recovery drain in seq order — released to the
   * ingestor as fresh `live` frames.
   */
  commitSnapshotBaseline(scope: string, epoch: TransportEpoch): void {
    const pending = this.pendingBaselines.get(scope);
    if (!pending || pending.epoch !== epoch) return;
    this.resumeFloorPositions.set(scope, { epoch: pending.epoch, lastSeq: pending.lastSeq });
    this.receivedPositions.set(scope, { epoch: pending.epoch, lastSeq: pending.lastSeq });
    this.pendingBaselines.delete(scope);
    this.recovering.delete(scope);
    // The baseline jumped the floor: any pending-satisfaction backlog is now at
    // or below it (subsumed) or from before the re-cold-start. Drop it; the
    // buffered frames drained below re-satisfy from the new baseline (#1261).
    this.satisfiedPending.delete(scope);
    const buffered = this.pendingBuffers.get(scope) ?? [];
    this.pendingBuffers.delete(scope);
    // Drain buffered live frames (> baseline seq) in transport-seq order. They
    // are genuinely new (never applied) → the `acceptEventFrame` contiguous path
    // accepts them and the ingestor applies + `markSatisfied`s each.
    for (const frame of buffered.sort((a, b) => a.seq - b.seq)) {
      if (this.acceptEventFrame(frame)) this.dispatch(frame);
    }
  }

  onFrame(listener: WsClientListener): () => void {
    this.frameListeners.add(listener);
    return () => this.frameListeners.delete(listener);
  }

  onStatus(listener: WsStatusListener): () => void {
    this.statusListeners.add(listener);
    listener(this.status);
    return () => this.statusListeners.delete(listener);
  }

  getStatus(): WsClientStatus {
    return this.status;
  }

  /**
   * The server-resolved viewer id from the most recent `authenticated`
   * frame, or null while the connection is establishing. Returned by
   * `getResolvedViewerScope()` and consumed by `isCurrentViewerScope()`.
   */
  getResolvedViewerId(): string | null {
    return this.resolvedViewerId;
  }

  private async openSocket(): Promise<void> {
    const generation = ++this.socketGeneration;
    this.clearReconnectTimer();
    this.setStatus("connecting");
    let url: string;
    try {
      const info = await getBackendInfo();
      url = eventWsUrl(info.url);
    } catch {
      if (generation === this.socketGeneration) this.scheduleReconnect();
      return;
    }

    if (!this.shouldReconnect || generation !== this.socketGeneration) return;
    let ws: EventSocket;
    try {
      ws = getPlatform().client.openEventSocket(url);
    } catch {
      this.scheduleReconnect();
      return;
    }
    this.ws = ws;
    this.clearConnectionState();

    ws.addEventListener("open", () => {
      if (this.ws !== ws) return;
      recordEventWsDiagnostic({ kind: "socket_open" });
      this.setStatus("authenticating");
    });

    ws.addEventListener("message", (raw) => {
      if (this.ws !== ws) return;
      const frame = parseServerFrame(raw.data);
      if (!frame) return;
      this.handleFrame(frame);
    });

    ws.addEventListener("close", (ev) => {
      if (this.ws !== ws) return;
      recordEventWsDiagnostic({
        kind: "socket_close",
        code: ev.code,
        wasClean: ev.wasClean,
      });
      this.ws = null;
      this.clearConnectionState();
      this.setStatus("disconnected");
      if (this.shouldReconnect) this.scheduleReconnect();
    });

    ws.addEventListener("error", () => {
      // `close` follows for network errors; keep the state transition in one path.
    });
  }

  /**
   * Reset the per-connection transient state (subscription acks, in-flight
   * recoveries, snapshot re-cold-start buffers). Deliberately RETAINS the
   * received-water + resume floor so a reconnect can resume/replay. Called on
   * every socket open/close/disconnect so a snapshot recovery never leaks its
   * buffer across connections (it re-arms via a fresh `snapshot_required`).
   */
  private clearConnectionState(): void {
    this.confirmedScopes.clear();
    this.recovering.clear();
    this.pendingBaselines.clear();
    this.pendingBuffers.clear();
    // The pending-satisfaction queue (#1261) is per-connection: a reconnect
    // resumes from the (retained) floor and re-satisfies every replayed frame, so
    // the backlog rebuilds itself; carrying it across would only risk stale-epoch
    // seqs. Received-water + resume floor are RETAINED (above) so the reconnect
    // can resume/replay.
    this.satisfiedPending.clear();
  }

  private handleFrame(frame: ServerFrame): void {
    // Orphan-scope gate (#task:1251). Retained through Stage-2 P2: its
    // preconditions for removal are not all met (a view hook still holds an
    // `onWsFrame`), and it stays complementary to the two-watermark model — it
    // pre-filters frames for scopes the client no longer desires, so a
    // post-unsubscribe race frame never touches received-water, floor, or
    // dispatch. Any scope-bearing frame for a non-desired scope is an orphan:
    // ignore it entirely. The retained floor stays frozen at the last applied
    // seq, so the resubscribe's resume replays the gap. See
    // docs/event-ws-transport-v1.md §4.1.
    if (
      (frame.type === "event" ||
        frame.type === "subscribed" ||
        frame.type === "snapshot_required") &&
      !this.desiredScopes.has(frame.scope)
    ) {
      recordEventWsDiagnostic({
        kind: "orphan_frame",
        scope: frame.scope,
        eventKind: frame.type === "event" ? frame.kind : frame.type,
        seq: frame.type === "snapshot_required" ? undefined : frame.seq,
      });
      return;
    }

    if (frame.type === "authenticated") {
      this.reconnectAttempt = 0;
      this.clearConnectionState();
      this.resolvedViewerId = frame.viewerId;
      recordEventWsDiagnostic({ kind: "authenticated", epoch: frame.epoch });
      // Attach viewer wrappers BEFORE status flips to 'connected'.
      // `subscribe()` is then refcount-only (sendIfConnected gated on
      // status==='connected' returns early), so the canonical viewer
      // scope is added to desiredScopes silently and picked up by the
      // single `resubscribeDesiredScopes()` pass below — no duplicate
      // subscribe frames.
      for (const sub of Array.from(this.viewerSubscribers)) {
        sub.attach();
      }
      this.setStatus("connected");
      this.resubscribeDesiredScopes();
    } else if (frame.type === "ping") {
      this.send({ type: "pong" });
    } else if (frame.type === "subscribed") {
      this.confirmedScopes.add(frame.scope);
      // The ack confirms the listener is registered; any in-flight gap
      // recovery for the scope is moot.
      this.recovering.delete(frame.scope);
      // Baseline from the ack ONLY when there's no usable retained resume floor.
      // A same-epoch retained floor is client-authoritative: `sendSubscribe`
      // paired a `resume` with it, so the server replays the gap and the ack
      // must NOT re-baseline to server head (the no-listener-gap skip,
      // #task:1251). A floor from a stale epoch is moot — baseline both water
      // marks fresh; a mismatched resume self-heals via snapshot_required.
      const floor = this.resumeFloorPositions.get(frame.scope);
      if (!floor || floor.epoch !== frame.epoch) {
        this.recordReceived(frame);
        this.recordApplied(frame);
      }
      // A retained SAME-epoch floor means `sendSubscribe`/`resubscribeDesiredScopes`
      // paired a `resume`, so the server REPLAYS `floor+1..head`; we must NOT
      // re-baseline the water marks to the ack head there (the no-listener-gap
      // skip, #task:1251). The replayed frames flow through the normal event path
      // and are accepted by `acceptEventFrame` in seq order.
      recordEventWsDiagnostic({
        kind: "subscribed",
        scope: frame.scope,
        seq: frame.seq,
        epoch: frame.epoch,
      });
    } else if (frame.type === "event") {
      // Snapshot re-cold-start in progress on a content-gated scope: buffer
      // frames past the pending baseline (drained on commit), drop frames the
      // upcoming snapshot already covers, and never dispatch during the pending
      // window (§6, isomorphic to cold-start buffering).
      const pending = isContentGated(frame.scope)
        ? this.pendingBaselines.get(frame.scope)
        : undefined;
      if (pending) {
        if (frame.epoch === pending.epoch && frame.seq > pending.lastSeq) {
          const buffer = this.pendingBuffers.get(frame.scope) ?? [];
          buffer.push(frame);
          this.pendingBuffers.set(frame.scope, buffer);
        }
        // else: covered by the snapshot (≤ baseline seq), or a mismatched-epoch
        // straggler (a yet-newer epoch re-arms via its own snapshot_required) →
        // drop.
        return;
      }

      if (!this.acceptEventFrame(frame)) {
        // Gap detected (or a frame still beyond the received-water while
        // recovery is in flight): swallow it. The server's in-order replay
        // redelivers it.
        return;
      }
      if (frame.kind === "message") {
        recordEventWsDiagnostic({
          kind: "message_event",
          scope: frame.scope,
          eventKind: frame.kind,
          seq: frame.seq,
          epoch: frame.epoch,
        });
      }
    } else if (frame.type === "snapshot_required") {
      this.recovering.delete(frame.scope);
      if (isContentGated(frame.scope)) {
        // Stage-2 §6: a snapshot_required IS a re-cold-start. Record the
        // server-stamped transport baseline but DO NOT advance the floor now —
        // the commit is apply-gated (waits for the ingestor's reconcile via
        // `commitSnapshotBaseline`). Keep the retained floor pinned meanwhile;
        // buffer live frames > baseline. (Stage-1 deleted the cursor here and
        // let the next event re-baseline — impossible once the floor is
        // markSatisfied-driven, hence the server-stamped baseline.)
        this.pendingBaselines.set(frame.scope, {
          epoch: frame.epoch,
          lastSeq: frame.currentSeq,
        });
        this.pendingBuffers.delete(frame.scope);
      } else {
        // Non-content-gated scopes keep Stage-1 behavior: they are cursor-inert
        // / self-healing (own REST + invalidate), so drop both water marks and
        // let the next event re-baseline.
        this.resumeFloorPositions.delete(frame.scope);
        this.receivedPositions.delete(frame.scope);
      }
      recordEventWsDiagnostic({
        kind: "snapshot_required",
        scope: frame.scope,
        reason: frame.reason,
      });
    }

    this.dispatch(frame);
  }

  private dispatch(frame: ServerFrame): void {
    for (const listener of Array.from(this.frameListeners)) {
      listener(frame);
    }
  }

  private resubscribeDesiredScopes(): void {
    for (const scope of this.desiredScopes.keys()) {
      this.send({ type: "subscribe", scope });
    }
    const resumeScopes: ResumeFrame["scopes"] = {};
    for (const [scope, floor] of this.resumeFloorPositions.entries()) {
      if (!this.desiredScopes.has(scope)) continue;
      resumeScopes[scope] = floor;
    }
    if (Object.keys(resumeScopes).length > 0) {
      this.sendResume(resumeScopes);
    }
  }

  /**
   * Gate a live `event` frame on seq continuity (received-water space). Returns
   * whether the frame should be dispatched.
   *
   * The bug this guards against: a frame dropped while the socket stays OPEN
   * (so resume-on-reconnect never fires) used to advance the cursor silently,
   * stranding the UI one-or-more events behind until a manual refresh. We detect
   * the forward gap on the received-water and trigger a resume FROM THE APPLIED
   * FLOOR so the server replays the hole in order (and re-delivers any
   * un-applied content below the received-water too — §4.1).
   *
   * Cases (same epoch as the received-water):
   * - `seq === received + 1` — contiguous. Advance received (and, for a
   *   non-content-gated scope, the auto floor), drop any recovery latch, accept.
   * - `seq > received + 1` — forward gap. Hold received, resume from the applied
   *   floor, latch `recovering` so the resume + diagnostic coalesce, swallow the
   *   frame (replay redelivers it).
   * - `seq <= received` — a duplicate re-delivery by definition (already observed).
   *   Accept without moving any water mark or disturbing recovery.
   *
   * A missing received-water or an epoch change is a fresh baseline rather than a
   * gap. The transport no longer classifies delivery provenance: idempotency of a
   * replayed frame is the ingestor's responsibility (#1263 (c): reply counts
   * recover authoritatively, rows dedupe by id), so a replay-drain frame and fresh
   * fanout are dispatched identically.
   */
  private acceptEventFrame(frame: EventFrame): boolean {
    const gated = isContentGated(frame.scope);
    const received = this.receivedPositions.get(frame.scope);

    if (!received || received.epoch !== frame.epoch) {
      this.recovering.delete(frame.scope);
      this.recordReceived(frame);
      // Non-content-gated scopes auto-advance the floor with received (no
      // content-apply seam). Content-gated scopes leave the floor to markSatisfied.
      if (!gated) this.recordApplied(frame);
      return true;
    }

    // Next contiguous transport seq — the sole legal +1 out. Comparing frame.seq
    // to this successor keeps gap-detection a same-space TransportSeq compare and
    // never lets the `+ 1` decay to a bare number.
    const expectedSeq = advanceTransportSeq(received.lastSeq);
    if (frame.seq > expectedSeq) {
      if (!this.recovering.has(frame.scope)) {
        this.recovering.add(frame.scope);
        recordEventWsDiagnostic({
          kind: "live_gap",
          scope: frame.scope,
          eventKind: frame.kind,
          seq: frame.seq,
          lastSeq: received.lastSeq,
          expectedSeq,
          epoch: frame.epoch,
        });
        // Resume from the applied floor (may lag received), NOT received — so a
        // frame that reached the socket but never applied is re-delivered.
        const floor = this.resumeFloorPositions.get(frame.scope) ?? received;
        this.sendResume({ [frame.scope]: floor });
      }
      return false;
    }

    if (frame.seq === expectedSeq) {
      this.recovering.delete(frame.scope);
      this.recordReceived(frame);
      if (!gated) this.recordApplied(frame);
      return true;
    }

    // seq <= received: a duplicate re-delivery (already observed). Accept, move no
    // water mark, disturb no recovery.
    return true;
  }

  private recordReceived(frame: EventFrame | SubscribedFrame): void {
    const previous = this.receivedPositions.get(frame.scope);
    if (previous && previous.epoch === frame.epoch && previous.lastSeq >= frame.seq) {
      return;
    }
    this.receivedPositions.set(frame.scope, {
      epoch: frame.epoch,
      lastSeq: frame.seq,
    });
  }

  private recordApplied(frame: EventFrame | SubscribedFrame): void {
    const previous = this.resumeFloorPositions.get(frame.scope);
    if (previous && previous.epoch === frame.epoch && previous.lastSeq >= frame.seq) {
      return;
    }
    this.resumeFloorPositions.set(frame.scope, {
      epoch: frame.epoch,
      lastSeq: frame.seq,
    });
  }

  private sendResume(scopes: ResumeFrame["scopes"]): void {
    if (Object.keys(scopes).length === 0) return;
    if (!this.sendIfConnected({ type: "resume", scopes })) return;
    for (const [scope, position] of Object.entries(scopes)) {
      recordEventWsDiagnostic({
        kind: "resume_sent",
        scope,
        epoch: position.epoch,
        lastSeq: position.lastSeq,
      });
    }
  }

  private sendIfConnected(frame: ClientFrame): boolean {
    if (this.status !== "connected" || !this.ws || this.ws.readyState !== 1) {
      return false;
    }
    this.send(frame);
    return true;
  }

  private send(frame: ClientFrame): void {
    if (!this.ws) return;
    this.sendOnSocket(this.ws, frame);
  }

  private sendOnSocket(ws: EventSocket, frame: ClientFrame): void {
    if (ws.readyState !== 1) return;
    ws.send(JSON.stringify(frame));
  }

  private scheduleReconnect(): void {
    this.clearReconnectTimer();
    const index = Math.min(this.reconnectAttempt, RECONNECT_DELAYS_MS.length - 1);
    const delay = RECONNECT_DELAYS_MS[index]!;
    this.reconnectAttempt += 1;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (this.shouldReconnect) void this.openSocket();
    }, delay);
  }

  private clearReconnectTimer(): void {
    if (!this.reconnectTimer) return;
    clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
  }

  private setStatus(status: WsClientStatus): void {
    if (this.status === status) return;
    this.status = status;
    for (const listener of Array.from(this.statusListeners)) {
      listener(status);
    }
  }
}

/**
 * A conversation scope is CONTENT-GATED — its resume floor advances only on a
 * real content apply (`markSatisfied`) / apply-settled snapshot commit, never on
 * frame receipt (see EventWsClient docstring / docs/event-ws-transport-v1.md
 * §4.1). Only the message-stream scope `conversation:<id>` qualifies; the
 * side-channel `conversation-side:<id>` is cursor-inert (self-healing) and every
 * other scope has no content-apply seam, so all of them auto-advance. Routed
 * through the shared parser only — never a fuzzy `startsWith('conversation')`,
 * which would capture `conversation-side:`.
 */
function isContentGated(scope: string): boolean {
  return parseConversationScope(scope) !== null;
}

export const eventWsClient = new EventWsClient();

/**
 * Bridges `subscribeViewerScope()` to the canonical `viewer:{id}` key.
 * Each handle owns at most one inner refcounted subscription. While
 * the viewer id is unknown the wrapper is "detached" — `attach()` is
 * a no-op until the next `authenticated` frame supplies the id, at
 * which point the wrapper materialises an inner `subscribe(canonical)`.
 *
 * Idempotency:
 * - `attach()` does nothing if already attached to the same canonical
 *   scope. Calling it on every `authenticated` frame is safe.
 * - `detach()` releases the inner refcount (decrement + possible
 *   unsubscribe). Calling it twice is safe (second call sees
 *   `inner === null`).
 *
 * The wrapper is not exported — callers receive a plain
 * `WsScopeSubscription` from `subscribeViewerScope()`.
 */
class ViewerSubscriber {
  private inner: WsScopeSubscription | null = null;
  private currentScope: string | null = null;

  constructor(private readonly client: EventWsClient) {}

  attach(): void {
    const id = this.client.getResolvedViewerId();
    if (!id) return;
    const scope = `viewer:${id}`;
    if (this.currentScope === scope) return;
    if (this.inner) this.inner.unsubscribe();
    this.inner = this.client.subscribe(scope);
    this.currentScope = scope;
  }

  detach(): void {
    if (this.inner) this.inner.unsubscribe();
    this.inner = null;
    this.currentScope = null;
  }
}

export function subscribeWsScope(scope: SubscribeFrame["scope"]): WsScopeSubscription {
  return eventWsClient.subscribe(scope);
}

/**
 * Subscribe to the authenticated viewer's private scope. Internally
 * keyed by the canonical `viewer:{resolvedViewerId}` once the server's
 * `authenticated` frame supplies the id, so disconnects/reconnects
 * resume against the same scope without losing events. The
 * `viewer:self` alias is the on-the-wire form only — never written to
 * the client's position maps. Pair with `isCurrentViewerScope()` for
 * frame-level filtering.
 */
export function subscribeViewerScope(): WsScopeSubscription {
  return eventWsClient.subscribeViewerScope();
}

/**
 * True iff `scope` is the current viewer's resolved scope. Hooks call
 * this on incoming `event`/`snapshot_required` frames so they don't
 * need to hardcode the viewer's id.
 */
export function isCurrentViewerScope(scope: string): boolean {
  const resolved = eventWsClient.getResolvedViewerId();
  if (!resolved) return false;
  return scope === `viewer:${resolved}`;
}

export function onWsFrame(listener: WsClientListener): () => void {
  return eventWsClient.onFrame(listener);
}

function recordEventWsDiagnostic(input: EventWsDiagnostic): void {
  if (typeof window === "undefined") return;
  const recorder = getPlatform().client.recordEventWsDiagnostic;
  if (!recorder) return;
  void recorder(input).catch(() => {
    // Diagnostics must never affect the live WS delivery path.
  });
}

function eventWsUrl(baseUrl: string): string {
  const url = new URL(EVENT_WS_PATH, baseUrl);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  return url.toString();
}

function parseServerFrame(data: unknown): ServerFrame | null {
  if (typeof data !== "string") return null;
  try {
    const parsed = JSON.parse(data) as unknown;
    if (!isRecord(parsed) || typeof parsed.type !== "string") return null;
    return parsed as unknown as ServerFrame;
  } catch {
    return null;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
