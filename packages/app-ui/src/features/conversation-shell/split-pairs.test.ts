import { beforeEach, describe, expect, it } from "vitest";
import {
  clearSplitPairs,
  dissolveSplitPair,
  getSplitPairs,
  pairOf,
  recordSplitPair,
} from "./split-pairs";

describe("split pairs", () => {
  beforeEach(() => {
    clearSplitPairs();
  });

  it("keeps several pairs and finds a pair from either half", () => {
    recordSplitPair({ left: "a", right: "b" });
    recordSplitPair({ left: "c", right: "d" });

    expect(getSplitPairs()).toHaveLength(2);
    expect(pairOf(getSplitPairs(), "b")).toEqual({ left: "a", right: "b" });
    expect(pairOf(getSplitPairs(), "x")).toBeNull();
  });

  it("a conversation belongs to one pair: re-pairing it replaces its old pair in place", () => {
    recordSplitPair({ left: "a", right: "b" });
    recordSplitPair({ left: "c", right: "d" });
    recordSplitPair({ left: "a", right: "d" });

    expect(getSplitPairs()).toEqual([{ left: "a", right: "d" }]);
  });

  it("dissolving through either half removes the pair", () => {
    recordSplitPair({ left: "a", right: "b" });
    dissolveSplitPair("b");

    expect(getSplitPairs()).toEqual([]);
  });
});
