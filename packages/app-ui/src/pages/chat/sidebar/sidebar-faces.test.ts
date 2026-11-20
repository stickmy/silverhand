import { describe, expect, it } from "vitest";
import { foldPairs, unfoldOrder, type RowFace } from "./sidebar-faces";

const face = (title: string): RowFace => ({ glyph: null, title, badge: null });
const faces = (...ids: string[]) => new Map(ids.map((id) => [id, face(id)]));
const id = (row: string) => row;

describe("foldPairs", () => {
  it("folds a pair whose halves sit in two lists into the left half's list", () => {
    const pairs = [{ left: "dm", right: "ch" }];
    const listed = faces("dm", "ch", "other");

    expect(foldPairs(["dm"], id, pairs, listed)).toEqual([
      { row: "dm", pair: { pair: pairs[0], right: listed.get("ch") } },
    ]);
    expect(foldPairs(["ch", "other"], id, pairs, listed)).toEqual([{ row: "other", pair: null }]);
  });

  it("keeps both rows when one half is not listed anywhere", () => {
    const pairs = [{ left: "a", right: "b" }];
    expect(foldPairs(["b"], id, pairs, faces("b"))).toEqual([{ row: "b", pair: null }]);
  });
});

describe("unfoldOrder", () => {
  it("puts a same-list right half after its left", () => {
    expect(unfoldOrder(["x", "a"], ["a", "b", "x"], [{ left: "a", right: "b" }])).toEqual([
      "x",
      "a",
      "b",
    ]);
  });

  it("leaves a right half held by another list out", () => {
    expect(unfoldOrder(["a", "x"], ["a", "x"], [{ left: "a", right: "dm" }])).toEqual(["a", "x"]);
  });
});
