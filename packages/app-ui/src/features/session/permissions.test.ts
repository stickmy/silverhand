import { describe, expect, it } from "vitest";
import type { RegistrationRequired, UserDto } from "@silverhand/shared";
import { canEditUserProfile, isRegistrationRequired } from "./permissions";

function makeViewer(overrides: Partial<UserDto> = {}): UserDto {
  return {
    id: "u-self",
    name: "self",
    display_name: "Self",
    avatar_url: null,
    created_at: 0,
    updated_at: 0,
    ...overrides,
  } as UserDto;
}

describe("canEditUserProfile", () => {
  it("returns true when the viewer id matches the target user", () => {
    expect(canEditUserProfile(makeViewer(), { id: "u-self" })).toBe(true);
  });

  it("returns false for a different user id (v2 multi-human guard)", () => {
    expect(canEditUserProfile(makeViewer(), { id: "u-other" })).toBe(false);
  });

  it("returns false when the viewer query has not resolved yet", () => {
    expect(canEditUserProfile(undefined, { id: "u-self" })).toBe(false);
  });

  it("returns false when registration is still required", () => {
    const pre: RegistrationRequired = {
      registration_required: true,
      suggested_name: "user-1234",
    };
    expect(canEditUserProfile(pre, { id: "u-self" })).toBe(false);
  });
});

describe("isRegistrationRequired", () => {
  it("accepts only an explicit `registration_required: true`", () => {
    expect(isRegistrationRequired({ registration_required: true, suggested_name: "" })).toBe(true);
    expect(isRegistrationRequired(makeViewer())).toBe(false);
  });

  it("rejects objects without a literal `registration_required: true`", () => {
    const bodies: object[] = [
      {},
      { registration_required: "true" },
      { registration_required: 1 },
      { registration_required: false },
    ];
    for (const body of bodies) {
      expect(isRegistrationRequired(body)).toBe(false);
    }
  });
});
