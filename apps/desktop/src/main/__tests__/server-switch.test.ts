import { readFileSync } from "node:fs";
import vm from "node:vm";
import ts from "typescript";
import { describe, expect, it, vi } from "vitest";
import {
  apiPaths,
  AuthSessionResponseSchema,
  HostRotateTokenEmbeddedResponseSchema,
  HostRevokeResponseSchema,
} from "@silverhand/shared";
import { SessionGeneration } from "@silverhand/client";

// Run the real composition functions with network/filesystem boundaries supplied,
// without loading Electron or touching the user's server/profile.
function harness() {
  const source = ts.createSourceFile(
    "index.ts",
    readFileSync(new URL("../index.ts", import.meta.url), "utf8"),
    ts.ScriptTarget.Latest,
    true,
  );
  const names = [
    "handleProfileSaveWithTest",
    "applyProfileToActiveRemoteBackend",
    "postAuthSession",
    "logoutUserSession",
    "clearUserSession",
    "rotateHostToken",
    "getHostProxyConfig",
    "saveHostProxyConfig",
    "proxyBackendRequest",
    "reconcileRevokedLocalHost",
  ];
  const code = names
    .map((name) => {
      const node = source.statements.find(
        (node) => ts.isFunctionDeclaration(node) && node.name?.text === name,
      );
      if (!node) throw new Error(`Missing ${name}`);
      return node.getText(source);
    })
    .join("\n");
  const events: string[] = [];
  const context = vm.createContext({
    URL,
    Headers,
    apiPaths,
    HostRotateTokenEmbeddedResponseSchema,
    HostRevokeResponseSchema,
    assertApiProxyPath: () => {},
    proxyHeaders: () => new Headers(),
    marshalBackendResponse: async (res: Response) => ({
      status: res.status,
      bodyText: await res.text(),
    }),
    shouldClearPasswordSession: () => false,
    reseatCachedUserSessionFromProxy: vi.fn(),
    normalizeDesktopHostProxyConfig: (config: unknown) => config,
    readDaemonHostConfig: vi.fn(() => ({
      host_id: "first-host",
      host_token: "first-host-token",
      server_url: "https://first.example",
    })),
    applyRotateSideEffects: vi.fn(),
    applyRevokeSideEffects: vi.fn(() => "cleared"),
    snapshotLocalHostInfo: () => ({}),
    clearUserSessionAfterUnauthorized: vi.fn(),
    AuthSessionResponseSchema,
    authPaths: { logout: "/api/auth/logout" },
    parseJson: JSON.parse,
    clientSessionGeneration: new SessionGeneration(),
    backend: { kind: "remote", url: "https://first.example", insecure: false },
    userSessionToken: "first-secret",
    userSessionUser: { id: "first-user" },
    switchingServer: false,
    bootState: { kind: "ready" },
    hydrateInFlight: null,
    activeAttachWatchdog: null,
    localHostClaim: { status: "claimed", hostId: "first-host" },
    hostClient: {},
    logger: undefined,
    partialApplyState: {},
    runtimeUpgradeMonitor: { reset: vi.fn(), onRestResponse: vi.fn() },
    desktopAllowsInsecureRemote: () => false,
    probeServerInfo: async ({ serverUrl }: { serverUrl: string }) => ({
      ok: true,
      kind: "system_ca_verified",
      originUrl: serverUrl,
    }),
    desktopAppPaths: () => ({ desktop: { serverProfile: "profile", computerBinding: "binding" } }),
    clearPersistedUserSession: () => events.push("clear-session"),
    syncEventWsSessionCookie: async (token: string | null) => {
      events.push(`cookie:${token}`);
    },
    removeDaemonHostConfig: () => events.push("remove-binding"),
    gracefullyDetachOutbound: async () => events.push("detach-host"),
    writeServerConnectionProfile: ({ serverUrl }: { serverUrl: string }) => {
      events.push("commit-profile");
      return { ok: true, profile: { server_url: serverUrl, version: 2 } };
    },
    runServerProfileApply: vi.fn(async () => ({ kind: "fully_applied", profileVersion: 2 })),
    buildRemoteBackendHandle: ({
      serverUrl,
      insecure,
    }: {
      serverUrl: string;
      insecure: boolean;
    }) => ({ kind: "remote", url: serverUrl, insecure }),
    installBackendEventWsOriginOverride: () => events.push("retarget-ws"),
    derivePartialApplyState: (outcome: unknown) => outcome,
    redactServerConnectionProfile: (profile: unknown) => profile,
    requireBackendUrl: () => context.backend.url,
    backendUrl: (path: string): string => new URL(path, context.backend.url).toString(),
    backendFetch: vi.fn(),
    storeUserSession: vi.fn(),
    claimLocalHostForUserSession: vi.fn(),
  });
  vm.runInContext(
    ts.transpileModule(code, {
      compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.None },
    }).outputText,
    context,
  );
  return { context, events };
}

