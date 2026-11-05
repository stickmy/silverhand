import { describe, expect, it } from "vitest";
import {
  classifyTopShift,
  getInitialListAnchor,
  getMessageListPreloadDistance,
  INITIAL_BOTTOM_ANCHOR_STATE,
  reduceBottomAnchor,
  shouldFollowBottom,
  shouldRemountForWindowReplacement,
  type BottomAnchorState,
} from "./message-list-window";

// Build the itemKey sequence for a contiguous seq range [from, to].
const range = (from: number, to: number): string[] =>
  Array.from({ length: to - from + 1 }, (_, i) => String(from + i));

describe("message list virtualization helpers", () => {
  it("preserves shared rows when pagination extends the leading activity cluster", () => {
    const previous = ["activity-cluster:100", "message:105", "message:106"];
    expect(
      classifyTopShift(previous, ["activity-cluster:90", "message:105", "message:106"]),
    ).toEqual({ kind: "none" });
    expect(
      classifyTopShift(previous, [
        "message:80",
        "activity-cluster:90",
        "message:105",
        "message:106",
      ]),
    ).toEqual({ kind: "prepend", count: 1 });
    expect(
      classifyTopShift(
        ["message:80", ...previous],
        ["activity-cluster:103", "message:105", "message:106"],
      ),
    ).toEqual({ kind: "head-trim", count: 1 });
  });
  describe("getMessageListPreloadDistance", () => {
    it("requests earlier on touch layouts and caps tall viewports", () => {
      expect(getMessageListPreloadDistance(true, 700)).toBe(1400);
      expect(getMessageListPreloadDistance(false, 700)).toBe(700);
      expect(getMessageListPreloadDistance(true, 3000)).toBe(1600);
      expect(getMessageListPreloadDistance(false, 3000)).toBe(1200);
      expect(getMessageListPreloadDistance(false, 0)).toBe(400);
    });
  });

  describe("getInitialListAnchor", () => {
    it("centers a present jump target over a saved reading position", () => {
      expect(
        getInitialListAnchor({ jumpTargetKey: "m42", restoreKey: "m10", preferBottom: false }),
      ).toEqual({ kind: "key", key: "m42", align: "center" });
    });
    it("restores the saved first visible row when no jump is active", () => {
      expect(
        getInitialListAnchor({ jumpTargetKey: null, restoreKey: "m10", preferBottom: false }),
      ).toEqual({ kind: "key", key: "m10", align: "start" });
    });
    it("opens ordinary mounts at the bottom", () => {
      expect(
        getInitialListAnchor({ jumpTargetKey: null, restoreKey: null, preferBottom: false }),
      ).toEqual({ kind: "bottom" });
    });
    it("lets back-to-bottom win over a still-resolving jump target", () => {
      expect(
        getInitialListAnchor({ jumpTargetKey: "m42", restoreKey: "m10", preferBottom: true }),
      ).toEqual({ kind: "bottom" });
    });
  });

  describe("shouldFollowBottom", () => {
    it("follows only at the real conversation tail with no jump in flight", () => {
      expect(shouldFollowBottom({ hasJumpTarget: false, isAtTail: true })).toBe(true);
      expect(shouldFollowBottom({ hasJumpTarget: true, isAtTail: true })).toBe(false);
      expect(shouldFollowBottom({ hasJumpTarget: false, isAtTail: false })).toBe(false);
    });
  });

  describe("shouldRemountForWindowReplacement", () => {
    it("remounts ordinary latest/window replacements", () => {
      expect(
        shouldRemountForWindowReplacement({
          hasJumpTarget: false,
          jumpTargetIndex: -1,
        }),
      ).toBe(true);
    });

    it("remounts jump windows only after the target is present", () => {
      expect(
        shouldRemountForWindowReplacement({
          hasJumpTarget: true,
          jumpTargetIndex: 12,
        }),
      ).toBe(true);
      expect(
        shouldRemountForWindowReplacement({
          hasJumpTarget: true,
          jumpTargetIndex: -1,
        }),
      ).toBe(false);
    });

    it("remounts a latest reset that still carries a cancelling jump when a bottom anchor is armed", () => {
      // The reported bug: non-tail + active jump → back-to-bottom resets to
      // latest. The fresh latest window can arrive before the cancelled jump
      // prop clears, so it still reads hasJumpTarget=true with the (early)
      // target absent. Without the armed anchor the replacement declines to
      // remount and the window stays parked on the stale around-window
      // position. The armed bottom anchor is one command with the reset, so it
      // forces the remount regardless of the lingering jump.
      expect(
        shouldRemountForWindowReplacement({
          hasJumpTarget: true,
          jumpTargetIndex: -1,
          preferBottom: true,
        }),
      ).toBe(true);
      // Same shape, anchor NOT armed → the original parked behaviour.
      expect(
        shouldRemountForWindowReplacement({
          hasJumpTarget: true,
          jumpTargetIndex: -1,
          preferBottom: false,
        }),
      ).toBe(false);
    });

    it("a spent bottom anchor does not force later unrelated replacements to the tail", () => {
      // After a back-to-bottom lands, the one-shot lifecycle resets the intent
      // (intent → 'default'), so `preferBottom` is false on any LATER,
      // unrelated window replacement. If the user has since scrolled up and
      // re-armed a jump toward off-window history, that replacement must follow
      // the ordinary rules — NOT force a remount to the tail. This is the
      // falsifier for the sticky-anchor leak: with the anchor spent, a jump
      // whose target is still absent parks (waits for the around-window)
      // exactly as an ordinary pending jump does.
      expect(
        shouldRemountForWindowReplacement({
          hasJumpTarget: true,
          jumpTargetIndex: -1,
          preferBottom: false,
        }),
      ).toBe(false);
      // ...and once that jump's target is present, the mount still centers on
      // it — the spent bottom command does not hijack the landing.
      expect(
        getInitialListAnchor({ jumpTargetKey: "m37", restoreKey: null, preferBottom: false }),
      ).toEqual({ kind: "key", key: "m37", align: "center" });
    });
  });

  describe("reduceBottomAnchor — back-to-bottom one-shot lifecycle", () => {
    // Replays the ACTUAL React/cache lifecycle at transition level: the reset
    // branch goes arm → (stale window still shown) →
    // transient empty (resetQueries cleared the cache) → latest rows mount.
    // The intent must survive the stale window AND the empty, then be spent by
    // the real latest mount — never earlier.
    it("arms bottom on a reset request", () => {
      const s = reduceBottomAnchor(INITIAL_BOTTOM_ANCHOR_STATE, {
        type: "arm-reset",
      });
      expect(s.intent).toBe("bottom");
      expect(s.pending).toEqual({ sawEmpty: false });
    });

    it("does NOT consume on the pre-reset stale window (rows-shown before the empty)", () => {
      let s = reduceBottomAnchor(INITIAL_BOTTOM_ANCHOR_STATE, {
        type: "arm-reset",
      });
      // The stale around-window is still non-empty at click time.
      s = reduceBottomAnchor(s, { type: "rows-shown" });
      expect(s.intent).toBe("bottom");
      expect(s.pending).toEqual({ sawEmpty: false });
    });

    it("consumes only after the reset empty, on the real latest mount", () => {
      let s: BottomAnchorState = reduceBottomAnchor(INITIAL_BOTTOM_ANCHOR_STATE, {
        type: "arm-reset",
      });
      // resetQueries clears the cache → transient empty. Still armed.
      s = reduceBottomAnchor(s, { type: "rows-hidden" });
      expect(s.intent).toBe("bottom");
      expect(s.pending).toEqual({ sawEmpty: true });
      // Latest rows mount → the mount reads the armed intent, then this spends.
      s = reduceBottomAnchor(s, { type: "rows-shown" });
      expect(s).toBe(INITIAL_BOTTOM_ANCHOR_STATE);
    });

    it("stays default through LATER unrelated churn after a reset landed", () => {
      let s: BottomAnchorState = reduceBottomAnchor(INITIAL_BOTTOM_ANCHOR_STATE, {
        type: "arm-reset",
      });
      s = reduceBottomAnchor(s, { type: "rows-hidden" });
      s = reduceBottomAnchor(s, { type: "rows-shown" }); // consumed
      // User scrolls up; an unrelated refetch empties + refills. Must NOT
      // re-arm the bottom intent (the sticky-anchor leak).
      s = reduceBottomAnchor(s, { type: "rows-hidden" });
      expect(s.intent).toBe("default");
      s = reduceBottomAnchor(s, { type: "rows-shown" });
      expect(s.intent).toBe("default");
      expect(s.pending).toBeNull();
    });

    it("a fresh jump / channel switch cancels an in-flight request", () => {
      const s = reduceBottomAnchor(
        reduceBottomAnchor(INITIAL_BOTTOM_ANCHOR_STATE, { type: "arm-reset" }),
        { type: "cancel" },
      );
      expect(s).toBe(INITIAL_BOTTOM_ANCHOR_STATE);
    });

    it("is a no-op (same reference) for lifecycle events with no live request", () => {
      // These fire on every rows change / channel mount; a no-op must return
      // the SAME reference so useReducer bails without a re-render.
      expect(reduceBottomAnchor(INITIAL_BOTTOM_ANCHOR_STATE, { type: "rows-shown" })).toBe(
        INITIAL_BOTTOM_ANCHOR_STATE,
      );
      expect(reduceBottomAnchor(INITIAL_BOTTOM_ANCHOR_STATE, { type: "rows-hidden" })).toBe(
        INITIAL_BOTTOM_ANCHOR_STATE,
      );
      expect(reduceBottomAnchor(INITIAL_BOTTOM_ANCHOR_STATE, { type: "cancel" })).toBe(
        INITIAL_BOTTOM_ANCHOR_STATE,
      );
    });
  });

  describe("classifyTopShift", () => {
    it("reports no shift when the top key is unchanged", () => {
      expect(classifyTopShift(range(1, 100), range(1, 100))).toEqual({
        kind: "none",
      });
    });

    it("detects a prepend by the old top moving down", () => {
      // Older rows loaded above: old top (4) now sits at index 3.
      expect(classifyTopShift(range(4, 103), range(1, 103))).toEqual({
        kind: "prepend",
        count: 3,
      });
    });

    it("detects a head-trim: live-appended latest page refetched to fixed size (#1505)", () => {
      // 100 rows → live-appended to 103 → invalidate re-fetches the latest 100,
      // dropping the first 3 head rows. The new top (4) was at index 3 in the
      // old window; the remaining window is contiguous.
      expect(classifyTopShift(range(1, 103), range(4, 103))).toEqual({
        kind: "head-trim",
        count: 3,
      });
    });

    it("treats a disjoint window (latest ↔ around jump) as a replacement", () => {
      expect(classifyTopShift(range(1, 100), range(500, 600))).toEqual({
        kind: "replace",
      });
    });

    it("treats a first-ever window (no previous keys) as a no-op", () => {
      // First data arrival / post-channel-switch: nothing to reconcile, and a
      // spurious replace here would remount on every channel's first load.
      expect(classifyTopShift([], range(1, 100))).toEqual({ kind: "none" });
    });
  });
});
