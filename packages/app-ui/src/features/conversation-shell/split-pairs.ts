import { useSyncExternalStore } from "react";

/**
 * Split-view pairs: two conversations the viewer reads side by side. The first
 * id is the left column (the route's conversation), the second the right
 * column (`?split`). A conversation belongs to at most one pair, so pairing it
 * again moves it out of its old pair.
 *
 * Viewer-local and identity-bound like the rest of place memory: kept in
 * localStorage, wiped on logout by `clearPlaceMemory`.
 */
export interface SplitPair {
  left: string;
  right: string;
}

export const SPLIT_PAIRS_KEY = "silverhand:layout.split-pairs";

function parsePairs(raw: unknown): SplitPair[] {
  if (!Array.isArray(raw)) return [];
  const out: SplitPair[] = [];
  for (const entry of raw) {
    if (!entry || typeof entry !== "object") continue;
    const { left, right } = entry as Record<string, unknown>;
    if (typeof left !== "string" || typeof right !== "string" || !left || !right) continue;
    if (left === right) continue;
    out.push({ left, right });
  }
  return out;
}

function load(): SplitPair[] {
  try {
    const raw = localStorage.getItem(SPLIT_PAIRS_KEY);
    return raw ? parsePairs(JSON.parse(raw)) : [];
  } catch {
    return [];
  }
}

let pairs: readonly SplitPair[] = load();
const listeners = new Set<() => void>();

function commit(next: readonly SplitPair[]): void {
  pairs = next;
  try {
    if (next.length === 0) localStorage.removeItem(SPLIT_PAIRS_KEY);
    else
      localStorage.setItem(
        SPLIT_PAIRS_KEY,
        JSON.stringify(next.map(({ left, right }) => ({ left, right }))),
      );
  } catch {
    // Quota / disabled storage: the in-memory list still holds.
  }
  for (const listener of listeners) listener();
}

/** `pairs` with `left`/`right` paired. An existing pair holding either id is
 *  replaced in place, so re-pairing keeps the row where the viewer left it. */
export function withPair(current: readonly SplitPair[], pair: SplitPair): readonly SplitPair[] {
  const { left, right } = pair;
  if (left === right) return current;
  if (current.some((p) => p.left === left && p.right === right)) return current;
  const next: SplitPair[] = [];
  let placed = false;
  for (const p of current) {
    const overlaps = p.left === left || p.right === left || p.left === right || p.right === right;
    if (!overlaps) {
      next.push(p);
    } else if (!placed) {
      next.push(pair);
      placed = true;
    }
  }
  if (!placed) next.push(pair);
  return next;
}

/** The pair holding `conversationId`, if any. */
export function pairOf(
  current: readonly SplitPair[],
  conversationId: string | null,
): SplitPair | null {
  if (!conversationId) return null;
  return current.find((p) => p.left === conversationId || p.right === conversationId) ?? null;
}

/** `pairs` without any pair holding `conversationId`. */
export function withoutConversation(
  current: readonly SplitPair[],
  conversationId: string,
): readonly SplitPair[] {
  const next = current.filter(
    (pair) => pair.left !== conversationId && pair.right !== conversationId,
  );
  return next.length === current.length ? current : next;
}

export function getSplitPairs(): readonly SplitPair[] {
  return pairs;
}

export function recordSplitPair(pair: SplitPair): void {
  const next = withPair(pairs, pair);
  if (next !== pairs) commit(next);
}

export function dissolveSplitPair(conversationId: string): void {
  const next = withoutConversation(pairs, conversationId);
  if (next !== pairs) commit(next);
}

export function clearSplitPairs(): void {
  commit([]);
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

const snapshot = (): readonly SplitPair[] => pairs;

export function useSplitPairs(): readonly SplitPair[] {
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}
