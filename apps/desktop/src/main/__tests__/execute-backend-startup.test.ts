import { describe, expect, it, vi } from "vitest";
import {
  decideBackendBootMode,
  executeBackendStartup,
  planBackendStartup,
  ProfileUnusableError,
  type DecideBackendBootModeInput,
} from "../shell/backend-boot-mode.js";
import { buildRemoteBackendHandle } from "../client/remote-backend-handle.js";

function startup(input: Partial<DecideBackendBootModeInput> = {}) {
  const decision = decideBackendBootMode({
    profileView: null,
    forceEmbedded: false,
    allowInsecureRemote: false,
    ...input,
  });
  return { decision, plan: planBackendStartup(decision) };
}

function dependencies() {
  return {
    loadOrCreateServerConfig: vi.fn(async (): Promise<unknown> => ({ port: 0 })),
    startEmbeddedAndPersistConfig: vi.fn(async (_config: unknown) => ({ port: 7321 })),
    buildRemoteHandle: vi.fn(buildRemoteBackendHandle),
    refuse: vi.fn((plan: { profileUrl: string; classifyMessage: string }): never => {
      throw new ProfileUnusableError({ mode: "profile_invalid", ...plan });
    }),
  };
}

describe("profile to running backend", () => {
  it.each([
    [null, false, false, "no_profile"],
    [null, false, true, "no_profile"],
    [null, true, false, "forced_by_env"],
    ["https://backend.example.test", true, false, "forced_by_env"],
    ["not a url", true, false, "forced_by_env"],
    ["http://10.0.0.5:8080", true, false, "forced_by_env"],
  ] as const)(
    "starts embedded for %s (force=%s, insecure=%s)",
    async (url, forceEmbedded, allowInsecureRemote, reason) => {
      const { decision, plan } = startup({
        profileView: url === null ? null : { server_url: url, updated_at: 1 },
        forceEmbedded,
        allowInsecureRemote,
      });
      expect(decision).toEqual({ mode: "embedded", reason, profileUrl: url ?? "" });
      expect(plan).toEqual({ mode: "embedded", embeddedReason: reason });
      const deps = dependencies();
      const config = { port: 8123 };
      let loaded!: (value: unknown) => void;
      deps.loadOrCreateServerConfig.mockImplementation(
        () =>
          new Promise((resolve) => {
            loaded = resolve;
          }),
      );
      const pending = executeBackendStartup(plan, deps);
      await Promise.resolve();
      expect(deps.startEmbeddedAndPersistConfig).not.toHaveBeenCalled();
      loaded(config);
      expect(await pending).toEqual({ kind: "embedded", running: { port: 7321 } });
      expect(deps.loadOrCreateServerConfig).toHaveBeenCalledTimes(1);
      expect(deps.startEmbeddedAndPersistConfig).toHaveBeenCalledExactlyOnceWith(config);
      expect(deps.buildRemoteHandle).not.toHaveBeenCalled();
      expect(deps.refuse).not.toHaveBeenCalled();
    },
  );

  it.each([
    ["https://backend.example.test", false, "https://backend.example.test", false],
    ["HTTPS://Backend.Example.Test:443/", false, "https://backend.example.test", false],
    ["https://backend.example.test:8443", true, "https://backend.example.test:8443", false],
    ["http://10.0.0.5:8080", true, "http://10.0.0.5:8080", true],
    ["http://127.0.0.1:18080", false, "http://127.0.0.1:18080", false],
    ["http://localhost:7321", false, "http://localhost:7321", false],
    ["http://[::1]:9000", false, "http://[::1]:9000", false],
    ["https://localhost", false, "https://localhost", false],
  ] as const)(
    "starts remote for %s without accessing embedded configuration",
    async (url, allowInsecureRemote, origin, insecure) => {
      const { plan } = startup({
        profileView: { server_url: url, updated_at: 1 },
        allowInsecureRemote,
      });
      const deps = dependencies();
      const handle = await executeBackendStartup(plan, deps);
      expect(handle).toMatchObject({ kind: "remote", url: origin, insecure });
      expect(deps.buildRemoteHandle).toHaveBeenCalledTimes(1);
      expect(deps.loadOrCreateServerConfig).not.toHaveBeenCalled();
      expect(deps.startEmbeddedAndPersistConfig).not.toHaveBeenCalled();
      expect(deps.refuse).not.toHaveBeenCalled();
      if (handle.kind !== "remote") throw new Error("expected remote handle");
      await expect(handle.close()).resolves.toBeUndefined();
      await expect(handle.close()).resolves.toBeUndefined();
    },
  );

  it.each([
    ["http://10.0.0.5:8080", false, /http|insecure/i],
    ["http://127.000.000.001:8080", true, /disguised/i],
    ["https://backend.example.test/sub/path", false, /origin/i],
    ["not a url", false, /.+/],
  ] as const)(
    "refuses invalid profile %s without starting or accessing config",
    async (url, allowInsecureRemote, diagnostic) => {
      const { plan } = startup({
        profileView: { server_url: url, updated_at: 1 },
        allowInsecureRemote,
      });
      expect(plan.mode).toBe("refuse");
      const deps = dependencies();
      const error = await executeBackendStartup(plan, deps).catch((caught: unknown) => caught);
      expect(error).toBeInstanceOf(ProfileUnusableError);
      if (!(error instanceof ProfileUnusableError)) throw new Error("expected profile refusal");
      expect(error.profileUrl).toBe(url);
      expect(error.classifyMessage).toMatch(diagnostic);
      expect(error.message).toContain(url);
      for (const repair of [
        "Clear saved profile",
        "SILVERHAND_ALLOW_INSECURE_REMOTE",
        "SILVERHAND_FORCE_EMBEDDED",
      ]) {
        expect(error.message).toContain(repair);
      }
      expect(deps.refuse).toHaveBeenCalledExactlyOnceWith(plan);
      expect(deps.loadOrCreateServerConfig).not.toHaveBeenCalled();
      expect(deps.startEmbeddedAndPersistConfig).not.toHaveBeenCalled();
      expect(deps.buildRemoteHandle).not.toHaveBeenCalled();
    },
  );

  it("still fails closed if a buggy refusal callback returns", async () => {
    const { plan } = startup({ profileView: { server_url: "not a url", updated_at: 1 } });
    const deps = dependencies();
    deps.refuse.mockImplementation(() => undefined as never);
    await expect(executeBackendStartup(plan, deps)).rejects.toThrow(/refuse\(\) returned/);
    expect(deps.loadOrCreateServerConfig).not.toHaveBeenCalled();
    expect(deps.startEmbeddedAndPersistConfig).not.toHaveBeenCalled();
    expect(deps.buildRemoteHandle).not.toHaveBeenCalled();
  });
});
