import { describe, expect, it } from "vitest";
import {
  INITIAL_ATTACHMENTS_WINDOW,
  reduceAttachmentsWindow,
  type AttachmentsWindowState,
} from "./attachments-window";
import { FIRST_ITEM_INDEX_START } from "./message-list-window";

// itemKey sequence for a contiguous id range [from, to].
const range = (from: number, to: number): string[] =>
  Array.from({ length: to - from + 1 }, (_, i) => String(from + i));

// Seed a settled state for a session by feeding it its first non-empty window.
const seed = (sessionKey: string, keys: string[]): AttachmentsWindowState =>
  reduceAttachmentsWindow(
    { ...INITIAL_ATTACHMENTS_WINDOW, sessionKey, keys: [] },
    { sessionKey, keys },
  );

describe("reduceAttachmentsWindow", () => {
  describe("in-session head shift (Axis 2)", () => {
    it("shifts firstItemIndex DOWN by the item count on a realtime prepend, no remount", () => {
      // A top-insert refetch adds 2 newer rows above the window head.
      const prev = seed("s1", range(10, 100));
      const next = reduceAttachmentsWindow(prev, {
        sessionKey: "s1",
        keys: range(8, 100),
      });
      expect(next.firstItemIndex).toBe(prev.firstItemIndex - 2);
      expect(next.mountEpoch).toBe(prev.mountEpoch); // NOT remounted
    });

    it("counts ITEMS only on a cross-day prepend (groups are irrelevant to the delta)", () => {
      // The grouping is a pure display concern; whether the 3 new rows land in a
      // brand-new date group or an existing one, the react-virtuoso grouped
      // contract wants firstItemIndex moved by the ITEM count (3), never the
      // group count. The reducer diffs flat item keys, so this holds by
      // construction — this test is the guard against a future "count groups"
      // regression.
      const prev = seed("s1", range(20, 100));
      const next = reduceAttachmentsWindow(prev, {
        sessionKey: "s1",
        keys: range(17, 100), // 3 new items prepended, any grouping
      });
      expect(next.firstItemIndex).toBe(prev.firstItemIndex - 3);
      expect(next.mountEpoch).toBe(prev.mountEpoch);
    });

    it("shifts firstItemIndex UP on an in-session head-trim, no remount", () => {
      const prev = seed("s1", range(1, 103));
      const next = reduceAttachmentsWindow(prev, {
        sessionKey: "s1",
        keys: range(4, 103), // 3 head rows dropped
      });
      expect(next.firstItemIndex).toBe(prev.firstItemIndex + 3);
      expect(next.mountEpoch).toBe(prev.mountEpoch);
    });

    it("holds the baseline on an append/no top change", () => {
      const prev = seed("s1", range(1, 100));
      const next = reduceAttachmentsWindow(prev, {
        sessionKey: "s1",
        keys: range(1, 130), // older rows appended below; top unchanged
      });
      expect(next.firstItemIndex).toBe(prev.firstItemIndex);
      expect(next.mountEpoch).toBe(prev.mountEpoch);
    });

    it("forgets keys across a transient empty window so a refill is not mis-read", () => {
      const prev = seed("s1", range(1, 100));
      const empty = reduceAttachmentsWindow(prev, { sessionKey: "s1", keys: [] });
      // Baseline held; keys forgotten (empty prev ⇒ the refill re-seeds fresh
      // rather than being classified against the pre-empty window).
      expect(empty.firstItemIndex).toBe(prev.firstItemIndex);
      expect(empty.mountEpoch).toBe(prev.mountEpoch);
      expect(empty.keys).toEqual([]);
      const refill = reduceAttachmentsWindow(empty, {
        sessionKey: "s1",
        keys: range(1, 100),
      });
      expect(refill.firstItemIndex).toBe(prev.firstItemIndex);
      expect(refill.mountEpoch).toBe(prev.mountEpoch);
    });

    it("treats an in-session DISJOINT window as a replacement: reset baseline + remount", () => {
      // A disjoint window can arrive WITHIN one session (authoritative cache
      // replacement, bulk delete / visibility change, recovery path). Session
      // identity must NOT suppress this content-replacement evidence — it resets
      // the baseline and bumps the mount epoch just like a session change, so
      // stale scroll/measurement is dropped. First move the baseline off START
      // via a real prepend so the reset is observable.
      const seeded = seed("s1", range(10, 100));
      const prepended = reduceAttachmentsWindow(seeded, {
        sessionKey: "s1",
        keys: range(8, 100), // 2 prepended → firstItemIndex below START
      });
      expect(prepended.firstItemIndex).toBeLessThan(FIRST_ITEM_INDEX_START);
      const disjoint = reduceAttachmentsWindow(prepended, {
        sessionKey: "s1", // SAME session
        keys: range(500, 560), // no overlap → replacement
      });
      expect(disjoint.firstItemIndex).toBe(FIRST_ITEM_INDEX_START);
      expect(disjoint.mountEpoch).toBe(prepended.mountEpoch + 1);
    });
  });

  describe("query-session change (Axis 1) — reset + remount", () => {
    it("resets the baseline and REMOUNTS on a filter switch even when IDs overlap and no empty window intervenes", () => {
      // The core falsifier: a cached filter session serves non-empty rows
      // directly (no transient empty) whose IDs OVERLAP the previous session.
      // Diffing keys alone would read this as a prepend/head-trim and inherit
      // the stale firstItemIndex; the session-key axis must force a fresh mount.
      const seeded = seed("sender:all", range(10, 100));
      // A real prepend pushes the baseline BELOW START, so a naive inherit of
      // the old firstItemIndex would be observable as ≠ START after the switch.
      const shifted = reduceAttachmentsWindow(seeded, {
        sessionKey: "sender:all",
        keys: range(7, 100), // 3 prepended
      });
      expect(shifted.firstItemIndex).toBe(FIRST_ITEM_INDEX_START - 3);
      const next = reduceAttachmentsWindow(shifted, {
        sessionKey: "sender:alice", // different session
        keys: range(20, 90), // overlapping IDs, non-empty, no empty in between
      });
      expect(next.sessionKey).toBe("sender:alice");
      expect(next.firstItemIndex).toBe(FIRST_ITEM_INDEX_START);
      expect(next.mountEpoch).toBe(shifted.mountEpoch + 1); // fresh mount
    });

    it("remounts on a session change that resolves through an empty window too", () => {
      const prev = seed("s1", range(1, 100));
      const loading = reduceAttachmentsWindow(prev, {
        sessionKey: "s2",
        keys: [],
      });
      expect(loading.sessionKey).toBe("s2");
      expect(loading.firstItemIndex).toBe(FIRST_ITEM_INDEX_START);
      expect(loading.mountEpoch).toBe(prev.mountEpoch + 1);
      // Data for s2 lands: same session now, seeds fresh (no spurious shift).
      const landed = reduceAttachmentsWindow(loading, {
        sessionKey: "s2",
        keys: range(200, 260),
      });
      expect(landed.firstItemIndex).toBe(FIRST_ITEM_INDEX_START);
      expect(landed.mountEpoch).toBe(loading.mountEpoch);
    });
  });
});
