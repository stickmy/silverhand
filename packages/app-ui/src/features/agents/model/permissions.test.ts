import { describe, expect, it } from "vitest";
import type { AgentDto, RegistrationRequired, UserDto } from "@silverhand/shared";
import {
  canEditAgentProfile,
  canManageAgentSchedules,
  canViewAgentFiles,
  canRemoveAgentFromChannel,
} from "./permissions";

const agent = (overrides: Partial<AgentDto> = {}): AgentDto =>
  ({
    id: "a-1",
    host_id: "h-1",
    name: "alice",
    display_name: "Alice",
    runtime: "claude",
    description: null,
    model: null,
    reasoning_effort: null,
    service_tier: null,
    avatar_url: null,
    created_by_user_id: "u-creator",
    created_at: 0,
    updated_at: 0,
    ...overrides,
  }) as AgentDto;

const viewer = (overrides: Partial<UserDto> = {}): UserDto =>
  ({
    id: "u-creator",
    name: "shiki",
    display_name: "shiki",
    avatar_url: null,
    created_at: 0,
    updated_at: 0,
    ...overrides,
  }) as UserDto;

const registrationRequired: RegistrationRequired = {
  registration_required: true,
  suggested_name: "user-1234",
};

describe("canEditAgentProfile", () => {
  it("returns true when the viewer is the creator", () => {
    expect(
      canEditAgentProfile(viewer({ id: "u-creator" }), agent({ created_by_user_id: "u-creator" })),
    ).toBe(true);
  });

  it("returns false when the viewer is not the creator", () => {
    expect(
      canEditAgentProfile(viewer({ id: "u-other" }), agent({ created_by_user_id: "u-creator" })),
    ).toBe(false);
  });

  it("returns false during pre-registration / registration_required state", () => {
    // Pre-registration shell never has edit rights — the registration
    // gate covers the app, but we still default-deny rather than rely
    // on the gate as a side effect.
    expect(canEditAgentProfile(registrationRequired, agent())).toBe(false);
  });

  it("returns false while /api/me is still loading (viewer undefined)", () => {
    // Conservatively read-only until the query resolves; flips to the
    // editable form on the next render once the dto arrives.
    expect(canEditAgentProfile(undefined, agent())).toBe(false);
  });

  it("does not surface an agent-self path — only the creator branch is modeled", () => {
    // The renderer never holds an agent-self principal; even if some
    // future code shape compares "viewer.id === agent.id", the helper
    // must NOT treat that as edit permission. Profile-field self-edit
    // is intentionally not shipped (Cindy PR-2/PR-3 guardrail).
    expect(
      canEditAgentProfile(
        viewer({ id: "a-1" }),
        agent({ id: "a-1", created_by_user_id: "u-creator" }),
      ),
    ).toBe(false);
  });
});

describe("canManageAgentSchedules", () => {
  it("mirrors the owner-only backend canEditAgent gate for the human schedules surface", () => {
    expect(
      canManageAgentSchedules(
        viewer({ id: "u-creator" }),
        agent({ created_by_user_id: "u-creator" }),
      ),
    ).toBe(true);
    expect(
      canManageAgentSchedules(
        viewer({ id: "u-other" }),
        agent({ created_by_user_id: "u-creator" }),
      ),
    ).toBe(false);
  });

  it("defaults closed before the viewer resolves or while pre-registration is active", () => {
    expect(canManageAgentSchedules(undefined, agent())).toBe(false);
    expect(canManageAgentSchedules(registrationRequired, agent())).toBe(false);
  });
});

describe("canViewAgentFiles", () => {
  it("mirrors the owner-only backend gate for L1/L2 agent workspace files", () => {
    expect(
      canViewAgentFiles(viewer({ id: "u-creator" }), agent({ created_by_user_id: "u-creator" })),
    ).toBe(true);
    expect(
      canViewAgentFiles(viewer({ id: "u-other" }), agent({ created_by_user_id: "u-creator" })),
    ).toBe(false);
  });

  it("defaults closed before the viewer resolves or while pre-registration is active", () => {
    expect(canViewAgentFiles(undefined, agent())).toBe(false);
    expect(canViewAgentFiles(registrationRequired, agent())).toBe(false);
  });
});

describe("canRemoveAgentFromChannel", () => {
  // Mirrors the backend remove-from-conversation gate (#task:957, plan A):
  // agent owner (creator) OR workspace owner/admin. No "channel owner".
  it("returns true for the agent creator regardless of workspace role", () => {
    // Creator branch short-circuits before role is even consulted — so a
    // creator who is only a plain member (or whose role hasn't resolved)
    // still gets the control.
    expect(
      canRemoveAgentFromChannel(
        viewer({ id: "u-creator" }),
        agent({ created_by_user_id: "u-creator" }),
        "member",
      ),
    ).toBe(true);
    expect(
      canRemoveAgentFromChannel(
        viewer({ id: "u-creator" }),
        agent({ created_by_user_id: "u-creator" }),
        null,
      ),
    ).toBe(true);
  });

  it("returns true for a non-creator workspace owner or admin", () => {
    expect(
      canRemoveAgentFromChannel(
        viewer({ id: "u-other" }),
        agent({ created_by_user_id: "u-creator" }),
        "owner",
      ),
    ).toBe(true);
    expect(
      canRemoveAgentFromChannel(
        viewer({ id: "u-other" }),
        agent({ created_by_user_id: "u-creator" }),
        "admin",
      ),
    ).toBe(true);
  });

  it("returns false for a non-creator plain member", () => {
    expect(
      canRemoveAgentFromChannel(
        viewer({ id: "u-other" }),
        agent({ created_by_user_id: "u-creator" }),
        "member",
      ),
    ).toBe(false);
  });

  it("returns false for a non-creator while the role is still unresolved", () => {
    // `useWorkspaceMemberLists` returns undefined viewerRole until it
    // lands; default-deny the wider affordance until then (the creator
    // branch is unaffected — see above).
    expect(
      canRemoveAgentFromChannel(
        viewer({ id: "u-other" }),
        agent({ created_by_user_id: "u-creator" }),
        undefined,
      ),
    ).toBe(false);
    expect(
      canRemoveAgentFromChannel(
        viewer({ id: "u-other" }),
        agent({ created_by_user_id: "u-creator" }),
        null,
      ),
    ).toBe(false);
  });

  it("returns false during pre-registration even for an owner", () => {
    expect(canRemoveAgentFromChannel(registrationRequired, agent(), "owner")).toBe(false);
  });

  it("returns false while /api/me is still loading (viewer undefined)", () => {
    expect(canRemoveAgentFromChannel(undefined, agent(), "owner")).toBe(false);
  });
});
