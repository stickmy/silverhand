import { getPlatform } from "@/platform";
import { toast } from "@/lib/toast";
import type { AttachmentSnapshot } from "@silverhand/shared";
import { fetchAttachmentBytes } from "@/lib/api";
import { t } from "@/lib/i18n";

/**
 * Save-as flow for a chat attachment. Fetches the bytes through the
 * authenticated backend proxy (no `URL.createObjectURL` in the chain
 * — see `fetchAttachmentBytes`), hands them to main over the
 * `attachment:save` IPC (which owns `showSaveDialog` + `writeFile`),
 * and toasts the outcome.
 *
 * User-cancel is intentionally silent — dismissing the save picker is
 * a normal action, not a failure. Real errors (fetch failed, write
 * failed, malformed payload) surface as an error toast.
 *
 * Source-blind: the caller only supplies an attachment snapshot;
 * this helper never touches the filesystem or dialog directly.
 *
 * Exported for testing.
 */
export async function saveAttachmentToDisk(
  attachment: Pick<AttachmentSnapshot, "id" | "filename">,
  deps: SaveAttachmentDeps = defaultDeps,
): Promise<void> {
  try {
    const { bytes } = await deps.fetchBytes(attachment.id);
    const result = await deps.save({ filename: attachment.filename, bytes });
    if (result.ok) {
      deps.toastSuccess(t("messages.saved"));
      return;
    }
    if ("canceled" in result) return;
    deps.toastError(t("messages.saveFailed", { reason: result.reason }));
  } catch (err) {
    deps.toastError(
      t("messages.saveFailed", { reason: err instanceof Error ? err.message : String(err) }),
    );
  }
}

/** Injectable seam — production wiring below, tests supply mocks. */
export interface SaveAttachmentDeps {
  fetchBytes: (id: string) => Promise<{ bytes: Uint8Array }>;
  save: (input: {
    filename: string;
    bytes: Uint8Array;
  }) => Promise<
    { ok: true; path?: string } | { ok: false; canceled: true } | { ok: false; reason: string }
  >;
  toastSuccess: (message: string) => void;
  toastError: (message: string) => void;
}

const defaultDeps: SaveAttachmentDeps = {
  fetchBytes: (id) => fetchAttachmentBytes(id),
  save: (input) => getPlatform().files.saveAttachment(input),
  toastSuccess: (msg) => {
    toast.success(msg);
  },
  toastError: (msg) => {
    toast.error(msg);
  },
};
