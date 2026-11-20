import { useSyncExternalStore } from "react";

/**
 * Conversation pairs: two conversations the viewer reads side by side. `left`
 * is the route's conversation, `right` its first conversation pane. A
 * conversation belongs to at most one pair, so pairing it again moves it out
 * of its old pair. The storage key keeps its first name so kept pairs survive.
 *
 * Viewer-local and identity-bound like the rest of place memory: kept in
 * localStorage, wiped on logout by `clearPlaceMemory`.
 */
export interface ConversationPair {
  left: string;
  right: string;
}

export const CONVERSATION_PAIRS_KEY = "silverhand:layout.split-pairs";

function parsePairs(raw: unknown): ConversationPair[] {
  if (!Array.isArray(raw)) return [];
  const out: ConversationPair[] = [];
  for (const entry of raw) {
    if (!entry || typeof entry !== "object") continue;
    const { left, right } = entry as Record<string, unknown>;
    if (typeof left !== "string" || typeof right !== "string" || !left || !right) continue;
    if (left === right) continue;
    out.push({ left, right });
  }
  return out;
}

function load(): ConversationPair[] {
  try {
    const raw = localStorage.getItem(CONVERSATION_PAIRS_KEY);
    return raw ? parsePairs(JSON.parse(raw)) : [];
  } catch {
    return [];
  }
}

let pairs: readonly ConversationPair[] = load();
const listeners = new Set<() => void>();

function commit(next: readonly ConversationPair[]): void {
  pairs = next;
  try {
    if (next.length === 0) localStorage.removeItem(CONVERSATION_PAIRS_KEY);
    else
      localStorage.setItem(
        CONVERSATION_PAIRS_KEY,
        JSON.stringify(next.map(({ left, right }) => ({ left, right }))),
      );
  } catch {
    // Quota / disabled storage: the in-memory list still holds.
  }
  for (const listener of listeners) listener();
}

/** `pairs` with `left`/`right` paired. An existing pair holding either id is
 *  replaced in place, so re-pairing keeps the row where the viewer left it. */
export function withPair(
  current: readonly ConversationPair[],
  pair: ConversationPair,
): readonly ConversationPair[] {
  const { left, right } = pair;
  if (left === right) return current;
  if (current.some((p) => p.left === left && p.right === right)) return current;
  const next: ConversationPair[] = [];
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
  current: readonly ConversationPair[],
  conversationId: string | null,
): ConversationPair | null {
  if (!conversationId) return null;
  return current.find((p) => p.left === conversationId || p.right === conversationId) ?? null;
}

/** `pairs` without any pair holding `conversationId`. */
export function withoutConversation(
  current: readonly ConversationPair[],
  conversationId: string,
): readonly ConversationPair[] {
  const next = current.filter(
    (pair) => pair.left !== conversationId && pair.right !== conversationId,
  );
  return next.length === current.length ? current : next;
}

export function getConversationPairs(): readonly ConversationPair[] {
  return pairs;
}

export function recordConversationPair(pair: ConversationPair): void {
  const next = withPair(pairs, pair);
  if (next !== pairs) commit(next);
}

export function dissolveConversationPair(conversationId: string): void {
  const next = withoutConversation(pairs, conversationId);
  if (next !== pairs) commit(next);
}

export function clearConversationPairs(): void {
  commit([]);
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

const snapshot = (): readonly ConversationPair[] => pairs;

export function useConversationPairs(): readonly ConversationPair[] {
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}
