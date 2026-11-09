import { useSyncExternalStore } from "react";
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
  // The bytes arrive whole before the picker can show, which on a large file
  // or a weak network takes seconds; every download control reads this to
  // show the wait, and a repeat tap joins the save already running.
  if (saving.has(attachment.id)) return;
  setSaving(attachment.id, true);
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
  } finally {
    setSaving(attachment.id, false);
  }
}

const saving = new Set<string>();
const listeners = new Set<() => void>();

function setSaving(id: string, value: boolean): void {
  if (value) {
    saving.add(id);
  } else {
    saving.delete(id);
  }
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Whether a save of this attachment is under way, from the tap until the
 *  picker closes. */
export function useAttachmentSaving(id: string): boolean {
  return useSyncExternalStore(subscribe, () => saving.has(id));
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
