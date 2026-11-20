import { describe, expect, it } from "vitest";
import {
  EMPTY_PANES,
  closePane,
  openPane,
  paneOpenIn,
  parsePaneQuery,
  promoteSplit,
  setSplit,
  writePaneQuery,
  type PaneQuery,
} from "./pane-query";

const q = (search: string, channelId: string | null = "c1"): PaneQuery =>
  parsePaneQuery(new URLSearchParams(search), channelId);
const write = (panes: PaneQuery, search = ""): string => {
  const params = new URLSearchParams(search);
  writePaneQuery(params, panes);
  return params.toString();
};

describe("pane query", () => {
  it("round-trips every pane and keeps unrelated params in place", () => {
    const search =
      "utm=1&split=c2&ctx=split&thread=r&files=1&attachment=a&task=t&settings=c1&settingsPage=add&fullscreen=1";
    expect(write(q(search), search)).toBe(search);
  });

  it("drops a settings page without settings, and ownership without a split", () => {
    expect(q("?settingsPage=add").settingsPage).toBeNull();
    expect(q("?ctx=split&thread=r").owner).toBe("primary");
    expect(write(q("?ctx=split&thread=r"))).toBe("thread=r");
  });

  it("an overlay keeps the restore stack; a replace pane clears it", () => {
    const base = q("?thread=r&side=s");
    expect(openPane(base, "task", "t")).toMatchObject({ task: "t", thread: "r", side: null });
    expect(openPane(q("?thread=r&task=t"), "side", "s")).toMatchObject({
      side: "s",
      thread: null,
      task: null,
    });
  });

  it("opening a column pane for the other column replaces that column's panes", () => {
    const panes = openPane(q("?split=c2&thread=r1&files=1"), "threads", true, { column: "split" });
    expect(panes).toMatchObject({ owner: "split", threads: true, thread: null, files: false });
  });

  it("closing the last column pane hands ownership back to the first column", () => {
    expect(closePane(q("?split=c2&ctx=split&thread=r"), "thread").owner).toBe("primary");
    expect(closePane(q("?split=c2&ctx=split&thread=r&files=1"), "files").owner).toBe("split");
  });

  it("a column pane opened without a split always belongs to the first column", () => {
    expect(openPane(EMPTY_PANES, "thread", "r", { column: "split" }).owner).toBe("primary");
  });

  it("reports a pane as open only in the column that owns it", () => {
    const panes = q("?split=c2&ctx=split&threads=1&task=t");
    expect(paneOpenIn(panes, "threads", "split")).toBe(true);
    expect(paneOpenIn(panes, "threads", "primary")).toBe(false);
    expect(paneOpenIn(panes, "task", "primary")).toBe(true);
  });

  it("replacing or promoting the second column moves only the panes it owned", () => {
    expect(setSplit(q("?split=c2&ctx=split&thread=r"), "c3")).toMatchObject({
      split: "c3",
      thread: null,
    });
    expect(setSplit(q("?split=c2&thread=r"), null)).toMatchObject({ split: null, thread: "r" });
    expect(promoteSplit(q("?split=c2&ctx=split&thread=r"))).toMatchObject({
      split: null,
      owner: "primary",
      thread: "r",
    });
    expect(promoteSplit(q("?split=c2&thread=r")).thread).toBeNull();
  });
});
