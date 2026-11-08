import { describe, expect, it } from "vitest";
import type { CachedMessage } from "./pending-message";
import {
  trimMessagesWindow,
  type MessagesCache,
  type MessagesPageParam,
} from "./message-cache-core";

const row = (id: string) => ({ id }) as CachedMessage;

function cache(pages: string[][], params: MessagesPageParam[]): MessagesCache {
  return {
    pages: pages.map((ids, index) => ({
      messages: ids.map(row),
      reply_counts_by_root: {},
      has_more_before: index === 0 ? false : true,
      has_more_after: false,
      stream_head_seq: 0 as never,
    })),
    pageParams: params,
  };
}

const ids = (data: MessagesCache | undefined) =>
  data?.pages.map((page) => page.messages.map((m) => m.id));

describe("trimMessagesWindow", () => {
  it("leaves a window within its cap untouched", () => {
    const data = cache([["a", "b"], ["c"]], [{ kind: "before", seq: 9 }, undefined]);
    expect(trimMessagesWindow(data, 3, "latest")).toBe(data);
  });

  it("keeping the latest drops the oldest rows and reopens history above", () => {
    const data = cache([["a", "b", "c"]], [undefined]);
    const trimmed = trimMessagesWindow(data, 2, "latest");
    expect(ids(trimmed)).toEqual([["b", "c"]]);
    expect(trimmed?.pages[0]?.has_more_before).toBe(true);
    expect(trimmed?.pages[0]?.has_more_after).toBe(false);
  });

  it("removes pages the trim empties, with their params", () => {
    const params: MessagesPageParam[] = [{ kind: "before", seq: 9 }, undefined];
    const trimmed = trimMessagesWindow(
      cache(
        [
          ["a", "b"],
          ["c", "d", "e"],
        ],
        params,
      ),
      2,
      "latest",
    );
    expect(ids(trimmed)).toEqual([["d", "e"]]);
    expect(trimmed?.pageParams).toEqual([undefined]);
    expect(trimmed?.pages[0]?.has_more_before).toBe(true);
  });

  it("keeping the earliest drops the newest rows and takes the window off the tail", () => {
    const params: MessagesPageParam[] = [{ kind: "before", seq: 9 }, undefined];
    const trimmed = trimMessagesWindow(
      cache(
        [
          ["a", "b"],
          ["c", "d"],
        ],
        params,
      ),
      3,
      "earliest",
    );
    expect(ids(trimmed)).toEqual([["a", "b"], ["c"]]);
    expect(trimmed?.pages[1]?.has_more_after).toBe(true);
    expect(trimmed?.pageParams).toEqual(params);
  });
});
