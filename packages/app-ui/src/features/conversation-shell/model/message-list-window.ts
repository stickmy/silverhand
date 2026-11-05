export const FIRST_ITEM_INDEX_START = 1_000_000;

// How far from the top the next older page is requested. Touch scrolling
// needs an earlier network head start. Distances follow the actual list
// viewport (including keyboard/rotation), capped for very tall windows.
export function getMessageListPreloadDistance(compact: boolean, viewportHeight: number) {
  const height = Math.max(0, viewportHeight);
  const clamp = (value: number, min: number, max: number) => Math.min(max, Math.max(min, value));
  return compact ? clamp(height * 2, 400, 1600) : clamp(height, 400, 1200);
}

export type InitialListAnchor =
  | { kind: "bottom" }
  | { kind: "key"; key: string; align: "start" | "center" };

// The anchor is consumed only when the list mounts. Bottom is the right
// default for ordinary channel opens, but it is wrong while a saved-message
// jump is pending: a non-tail around window has a local bottom that can be
// hours behind the real conversation tail.
export function getInitialListAnchor(args: {
  jumpTargetKey: string | null;
  restoreKey: string | null;
  // A back-to-bottom reset was just requested. It must win over a
  // still-resolving jump target: the parent clears the jump prop
  // asynchronously, so without this the fresh latest window would re-center
  // on the jump target and the command would appear to do nothing.
  preferBottom: boolean;
}): InitialListAnchor {
  if (args.preferBottom) {
    return { kind: "bottom" };
  }
  if (args.jumpTargetKey) {
    return { kind: "key", key: args.jumpTargetKey, align: "center" };
  }
  if (args.restoreKey) {
    return { kind: "key", key: args.restoreKey, align: "start" };
  }
  return { kind: "bottom" };
}

// A disjoint window replacement remounts the list so it opens at its own
// anchor. During a jump we wait until the replacement actually contains the
// target, otherwise the remount would read the ordinary bottom fallback.
export function shouldRemountForWindowReplacement(args: {
  hasJumpTarget: boolean;
  jumpTargetIndex: number;
  // A back-to-bottom re-anchor is armed. Non-tail back-to-bottom issues an
  // authoritative latest reset; when the fresh window arrives the jump prop
  // may not have cleared yet (the parent clears it asynchronously). Without
  // this, the replacement would see `hasJumpTarget && target absent` and
  // decline to remount, so the new window would keep the stale around-window
  // position. The armed anchor is one command with the reset, so it forces
  // the remount regardless of jump state.
  preferBottom?: boolean;
}): boolean {
  if (args.preferBottom) return true;
  return !args.hasJumpTarget || args.jumpTargetIndex >= 0;
}

// ── Top-of-window shift classification (#task:1505) ──────────────────────
//
// When the loaded window's first row changes, three shapes must be told apart:
// prepends and head-trims keep the reader's place; only a disjoint window
// remounts the list:
//
//   • prepend    — older rows loaded above the old top (fetchPreviousPage). The
//                  old top is still present, deeper in the window.
//   • head-trim  — rows dropped from the HEAD while the rest of the window stays
//                  contiguous. This is the #1505 case: the latest cache page was
//                  live-appended PAST the page limit, so a refetch re-fetches the
//                  fixed-size latest window and trims the surplus head rows. The
//                  old top vanishes, but the reader's visible anchor (below the
//                  trimmed head) survives — so we must preserve position, NOT
//                  treat it as a disjoint replacement.
//   • replace    — a genuinely disjoint window (latest ↔ around-message jump):
//                  no overlap with the previous window.
//
// Overlap decides the shape: if the old top still appears in the new window it
// prepended; else if the new top still appears in the OLD window the head was
// trimmed. If a boundary cluster changes its key, compare the surviving rows
// before declaring the windows disjoint. Classification relies on the query's
// chronological row order (index lookup, no full contiguity proof).
export type TopShift =
  | { kind: "none" }
  | { kind: "prepend"; count: number }
  | { kind: "head-trim"; count: number }
  | { kind: "replace" };

