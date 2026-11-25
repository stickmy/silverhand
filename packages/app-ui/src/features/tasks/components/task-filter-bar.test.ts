import { describe, expect, it } from "vitest";

import { buildChannelFilterOptions, type ChannelFilterCandidate } from "./task-filter-bar";
import { workspaceChannelCandidates } from "@/features/tasks/hooks/use-task-filter-options";

function candidate(over: Partial<ChannelFilterCandidate> & { id: string }): ChannelFilterCandidate {
  return {
    workspaceId: "ws-default",
    title: `title-${over.id}`,
    archived_at: null,
    fromAgentSpace: false,
    ...over,
  };
}

describe("buildChannelFilterOptions", () => {
  it("maps a plain channel to a channel option (title is the label)", () => {
    const out = buildChannelFilterOptions([candidate({ id: "c1", title: "General" })]);
    expect(out).toEqual([
      { id: "c1", workspaceId: "ws-default", label: "General", group: "workspace" },
    ]);
  });

  it("skips archived channels — this is the archived-channel leak being fixed", () => {
    const out = buildChannelFilterOptions([
      candidate({ id: "c1" }),
      candidate({ id: "c2", archived_at: 1700000000000 }),
    ]);
    expect(out.map((o) => o.id)).toEqual(["c1"]);
  });

  it("dedups by id, first occurrence wins", () => {
    const out = buildChannelFilterOptions([
      candidate({ id: "c1", title: "First", sublabel: "ws-a" }),
      candidate({ id: "c1", title: "Second", sublabel: "ws-b" }),
    ]);
    expect(out).toEqual([
      {
        id: "c1",
        workspaceId: "ws-default",
        label: "First",
        group: "workspace",
        sublabel: "ws-a",
      },
    ]);
  });

  it("carries the owning workspaceId through for the cascade (#task:1387)", () => {
    const out = buildChannelFilterOptions([
      candidate({ id: "c1", workspaceId: "ws-eng" }),
      candidate({ id: "c2", workspaceId: "ws-design" }),
    ]);
    expect(out.map((o) => o.workspaceId)).toEqual(["ws-eng", "ws-design"]);
  });

  // One determinant, one test. There used to be two — one keyed on
  // `roster_policy === 'fixed'`, one on the container — because the group read
  // both. The roster disjunct is gone (#task:2077): roster is a capability
  // fact, the group is a container question, and a fixed-roster channel in a
  // people-owned workspace belongs under Workspaces.
  it("files a channel from an agent space under the agent-space group", () => {
    const out = buildChannelFilterOptions([candidate({ id: "c1", fromAgentSpace: true })]);
    expect(out[0]?.group).toBe("agent_space");
  });

  it("omits sublabel when none is provided (single-workspace surface)", () => {
    const out = buildChannelFilterOptions([candidate({ id: "c1" })]);
    expect(out[0]).not.toHaveProperty("sublabel");
  });

  it("preserves input order so a pre-sorted caller keeps its order", () => {
    const out = buildChannelFilterOptions([
      candidate({ id: "b" }),
      candidate({ id: "a" }),
      candidate({ id: "c" }),
    ]);
    expect(out.map((o) => o.id)).toEqual(["b", "a", "c"]);
  });
});

describe("workspaceChannelCandidates — channel picker label/subtitle rule", () => {
  const ch = (id: string, title: string, over: Partial<{ archived_at: number | null }> = {}) => ({
    id,
    title,
    archived_at: over.archived_at ?? null,
  });

  it("people-owned workspace channel: title is the label, workspace name is the subtitle", () => {
    const out = workspaceChannelCandidates(
      { id: "ws-eng", name: "Engineering", owner_agent_id: null },
      [ch("c1", "general")],
    );
    expect(out[0]).toMatchObject({
      id: "c1",
      workspaceId: "ws-eng",
      title: "general",
      sublabel: "Engineering",
      fromAgentSpace: false,
    });
  });

  it("an agent's multiple channels stay distinct: titles differ, subtitle shares the agent name (the #1165 bug)", () => {
    // Alice/Cindy/Norman each own `Mentions` + `Onboarding`; before the fix
    // both rendered as the same `Alice` label. Now the channel title is the
    // label, the agent name the subtitle — so the rows are distinguishable.
    // The name arrives bare — an agent space's row name IS the agent's handle
    // (#task:2066 retired the `dm-` list prefix).
    const out = workspaceChannelCandidates(
      { id: "ws-alice", name: "Alice", owner_agent_id: "agent-alice" },
      [ch("c1", "Mentions"), ch("c2", "Onboarding")],
    );
    expect(out.map((c) => c.title)).toEqual(["Mentions", "Onboarding"]);
    expect(out.every((c) => c.sublabel === "Alice")).toBe(true);
    // No two rows collide on (label, subtitle).
    const built = buildChannelFilterOptions(out);
    const keys = built.map((o) => `${o.label}::${o.sublabel}`);
    expect(new Set(keys).size).toBe(built.length);
    expect(built.map((o) => o.label)).toEqual(["Mentions", "Onboarding"]);
  });

  it("a single-channel agent space still carries the agent-name subtitle", () => {
    const out = workspaceChannelCandidates(
      { id: "ws-judy", name: "Judy", owner_agent_id: "agent-judy" },
      [ch("c1", "Onboarding")],
    );
    expect(out[0]).toMatchObject({ title: "Onboarding", sublabel: "Judy" });
  });
});
