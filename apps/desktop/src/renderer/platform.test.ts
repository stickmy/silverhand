import { describe, expect, it, vi } from "vitest";
import type { DesktopBridge } from "../preload/contract";
import { createDesktopPlatform } from "./platform";

describe("Desktop platform composition", () => {
  it("publishes connection facts independently of the optional local computer", async () => {
    const bridge = {
      getBackendInfo: vi.fn(async () => ({
        url: "https://server.example",
        mode: "remote",
        local_host_id: "computer",
        local_host_status: "claimed",
      })),
    } as unknown as DesktopBridge;
    const platform = createDesktopPlatform(bridge, vi.fn());
    expect(await platform.client.getConnectionInfo()).toEqual({
      url: "https://server.example",
      mode: "remote",
    });
    expect(await platform.localHost!.getSnapshot()).toEqual({
      local_host_id: "computer",
      local_host_status: "claimed",
    });
    expect(platform.client).not.toHaveProperty("rotateHostToken");
    expect(platform.files).not.toHaveProperty("backendRequest");
  });

  it("offers image export as copy and save, routed to the bridge", async () => {
    const bridge = {
      copyPngExport: vi.fn(async () => ({ ok: true })),
      savePngExport: vi.fn(async () => ({ ok: false, canceled: true })),
    } as unknown as DesktopBridge;
    const { files } = createDesktopPlatform(bridge, vi.fn());
    const pngBytes = Uint8Array.of(1, 2, 3);
    expect(files.sharePngExport).toBeUndefined();
    expect(await files.copyPngExport!({ pngBytes })).toEqual({ ok: true });
    expect(await files.savePngExport!({ pngBytes, suggestedName: "a.png" })).toEqual({
      ok: false,
      canceled: true,
    });
    expect(bridge.copyPngExport).toHaveBeenCalledWith({ pngBytes });
    expect(bridge.savePngExport).toHaveBeenCalledWith({ pngBytes, suggestedName: "a.png" });
  });

  it("restarts the renderer after a server change to discard caches and WS resume floors", async () => {
    let url = "https://first.example";
    const bridge = {
      getBackendInfo: vi.fn(async () => ({ url, mode: "remote" })),
      saveServerConnectionProfile: vi.fn(async () => {
        url = "https://second.example";
        return { ok: true };
      }),
    } as unknown as DesktopBridge;
    const restart = vi.fn();
    const platform = createDesktopPlatform(bridge, restart);
    await platform.client.saveServerConnectionProfile({ serverUrl: "https://second.example" });
    expect(restart).toHaveBeenCalledOnce();
    await platform.client.saveServerConnectionProfile({ serverUrl: "https://second.example" });
    expect(restart).toHaveBeenCalledOnce();
  });
});
