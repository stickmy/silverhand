import { useLayoutEffect, useSyncExternalStore, type ReactNode } from "react";
import type { RowBadge } from "@/features/attention/attention-mode";
import type { SplitPair } from "@/features/conversation-shell/split-pairs";

/**
 * The face each sidebar list gives the conversations it lists, readable by the
 * other lists. A pair can join conversations from two lists (a DM beside a
 * channel), and its row stands where its left conversation is listed, so that
 * list needs the right half's face, and the right half's own list needs to know
 * the pair row took it.
 */
export interface RowFace {
  glyph: ReactNode;
  title: string;
  badge: RowBadge | null;
  /** Changes when `glyph` would draw differently (an avatar landed). */
  glyphKey?: string;
}

const byList = new Map<string, ReadonlyMap<string, RowFace>>();
let merged = new Map<string, RowFace>();
const listeners = new Set<() => void>();

function signature(faces: ReadonlyMap<string, RowFace>): string {
  let out = "";
  for (const [id, face] of faces) {
    out += `${id}\u0000${face.title}\u0000${face.badge?.count ?? ""}\u0000${face.badge?.tier ?? ""}\u0000${face.glyphKey ?? ""}\u0001`;
  }
  return out;
}

const signatures = new Map<string, string>();

function publish(): void {
  merged = new Map();
  for (const faces of byList.values()) {
    for (const [id, face] of faces) merged.set(id, face);
  }
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

const snapshot = () => merged;

/** Publish a list's faces for as long as it is mounted. */
export function useRegisterRowFaces(list: string, faces: ReadonlyMap<string, RowFace>): void {
  useLayoutEffect(() => {
    const next = signature(faces);
    byList.set(list, faces);
    if (signatures.get(list) !== next) {
      signatures.set(list, next);
      publish();
    }
  });

  useLayoutEffect(
    () => () => {
      byList.delete(list);
      signatures.delete(list);
      publish();
    },
    [list],
  );
}

/** Every listed conversation's face, across the sidebar's lists. */
export function useRowFaces(): ReadonlyMap<string, RowFace> {
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}

/** A row in a list that folds kept pairs. */
export type FoldedRow<T> = {
  row: T;
  /** Set on the row a pair takes over: the pair and its right half's face. */
  pair: { pair: SplitPair; right: RowFace } | null;
};

/**
 * Fold kept pairs into one list's rows. A pair's row stands in its left
 * conversation's slot, wherever that is listed; the right conversation's own
 * row is dropped when the left is listed somewhere. Halves that are not both
 * listed stay plain rows.
 */
export function foldPairs<T>(
  rows: readonly T[],
  idOf: (row: T) => string,
  pairs: readonly SplitPair[],
  faces: ReadonlyMap<string, RowFace>,
): FoldedRow<T>[] {
  const out: FoldedRow<T>[] = [];
  for (const row of rows) {
    const id = idOf(row);
    const pair = pairs.find((p) => p.left === id || p.right === id);
    const right = pair ? faces.get(pair.right) : undefined;
    const bothListed = pair !== undefined && right !== undefined && faces.has(pair.left);
    if (!bothListed) {
      out.push({ row, pair: null });
    } else if (pair.left === id) {
      out.push({ row, pair: { pair, right } });
    }
  }
  return out;
}

/** Expand a reordered list of folded rows back to every row id: a pair's right
 *  half, when this list holds it, goes right after its left. */
export function unfoldOrder(
  orderedIds: readonly string[],
  allIds: readonly string[],
  pairs: readonly SplitPair[],
): string[] {
  const listed = new Set(allIds);
  const placed = new Set(orderedIds);
  return orderedIds.flatMap((id) => {
    const pair = pairs.find((p) => p.left === id);
    return pair && listed.has(pair.right) && !placed.has(pair.right) ? [id, pair.right] : [id];
  });
}
