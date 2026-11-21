import { useEffect, useId } from "react";
import { subscribeWsScope, type WsScopeSubscription } from "@/lib/ws-client";

export type InterestScope = Parameters<typeof subscribeWsScope>[0];
export type HolderToken = string;

export interface InterestHandle {
  /** Idempotent + generation-safe (see below). */
  release(): void;
}

interface ScopeEntry {
  // Exactly one transport subscription per scope, created on 0→1.
  subscription: WsScopeSubscription;
  // token → the holder's CURRENT generation. A generation lets a stale handle
  // (same token re-retained after it) be told apart from the fresh holder, so a
  // late/duplicate `release()` can never evict a live holder.
  holders: Map<HolderToken, number>;
}

export interface InterestRegistry {
  retain(scope: InterestScope, holder: HolderToken): InterestHandle;
  /** Diagnostic view — scope → its current holder tokens (leak attribution). */
  snapshot(): ReadonlyMap<InterestScope, ReadonlySet<HolderToken>>;
}

/**
 * Factory so tests get an isolated registry and can inject the subscribe
 * primitive. The app uses the {@link interestRegistry} singleton.
 */
export function createInterestRegistry(
  subscribe: (scope: InterestScope) => WsScopeSubscription = subscribeWsScope,
): InterestRegistry {
  const scopes = new Map<InterestScope, ScopeEntry>();
  // Registry-wide, never reset: a holder evicted and retained again must not
  // get back a generation an old handle still carries.
  let nextGeneration = 0;

  function retain(scope: InterestScope, holder: HolderToken): InterestHandle {
    let entry = scopes.get(scope);
    if (!entry) {
      // 0→1: the registry is the sole caller of the transport subscribe for
      // this scope group; create the single per-scope subscription now.
      entry = { subscription: subscribe(scope), holders: new Map() };
      scopes.set(scope, entry);
    }
    const generation = ++nextGeneration;
    entry.holders.set(holder, generation);

    let released = false;
    return {
      release() {
        if (released) return; // idempotent per handle
        released = true;
        const e = scopes.get(scope);
        if (!e) return;
        // Generation-safe: only evict this holder if THIS handle's generation is
        // still the holder's current one. If the same token was re-retained
        // after this handle (generation moved on), a stale release must NOT
        // evict the fresh holder — otherwise the holder set could empty and we
        // would unsubscribe a still-watched live scope (drop satisfiedPending +
        // real unsubscribe → silent no-listener gap). This is correctness, not
        // counting hygiene.
        if (e.holders.get(holder) !== generation) return;
        e.holders.delete(holder);
        if (e.holders.size === 0) {
          // n→0: the ONLY path to a transport unsubscribe → the existing
          // last-unsubscribe backlog-drop (`80aec14`) runs. Zero backlog logic
          // here.
          e.subscription.unsubscribe();
          scopes.delete(scope);
        }
      },
    };
  }

  function snapshot(): ReadonlyMap<InterestScope, ReadonlySet<HolderToken>> {
    const out = new Map<InterestScope, ReadonlySet<HolderToken>>();
    for (const [scope, entry] of scopes) {
      out.set(scope, new Set(entry.holders.keys()));
    }
    return out;
  }

  return { retain, snapshot };
}

/** App-wide singleton, wrapping the real transport subscribe. */
export const interestRegistry: InterestRegistry = createInterestRegistry();

/**
 * Declare interest in `scope` for the lifetime of this hook instance, keyed by a
 * stable per-instance token (so a re-render never double-counts and a leak is
 * attributable). `null` scope = no interest (e.g. no channel open). This is the
 * pass-through replacement for
 * `useEffect(() => { const s = subscribeWsScope(x); return () => s.unsubscribe(); }, [x])`.
 */
export function useInterest(scope: InterestScope | null): void {
  const token = useId();
  useEffect(() => {
    if (!scope) return;
    const handle = interestRegistry.retain(scope, token);
    return () => handle.release();
  }, [scope, token]);
}
