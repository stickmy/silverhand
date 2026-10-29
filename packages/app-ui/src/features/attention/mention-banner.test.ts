import { describe, expect, it } from "vitest";
import { readableExcerpt } from "./mention-banner";

describe("readableExcerpt", () => {
  it("reads references and locators as the names their chips show", () => {
    expect(
      readableExcerpt(
        "<silverhand://agent/Norman> see <silverhand://workspace/silverhand/ui%20polish> in <silverhand://workspace/silverhand>",
      ),
    ).toBe("@Norman see #ui polish in silverhand");
  });

  it("keeps anything it cannot parse as written", () => {
    expect(readableExcerpt("<silverhand://nope/x> and plain <b>")).toBe(
      "<silverhand://nope/x> and plain <b>",
    );
  });

  it("keeps a DM locator as written", () => {
    expect(readableExcerpt("<silverhand://dm/a/b>")).toBe("<silverhand://dm/a/b>");
  });
});
