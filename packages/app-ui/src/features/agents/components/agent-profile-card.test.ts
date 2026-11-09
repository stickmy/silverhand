import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { StaticRouter } from "react-router-dom/server";
import { describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { AgentDto, UserDto } from "@silverhand/shared";
import { AgentProfileCard } from "./agent-profile-card";
import { setCurrentUserId } from "@/features/session/current-user";

vi.mock("@/features/profile/profile-message-activity", () => ({
  ProfileMessageActivity: () => null,
}));

vi.mock("@/router", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/router")>()),
  useNav: () => ({ toComputer: () => {} }),
}));

setCurrentUserId("u-creator", "creator");

function makeAgent(overrides: Partial<AgentDto> = {}): AgentDto {
  return {
    id: "a-1",
    host_id: "h-1",
    name: "alice",
    display_name: "Alice",
    runtime: "claude",
    session_scope: "conversation",
    description: "Helpful assistant",
    model: null,
    reasoning_effort: null,
    fast_mode: null,
    avatar_url: null,
    created_by_user_id: "u-creator",
    created_at: 0,
    updated_at: 0,
    ...overrides,
  } as AgentDto;
}

function makeViewer(id: string): UserDto {
  return {
    id,
    name: id,
    display_name: id,
    avatar_url: null,
    created_at: 0,
    updated_at: 0,
  } as UserDto;
}

function render(agent: AgentDto, viewerId: string): string {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  // Prime the viewer query so `useViewerUser` resolves synchronously
  // during SSR — without this, `viewer` is undefined and the helper
  // falls into the conservative read-only branch unconditionally
  // (which would make the "creator can edit" assertion pass for the
  // wrong reason).
  qc.setQueryData(["me"], makeViewer(viewerId));
  return renderToStaticMarkup(
    React.createElement(
      QueryClientProvider,
      { client: qc },
      React.createElement(
        StaticRouter,
        { location: "/members/a-1" },
        React.createElement(AgentProfileCard, {
          agent,
        }),
      ),
    ),
  );
}

describe("AgentProfileCard creator/read-only render branches", () => {
  it("non-creator viewer sees no editable inputs", () => {
    const html = render(makeAgent({ created_by_user_id: "u-creator" }), "u-other");
    // Description text appears as static text, not inside a textarea.
    expect(html).toContain("Helpful assistant");
    expect(html).not.toContain("<textarea");
    // No editable file input — non-creator never sees the avatar picker.
    expect(html).not.toContain('type="file"');
    // No edit affordances on the non-creator branch (identified by a11y
    // command name, not by read-only copy).
    expect(html).not.toContain('aria-label="Edit display name"');
  });

  it("creator viewer sees editable inputs and the avatar picker", () => {
    const html = render(makeAgent({ created_by_user_id: "u-creator" }), "u-creator");
    // #task:1472 refactored the card to a pencil-edit pattern: fields are
    // readonly by default and only mount their input chrome on demand. So
    // an SSR snapshot shows no `<textarea>` for description and no combobox
    // for model. What the creator branch DOES surface unconditionally is:
    // the display-name pencil trigger, the description pencil trigger, the
    // runtime-configuration pencil trigger — all lifted to the section
    // header via `ProfileSection titleAdornment` — plus the avatar picker.
    // Identified by a11y command names, not by button copy.
    expect(html).toContain('aria-label="Edit display name"');
    expect(html).toContain('aria-label="Edit description"');
    expect(html).toContain('aria-label="Edit runtime configuration"');
    // Editable avatar picker is mounted — the whole avatar square is the
    // click target now (a hidden file input + a hover label band).
    expect(html).toContain('type="file"');
    expect(html).toMatch(/aria-label="(Change avatar|Add avatar) for @Alice"/);
  });

  it('non-creator branch does not surface "agent self can edit profile" affordances', () => {
    // Cindy PR-3 review #2: the read-only form must not leak any
    // "you would be able to edit if you were the agent" hints. PR-2
    // only shipped agent-self avatar self-edit on the internal route;
    // profile fields are NOT covered. This assertion locks that
    // guardrail at the render layer.
    const html = render(makeAgent({ created_by_user_id: "u-creator" }), "u-other");
    expect(html).not.toMatch(/agent\s+self/i);
    expect(html).not.toMatch(/sign in as|log in as/i);
  });

  it("runtime configuration is readonly on first mount — combobox only appears after entering edit mode", () => {
    // #task:1472: model no longer sits in the always-editable Capabilities
    // grid. It lives inside RuntimeConfigEditor which is readonly by
    // default; the SSR pass therefore shows neither a combobox nor a
    // datalist. The Edit trigger is the pencil at the section header.
    const html = render(
      makeAgent({ runtime: "qoder", created_by_user_id: "u-creator" }),
      "u-creator",
    );
    expect(html).not.toContain('role="combobox"');
    expect(html).not.toContain("<datalist");
    expect(html).toContain('aria-label="Edit runtime configuration"');
  });
});
