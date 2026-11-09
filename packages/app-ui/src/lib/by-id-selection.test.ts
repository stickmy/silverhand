import { describe, expect, it } from "vitest";
import { ApiError } from "@/lib/api";
import { byIdSelection } from "./by-id-selection";

const read = (over: Partial<{ isSuccess: boolean; error: unknown }>) => ({
  isSuccess: false,
  error: null,
  ...over,
});

describe("byIdSelection", () => {
  it("waits while the read is in flight", () => {
    expect(byIdSelection(read({}))).toBe("pending");
  });

  it("keeps a found target", () => {
    expect(byIdSelection(read({ isSuccess: true }))).toBe("kept");
  });

  it("falls back only on a definite 404", () => {
    expect(byIdSelection(read({ error: new ApiError(404, "Not Found", "") }))).toBe("missing");
  });

  it("keeps the target on a server error, a network failure, or a bad body", () => {
    expect(byIdSelection(read({ error: new ApiError(500, "Server Error", "") }))).toBe("kept");
    expect(byIdSelection(read({ error: new TypeError("Failed to fetch") }))).toBe("kept");
    expect(byIdSelection(read({ error: new Error("parse") }))).toBe("kept");
  });
});
