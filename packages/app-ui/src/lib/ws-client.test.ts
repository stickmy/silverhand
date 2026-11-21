vi.mock("@/platform", () => ({
  getPlatform: () => ({
    client: {
      openEventSocket: (url: string) => new WebSocket(url),
      recordEventWsDiagnostic: async () => {},
    },
  }),
}));
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// Mock the api module BEFORE importing the client so its module-level
// import of `getBackendInfo` resolves to our stub. This must precede
// the dynamic import below.
vi.mock("./api", () => ({
  getBackendInfo: vi.fn().mockResolvedValue({
    url: "http://127.0.0.1:1234",
    mode: "embedded",
    local_host_id: "host-local",
    local_host_status: "claimed",
    local_host_ws_attachment: { observable: true, attached_at: null },
    local_host_server_url: "http://127.0.0.1:1234",
  }),
}));

import { EventWsClient, type WsScopeSubscription } from "./ws-client";
import { decodeTransportSeq, decodeTransportEpoch } from "@silverhand/shared";

const ts = (n: number) => decodeTransportSeq(n);
const te = (s: string) => decodeTransportEpoch(s);

/**
 * Minimal WebSocket double: records outbound frames in `sent`, exposes
 * `fireOpen()` / `fireMessage()` / `fireClose()` helpers so the test
 * drives the lifecycle deterministically. Mirrors the static
 * `OPEN/CONNECTING/CLOSED` constants the real `WebSocket` exposes
 * because ws-client.ts checks them directly via `WebSocket.OPEN`.
 */
class FakeWebSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;

  readyState: number = FakeWebSocket.CONNECTING;
  /** Parsed JSON of every frame the client sent on this socket. */
  readonly sent: Array<Record<string, unknown>> = [];
  private readonly listeners = new Map<string, Set<(ev: unknown) => void>>();

  constructor(public readonly url: string) {}

  addEventListener(type: string, fn: (ev: unknown) => void): void {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type)!.add(fn);
  }

  send(data: string): void {
    this.sent.push(JSON.parse(data) as Record<string, unknown>);
  }

  close(_code?: number, _reason?: string): void {
    if (this.readyState === FakeWebSocket.CLOSED) return;
    this.readyState = FakeWebSocket.CLOSED;
    this.dispatch("close", {});
  }

  // Test-only drivers ──────────────────────────────────────────────

  fireOpen(): void {
    this.readyState = FakeWebSocket.OPEN;
    this.dispatch("open", {});
  }

  fireMessage(frame: Record<string, unknown>): void {
    this.dispatch("message", { data: JSON.stringify(frame) });
  }

  fireClose(): void {
    if (this.readyState === FakeWebSocket.CLOSED) return;
    this.readyState = FakeWebSocket.CLOSED;
    this.dispatch("close", {});
  }

  private dispatch(type: string, ev: unknown): void {
    const set = this.listeners.get(type);
    if (!set) return;
    for (const fn of Array.from(set)) fn(ev);
  }
}

/** Stash the original global WebSocket so we can restore it. */
const originalWebSocket = (globalThis as { WebSocket?: unknown }).WebSocket;

let createdSockets: FakeWebSocket[] = [];
/** Track every client we construct so we can `disconnect()` it after
 *  each test — that clears any pending reconnect `setTimeout`, which
 *  would otherwise fire in a later test, push a stray socket into the
 *  fresh `createdSockets` array, and break length assertions. */
const liveClients: EventWsClient[] = [];

function makeClient(): EventWsClient {
  const c = new EventWsClient();
  liveClients.push(c);
  return c;
}

function installFakeWebSocket(): void {
  createdSockets = [];
  const ctor = function (this: unknown, url: string): FakeWebSocket {
    const ws = new FakeWebSocket(url);
    createdSockets.push(ws);
    return ws;
  } as unknown as typeof WebSocket;
  // The client checks `ws.readyState === WebSocket.OPEN` against the
  // global constructor's static constants — keep them aligned with the
  // fake's instance values.
  Object.assign(ctor, {
    CONNECTING: FakeWebSocket.CONNECTING,
    OPEN: FakeWebSocket.OPEN,
    CLOSING: FakeWebSocket.CLOSING,
    CLOSED: FakeWebSocket.CLOSED,
  });
  (globalThis as { WebSocket?: unknown }).WebSocket = ctor;
}

function restoreWebSocket(): void {
  if (originalWebSocket === undefined) {
    delete (globalThis as { WebSocket?: unknown }).WebSocket;
  } else {
    (globalThis as { WebSocket?: unknown }).WebSocket = originalWebSocket;
  }
}

/**
 * Wait for `openSocket()`'s `await getBackendInfo()` microtask plus
 * the subsequent `new WebSocket(...)` to be observable. A single
 * `await Promise.resolve()` isn't enough because there's an async fn
 * + an awaited promise chain.
 */
async function flushMicrotasks(): Promise<void> {
  for (let i = 0; i < 5; i++) {
    await Promise.resolve();
  }
}

beforeEach(() => {
  installFakeWebSocket();
});

afterEach(() => {
  while (liveClients.length > 0) {
    liveClients.pop()!.disconnect();
  }
  restoreWebSocket();
});