export function classifyTopShift(prevKeys: string[], keys: string[]): TopShift {
  // Nothing observed yet (first data arrival / post-channel-switch): there is no
  // prior anchor to reconcile. The mount itself owns initial positioning, so
  // this must be a no-op — treating it as a replacement would remount spuriously.
  if (keys.length === 0 || prevKeys.length === 0) return { kind: "none" };
  const prevTop = prevKeys[0]!;
  const newTop = keys[0]!;
  if (prevTop === newTop) return { kind: "none" };
  // Old top still below the new head → rows prepended above it.
  const prevTopInNew = keys.indexOf(prevTop);
  if (prevTopInNew > 0) return { kind: "prepend", count: prevTopInNew };
  // New top still inside the old window → the head was trimmed.
  const newTopInPrev = prevKeys.indexOf(newTop);
  if (newTopInPrev > 0) return { kind: "head-trim", count: newTopInPrev };
  // A system-activity cluster is keyed by its first message. Loading older
  // events can extend that cluster and change its key, even though the rows
  // after it still overlap. Preserve those rows instead of remounting at tail.
  const previousIndexes = new Map(prevKeys.map((key, index) => [key, index]));
  for (let index = 0; index < keys.length; index++) {
    const previousIndex = previousIndexes.get(keys[index]!);
    if (previousIndex === undefined) continue;
    const delta = index - previousIndex;
    if (delta > 0) return { kind: "prepend", count: delta };
    if (delta < 0) return { kind: "head-trim", count: -delta };
    return { kind: "none" };
  }
  return { kind: "replace" };
}

// The list's bottom is local to the loaded window. Follow new rows only at
// the real conversation tail; otherwise incoming rows can pull the reader
// away from a saved-message target.
export function shouldFollowBottom(args: { hasJumpTarget: boolean; isAtTail: boolean }): boolean {
  return !args.hasJumpTarget && args.isAtTail;
}

// ── Back-to-bottom reset lifecycle ───────────────────────────────────────
//
// When the loaded window is behind the tail, back-to-bottom resets the query
// to the latest window and arms a one-shot bottom intent that the fresh
// window's mount consumes, then reverts to 'default' so later, unrelated
// window churn (a refetch after the reader scrolled up) does not drag them
// back to the tail. `resetQueries` clears the cache to [] BEFORE the latest
// window lands, so the list unmounts and re-mounts naturally. Consumption
// cannot fire on the first non-empty render, because the PRE-reset stale
// around-window is still non-empty at click time: it waits for the transient
// empty (`rows-hidden`) and then the real latest mount (`rows-shown`).
export type BottomAnchorIntent = "default" | "bottom";

export type BottomAnchorState = {
  intent: BottomAnchorIntent;
  // `sawEmpty` latches once the reset's transient empty has been observed, so
  // the following non-empty mount is known to be the fresh latest window.
  pending: null | { sawEmpty: boolean };
};

export type BottomAnchorEvent =
  | { type: "arm-reset" }
  // The list is not showing rows (empty cache / loading skeleton).
  | { type: "rows-hidden" }
  // The list is showing real rows.
  | { type: "rows-shown" }
  // A fresh jump or channel switch overrides any in-flight request.
  | { type: "cancel" };

export const INITIAL_BOTTOM_ANCHOR_STATE: BottomAnchorState = {
  intent: "default",
  pending: null,
};

// Pure transition. Returns the SAME reference on a no-op so `useReducer`
// bails without a re-render (these events fire on every rows change).
export function reduceBottomAnchor(
  state: BottomAnchorState,
  event: BottomAnchorEvent,
): BottomAnchorState {
  switch (event.type) {
    case "arm-reset":
      return { intent: "bottom", pending: { sawEmpty: false } };
    case "rows-hidden":
      if (!state.pending || state.pending.sawEmpty) return state;
      return { intent: "bottom", pending: { sawEmpty: true } };
    case "rows-shown":
      // A request that has not yet seen empty is still looking at the
      // pre-reset stale window — leave it armed.
      if (!state.pending?.sawEmpty) return state;
      return INITIAL_BOTTOM_ANCHOR_STATE;
    case "cancel":
      if (state.intent === "default" && state.pending === null) return state;
      return INITIAL_BOTTOM_ANCHOR_STATE;
    default:
      return state;
  }
}