describe("remote server session isolation", () => {
  it("clears client and computer credentials before persisting a different origin", async () => {
    const { context, events } = harness();
    const result = await context.handleProfileSaveWithTest({ serverUrl: "https://second.example" });
    expect(result.ok).toBe(true);
    expect(events).toEqual([
      "clear-session",
      "remove-binding",
      "cookie:null",
      "detach-host",
      "commit-profile",
      "retarget-ws",
      "cookie:null",
    ]);
    expect(context.userSessionToken).toBeNull();
    expect(context.backend.url).toBe("https://second.example");
    expect(context.runServerProfileApply).not.toHaveBeenCalled();
    expect(context.switchingServer).toBe(false);
  });

  it("after a failed boot only writes the profile, touching no session or host", async () => {
    const { context, events } = harness();
    context.bootState = { kind: "failed" };
    const result = await context.handleProfileSaveWithTest({ serverUrl: "https://second.example" });
    expect(result).toMatchObject({ ok: true, phase2: { kind: "no_op_no_host_claim" } });
    expect(events).toEqual(["commit-profile"]);
    expect(context.runServerProfileApply).not.toHaveBeenCalled();
    expect(context.userSessionToken).toBe("first-secret");
    expect(context.backend.url).toBe("https://first.example");
  });

  it("keeps the same-origin Desktop host configuration apply behavior", async () => {
    const { context, events } = harness();
    await context.handleProfileSaveWithTest({ serverUrl: "https://first.example" });
    expect(events).toEqual(["commit-profile"]);
    expect(context.runServerProfileApply).toHaveBeenCalledOnce();
    expect(context.userSessionToken).toBe("first-secret");
  });

  it("does not persist or claim with a late login result from the old origin", async () => {
    const { context } = harness();
    let finish!: (response: Response) => void;
    context.backendFetch.mockImplementation(
      () =>
        new Promise<Response>((resolve) => {
          finish = resolve;
        }),
    );
    const pending = context.postAuthSession("/api/auth/login", {
      email: "user@example.com",
      password: "pw",
    });
    await context.handleProfileSaveWithTest({ serverUrl: "https://second.example" });
    finish(
      new Response(
        JSON.stringify({
          user_token: "late-first-token",
          user: {
            id: "u",
            name: "user",
            display_name: "User",
            avatar_url: null,
            created_at: 0,
            updated_at: 0,
          },
        }),
      ),
    );
    expect(await pending).toMatchObject({ ok: false, status: 409 });
    expect(context.storeUserSession).not.toHaveBeenCalled();
    expect(context.claimLocalHostForUserSession).not.toHaveBeenCalled();
  });

  it("keeps a logout token bound to its captured server while cookie cleanup awaits", async () => {
    const { context } = harness();
    let finish!: () => void;
    context.syncEventWsSessionCookie = () =>
      new Promise<void>((resolve) => {
        finish = resolve;
      });
    context.backendFetch.mockResolvedValue(new Response(null, { status: 204 }));
    const pending = context.logoutUserSession();
    context.backend.url = "https://second.example";
    finish();
    await pending;
    expect(context.backendFetch).toHaveBeenCalledWith("https://first.example/api/auth/logout", {
      method: "POST",
      headers: { Authorization: "Bearer first-secret" },
    });
  });
});

const revokedHost = {
  id: "first-host",
  name: "first",
  kind: "local",
  provisioning_source: "app_install",
  created_at: 0,
  revoked_at: 1,
  last_seen_at: null,
  display_label: null,
};

