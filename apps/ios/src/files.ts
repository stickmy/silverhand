import type { ClientPlatform } from "@silverhand/client";
import type { NativeBridge } from "./bridge";

export function createIOSFiles(bridge: NativeBridge): ClientPlatform["files"] {
  return {
    sharePngExport: async ({ pngBytes }) => {
      try {
        const { completed } = await bridge.sharePng({ base64: encodeBytes(pngBytes) });
        return completed ? { ok: true } : { ok: false, canceled: true };
      } catch (error) {
        return {
          ok: false,
          error: error instanceof Error ? error.message : "Unable to share image",
        };
      }
    },
    previewPngExport: ({ image, ...labels }) =>
      bridge.previewPng({
        ...("attachmentId" in image
          ? { attachmentId: image.attachmentId }
          : { base64: encodeBytes(image.pngBytes) }),
        ...labels,
      }),
    openExternal: (url) => bridge.openExternal({ url }),
    saveAttachment: async ({ filename, bytes }) => {
      try {
        const { completed } = await bridge.saveAttachment({ filename, base64: encodeBytes(bytes) });
        return completed ? { ok: true } : { ok: false, canceled: true };
      } catch (error) {
        return {
          ok: false,
          reason: error instanceof Error ? error.message : "Unable to save attachment",
        };
      }
    },
    // ImagePreviewModal and Lightbox use sharePngExport on iOS, so their
    // desktop Copy/Save actions never call these methods. Keep explicit
    // failures for accidental direct calls; saving via the system share
    // sheet is handled by sharePngExport, not by these interface stubs.
    savePngExport: async () => ({
      ok: false,
      error: "Image export is not available in this iOS preview",
    }),
    copyPngExport: async () => ({
      ok: false,
      error: "Image clipboard is not available in this iOS preview",
    }),
  };
}

export function encodeBytes(bytes: Uint8Array): string {
  let binary = "";
  for (let start = 0; start < bytes.length; start += 8192) {
    binary += String.fromCharCode(...bytes.subarray(start, start + 8192));
  }
  return btoa(binary);
}
