import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { createInterestRegistry } from "./interest-registry";

// A recording transport double: every subscribe/unsubscribe is logged so tests
// assert the EXACT transport call sequence the registry produces (P1's
// behavior-preserving contract lives on this sequence).
function recordingTransport() {
  const subscribed: string[] = [];
  const unsubscribed: string[] = [];
  const subscribe = (scope: string) => {
    subscribed.push(scope);
    return {
      unsubscribe() {
        unsubscribed.push(scope);
      },
    };
  };
  return { subscribe, subscribed, unsubscribed };
}

describe("InterestRegistry — refcount pass-through (#task:1260 P1)", () => {
  it("0→1 subscribes once; last release unsubscribes once", () => {
    const t = recordingTransport();
    const reg = createInterestRegistry(t.subscribe);

    const h = reg.retain("conversation:c1", "holder-a");
    expect(t.subscribed).toEqual(["conversation:c1"]);
    expect(t.unsubscribed).toEqual([]);

    h.release();
    expect(t.unsubscribed).toEqual(["conversation:c1"]);
  });

  it("N holders share ONE transport subscription; unsubscribe only on the last", () => {
    const t = recordingTransport();
    const reg = createInterestRegistry(t.subscribe);

    const a = reg.retain("conversation:c1", "holder-a");
    const b = reg.retain("conversation:c1", "holder-b");
    const c = reg.retain("conversation:c1", "holder-c");
    expect(t.subscribed).toEqual(["conversation:c1"]); // one subscribe for three holders

    a.release();
    b.release();
    expect(t.unsubscribed).toEqual([]); // still one holder left

    c.release();
    expect(t.unsubscribed).toEqual(["conversation:c1"]); // now the last
  });

  it("distinct scopes are independent (scope change releases only the old)", () => {
    const t = recordingTransport();
    const reg = createInterestRegistry(t.subscribe);

    const a = reg.retain("conversation:A", "holder-x");
    const b = reg.retain("conversation:B", "holder-x"); // same token, different scope
    expect(t.subscribed).toEqual(["conversation:A", "conversation:B"]);

    a.release();
    expect(t.unsubscribed).toEqual(["conversation:A"]); // B still held
    b.release();
    expect(t.unsubscribed).toEqual(["conversation:A", "conversation:B"]);
  });

  it("snapshot attributes holders to scopes (leak attribution)", () => {
    const t = recordingTransport();
    const reg = createInterestRegistry(t.subscribe);
    reg.retain("conversation:c1", "holder-a");
    reg.retain("conversation:c1", "holder-b");
    reg.retain("conversation-side:c1", "holder-a");

    const snap = reg.snapshot();
    expect(new Set(snap.get("conversation:c1"))).toEqual(new Set(["holder-a", "holder-b"]));
    expect(new Set(snap.get("conversation-side:c1"))).toEqual(new Set(["holder-a"]));
  });
});

