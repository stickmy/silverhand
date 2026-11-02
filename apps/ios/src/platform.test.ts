import { describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { DESKTOP_SERVER_API_EPOCH, MINIMUM_SERVER_API_EPOCH } from "@silverhand/shared";
import { nativeProtocol, nativeProtocolJson } from "../native-protocol";
import { createIOSPlatform } from "./platform";
import type { NativeBridge } from "./bridge";

function harness() {
  const native = {
    navigationBadges: vi.fn(async () => {}),
    navigationSync: vi.fn(async () => ({ tabBarHeight: 0 })),
    navigationReset: vi.fn(async () => {}),
    navigationAvatar: vi.fn(async () => {}),
    setLocale: vi.fn(async () => {}),
    titlebar: vi.fn(async () => {}),
    titlebarEndSearch: vi.fn(async () => {}),
    pageBar: vi.fn(async () => {}),
    pageBarBackProgress: vi.fn(async () => {}),
    composerSync: vi.fn(async () => {}),
    composerRemove: vi.fn(async () => {}),
    composerSetText: vi.fn(async () => {}),
    selectionBarSync: vi.fn(async () => {}),
    selectionBarRemove: vi.fn(async () => {}),
    bannerShow: vi.fn(async () => {}),
    toastShow: vi.fn(async () => {}),
    toastDismiss: vi.fn(async () => {}),
    getProfile: vi.fn(async () => ({ server_url: "https://server.example", updated_at: 1 })),
    authState: vi.fn(async () => ({
      serverUrl: "https://server.example",
      info: {
        status: 200,
        statusText: "OK",
        headers: {},
        bodyText: JSON.stringify({
          signup_enabled: true,
          desktop_api: {
            current_epoch: DESKTOP_SERVER_API_EPOCH,
            minimum_client_epoch: DESKTOP_SERVER_API_EPOCH,
          },
        }),
      },
    })),
    serverInfo: vi.fn(async () => ({
      status: 200,
      statusText: "OK",
      headers: {},
      bodyText: JSON.stringify({
        signup_enabled: true,
        desktop_api: {
          current_epoch: DESKTOP_SERVER_API_EPOCH,
          minimum_client_epoch: DESKTOP_SERVER_API_EPOCH,
        },
      }),
    })),
    saveProfile: vi.fn(),
    clearProfile: vi.fn(),
    request: vi.fn(),
    upload: vi.fn(async () => ({
      status: 201,
      headers: {},
      statusText: "Created",
      bodyText: "{}",
    })),
    authenticate: vi.fn(),
    passwordRecovery: vi.fn(),
    logout: vi.fn(),
    addListener: vi.fn(async () => ({ remove: vi.fn() })),
    saveAttachment: vi.fn(async () => ({ completed: true })),
    sharePng: vi.fn(async () => ({ completed: true })),
    previewPng: vi.fn(async () => ({ shared: true })),
    avatarMakerEdit: vi.fn(async () => ({})),
    contextMenuAvailable: vi.fn(async () => ({ available: true })),
    contextMenuShow: vi.fn(async () => ({})),
    openExternal: vi.fn(),
    appVersion: vi.fn(async () => ({ version: "1.0" })),
    openSocket: vi.fn(),
    sendSocket: vi.fn(),
    closeSocket: vi.fn(),
  };
  const reset = vi.fn();
  return { native, reset, platform: createIOSPlatform(native as NativeBridge, reset) };
}

describe("iOS platform", () => {
  it("saves arbitrary original bytes and filename, with silent cancellation and explicit failure", async () => {
    const { native, platform } = harness();
    const bytes = Uint8Array.from({ length: 20000 }, (_, i) => i % 256);
    const input = { filename: "报告.zip", bytes };
    expect(await platform.files.saveAttachment(input)).toEqual({ ok: true });
    const payload = native.saveAttachment.mock.calls[0] as unknown as [
      { filename: string; base64: string },
    ];
    expect(payload[0].filename).toBe(input.filename);
    expect(Uint8Array.from(atob(payload[0].base64), (c) => c.charCodeAt(0))).toEqual(bytes);
    native.saveAttachment.mockResolvedValueOnce({ completed: false });
    expect(await platform.files.saveAttachment(input)).toEqual({ ok: false, canceled: true });
    native.saveAttachment.mockRejectedValueOnce(new Error("Disk full"));
    expect(await platform.files.saveAttachment(input)).toEqual({ ok: false, reason: "Disk full" });
  });

  it("orders native presentation commands and continues after a failed sync", async () => {
    const { native, platform } = harness();
    let rejectSync!: (error: Error) => void;
    native.navigationSync.mockImplementationOnce(
      () =>
        new Promise<{ tabBarHeight: number }>((_, reject) => {
          rejectSync = reject;
        }),
    );
    const navigation = platform.navigation!;
    const sync = navigation.sync({
      session: "s",
      key: "entry",
      tab: "chat",
      blocked: false,
      modalVersion: 0,
      visible: true,
      dark: false,
    });
    const failed = expect(sync).rejects.toThrow("host unavailable");
    const reset = navigation.reset("s");
    await Promise.resolve();
    expect(native.navigationSync).toHaveBeenCalledOnce();
    expect(native.navigationReset).not.toHaveBeenCalled();

    rejectSync(new Error("host unavailable"));
    await failed;
    await reset;
    expect(native.navigationReset).toHaveBeenCalledExactlyOnceWith({ session: "s" });
  });

  it("shares exact image bytes and distinguishes cancellation and failure", async () => {
    const { native, platform } = harness();
    const pngBytes = Uint8Array.from({ length: 20000 }, (_, i) => i % 256);
    expect(await platform.files.sharePngExport!({ pngBytes })).toEqual({ ok: true });
    const encoded = native.sharePng.mock.calls[0] as unknown as [{ base64: string }];
    expect(Uint8Array.from(atob(encoded[0].base64), (c) => c.charCodeAt(0))).toEqual(pngBytes);
    native.sharePng.mockResolvedValueOnce({ completed: false });
    expect(await platform.files.sharePngExport!({ pngBytes })).toEqual({
      ok: false,
      canceled: true,
    });
    native.sharePng.mockRejectedValueOnce(new Error("Sharing unavailable"));
    expect(await platform.files.sharePngExport!({ pngBytes })).toEqual({
      ok: false,
      error: "Sharing unavailable",
    });
  });
  it("returns the native avatar editor's config and exact PNG bytes, or null when dismissed", async () => {
    const { native, platform } = harness();
    const config = { parts: { eyes: 2 }, bg: 1 };
    expect(await platform.avatarMaker!.edit({ config })).toBeNull();
    expect(native.avatarMakerEdit).toHaveBeenCalledWith({ config });

    const pngBytes = Uint8Array.from([137, 80, 78, 71, 0, 255]);
    native.avatarMakerEdit.mockResolvedValueOnce({
      config: { parts: { eyes: 3 }, bg: 0 },
      base64: btoa(String.fromCharCode(...pngBytes)),
    } as never);
    expect(await platform.avatarMaker!.edit({ config })).toEqual({
      config: { parts: { eyes: 3 }, bg: 0 },
      pngBytes,
    });
  });
  it("never advertises local host or desktop shell capabilities", async () => {
    const { platform } = harness();
    expect(platform.localHost).toBeUndefined();
    expect(platform.shell).toBeUndefined();
    expect(platform.notifications).toBeUndefined();
    expect(await platform.client.getConnectionInfo()).toEqual({
      url: "https://server.example",
      mode: "remote",
    });
    expect(await platform.client.getAuthState()).toEqual({ status: "unauthenticated" });
  });

  it("reads server info from the selected server and rejects a failed discovery", async () => {
    const { native, platform } = harness();
    native.serverInfo.mockResolvedValueOnce({
      status: 200,
      statusText: "OK",
      headers: {},
      bodyText: JSON.stringify({
        signup_enabled: true,
        features: { signup_enabled: false, account_deletion_enabled: true },
      }),
    });
    expect(await platform.client.getServerInfo()).toMatchObject({
      features: { signup_enabled: false, account_deletion_enabled: true },
    });
    native.serverInfo.mockResolvedValueOnce({
      status: 503,
      statusText: "Unavailable",
      headers: {},
      bodyText: "",
    });
    await expect(platform.client.getServerInfo()).rejects.toThrow("503");
  });

  it("routes password recovery natively without a session", async () => {
    const { native, platform } = harness();
    native.passwordRecovery.mockResolvedValue({ status: 202, bodyText: '{"ok":true}' });
    expect(
      await platform.client.passwordRecovery({
        action: "request",
        body: '{"email":"u@example.com"}',
      }),
    ).toEqual({ status: 202, bodyText: '{"ok":true}' });
    expect(native.passwordRecovery).toHaveBeenCalledWith({
      action: "request",
      body: '{"email":"u@example.com"}',
    });
  });

  it("does not reset UI if native secure profile commit fails", async () => {
    const { native, platform, reset } = harness();
    native.saveProfile.mockRejectedValue(new Error("Keychain failed"));
    expect(
      await platform.client.saveServerConnectionProfile({ serverUrl: "https://b.example" }),
    ).toMatchObject({ ok: false });
    expect(reset).not.toHaveBeenCalled();
  });

  it("resets all browser state after a successful native switch", async () => {
    const { native, platform, reset } = harness();
    native.saveProfile.mockResolvedValue({
      server_url: "https://b.example",
      updated_at: 2,
      info: { signup_enabled: true },
    });
    expect(
      await platform.client.saveServerConnectionProfile({ serverUrl: "https://b.example" }),
    ).toMatchObject({ ok: true });
    expect(reset).toHaveBeenCalledOnce();
  });

  it("routes login natively and never asks native to reveal a token", async () => {
    const { native, platform } = harness();
    native.authenticate.mockResolvedValue({ ok: true, user: { id: "u" } });
    await platform.client.login({ email: "u@example.com", password: "password" });
    expect(native.authenticate).toHaveBeenCalledWith({
      action: "login",
      body: '{"email":"u@example.com","password":"password"}',
    });
  });

  it("keeps binary bytes intact across JSON transport", async () => {
    const { native, platform } = harness();
    native.request.mockResolvedValue({
      status: 200,
      headers: {},
      statusText: "OK",
      bodyBase64: "AP+A",
    });
    expect(
      (await platform.client.backendRequest({ path: "/api/bytes", responseType: "arrayBuffer" }))
        .bodyBytes,
    ).toEqual(new Uint8Array([0, 255, 128]));
  });

  it("omits external attachment opening and sends uploads through the native session", async () => {
    const { native, platform } = harness();
    expect(platform.files.openAttachmentExternal).toBeUndefined();
    const response = await platform.client.backendUpload({
      path: "/api/upload",
      fields: [{ name: "payload", value: "{}" }],
      files: [
        {
          fieldName: "file",
          filename: "a.png",
          contentType: "image/png",
          bytes: new Uint8Array([0, 255, 128]),
        },
      ],
    });
    expect(response.status).toBe(201);
    expect(native.upload).toHaveBeenCalledWith({
      path: "/api/upload",
      fields: [{ name: "payload", value: "{}" }],
      files: [{ fieldName: "file", filename: "a.png", contentType: "image/png", base64: "AP+A" }],
    });
  });

  it("keeps native admission and route resource tied to shared authority", () => {
    const committed = readFileSync(
      new URL("../native/Sources/SilverhandNative/Resources/Protocol.json", import.meta.url),
      "utf8",
    );
    expect(committed).toBe(nativeProtocolJson());
    expect(nativeProtocol()).toMatchObject({
      desktopApi: DESKTOP_SERVER_API_EPOCH,
      desktopMinServer: MINIMUM_SERVER_API_EPOCH,
    });
  });

  it("bundles the same avatar part library the web editor draws from", () => {
    const read = (path: string) => readFileSync(new URL(path, import.meta.url), "utf8");
    expect(read("../native/Sources/SilverhandNative/Resources/AvatarLibrary.json")).toBe(
      read("../../../packages/app-ui/src/features/avatar-maker/model/avatar-library.json"),
    );
  });

  it("has no remote WebView URL, wildcard navigation or relaxed TLS", () => {
    const config = readFileSync(new URL("../capacitor.config.ts", import.meta.url), "utf8");
    const plist = readFileSync(new URL("../ios/App/App/Info.plist", import.meta.url), "utf8");
    expect(config).not.toMatch(/server\s*:|allowNavigation|allowMixedContent/);
    expect(plist).not.toMatch(/NSAllowsArbitraryLoads|NSExceptionAllowsInsecureHTTPLoads/);
  });
});
