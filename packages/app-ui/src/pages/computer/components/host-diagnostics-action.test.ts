/**
 * #task:1445 — renderer-side half of the diagnostics redaction boundary.
 *
 * The shared `buildHostDiagnosticsText` has its own exhaustive no-leak tests
 * (Cindy). These pin the OTHER layer: the renderer maps already-fetched DTOs
 * into the narrow `HostDiagnosticsInput` by EXPLICIT per-field picks, so even a
 * secret-shaped runtime property on a source DTO is dropped before it can reach
 * the builder — and client errors are normalized to a category, never a raw
 * message that could carry a URL/token.
 */
import { describe, expect, it } from "vitest";
import type { HostDto, HostStatusResponse } from "@silverhand/shared";
import { ApiError } from "@/lib/api";
import { toComputerInput, toErrorKind, toStatusInput } from "./host-diagnostics-action";

function hostDto(over: Partial<HostDto> = {}): HostDto {
  return {
    id: "host_1",
    name: "studio-mini",
    kind: "remote",
    created_at: 1000,
    revoked_at: null,
    last_seen_at: 2000,
    activated_at: 1500,
    finalized_at: 1600,
    connected_host_name: "studio.local",
    provisioning_source: "user_invite",
    status: "active",
    placeable: true,
    creator_user_id: "user_1",
    display_label: "Studio Mini",
    ...over,
  };
}

describe("toComputerInput", () => {
  it("copies only the allowlisted computer fields", () => {
    const out = toComputerInput(hostDto());
    expect(out).toEqual({
      id: "host_1",
      name: "studio-mini",
      display_label: "Studio Mini",
      kind: "remote",
      provisioning_source: "user_invite",
      status: "active",
      placeable: true,
      created_at: 1000,
      last_seen_at: 2000,
      activated_at: 1500,
      finalized_at: 1600,
      revoked_at: null,
      connected_host_name: "studio.local",
    });
    // creator_user_id is a cross-entity id — intentionally NOT carried.
    expect(out).not.toHaveProperty("creator_user_id");
  });

  it("drops secret-shaped runtime extras on the source DTO", () => {
    const tainted = hostDto() as HostDto & Record<string, unknown>;
    tainted.host_token = "secret-token";
    tainted.auth_token = "bearer-abc";
    tainted.connect_token = "connect-xyz";
    const out = toComputerInput(tainted) as unknown as Record<string, unknown>;
    expect(out).not.toHaveProperty("host_token");
    expect(out).not.toHaveProperty("auth_token");
    expect(out).not.toHaveProperty("connect_token");
    expect(JSON.stringify(out)).not.toContain("secret-token");
  });
});

describe("toStatusInput", () => {
  it("reduces agents to handle/runtime/online (drops id/name/avatar)", () => {
    const status: HostStatusResponse = {
      host_id: "host_1",
      availability: "disconnected",
      last_seen_at: 2000,
      agents: [
        {
          agent_id: "agent_secret_id",
          display_name: "Alice Real Name",
          handle: "@alice",
          runtime: "claude",
          avatar_url: "https://cdn/secret-avatar.png",
          model: null,
          reasoning_effort: null,
          fast_mode: null,
          online: "offline",
        },
      ],
    };
    const out = toStatusInput(status);
    expect(out.agents).toEqual([{ handle: "@alice", runtime: "claude", online: "offline" }]);
    const serialized = JSON.stringify(out);
    expect(serialized).not.toContain("agent_secret_id");
    expect(serialized).not.toContain("Alice Real Name");
    expect(serialized).not.toContain("secret-avatar");
    // disconnected arm: no snapshot, no reason.
    expect(out).not.toHaveProperty("snapshot");
    expect(out).not.toHaveProperty("reason");
  });

  it("carries the snapshot only on the available arm", () => {
    const status: HostStatusResponse = {
      host_id: "host_1",
      availability: "available",
      snapshot: {
        os: "macOS 14.5",
        os_arch: "arm64",
        daemon_version: "1.2.3",
        daemon_pid: 4242,
        started_at: 900,
        uptime_ms: 60_000,
      },
      agents: [],
      last_seen_at: 2000,
    };
    const out = toStatusInput(status);
    expect(out.snapshot).toEqual({
      os: "macOS 14.5",
      os_arch: "arm64",
      daemon_version: "1.2.3",
      daemon_pid: 4242,
      started_at: 900,
      uptime_ms: 60_000,
    });
  });

  it("carries the typed reason only on the unreachable arm", () => {
    const status: HostStatusResponse = {
      host_id: "host_1",
      availability: "unreachable",
      reason: "timeout",
      agents: [],
    };
    const out = toStatusInput(status);
    expect(out.reason).toBe("timeout");
    expect(out).not.toHaveProperty("snapshot");
  });
});

describe("toErrorKind", () => {
  it("returns undefined when there is no error", () => {
    expect(toErrorKind(null)).toBeUndefined();
    expect(toErrorKind(undefined)).toBeUndefined();
  });

  it("maps HTTP status to a category", () => {
    expect(toErrorKind(new ApiError(403, "Forbidden", "nope"))).toBe("forbidden");
    expect(toErrorKind(new ApiError(404, "Not Found", "nope"))).toBe("not_found");
    expect(toErrorKind(new ApiError(500, "Server Error", "boom"))).toBe("unknown");
  });

  it("maps unreachable/network transport errors to network", () => {
    expect(toErrorKind(new ApiError(0, "", "", "backend_unreachable"))).toBe("network");
    expect(toErrorKind(new ApiError(0, "", "proxy"))).toBe("network");
  });

  it("never surfaces a raw error message (non-ApiError → unknown)", () => {
    expect(toErrorKind(new Error("GET http://x/api?token=secret failed"))).toBe("unknown");
  });
});