describe("InterestRegistry — churn / generation-safety (#task:1260 P1, Cindy Q3)", () => {
  it("release() is idempotent per handle (no double unsubscribe)", () => {
    const t = recordingTransport();
    const reg = createInterestRegistry(t.subscribe);

    const h = reg.retain("conversation:c1", "holder-a");
    h.release();
    h.release(); // StrictMode-style double cleanup
    h.release();
    expect(t.unsubscribed).toEqual(["conversation:c1"]); // exactly one
  });

  it("generation-safe: a STALE handle release must not evict the fresh holder", () => {
    // The load-bearing edge (Alice's failure mode): same token retained twice
    // (gen1, gen2). If the stale gen1 release evicted the holder, the set would
    // empty and we would unsubscribe a still-watched live scope → silent
    // no-listener gap.
    const t = recordingTransport();
    const reg = createInterestRegistry(t.subscribe);

    const gen1 = reg.retain("conversation:c1", "holder-a");
    const gen2 = reg.retain("conversation:c1", "holder-a"); // same token, re-retain
    expect(t.subscribed).toEqual(["conversation:c1"]); // still one subscription

    gen1.release(); // STALE — must be a no-op for eviction
    expect(t.unsubscribed).toEqual([]); // fresh holder (gen2) survives
    expect(new Set(reg.snapshot().get("conversation:c1"))).toEqual(new Set(["holder-a"]));

    gen2.release(); // the current holder → now it really unsubscribes
    expect(t.unsubscribed).toEqual(["conversation:c1"]);
  });

  it("generation-safe regardless of release order (fresh-first then stale)", () => {
    const t = recordingTransport();
    const reg = createInterestRegistry(t.subscribe);

    const gen1 = reg.retain("conversation:c1", "holder-a");
    const gen2 = reg.retain("conversation:c1", "holder-a");

    gen2.release(); // current generation → evicts + unsubscribes
    expect(t.unsubscribed).toEqual(["conversation:c1"]);

    gen1.release(); // stale + scope already gone → no throw, no second unsubscribe
    expect(t.unsubscribed).toEqual(["conversation:c1"]);
  });

  it("a stale handle cannot evict a holder re-retained after its eviction", () => {
    const t = recordingTransport();
    const reg = createInterestRegistry(t.subscribe);

    const a = reg.retain("conversation:c1", "holder-a");
    const b = reg.retain("conversation:c1", "holder-a");
    b.release();
    const c = reg.retain("conversation:c1", "holder-a");

    a.release();
    expect(t.unsubscribed).toEqual(["conversation:c1"]);
    expect(new Set(reg.snapshot().get("conversation:c1"))).toEqual(new Set(["holder-a"]));

    c.release();
    expect(t.unsubscribed).toEqual(["conversation:c1", "conversation:c1"]);
  });

  it("n→0→n resubscribe uses a FRESH subscription (borrows no old state)", () => {
    // After a full release, re-retaining must call the transport subscribe again
    // (a fresh WsScopeSubscription that resumes from the retained floor) — the
    // registry keeps no leftover entry, so it can never reuse a dropped
    // satisfiedPending backlog. Mirrors the transport drop/retain (`80aec14`)
    // at the registry layer.
    const t = recordingTransport();
    const reg = createInterestRegistry(t.subscribe);

    const a1 = reg.retain("conversation:c1", "holder-a");
    a1.release();
    expect(reg.snapshot().has("conversation:c1")).toBe(false); // entry fully gone

    const a2 = reg.retain("conversation:c1", "holder-a");
    expect(t.subscribed).toEqual(["conversation:c1", "conversation:c1"]); // TWO subscribes
    a2.release();
    expect(t.unsubscribed).toEqual(["conversation:c1", "conversation:c1"]); // TWO unsubscribes
  });

  it("enabled-toggle parity (null↔scope): false→true→false→true matches the raw useEffect", () => {
    // useChannelThreads' `enabled` gate maps to useInterest(null↔scope): each
    // enable retains (subscribe), each disable releases → n→0 → unsubscribe. The
    // registry sequence is byte-identical to the direct `useEffect` it replaced,
    // and every re-enable is a FRESH subscribe (transport resumes from the
    // retained floor → frames missed while disabled replay, #1251 — unchanged).
    // So `enabled` is a bandwidth gate, not a correctness boundary.
    const t = recordingTransport();
    const reg = createInterestRegistry(t.subscribe);
    const token = "threads-reader";
    reg.retain("conversation:c1", token).release(); // enable → disable
    reg.retain("conversation:c1", token).release(); // re-enable → disable
    expect(t.subscribed).toEqual(["conversation:c1", "conversation:c1"]);
    expect(t.unsubscribed).toEqual(["conversation:c1", "conversation:c1"]);
  });
});

// Source fence (node env, no RTL — mirrors realtime-ingestion-read-inert.test.ts):
// the registry owns SUBSCRIPTION LIFETIME ONLY. It must never grow floor /
// backlog / cache / read logic — those belong to the transport (ws-client) and
// the ingestor. If any of these symbols appear here, the "registry is inert
// except for subscribe/unsubscribe" invariant has regressed.
describe("InterestRegistry — source fence: subscription-lifetime only (#task:1260 P1)", () => {
  const source = readFileSync(
    fileURLToPath(new URL("./interest-registry.ts", import.meta.url)),
    "utf8",
  );

  it("is the caller of subscribeWsScope (sanity — fencing the live module)", () => {
    expect(source).toContain("subscribeWsScope");
  });

  it("useInterest treats a null scope as no interest (enabled-gate form, #1260 P2)", () => {
    // A call site's `enabled`/conditional gate maps to `useInterest(null)`; the
    // hook must declare NO interest for null (no retain) — so a disabled reader
    // routes to release → the transport unsubscribe (drops pending, RETAINS
    // floor), and re-enable fresh-subscribes + resume-replays (#1251). This is
    // what makes `enabled` a bandwidth gate, not a correctness boundary.
    expect(source).toContain("if (!scope) return");
  });

  it("never writes the resume floor / snapshot baseline", () => {
    for (const sym of ["markSatisfied", "commitSnapshotBaseline", "recordPosition"]) {
      expect(source).not.toContain(sym);
    }
  });

  it("never writes React Query caches (not a cache mutator)", () => {
    for (const sym of ["invalidateQueries", "setQueryData", "removeQueries"]) {
      expect(source).not.toContain(sym);
    }
  });

  it("never writes viewer read state", () => {
    for (const sym of ["/api/notifications/read", "conversation_reads", "markMessageChannelRead"]) {
      expect(source).not.toContain(sym);
    }
  });

  it("does not touch the viewer scope in v1 (Q2 — dedicated entry deferred)", () => {
    expect(source).not.toContain("subscribeViewerScope");
  });
});