describe("EventWsClient — failed-upgrade reconnect discovery (#task:1763)", () => {
  /**
   * Runtime epoch promotion, WS half. A rejected upgrade is only visible to
   * the renderer as a bare close (spike-verified: no status/body at any
   * layer), so the client must NOT infer epoch semantics from the close.
   * What it MUST do — and what main relies on — is re-run getBackendInfo()
   * before every reconnect attempt: that IPC lands in the backend:info
   * handler, whose server-info fetch feeds the runtime-upgrade monitor with
   * the authoritative desktop_api advertisement. This test pins the renderer
   * half of that chain: close → scheduled reconnect (first delay tier) →
   * getBackendInfo BEFORE the next socket.
   */
  it("re-runs backend-info discovery before the reconnect socket after a non-clean close", async () => {
    vi.useFakeTimers();
    try {
      const api = await import("./api");
      const client = makeClient();
      client.subscribeViewerScope();
      await flushMicrotasks();
      expect(createdSockets).toHaveLength(1);
      const callsBefore = vi.mocked(api.getBackendInfo).mock.calls.length;

      createdSockets[0]!.fireClose();
      // No reconnect (and no discovery) synchronously on close.
      await flushMicrotasks();
      expect(createdSockets).toHaveLength(1);

      // First backoff tier elapses → openSocket() → getBackendInfo() → socket.
      await vi.advanceTimersByTimeAsync(500);
      await flushMicrotasks();
      expect(vi.mocked(api.getBackendInfo).mock.calls.length).toBe(callsBefore + 1);
      expect(createdSockets).toHaveLength(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("a failed discovery keeps reconnecting without opening a socket (no epoch inference from the failure)", async () => {
    vi.useFakeTimers();
    try {
      const api = await import("./api");
      vi.mocked(api.getBackendInfo).mockRejectedValueOnce(new Error("backend down"));
      const client = makeClient();
      client.subscribeViewerScope();
      await flushMicrotasks();
      // Discovery failed: no socket, but a reconnect was scheduled.
      expect(createdSockets).toHaveLength(0);
      await vi.advanceTimersByTimeAsync(500);
      await flushMicrotasks();
      // Second attempt succeeded (mock restored) and opened the socket.
      expect(createdSockets).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("EventWsClient — viewer scope canonicalisation", () => {
  /**
   * The B1 contract: whatever the renderer subscribes to via
   * `subscribeViewerScope()` must end up keyed by the canonical
   * `viewer:<id>` everywhere internal — `desiredScopes` /
   * `confirmedScopes` / `positions`. The on-the-wire form
   * `viewer:self` must NEVER appear in subsequent client→server
   * frames once `authenticated` has supplied the id.
   */
  it("subscribes against canonical viewer:<id> after authenticated", async () => {
    const client = makeClient();

    // Subscribe BEFORE the connection has completed — this is the hot
    // path: hooks mount during initial render, viewer hydration is async.
    client.subscribeViewerScope();

    await flushMicrotasks();
    expect(createdSockets).toHaveLength(1);
    const ws = createdSockets[0]!;

    ws.fireOpen();
    // Auth is owned by Electron main's HttpOnly websocket cookie; the
    // renderer must not send any bearer-bearing frame.
    expect(ws.sent).toEqual([]);

    ws.fireMessage({
      type: "authenticated",
      viewerId: "abcdef0123456789",
      epoch: "epoch-1",
    });

    // After authenticated, the client should have sent a `subscribe`
    // frame keyed by canonical `viewer:<id>`. The alias `viewer:self`
    // must NOT appear anywhere on the wire.
    const subscribeFrames = ws.sent.filter((f) => f.type === "subscribe");
    expect(subscribeFrames).toEqual([{ type: "subscribe", scope: "viewer:abcdef0123456789" }]);

    expect(client.getResolvedViewerId()).toBe("abcdef0123456789");
  });

  /**
   * The actual B1 blocker: events accumulated against the canonical
   * scope must be re-claimed via a `resume` frame on reconnect.
   * Pre-fix, `desiredScopes` was keyed by alias and `positions` by
   * canonical, so `resubscribeDesiredScopes` would send `subscribe
   * viewer:self` again but never carry over the lastSeq.
   */
  it("reconnect resume frame carries canonical viewer:<id> with last seq", async () => {
    const client = makeClient();
    client.subscribeViewerScope();

    await flushMicrotasks();
    const ws1 = createdSockets[0]!;
    ws1.fireOpen();
    ws1.fireMessage({
      type: "authenticated",
      viewerId: "u-alice",
      epoch: "epoch-1",
    });

    // Server confirms the canonical subscription and pushes an event.
    // Seqs are per-scope contiguous (see scope-event-bus `nextSeq`), so the
    // event after a `subscribed` at seq 9 is seq 10 — the client now treats
    // a non-contiguous jump as a dropped-frame gap (#1241), not normal flow.
    ws1.fireMessage({
      type: "subscribed",
      scope: "viewer:u-alice",
      epoch: "epoch-1",
      seq: 9,
    });
    ws1.fireMessage({
      type: "event",
      scope: "viewer:u-alice",
      epoch: "epoch-1",
      seq: 10,
      kind: "unread_updated",
      payload: {},
    });

    // Simulate transient network loss (NOT explicit disconnect — the
    // wrapper handle stays live, viewerSubscribers retain currentScope
    // so reconnect re-authenticated is a no-op attach).
    ws1.fireClose();

    // The reconnect timer fires after a delay; bypass it by calling
    // connect() directly (idempotent — `shouldReconnect` is still true,
    // status flipped to 'disconnected'). connect() kicks off a new
    // openSocket().
    client.connect();
    await flushMicrotasks();
    expect(createdSockets).toHaveLength(2);
    const ws2 = createdSockets[1]!;
    ws2.fireOpen();
    ws2.fireMessage({
      type: "authenticated",
      viewerId: "u-alice",
      epoch: "epoch-2",
    });

    // After re-auth, the resume frame on the new socket MUST include
    // the canonical viewer scope with the last seq we'd recorded. This
    // is the assertion that broke pre-fix.
    const resumeFrames = ws2.sent.filter((f) => f.type === "resume");
    expect(resumeFrames).toHaveLength(1);
    expect(resumeFrames[0]).toEqual({
      type: "resume",
      scopes: {
        "viewer:u-alice": { epoch: "epoch-1", lastSeq: 10 },
      },
    });

    // And the subscribe-on-reconnect must also be canonical, never
    // the alias.
    const subscribeFrames = ws2.sent.filter((f) => f.type === "subscribe");
    expect(subscribeFrames).toEqual([{ type: "subscribe", scope: "viewer:u-alice" }]);
  });

  /**
   * Defensive: the literal `'viewer:self'` alias must never leak into
   * any client→server frame on either socket. The fix's value
   * proposition is purely "alias kept at the boundary", so a regex
   * sweep across all sent frames is the cleanest drift guard.
   */
  it("never emits viewer:self alias on the wire after authenticated", async () => {
    const client = makeClient();
    client.subscribeViewerScope();

    await flushMicrotasks();
    const ws1 = createdSockets[0]!;
    ws1.fireOpen();
    ws1.fireMessage({
      type: "authenticated",
      viewerId: "u-bob",
      epoch: "epoch-1",
    });
    ws1.fireMessage({
      type: "event",
      scope: "viewer:u-bob",
      epoch: "epoch-1",
      seq: 3,
      kind: "saved_changed",
      payload: {},
    });
    ws1.fireClose();

    client.connect();
    await flushMicrotasks();
    const ws2 = createdSockets[1]!;
    ws2.fireOpen();
    ws2.fireMessage({
      type: "authenticated",
      viewerId: "u-bob",
      epoch: "epoch-2",
    });

    const allSent = [...ws1.sent, ...ws2.sent];
    const offenders = allSent.filter((frame) => JSON.stringify(frame).includes("viewer:self"));
    expect(offenders).toEqual([]);
  });
});

/**
 * #1241 — live forward-gap recovery. A frame dropped while the socket
 * stays OPEN (so reconnect/resume never fires) used to advance the
 * cursor silently, stranding the UI behind until a manual refresh. The
 * client now detects the seq discontinuity and resumes in place.
 */
describe("EventWsClient — live forward-gap recovery", () => {
  /** Seqs of `event` frames that actually reached `onFrame` listeners. */
  function eventSeqs(frames: Array<Record<string, unknown>>): number[] {
    return frames.filter((f) => f.type === "event").map((f) => f.seq as number);
  }

  function fireEvent(ws: FakeWebSocket, scope: string, seq: number, epoch = "epoch-1"): void {
    ws.fireMessage({ type: "event", scope, epoch, seq, kind: "message", payload: {} });
  }

  /**
   * Bring a client to `connected`, subscribed to `scope`, with the
   * resume cursor baselined at `seq` (via a `subscribed` frame). Returns
   * the captured dispatch log and the socket with its outbound buffer
   * cleared, so later `resume`/dispatch assertions only see gap traffic.
   */
  async function connectedAt(
    scope: string,
    seq: number,
  ): Promise<{
    client: EventWsClient;
    ws: FakeWebSocket;
    sub: WsScopeSubscription;
    dispatched: Array<Record<string, unknown>>;
  }> {
    const client = makeClient();
    const dispatched: Array<Record<string, unknown>> = [];
    client.onFrame((f) => dispatched.push(f as unknown as Record<string, unknown>));
    const sub = client.subscribe(scope);
    await flushMicrotasks();
    const ws = createdSockets[0]!;
    ws.fireOpen();
    ws.fireMessage({ type: "authenticated", viewerId: "u", epoch: "epoch-1" });
    ws.fireMessage({ type: "subscribed", scope, epoch: "epoch-1", seq });
    ws.sent.length = 0;
    return { client, ws, sub, dispatched };
  }

  it("dispatches contiguous events and advances without resuming", async () => {
    const { ws, dispatched } = await connectedAt("conv:room", 5);

    fireEvent(ws, "conv:room", 6);
    fireEvent(ws, "conv:room", 7);
    fireEvent(ws, "conv:room", 8);

    expect(eventSeqs(dispatched)).toEqual([6, 7, 8]);
    expect(ws.sent.filter((f) => f.type === "resume")).toHaveLength(0);
  });

  it("swallows a gapped frame and resumes from the held cursor", async () => {
    const { ws, dispatched } = await connectedAt("conv:room", 5);

    // seq 9 with cursor at 5 → 6/7/8 were dropped on the open socket.
    fireEvent(ws, "conv:room", 9);

    // The gapped frame must NOT be dispatched ahead of the missing ones…
    expect(eventSeqs(dispatched)).toEqual([]);
    // …and a resume from the held cursor (5, not 9) must be sent.
    expect(ws.sent.filter((f) => f.type === "resume")).toEqual([
      { type: "resume", scopes: { "conv:room": { epoch: "epoch-1", lastSeq: 5 } } },
    ]);

    // Server replays the hole in order; everything dispatches once, in order.
    for (const seq of [6, 7, 8, 9]) fireEvent(ws, "conv:room", seq);
    expect(eventSeqs(dispatched)).toEqual([6, 7, 8, 9]);

    // Recovery latch dropped: a later contiguous live frame flows normally.
    fireEvent(ws, "conv:room", 10);
    expect(eventSeqs(dispatched)).toEqual([6, 7, 8, 9, 10]);
  });

  it("clears recovery on snapshot_required and re-baselines from it", async () => {
    const { ws, dispatched } = await connectedAt("conv:room", 5);

    fireEvent(ws, "conv:room", 9); // gap → resume + recovering latch
    expect(ws.sent.filter((f) => f.type === "resume")).toHaveLength(1);

    // Replay buffer too small → server gives up on the gap.
    ws.fireMessage({
      type: "snapshot_required",
      scope: "conv:room",
      epoch: "epoch-1",
      reason: "gap_too_large",
    });

    // The frame reaches listeners so conversation hooks can refetch.
    expect(dispatched.some((f) => f.type === "snapshot_required" && f.scope === "conv:room")).toBe(
      true,
    );

    // Cursor wiped + latch cleared: the next event re-baselines (no stale
    // gap detection against the deleted cursor, no leftover suppression).
    ws.sent.length = 0;
    fireEvent(ws, "conv:room", 20);
    expect(eventSeqs(dispatched)).toContain(20);
    expect(ws.sent.filter((f) => f.type === "resume")).toHaveLength(0);
  });

  it("coalesces concurrent gapped frames into a single in-flight resume", async () => {
    const { ws, dispatched } = await connectedAt("conv:room", 5);

    // Three live frames land while the gap is open, before any replay.
    fireEvent(ws, "conv:room", 9);
    fireEvent(ws, "conv:room", 10);
    fireEvent(ws, "conv:room", 11);

    // Exactly one resume — the latch suppressed the redundant two.
    expect(ws.sent.filter((f) => f.type === "resume")).toHaveLength(1);
    expect(eventSeqs(dispatched)).toEqual([]);

    // Replay fills the hole in order; a fresh live frame follows.
    for (const seq of [6, 7, 8, 9, 10, 11]) fireEvent(ws, "conv:room", seq);
    fireEvent(ws, "conv:room", 12);

    expect(eventSeqs(dispatched)).toEqual([6, 7, 8, 9, 10, 11, 12]);
    // Still only ever one resume across the whole episode.
    expect(ws.sent.filter((f) => f.type === "resume")).toHaveLength(1);
  });
});

/**
 * no-listener gap. When a scope's last subscriber goes away
 * (component unmount) and resubscribes (remount), events published in the
 * gap used to be lost: `unsubscribe()` dropped the per-scope cursor, so the
 * next `subscribe()` re-baselined to server head off the `subscribed` ack,
 * silently skipping anything published while no listener was attached. The
 * pre-existing transport bug (`646d223`) was exposed/amplified by the
 * inbox/Activity churn that mounts and unmounts conversation views far more
 * often.
 *
 * The fix: the transport cursor is client-authoritative. `unsubscribe()`
 * retains `positions[scope]`; `subscribe()` pairs a `resume` from it so the
 * server replays the gap; the `subscribed` ack does NOT re-baseline a
 * retained same-epoch cursor. Retained-but-detached cursors are bounded by
 * an LRU so they can't grow without limit — eviction degrades to a fresh
 * ack baseline, never a silent skip.
 *
 * Three-axis discipline (the #1244 "surfaced ≠ eliminated" failure class,
 * re-asserted at the transport layer): replay only touches the transport
 * frame cursor. It NEVER writes the read cursor or directed `read_at` — a
 * replayed frame is byte-identical to a live one and read-inert by
 * construction.
 */
describe("EventWsClient — no-listener gap recovery (#1251)", () => {
  function eventSeqs(frames: Array<Record<string, unknown>>): number[] {
    return frames.filter((f) => f.type === "event").map((f) => f.seq as number);
  }

  function fireEvent(ws: FakeWebSocket, scope: string, seq: number, epoch = "epoch-1"): void {
    ws.fireMessage({ type: "event", scope, epoch, seq, kind: "message", payload: {} });
  }

  async function connectedAt(
    scope: string,
    seq: number,
  ): Promise<{
    client: EventWsClient;
    ws: FakeWebSocket;
    sub: WsScopeSubscription;
    dispatched: Array<Record<string, unknown>>;
  }> {
    const client = makeClient();
    const dispatched: Array<Record<string, unknown>> = [];
    client.onFrame((f) => dispatched.push(f as unknown as Record<string, unknown>));
    const sub = client.subscribe(scope);
    await flushMicrotasks();
    const ws = createdSockets[0]!;
    ws.fireOpen();
    ws.fireMessage({ type: "authenticated", viewerId: "u", epoch: "epoch-1" });
    ws.fireMessage({ type: "subscribed", scope, epoch: "epoch-1", seq });
    ws.sent.length = 0;
    return { client, ws, sub, dispatched };
  }

  it("retains the cursor across unsubscribe and replays the gap on resubscribe", async () => {
    const { client, ws, sub, dispatched } = await connectedAt("conv:room", 5);

    // A live event advances the cursor to 6.
    fireEvent(ws, "conv:room", 6);
    expect(eventSeqs(dispatched)).toEqual([6]);

    // Last subscriber goes away (unmount). The unsubscribe frame goes out
    // but the cursor must be RETAINED for the eventual remount.
    sub.unsubscribe();
    expect(ws.sent.filter((f) => f.type === "unsubscribe")).toEqual([
      { type: "unsubscribe", scope: "conv:room" },
    ]);
    ws.sent.length = 0;

    // [server publishes seq 7, 8 here — no listener attached: the gap.]

    // Remount. The resubscribe must re-subscribe AND resume from the
    // retained cursor (6) so the server replays the gap — not re-baseline
    // to head and skip it.
    client.subscribe("conv:room");
    expect(ws.sent.filter((f) => f.type === "subscribe")).toEqual([
      { type: "subscribe", scope: "conv:room" },
    ]);
    expect(ws.sent.filter((f) => f.type === "resume")).toEqual([
      { type: "resume", scopes: { "conv:room": { epoch: "epoch-1", lastSeq: 6 } } },
    ]);

    // Server replays the hole in order; every gap event dispatches once.
    for (const seq of [7, 8]) fireEvent(ws, "conv:room", seq);
    expect(eventSeqs(dispatched)).toEqual([6, 7, 8]);
  });

  it("does not let the subscribed ack re-baseline a retained same-epoch cursor", async () => {
    const { client, ws, sub } = await connectedAt("conv:room", 5);
    fireEvent(ws, "conv:room", 6); // cursor → {epoch-1, 6}
    sub.unsubscribe();
    ws.sent.length = 0;

    client.subscribe("conv:room"); // subscribe + resume{6}

    // Server acks the resubscription at its current head (8) before the
    // replay lands. The ack must NOT advance the retained cursor to 8.
    ws.fireMessage({ type: "subscribed", scope: "conv:room", epoch: "epoch-1", seq: 8 });
    ws.sent.length = 0;

    // A forward-gapped live frame reveals where the cursor actually sits:
    // if the ack had clobbered it to 8, seq 9 would be contiguous (no
    // resume). Because the cursor is still the client-authoritative 6, seq 9
    // is a forward gap and resumes from 6.
    fireEvent(ws, "conv:room", 9);
    expect(ws.sent.filter((f) => f.type === "resume")).toEqual([
      { type: "resume", scopes: { "conv:room": { epoch: "epoch-1", lastSeq: 6 } } },
    ]);
  });

  it("re-baselines from the ack when the retained cursor is from a stale epoch", async () => {
    const { client, ws, sub } = await connectedAt("conv:room", 5);
    fireEvent(ws, "conv:room", 6); // cursor → {epoch-1, 6}
    sub.unsubscribe();
    ws.sent.length = 0;

    client.subscribe("conv:room");
    // New epoch (server buffer rotated) — the retained epoch-1 cursor is
    // moot; baseline fresh from the new-epoch ack. (The resume{epoch-1,6}
    // we optimistically sent self-heals server-side via snapshot_required.)
    ws.fireMessage({ type: "subscribed", scope: "conv:room", epoch: "epoch-2", seq: 50 });
    ws.sent.length = 0;

    // Contiguous in the new epoch from 50 → dispatch, no gap/resume.
    fireEvent(ws, "conv:room", 51, "epoch-2");
    expect(ws.sent.filter((f) => f.type === "resume")).toHaveLength(0);
  });

  it("retains detached cursors for the whole session (no lossy eviction)", async () => {
    // A bounded LRU here would re-create the no-listener gap: an evicted
    // scope resubscribes with no `resume`, the server never emits
    // `snapshot_required`, and the `subscribed` ack baselines to head —
    // silently skipping gap traffic. So cursors are retained for the
    // session; this guards that the oldest detached scope still resumes
    // even after far more detach/resubscribe churn than any past LRU bound.
    const CHURN = 200; // well past the old cap of 64

    const client = makeClient();
    client.subscribe("conv:keep"); // a live scope, just to drive connect()
    await flushMicrotasks();
    const ws = createdSockets[0]!;
    ws.fireOpen();
    ws.fireMessage({ type: "authenticated", viewerId: "u", epoch: "epoch-1" });

    // The oldest detached scope: subscribe, give it a cursor, detach first.
    const oldest = "conv:oldest";
    const osub = client.subscribe(oldest);
    ws.fireMessage({ type: "subscribed", scope: oldest, epoch: "epoch-1", seq: 7 });
    osub.unsubscribe();

    // Churn far more scopes through detach than any LRU bound would hold.
    for (let i = 0; i < CHURN; i++) {
      const scope = `conv:churn${i}`;
      const sub = client.subscribe(scope);
      ws.fireMessage({ type: "subscribed", scope, epoch: "epoch-1", seq: 1 });
      sub.unsubscribe();
    }
    ws.sent.length = 0;

    // The oldest scope's cursor must still be retained → resubscribe resumes
    // from it (7), never a silent baseline-to-head.
    client.subscribe(oldest);
    expect(ws.sent.filter((f) => f.type === "subscribe")).toEqual([
      { type: "subscribe", scope: oldest },
    ]);
    expect(ws.sent.filter((f) => f.type === "resume")).toEqual([
      { type: "resume", scopes: { "conv:oldest": { epoch: "epoch-1", lastSeq: 7 } } },
    ]);
  });

  it("ignores an orphan event after unsubscribe — cursor frozen at last applied seq", async () => {
    const { client, ws, sub, dispatched } = await connectedAt("conv:room", 5);
    fireEvent(ws, "conv:room", 6); // applied while desired → cursor 6
    expect(eventSeqs(dispatched)).toEqual([6]);

    // View unmounts: scope leaves desiredScopes, cursor retained at 6.
    sub.unsubscribe();
    ws.sent.length = 0;

    // Race-window frame: server hasn't processed the unsubscribe yet, so seq
    // 7 still arrives. No consumer → it must NOT dispatch and must NOT
    // advance the cursor.
    fireEvent(ws, "conv:room", 7);
    expect(eventSeqs(dispatched)).toEqual([6]); // 7 swallowed, not dispatched

    client.subscribe("conv:room");
    expect(ws.sent.filter((f) => f.type === "resume")).toEqual([
      { type: "resume", scopes: { "conv:room": { epoch: "epoch-1", lastSeq: 6 } } },
    ]);
  });

  it("ignores a late subscribed ack for a no-longer-desired scope (no re-baseline)", async () => {
    const { client, ws, sub } = await connectedAt("conv:room", 5);
    fireEvent(ws, "conv:room", 6); // cursor 6
    sub.unsubscribe();
    ws.sent.length = 0;

    // A subscribed ack from the just-cancelled subscription lands late, at
    // server head (50). Scope is no longer desired → it must not
    // confirm/baseline; the retained cursor stays 6.
    ws.fireMessage({ type: "subscribed", scope: "conv:room", epoch: "epoch-1", seq: 50 });

    client.subscribe("conv:room");
    expect(ws.sent.filter((f) => f.type === "resume")).toEqual([
      { type: "resume", scopes: { "conv:room": { epoch: "epoch-1", lastSeq: 6 } } },
    ]);
  });

  it("ignores a late snapshot_required for a no-longer-desired scope (cursor kept)", async () => {
    const { client, ws, sub } = await connectedAt("conv:room", 5);
    fireEvent(ws, "conv:room", 6); // cursor 6
    sub.unsubscribe();
    ws.sent.length = 0;

    // A snapshot_required for the cancelled scope must not delete the
    // retained cursor (no consumer to refetch); the cursor survives so the
    // next subscribe still resumes.
    ws.fireMessage({
      type: "snapshot_required",
      scope: "conv:room",
      epoch: "epoch-1",
      reason: "gap_too_large",
    });

    client.subscribe("conv:room");
    expect(ws.sent.filter((f) => f.type === "resume")).toEqual([
      { type: "resume", scopes: { "conv:room": { epoch: "epoch-1", lastSeq: 6 } } },
    ]);
  });
});

describe("EventWsClient — two-watermark content-gated scope (#1255 P2 step-4)", () => {
  const SCOPE = "conversation:room";

  function fireEvent(
    ws: FakeWebSocket,
    scope: string,
    seq: number,
    epoch = "epoch-1",
    kind = "message",
  ): void {
    ws.fireMessage({ type: "event", scope, epoch, seq, kind, payload: {} });
  }

  interface Delivered {
    frame: Record<string, unknown>;
  }

  function eventSeqs(delivered: Delivered[]): number[] {
    return delivered.filter((d) => d.frame.type === "event").map((d) => d.frame.seq as number);
  }

  function resumes(ws: FakeWebSocket): Array<Record<string, unknown>> {
    return ws.sent.filter((f) => f.type === "resume");
  }

  async function connectedGated(
    scope: string,
    seq: number,
  ): Promise<{
    client: EventWsClient;
    ws: FakeWebSocket;
    sub: WsScopeSubscription;
    delivered: Delivered[];
  }> {
    const client = makeClient();
    const delivered: Delivered[] = [];
    client.onFrame((f) => delivered.push({ frame: f as unknown as Record<string, unknown> }));
    const sub = client.subscribe(scope);
    await flushMicrotasks();
    const ws = createdSockets[0]!;
    ws.fireOpen();
    ws.fireMessage({ type: "authenticated", viewerId: "u", epoch: "epoch-1" });
    ws.fireMessage({ type: "subscribed", scope, epoch: "epoch-1", seq });
    ws.sent.length = 0;
    return { client, ws, sub, delivered };
  }

  it("advances received on events but pins the resume floor until markSatisfied", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    fireEvent(ws, SCOPE, 7);

    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 5 } } },
    ]);
  });

  it("resumes from the markSatisfied floor once content is applied", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    client.markSatisfied(SCOPE, ts(6), te("epoch-1"));
    fireEvent(ws, SCOPE, 7);
    client.markSatisfied(SCOPE, ts(7), te("epoch-1"));

    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 7 } } },
    ]);
  });

  it("pins the floor when an intervening seq is never applied (strict-contiguous)", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    client.markSatisfied(SCOPE, ts(6), te("epoch-1")); // floor → 6
    fireEvent(ws, SCOPE, 7); // no-op: never applied (off-window / readerless)
    fireEvent(ws, SCOPE, 8);
    client.markSatisfied(SCOPE, ts(8), te("epoch-1")); // 8 !== 6+1 → floor stays 6

    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 6 } } },
    ]);
  });

  // ────────────────────── pending-satisfaction queue ──────────────────────
  // The strict-contiguous pin above is preserved, but a satisfied-yet-non-
  // contiguous seq is now REMEMBERED and the floor drains through it the moment
  // the gap below fills — so a cursor-inert frame no longer pins the floor behind
  // it. `markSatisfied` generalizes the old `markApplied`: content-apply is one
  // satisfied disposition, a vacuous / derived-invalidate-settled frame another.

  it("drains the pending backlog contiguously once the gap fills (#1261)", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    fireEvent(ws, SCOPE, 7);
    fireEvent(ws, SCOPE, 8);
    client.markSatisfied(SCOPE, ts(6), te("epoch-1")); // floor → 6
    client.markSatisfied(SCOPE, ts(8), te("epoch-1")); // 8 !== 6+1 → pending {8}, floor 6
    // The gap fills: 7 satisfied → floor drains through 7 AND the pending 8 → 8.
    client.markSatisfied(SCOPE, ts(7), te("epoch-1"));

    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 8 } } },
    ]);
  });

  it("absorbs out-of-order satisfaction (later seqs satisfied first) (#1261)", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    fireEvent(ws, SCOPE, 7);
    fireEvent(ws, SCOPE, 8);
    // Satisfied newest-first: the floor only moves when 6 (floor+1) fills.
    client.markSatisfied(SCOPE, ts(8), te("epoch-1")); // pending {8}, floor 5
    client.markSatisfied(SCOPE, ts(7), te("epoch-1")); // pending {7,8}, floor 5
    client.markSatisfied(SCOPE, ts(6), te("epoch-1")); // 6 === 5+1 → drain 6,7,8 → floor 8

    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 8 } } },
    ]);
  });

  it("bounds the pending backlog behind a gap that never fills", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 5);

    // 6 never arrives; 7..506 are all satisfied.
    for (let seq = 7; seq <= 506; seq++) {
      fireEvent(ws, SCOPE, seq);
      client.markSatisfied(SCOPE, ts(seq), te("epoch-1"));
    }
    const pending = (client as unknown as { satisfiedPending: Map<string, { seqs: Set<number> }> })
      .satisfiedPending;
    expect(pending.get(SCOPE)!.seqs.size).toBe(200);

    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 5 } } },
    ]);
  });

  it("keeps the seqs nearest the gap so the floor drains through them once it fills", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 5);

    for (let seq = 7; seq <= 306; seq++) {
      fireEvent(ws, SCOPE, seq);
      client.markSatisfied(SCOPE, ts(seq), te("epoch-1"));
    }
    client.markSatisfied(SCOPE, ts(6), te("epoch-1"));

    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 206 } } },
    ]);
  });

  it("treats a re-satisfy at or below the floor as an idempotent no-op (#1261)", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    fireEvent(ws, SCOPE, 7);
    client.markSatisfied(SCOPE, ts(6), te("epoch-1")); // floor 6
    client.markSatisfied(SCOPE, ts(7), te("epoch-1")); // floor 7
    // A resume re-delivers 6 and 7; re-satisfying them must not disturb the floor.
    client.markSatisfied(SCOPE, ts(6), te("epoch-1"));
    client.markSatisfied(SCOPE, ts(7), te("epoch-1"));

    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 7 } } },
    ]);
  });

  it("clears the pending backlog on reconnect but retains the floor — no stale drain (#1261)", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    client.markSatisfied(SCOPE, ts(6), te("epoch-1")); // floor 6
    fireEvent(ws, SCOPE, 8);
    client.markSatisfied(SCOPE, ts(8), te("epoch-1")); // pending {8}, floor 6

    // Reconnect on the socket: `authenticated` runs clearConnectionState (drops
    // the pending backlog) but RETAINS the resume floor; the same-epoch ack
    // re-arms the replay window without re-baselining.
    ws.fireMessage({ type: "authenticated", viewerId: "u", epoch: "epoch-1" });
    ws.fireMessage({ type: "subscribed", scope: SCOPE, epoch: "epoch-1", seq: 8 });

    // Only 7 is satisfied. Had the backlog leaked, the floor would jump 6→8
    // (draining the stale 8); because it was cleared, it stops at 7 until 8 is
    // re-satisfied from the replay.
    client.markSatisfied(SCOPE, ts(7), te("epoch-1"));

    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 7 } } },
    ]);
  });

  it("drops the pending backlog on last-unsubscribe; same-socket resubscribe rebuilds via replay (#1261, Cindy blocker)", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6); // received 6, floor 5 (off-window, not satisfied)
    fireEvent(ws, SCOPE, 7); // received 7, floor 5
    fireEvent(ws, SCOPE, 8); // received 8, floor 5
    client.markSatisfied(SCOPE, ts(8), te("epoch-1")); // pending {8}, floor 5

    // Last subscriber leaves. Floor (5) + received (8) are retained for the
    // #1251 no-listener replay, but the pending backlog {8} — a temporary
    // "cache/display satisfied" fact — MUST be dropped: no listener means the
    // caches behind it may be gone before the resubscribe.
    sub.unsubscribe();

    // Same socket, resubscribe: resumes from the retained floor (5); the server
    // replays 6,7,8 and the ingestor re-satisfies them in seq order.
    const resub = client.subscribe(SCOPE);
    ws.fireMessage({ type: "subscribed", scope: SCOPE, epoch: "epoch-1", seq: 8 });

    // Re-satisfy ONLY 6 and 7 (8 not yet re-dispatched this round). Had the stale
    // {8} survived last-unsubscribe, the floor would jump 5→8; it must stop at 7.
    client.markSatisfied(SCOPE, ts(6), te("epoch-1"));
    client.markSatisfied(SCOPE, ts(7), te("epoch-1"));

    resub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 7 } } },
    ]);
  });

  it("drains a floor pinned by a cursor-inert frame so the later reply won't replay (Cindy's #1263 falsifier, transport half) (#1261)", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 9);

    // seq10 = off-window reply (cursor-inert, not yet satisfiable — a warm
    // inactive target still to be removed). seq11 = in-window reply r1.
    fireEvent(ws, SCOPE, 10);
    fireEvent(ws, SCOPE, 11);
    client.markSatisfied(SCOPE, ts(11), te("epoch-1")); // pending {11}, floor pinned 9
    // Under strict-contiguous this pins forever; the satisfaction queue lets the
    // cursor-inert seq10 satisfy (vacuous, once removeQueries'd) and drain 10+11.
    client.markSatisfied(SCOPE, ts(10), te("epoch-1")); // 10 === 9+1 → drain 10,11 → floor 11

    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    // Floor at 11 → a resume replays from 11, so seq11 (r1) is NOT re-delivered:
    // no re-fold, no double-count even before the idempotent-invalidate backstop.
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 11 } } },
    ]);
  });

  it("detects gaps on received-water, not the lagging applied floor", async () => {
    const { ws } = await connectedGated(SCOPE, 5);

    // received advances 6, 7 while the floor stays pinned at 5 (no markSatisfied).
    fireEvent(ws, SCOPE, 6);
    fireEvent(ws, SCOPE, 7);
    // 8 is contiguous in received space (7+1) → no false gap despite floor at 5.
    fireEvent(ws, SCOPE, 8);
    expect(resumes(ws)).toHaveLength(0);
  });

  it("resumes a live gap from the applied floor, not the received-water", async () => {
    const { client, ws } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    client.markSatisfied(SCOPE, ts(6), te("epoch-1")); // floor 6, received 6
    fireEvent(ws, SCOPE, 7); // received 7, floor stays 6 (no markSatisfied)
    fireEvent(ws, SCOPE, 9); // gap (9 > 7+1) → resume from the floor (6)

    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 6 } } },
    ]);
  });

  it("dispatches a duplicate re-delivery (seq <= received) without moving the water", async () => {
    const { ws, delivered } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6); // extends received
    fireEvent(ws, SCOPE, 6); // seq <= received → duplicate re-delivery, still dispatched

    const evs = delivered.filter((d) => d.frame.type === "event");
    expect(evs.map((d) => d.frame.seq)).toEqual([6, 6]);
  });

  it("snapshot_required pins the floor until commit (apply-gated), buffering live frames", async () => {
    const { client, ws, sub, delivered } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    client.markSatisfied(SCOPE, ts(6), te("epoch-1")); // floor 6

    // Server can't cover the gap → re-cold-start with a server-stamped baseline.
    ws.fireMessage({
      type: "snapshot_required",
      scope: SCOPE,
      epoch: "epoch-1",
      reason: "gap_too_large",
      currentSeq: 20,
    });

    // A live frame past the baseline is BUFFERED (not dispatched) while pending;
    // a frame the snapshot already covers (≤ 20) is dropped.
    fireEvent(ws, SCOPE, 21);
    fireEvent(ws, SCOPE, 15);
    expect(eventSeqs(delivered)).toEqual([6]); // nothing new dispatched pre-commit

    // Commit (the ingestor's reconcile settled): floor jumps to the baseline (20)
    // and the buffered frame 21 drains as a fresh live frame.
    client.commitSnapshotBaseline(SCOPE, te("epoch-1"));
    expect(eventSeqs(delivered)).toEqual([6, 21]);

    // The drained frame would be markSatisfied by the ingestor; that advance is
    // only legal because the commit set the floor to 20 (21 === 20 + 1).
    client.markSatisfied(SCOPE, ts(21), te("epoch-1"));
    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 21 } } },
    ]);
  });

  it("discards a stale-epoch or no-pending commitSnapshotBaseline", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    client.markSatisfied(SCOPE, ts(6), te("epoch-1")); // floor 6

    // No pending baseline yet → the commit is a no-op.
    client.commitSnapshotBaseline(SCOPE, te("epoch-1"));

    ws.fireMessage({
      type: "snapshot_required",
      scope: SCOPE,
      epoch: "epoch-1",
      reason: "gap_too_large",
      currentSeq: 20,
    });
    // Wrong epoch → discarded; floor stays pinned at 6, pending survives.
    client.commitSnapshotBaseline(SCOPE, te("epoch-2"));
    // The correct-epoch commit still lands afterwards → floor 20.
    client.commitSnapshotBaseline(SCOPE, te("epoch-1"));
    client.markSatisfied(SCOPE, ts(21), te("epoch-1")); // 21 === 20 + 1 → floor 21

    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 21 } } },
    ]);
  });

  // ── Replay dispatch: a resume redelivers frames the client never received ────
  // `seq <= received` catches DUPLICATE re-delivery, but a replay-buffer drain
  // also redelivers frames the client NEVER received (a forward-gap fill, a
  // new-during-disconnect frame) — those are `> received`. The transport accepts
  // and dispatches them all in order; idempotency of a replayed frame is the
  // ingestor's job (#1263 (c): authoritative-invalidate, id-dedup), so the
  // transport no longer classifies delivery provenance.

  it("dispatches a forward-gap replay fill in order (resume from the floor)", async () => {
    const { client, ws, delivered } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    client.markSatisfied(SCOPE, ts(6), te("epoch-1")); // floor 6, received 6
    // Forward gap: 8 arrives before 7 → resume from the floor (6), swallow 8.
    fireEvent(ws, SCOPE, 8);
    // Server replays in order. 7 FILLS a seq the client never received; 8
    // (previously swallowed) redelivered; 9 is fresh fanout — all dispatched.
    fireEvent(ws, SCOPE, 7);
    fireEvent(ws, SCOPE, 8);
    fireEvent(ws, SCOPE, 9);

    const evs = delivered.filter((d) => d.frame.type === "event");
    expect(evs.map((d) => d.frame.seq)).toEqual([6, 7, 8, 9]);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 6 } } },
    ]);
  });

  it("dispatches a no-listener resubscribe replay in order (ack does not re-baseline)", async () => {
    const { client, ws, sub, delivered } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    client.markSatisfied(SCOPE, ts(6), te("epoch-1")); // floor 6, received 6
    sub.unsubscribe(); // retains floor 6 + received-water 6
    ws.sent.length = 0;

    // [server publishes 7, 8 while no listener is attached — the no-listener gap.]
    client.subscribe(SCOPE); // subscribe + resume{6}
    // Ack at the current head (8): a retained SAME-epoch floor is present, so a
    // resume was paired. The ack does NOT re-baseline the retained floor.
    ws.fireMessage({ type: "subscribed", scope: SCOPE, epoch: "epoch-1", seq: 8 });

    // Replay of 7, 8 — frames the client NEVER received — plus fresh 9, all dispatched.
    fireEvent(ws, SCOPE, 7);
    fireEvent(ws, SCOPE, 8);
    fireEvent(ws, SCOPE, 9);

    const evs = delivered.filter((d) => d.frame.type === "event");
    expect(evs.map((d) => d.frame.seq)).toEqual([6, 7, 8, 9]);
  });

  it("coalesces multiple gaps into a single resume and dispatches the replay in order", async () => {
    const { client, ws, delivered } = await connectedGated(SCOPE, 5);

    fireEvent(ws, SCOPE, 6);
    client.markSatisfied(SCOPE, ts(6), te("epoch-1")); // floor 6, received 6

    // First gap: 9 → resume from the floor (6), swallow.
    fireEvent(ws, SCOPE, 9);
    // While recovering, a LATER gapped live frame 12 is also swallowed. The resume
    // stays coalesced to the single in-flight one (the `recovering` latch).
    fireEvent(ws, SCOPE, 12);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 6 } } },
    ]);

    // Server replays 7..12 in order, then 13 is fresh fanout — all dispatched.
    for (const seq of [7, 8, 9, 10, 11, 12]) fireEvent(ws, SCOPE, seq);
    fireEvent(ws, SCOPE, 13);

    const evs = delivered.filter((d) => d.frame.type === "event");
    expect(evs.map((d) => d.frame.seq)).toEqual([6, 7, 8, 9, 10, 11, 12, 13]);
    // Still exactly one resume — the latch coalesced both gaps.
    expect(resumes(ws)).toHaveLength(1);
  });

  // ── markSatisfied is not a baseline authority (Safety blocker 2) ──────────────
  // Only the `subscribed` ack and `commitSnapshotBaseline` may ESTABLISH a floor
  // (they reflect the server head). markSatisfied establishing one from an
  // arbitrary applied seq would let the next resume start there, skipping every
  // un-received seq below it (the #1251 skip-unseen-frames hole).

  it("markSatisfied is a no-op with no same-epoch floor (Safety blocker 2)", async () => {
    const client = makeClient();
    const sub = client.subscribe(SCOPE);
    await flushMicrotasks();
    const ws = createdSockets[0]!;
    ws.fireOpen();
    ws.fireMessage({ type: "authenticated", viewerId: "u", epoch: "epoch-1" });
    // NO `subscribed` ack yet → no baseline floor for SCOPE.

    // A stray content-apply must NOT conjure a floor from thin air.
    client.markSatisfied(SCOPE, ts(500), te("epoch-1"));

    // The ack now baselines the floor to the REAL head (5). A resubscribe must
    // resume from 5, never the phantom 500.
    ws.fireMessage({ type: "subscribed", scope: SCOPE, epoch: "epoch-1", seq: 5 });
    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 5 } } },
    ]);
  });

  it("markSatisfied is a no-op when the floor is from a stale epoch (Safety blocker 2)", async () => {
    const { client, ws, sub } = await connectedGated(SCOPE, 5); // floor {epoch-1, 5}

    // A late apply tagged with a NEWER epoch than the current floor must not
    // clobber the floor to an arbitrary seq.
    client.markSatisfied(SCOPE, ts(500), te("epoch-2"));

    sub.unsubscribe();
    ws.sent.length = 0;
    client.subscribe(SCOPE);
    // Floor unchanged at the epoch-1 baseline (5), not epoch-2/500.
    expect(resumes(ws)).toEqual([
      { type: "resume", scopes: { [SCOPE]: { epoch: "epoch-1", lastSeq: 5 } } },
    ]);
  });
});

describe("platform socket construction", () => {
  it("discards a pending old-origin lookup after disconnect and reconnect", async () => {
    const api = await import("./api");
    let finish!: (info: { url: string; mode: "remote" }) => void;
    vi.mocked(api.getBackendInfo).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    );
    const client = new EventWsClient();
    liveClients.push(client);
    client.connect();
    client.disconnect();
    vi.mocked(api.getBackendInfo).mockResolvedValueOnce({
      url: "https://second.example",
      mode: "remote",
    });
    client.connect();
    await flushMicrotasks();
    finish({ url: "https://first.example", mode: "remote" });
    await flushMicrotasks();
    expect(createdSockets).toHaveLength(1);
    expect(createdSockets[0]!.url).toContain("second.example");
  });
});
