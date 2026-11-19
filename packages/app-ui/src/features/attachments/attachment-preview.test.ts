import { describe, expect, it } from "vitest";
import { attachmentPreviewKind } from "./attachment-preview";

describe("attachmentPreviewKind", () => {
  it("classifies an explicit text/markdown type", () => {
    expect(attachmentPreviewKind("text/markdown", "notes.md")).toBe("markdown");
  });

  it("classifies an explicit text/html type", () => {
    expect(attachmentPreviewKind("text/html", "mockup.html")).toBe("html");
  });

  it("drops a charset parameter before matching", () => {
    expect(attachmentPreviewKind("text/html; charset=utf-8", "a.html")).toBe("html");
  });

  it("falls back to the filename suffix for a legacy octet-stream row", () => {
    // Old rows persist application/octet-stream even for .md/.html — a
    // content-type-only check would miss every one of them.
    expect(attachmentPreviewKind("application/octet-stream", "readme.md")).toBe("markdown");
    expect(attachmentPreviewKind("application/octet-stream", "page.html")).toBe("html");
    expect(attachmentPreviewKind("application/octet-stream", "page.htm")).toBe("html");
    expect(attachmentPreviewKind("application/octet-stream", "doc.markdown")).toBe("markdown");
  });

  it("falls back to the suffix when the type is empty", () => {
    expect(attachmentPreviewKind("", "notes.md")).toBe("markdown");
  });

  it("lets a non-generic stored type win over a mismatched suffix", () => {
    // A file explicitly served as text/plain must NOT be sandbox-rendered as
    // HTML just because it is named .html.
    expect(attachmentPreviewKind("text/plain", "trick.html")).toBeNull();
  });

  it("is null for a non-previewable type", () => {
    expect(attachmentPreviewKind("image/png", "shot.png")).toBeNull();
    expect(attachmentPreviewKind("application/pdf", "doc.pdf")).toBeNull();
  });

  it("is null for a generic type with an unknown suffix", () => {
    expect(attachmentPreviewKind("application/octet-stream", "archive.bin")).toBeNull();
  });
});
