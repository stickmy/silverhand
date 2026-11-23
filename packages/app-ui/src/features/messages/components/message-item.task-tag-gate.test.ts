import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

// In a public channel, task state is readable by anyone but only members may
// WRITE. The inline task chip (`MessageTaskTag`) therefore gates its status
// picker (the mutation entry) on `canMutateLinkedTask`: a member of an open
// channel gets the interactive "Move to" picker; a non-member (or any read-only
// surface) sees the SAME chip — identical glyph / colour / number / assignee —
// as an inert `<span>` with no mutation path. Reading the task state is public;
// only the write is member-gated.
//
// This is a security write-boundary decision branch + an a11y difference, not
// painting — so it earns a test. The desktop test env is `node` (no
// jsdom/RTL), so — like the sibling fences — we assert the source structure
// rather than render.

const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), "utf8");

describe("MessageTaskTag — status-picker mutation is member-gated", () => {
  const source = read("./message-task-tag.tsx");

  it("has exactly one status mutation entry — the read-only branch cannot write", () => {
    const mutations = source.split("updateStatus.mutate(").length - 1;
    expect(mutations).toBe(1);
  });

  it("only the canMutate branch reaches the status picker", () => {
    const gateAt = source.indexOf("canMutate ? (");
    const mutateAt = source.indexOf("updateStatus.mutate(");
    expect(gateAt).toBeGreaterThan(-1);
    expect(gateAt).toBeLessThan(mutateAt);
    expect(source.indexOf(") : (", mutateAt)).toBeGreaterThan(mutateAt);
  });

  it("exposes status to assistive tech on the read-only cube via real sr-only text", () => {
    expect(source).toContain('<span className="sr-only">{label}</span>');
  });
});

describe("canMutateLinkedTask is threaded on the write axis", () => {
  it("defaults fail-closed in MessageItem and the list", () => {
    expect(read("./message-item.tsx")).toContain("canMutateLinkedTask = false");
    expect(read("../../conversation-shell/components/channel-view/message-list.tsx")).toContain(
      "canMutateLinkedTask = false",
    );
  });

  it("ChannelView gates it on membership write access (gate.allowMutations)", () => {
    expect(read("../../conversation-shell/components/channel-view.tsx")).toContain(
      "canMutateLinkedTask={!closed && !isArchived && gate.allowMutations}",
    );
  });

  it("ThreadPanel gates it on membership write access (gate.allowMutations)", () => {
    expect(read("../../conversation-shell/components/thread-panel.tsx")).toContain(
      "canMutateLinkedTask={!closed && !archived && gate.allowMutations}",
    );
  });
});
