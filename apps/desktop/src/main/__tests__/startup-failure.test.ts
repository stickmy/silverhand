import { describe, expect, it } from "vitest";
import {
  describeStartupFailure,
  failedBootState,
  formatDiagnostics,
  issueUrl,
} from "../shell/startup-failure";
import {
  ProfileUnusableError,
  RemoteOnlyServerProfileRequiredError,
} from "../shell/backend-boot-mode";

const DIAGNOSTICS = { version: "0.5.1", commit: "abc123", dirty: false, logsDir: "/logs" };

const profileRefusal = new ProfileUnusableError({
  mode: "profile_invalid",
  profileUrl: "http://box.local:8765",
  classifyMessage: "plaintext http remote requires an opt-in build",
});

describe("describeStartupFailure", () => {
  it("keeps the refused address and the classifier reason, not the repair essay", () => {
    expect(describeStartupFailure(profileRefusal)).toEqual({
      code: "profile_invalid",
      detail: "plaintext http remote requires an opt-in build",
      profileUrl: "http://box.local:8765",
    });
  });

  it("names the remote-only refusal", () => {
    expect(
      describeStartupFailure(
        new RemoteOnlyServerProfileRequiredError("Boot decision landed on embedded/no_profile."),
      ),
    ).toEqual({
      code: "remote_profile_required",
      detail: "Boot decision landed on embedded/no_profile.",
      profileUrl: null,
    });
  });

  it("collapses whitespace, caps length, and omits an empty detail", () => {
    const notice = describeStartupFailure(new Error(`line one\n   line two ${"x".repeat(400)}`));
    expect(notice.code).toBe("startup_failed");
    expect(notice.detail).not.toContain("\n");
    expect(notice.detail!.length).toBeLessThanOrEqual(200);
    expect(notice.detail!.endsWith("…")).toBe(true);
    expect(describeStartupFailure(new Error("   ")).detail).toBeNull();
    expect(describeStartupFailure("boom").detail).toBe("boom");
  });
});

describe("failedBootState", () => {
  it.each([
    ["profile_invalid", "server_address"],
    ["remote_profile_required", "server_address"],
    ["startup_failed", "none"],
  ] as const)("%s offers repair %s", (code, repair) => {
    const state = failedBootState({
      notice: { code, detail: null, profileUrl: null },
      diagnostics: DIAGNOSTICS,
    });
    expect(state).toMatchObject({ kind: "failed", repair });
  });
});

describe("diagnostics", () => {
  const state = failedBootState({
    notice: describeStartupFailure(profileRefusal),
    diagnostics: { ...DIAGNOSTICS, dirty: true },
  });
  if (state.kind !== "failed") throw new Error("expected failed state");

  it("carries everything needed to identify the build and the failure", () => {
    const text = formatDiagnostics(state);
    for (const part of [
      "Code: profile_invalid",
      "Server: http://box.local:8765",
      "Detail: plaintext http remote requires an opt-in build",
      "Version: 0.5.1 · abc123 (modified)",
      "Logs: /logs",
    ]) {
      expect(text).toContain(part);
    }
  });

  it("prefills a new issue with the same diagnostics", () => {
    const url = new URL(issueUrl(state));
    expect(url.origin + url.pathname).toBe("https://github.com/stickmy/silverhand/issues/new");
    expect(url.searchParams.get("title")).toBe("[Startup] profile_invalid");
    expect(url.searchParams.get("body")).toContain(formatDiagnostics(state));
  });
});
