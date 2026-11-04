import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

// #task:1267 slice B — a pending optimistic message row has no server id/seq, so
// every affordance that needs a real server row (reaction, thread, save, share,
// copy, select) must NOT be rendered on it. The action-surface handlers keep a
// `isCommittedMessage` guard as defense-in-depth, but a no-op'd handler behind a
// still-visible button is a dead affordance (Cindy slice-B review) — and the
// top-corner ReactionPicker had no handler guard at all, so an ungated pending
// row would fire `toggleReaction.mutate({ messageId: 'pending-<uuid>' })` at the
// backend (the pre-split server-reject path). The fix gates affordance RENDERING
// on `committed` in MessageItem.
//
// The desktop test env is `node` (no jsdom/RTL), so — like
// `realtime-ingestion-read-inert` / `notification-toast-purity` — we statically
// fence the source rather than render.

describe("MessageItem — pending rows render no server-row affordance (#task:1267)", () => {
  const source = readFileSync(
    fileURLToPath(new URL("./message-item.tsx", import.meta.url)),
    "utf8",
  );

  it("computes the committed gate from the message", () => {
    expect(source).toContain("const committed = isCommittedMessage(message)");
  });

  it("gates the top-corner action cluster (reaction / thread / save) on committed", () => {
    // The toolbar (ReactionPicker + Thread + Save) is wrapped in
    // `{committed ? ( <div …track…> … ) : null}`. Assert the wrap sits
    // immediately before its track so a pending row renders none of them.
    expect(source.replace(/\s+/g, " ")).toContain(
      '{committed ? ( <div className="pointer-events-none absolute inset-y-0 right-2',
    );
  });

  it("gates selection checkbox + row-toggle capture on committed", () => {
    // Both the capture handler and the checkbox render are `selectionMode &&
    // committed` — a pending row shows no checkbox and does not toggle-select.
    const occurrences = source.split("selectionMode && committed").length - 1;
    expect(occurrences).toBeGreaterThanOrEqual(2);
  });

  it("gates every context-menu callback on committed", () => {
    for (const cb of [
      "committed && onCopyMarkdown",
      "committed && onSave",
      "committed && onShare",
      "committed && onUnfollowThread",
    ]) {
      expect(source).toContain(cb);
    }
  });
});
