import { describe, expect, it } from "vitest";
import {
  attachmentImageUrl,
  initialAttachmentImageLoad,
  nextAttachmentImageLoad,
  type AttachmentImageEvent,
  type AttachmentImageLoad,
} from "./use-attachment-image";

function run(start: AttachmentImageLoad, events: AttachmentImageEvent[]): AttachmentImageLoad {
  return events.reduce(nextAttachmentImageLoad, start);
}

describe("attachment image load", () => {
  it("shows a cached url without a loading step", () => {
    expect(initialAttachmentImageLoad("blob:a")).toEqual({
      status: "shown",
      url: "blob:a",
      attempt: 1,
    });
    expect(initialAttachmentImageLoad(undefined).status).toBe("loading");
  });

  it("refetches once after an image error, then stops on the second", () => {
    const afterFirst = run(initialAttachmentImageLoad("blob:a"), [{ type: "imageError" }]);
    expect(afterFirst).toEqual({ status: "loading", url: null, attempt: 2 });

    const afterRefetch = run(afterFirst, [{ type: "loaded", url: "blob:b" }]);
    expect(afterRefetch).toEqual({ status: "shown", url: "blob:b", attempt: 2 });

    const terminal = run(afterRefetch, [{ type: "imageError" }]);
    expect(terminal).toEqual({ status: "failed", cause: "decode" });
    expect(attachmentImageUrl(terminal)).toBeNull();
    expect(run(terminal, [{ type: "imageError" }])).toBe(terminal);
  });

  it("re-arms the retry for a new generation and keeps the old picture while loading", () => {
    const shown = run(initialAttachmentImageLoad(undefined), [{ type: "loaded", url: "blob:a" }]);
    const next = run(shown, [{ type: "imageError" }, { type: "loaded", url: "blob:b" }]);
    expect(next).toMatchObject({ attempt: 2 });

    const updated = run(next, [{ type: "generation", cachedUrl: undefined }]);
    expect(updated).toEqual({ status: "loading", url: "blob:b", attempt: 1 });
  });

  it("lets a new generation recover from a decode failure", () => {
    const terminal: AttachmentImageLoad = { status: "failed", cause: "decode" };
    expect(run(terminal, [{ type: "generation", cachedUrl: undefined }])).toEqual({
      status: "loading",
      url: null,
      attempt: 1,
    });
  });

  it("keeps the previous picture when a refetch for a new generation fails", () => {
    const loading: AttachmentImageLoad = { status: "loading", url: "blob:a", attempt: 1 };
    const failed = run(loading, [{ type: "fetchFailed", message: "offline" }]);
    expect(failed).toEqual({ status: "failed", cause: "fetch", message: "offline", url: "blob:a" });
    expect(attachmentImageUrl(failed)).toBe("blob:a");
  });

  it("ignores a late load result once the load has settled", () => {
    const shown: AttachmentImageLoad = { status: "shown", url: "blob:a", attempt: 1 };
    expect(run(shown, [{ type: "loaded", url: "blob:z" }])).toBe(shown);
    expect(run(shown, [{ type: "fetchFailed", message: "x" }])).toBe(shown);
  });
});
