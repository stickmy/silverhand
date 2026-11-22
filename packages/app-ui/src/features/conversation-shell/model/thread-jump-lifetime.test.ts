import { describe, expect, it } from "vitest";
import { threadJumpOutlivedPane } from "./thread-jump-lifetime";

const a = { conversationId: "c", rootId: "a" };
const b = { conversationId: "c", rootId: "b" };
const jumpA = { channelId: "c", threadRootId: "a" };

describe("threadJumpOutlivedPane", () => {
  it("ends a thread jump when its pane closes or switches to another thread", () => {
    expect(threadJumpOutlivedPane(jumpA, [a], [])).toBe(true);
    expect(threadJumpOutlivedPane(jumpA, [a], [b])).toBe(true);
  });

  it("keeps a jump whose pane has not opened yet or is still open", () => {
    expect(threadJumpOutlivedPane(jumpA, [], [])).toBe(false);
    expect(threadJumpOutlivedPane(jumpA, [], [a])).toBe(false);
    expect(threadJumpOutlivedPane(jumpA, [a], [a])).toBe(false);
  });

  it("leaves jumps aimed elsewhere alone", () => {
    expect(threadJumpOutlivedPane({ channelId: "c", threadRootId: "b" }, [a], [])).toBe(false);
    expect(threadJumpOutlivedPane({ channelId: "c", threadRootId: null }, [a], [])).toBe(false);
    expect(threadJumpOutlivedPane(null, [a], [])).toBe(false);
  });
});
