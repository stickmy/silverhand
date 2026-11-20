import { beforeEach, describe, expect, it } from "vitest";
import {
  clearConversationPairs,
  dissolveConversationPair,
  getConversationPairs,
  pairOf,
  recordConversationPair,
} from "./conversation-pairs";

describe("conversation pairs", () => {
  beforeEach(() => {
    clearConversationPairs();
  });

  it("keeps several pairs and finds a pair from either half", () => {
    recordConversationPair({ left: "a", right: "b" });
    recordConversationPair({ left: "c", right: "d" });

    expect(getConversationPairs()).toHaveLength(2);
    expect(pairOf(getConversationPairs(), "b")).toEqual({ left: "a", right: "b" });
    expect(pairOf(getConversationPairs(), "x")).toBeNull();
  });

  it("a conversation belongs to one pair: re-pairing it replaces its old pair in place", () => {
    recordConversationPair({ left: "a", right: "b" });
    recordConversationPair({ left: "c", right: "d" });
    recordConversationPair({ left: "a", right: "d" });

    expect(getConversationPairs()).toEqual([{ left: "a", right: "d" }]);
  });

  it("dissolving through either half removes the pair", () => {
    recordConversationPair({ left: "a", right: "b" });
    dissolveConversationPair("b");

    expect(getConversationPairs()).toEqual([]);
  });
});
