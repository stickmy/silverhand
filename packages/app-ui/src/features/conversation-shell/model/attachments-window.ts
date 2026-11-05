import {
  classifyTopShift,
  FIRST_ITEM_INDEX_START,
} from "@/features/conversation-shell/model/message-list-window";

// ── Attachments grouped-window position tracking ─────────────────────────
//
// `GroupedVirtuoso` tracks scroll by ABSOLUTE item index, so when the loaded
// window shifts we must keep `firstItemIndex` in step or the viewport slides.
// Two distinct axes get conflated if you only diff the row keys:
//
//   1. Query SESSION change — a different dataset entirely (channel, sender
//      filter, time filter, or resolved cutoff). TanStack can serve a cached
//      non-empty page for the new session directly, so this does NOT reliably
//      pass through an empty window, and the two datasets' IDs can overlap.
//      Diffing keys alone would mis-read it as prepend/head-trim/none and
//      inherit the previous session's `firstItemIndex`. A session change must
//      reset the baseline AND remount the list (the consumer keys the list by
//      `mountEpoch`, bumped here) so stale scroll/measurement state is dropped.
//
//   2. In-session window shift — same dataset, rows added above (a realtime
//      top-insert refetches page 0) or trimmed from the head. Here the reader's
//      scroll must be preserved: `classifyTopShift` reports the item delta
//      (groups excluded, per the react-virtuoso grouped contract), which maps
//      straight onto `firstItemIndex`. An in-session DISJOINT window (no
//      overlap) is a real content replacement — treated like a session change
//      (reset + remount), since nothing guarantees a keyset session can't
//      produce one (authoritative cache replacement, bulk delete, recovery).
//
// This reducer is the single decision point for both axes so it can be locked
// by a pure falsifier independent of React/Virtuoso timing.
export type AttachmentsWindowState = {
  sessionKey: string;
  keys: string[];
  firstItemIndex: number;
  // Bumped whenever a fresh mount is required — a query-session change OR an
  // in-session disjoint replacement. The consumer feeds it into the list's
  // React `key` so a new dataset drops the old scroll/measurement queue instead
  // of reusing it.
  mountEpoch: number;
};

export const INITIAL_ATTACHMENTS_WINDOW: AttachmentsWindowState = {
  sessionKey: "",
  keys: [],
  firstItemIndex: FIRST_ITEM_INDEX_START,
  mountEpoch: 0,
};

export function reduceAttachmentsWindow(
  prev: AttachmentsWindowState,
  next: { sessionKey: string; keys: string[] },
): AttachmentsWindowState {
  // Axis 1 — a different query session. Reset the baseline and bump the mount
  // epoch unconditionally: never keyed off key overlap or a transient empty
  // window, both of which a cache-hit filter switch can skip.
  if (next.sessionKey !== prev.sessionKey) {
    return {
      sessionKey: next.sessionKey,
      keys: next.keys,
      firstItemIndex: FIRST_ITEM_INDEX_START,
      mountEpoch: prev.mountEpoch + 1,
    };
  }
  // A transient empty window inside a live session (loading / refetch). Hold the
  // baseline and forget the keys so the next non-empty render re-seeds fresh
  // (classifyTopShift treats an empty prev as a no-op, not a replacement).
  if (next.keys.length === 0) {
    return prev.keys.length === 0 ? prev : { ...prev, keys: [] };
  }
  // Axis 2 — same session, non-empty. Preserve the reader's scroll on a head
  // shift; treat a genuinely disjoint window as a replacement.
  const shift = classifyTopShift(prev.keys, next.keys);
  switch (shift.kind) {
    case "prepend":
      return {
        ...prev,
        keys: next.keys,
        firstItemIndex: prev.firstItemIndex - shift.count,
      };
    case "head-trim":
      return {
        ...prev,
        keys: next.keys,
        firstItemIndex: prev.firstItemIndex + shift.count,
      };
    case "replace":
      // A disjoint window WITHIN the same session — the API/cache layer does NOT
      // guarantee this can't happen: an authoritative cache replacement, a bulk
      // delete / visibility change, or a recovery path can hand back a
      // non-empty window with no overlap. Content replacement is real evidence
      // regardless of session identity, so treat it exactly like a session
      // change: reset the baseline and bump the mount epoch so stale
      // scroll/measurement state is dropped, never carried onto the new rows.
      return {
        sessionKey: prev.sessionKey,
        keys: next.keys,
        firstItemIndex: FIRST_ITEM_INDEX_START,
        mountEpoch: prev.mountEpoch + 1,
      };
    // 'none' — top unchanged (append below / no change): keep the baseline.
    default:
      return prev.keys === next.keys ? prev : { ...prev, keys: next.keys };
  }
}