describe("credential failure and delayed host responses", () => {
  it.each(["clearPersistedUserSession", "removeDaemonHostConfig"])(
    "does not commit a new origin when %s fails",
    async (boundary) => {
      const { context, events } = harness();
      context[boundary] = () => {
        throw Object.assign(new Error("access denied"), { code: "EACCES" });
      };
      await expect(
        context.handleProfileSaveWithTest({ serverUrl: "https://second.example" }),
      ).rejects.toMatchObject({ code: "EACCES" });
      expect(events).not.toContain("commit-profile");
      expect(events).not.toContain("retarget-ws");
      expect(context.backend.url).toBe("https://first.example");
      expect(context.switchingServer).toBe(false);
    },
  );

  it.each(["rotate", "proxy-get", "proxy-save", "revoke"])(
    "rejects a late %s 401 without clearing the new session",
    async (operation) => {
      const { context } = harness();
      let finish!: (res: Response) => void;
      context.backendFetch.mockImplementation(
        () =>
          new Promise<Response>((resolve) => {
            finish = resolve;
          }),
      );
      const pending =
        operation === "rotate"
          ? context.rotateHostToken("first-host")
          : operation === "proxy-get"
            ? context.getHostProxyConfig("first-host")
            : operation === "proxy-save"
              ? context.saveHostProxyConfig({ hostId: "first-host", config: {} })
              : context.proxyBackendRequest({
                  method: "POST",
                  path: apiPaths.hostRevoke("first-host"),
                });
      await context.handleProfileSaveWithTest({ serverUrl: "https://second.example" });
      context.userSessionToken = "second-secret";
      finish(new Response("{}", { status: 401 }));
      await pending;
      expect(context.userSessionToken).toBe("second-secret");
      expect(context.clearUserSessionAfterUnauthorized).not.toHaveBeenCalled();
    },
  );

  it("does not persist a late successful rotate under another server", async () => {
    const { context } = harness();
    let finish!: (res: Response) => void;
    context.backendFetch.mockImplementation(
      () =>
        new Promise<Response>((resolve) => {
          finish = resolve;
        }),
    );
    const pending = context.rotateHostToken("first-host");
    await context.handleProfileSaveWithTest({ serverUrl: "https://second.example" });
    finish(
      new Response(
        JSON.stringify({ host_id: "first-host", host_token: "A-secret", host: revokedHost }),
      ),
    );
    expect(await pending).toMatchObject({ ok: false });
    expect(context.applyRotateSideEffects).not.toHaveBeenCalled();
  });

  it("cleans up the Desktop binding after a current-session server revoke", async () => {
    const { context } = harness();
    context.backendFetch.mockResolvedValue(new Response(JSON.stringify({ host: revokedHost })));
    await context.proxyBackendRequest({ method: "POST", path: apiPaths.hostRevoke("first-host") });
    expect(context.applyRevokeSideEffects).toHaveBeenCalledWith({
      daemonConfigPath: "binding",
      hostId: "first-host",
    });
    expect(context.localHostClaim).toEqual({ status: "revoked", hostId: "first-host" });
  });

  it("does not clear a binding after a delayed revoke body crosses generations", async () => {
    const { context } = harness();
    let finish!: (body: string) => void;
    context.backendFetch.mockResolvedValue({
      status: 200,
      text: () =>
        new Promise<string>((resolve) => {
          finish = resolve;
        }),
    });
    const pending = context.proxyBackendRequest({
      method: "POST",
      path: apiPaths.hostRevoke("first-host"),
    });
    await Promise.resolve();
    await context.handleProfileSaveWithTest({ serverUrl: "https://second.example" });
    context.userSessionToken = "first-secret"; // token equality alone cannot establish ownership
    finish(JSON.stringify({ host: revokedHost }));
    expect(await pending).toMatchObject({ status: 409 });
    expect(context.applyRevokeSideEffects).not.toHaveBeenCalled();
  });
});

describe("current host rotation", () => {
  it("persists a valid current rotate against its captured origin", async () => {
    const { context } = harness();
    context.backendFetch.mockResolvedValue(
      new Response(
        JSON.stringify({ host_id: "first-host", host_token: "new-token", host: revokedHost }),
      ),
    );
    expect(await context.rotateHostToken("first-host")).toMatchObject({ ok: true });
    expect(context.applyRotateSideEffects).toHaveBeenCalledWith({
      daemonConfigPath: "binding",
      serverUrl: "https://first.example",
      hostId: "first-host",
      hostToken: "new-token",
    });
  });

  it("does not restore a binding revoked while rotation was pending", async () => {
    const { context } = harness();
    let finish!: (res: Response) => void;
    context.backendFetch.mockImplementation(
      () =>
        new Promise<Response>((resolve) => {
          finish = resolve;
        }),
    );
    const pending = context.rotateHostToken("first-host");
    context.readDaemonHostConfig.mockReturnValue(null);
    finish(
      new Response(
        JSON.stringify({ host_id: "first-host", host_token: "new-token", host: revokedHost }),
      ),
    );
    expect(await pending).toMatchObject({ ok: false, code: "not_current_host" });
    expect(context.applyRotateSideEffects).not.toHaveBeenCalled();
  });
});
