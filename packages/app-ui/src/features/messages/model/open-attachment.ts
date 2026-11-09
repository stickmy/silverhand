import { getPlatform } from "@/platform";
import { toast } from "@/lib/toast";
import type { AttachmentSnapshot } from "@silverhand/shared";
import { fetchAttachmentBytes } from "@/lib/api";
import { t } from "@/lib/i18n";
import { runAttachmentTask } from "./attachment-busy";

/**
 * Open-with-OS flow for a non-image chat attachment. Fetches bytes through
 * the authenticated backend proxy (no `URL.createObjectURL` in the chain —
 * `shell.openExternal` can't resolve a renderer-scoped `blob:` URL), hands
 * them to main over the `attachment:openExternal` IPC (which owns the
 * safe-extension allowlist + temp write + `shell.openPath`), and toasts /
 * inline-reports on failure. Image attachments open in the in-app Lightbox
 * instead and must NOT be routed here.
 *
 * Single source of truth shared between `MessageItem` (attachment tile) and
 * `ChannelAttachmentsPanel` (Attachments list row): both surfaces use the same
 * reason → user-facing string mapping and the same safe main-process boundary,
 * and both fail-visible rather than swallow errors silently.
 *
 * Errors surface through:
 *   1. `onInlineError(msg)` if provided — panels that carry their own
 *      inline error slot pass it (a per-row hint keeps the toast lane
 *      quiet in dense lists).
 *   2. `toast.error(msg)` otherwise — used by the message tile.
 *
 * Exported for testing.
 */
export async function openAttachmentExternally(
  attachment: Pick<AttachmentSnapshot, "id" | "filename">,
  opts: OpenAttachmentOptions = {},
  deps: OpenAttachmentDeps = defaultDeps,
): Promise<void> {
  await runAttachmentTask("open", attachment.id, () =>
    openBytesExternally(
      () => deps.fetchBytes(attachment.id),
      attachment.id,
      attachment.filename,
      opts,
      deps,
    ),
  );
}

/** Shared core: obtain bytes (however the caller sources them), hand them to
 *  the safe main-process open boundary, and map any failure to a user-facing
 *  string. Kept separate from the membership entry point above so the bytes
 *  source stays the only axis an additional caller would vary. */
async function openBytesExternally(
  fetchBytes: () => Promise<{ bytes: Uint8Array }>,
  attachmentId: string,
  filename: string,
  opts: OpenAttachmentOptions,
  deps: OpenAttachmentCoreDeps,
): Promise<void> {
  try {
    const { bytes } = await fetchBytes();
    const result = await deps.open({ attachmentId, filename, bytes });
    if (result.ok) return;
    reportFailure(describeOpenFailure(result.reason), opts, deps);
  } catch (err) {
    reportFailure(formatOpenException(err), opts, deps);
  }
}

/** Translate a THROWN exception from the open path (fetch failed, IPC
 *  reject, etc.) into a user-facing string. Distinct from
 *  {@link describeOpenFailure}, which maps a canonical IPC `reason` code
 *  from main — a thrown error carries its own message and doesn't fit
 *  into the reason-code table. Kept exported so any attachment surface
 *  (row click, lightbox host, preview refetch) formats blob-fetch
 *  exceptions identically. */
export function formatOpenException(err: unknown): string {
  const msg = err instanceof Error ? err.message : String(err);
  return t("messages.openFailed", { reason: msg });
}

/** Translate the IPC failure reason into a user-facing string. Kept short —
 *  used both as a toast body and a tile's `title` hint. */
export function describeOpenFailure(reason: string): string {
  if (reason === "unsafe_extension") {
    return t("messages.thisFileTypeCant");
  }
  if (reason.startsWith("open_failed")) {
    return t("messages.macosCouldNotOpen");
  }
  if (reason === "empty_payload" || reason === "invalid_payload") {
    return t("messages.theAttachmentDataIs");
  }
  return reason;
}

function reportFailure(
  msg: string,
  opts: OpenAttachmentOptions,
  deps: OpenAttachmentCoreDeps,
): void {
  if (opts.onInlineError) {
    opts.onInlineError(msg);
    return;
  }
  deps.toastError(msg);
}

export interface OpenAttachmentOptions {
  /** Consumed by callers that render their own inline error slot (the
   *  message tile keeps this; the panel currently opts out and lets the
   *  toast fire). When absent the failure surfaces as a toast. */
  onInlineError?: (message: string) => void;
}

/** The half of the seam every open path shares: the safe OS boundary and the
 *  failure surface. Split out from {@link OpenAttachmentDeps} so a caller that
 *  sources its own bytes needs only these two. */
export interface OpenAttachmentCoreDeps {
  open: (input: {
    attachmentId: string;
    filename: string;
    bytes: Uint8Array;
  }) => Promise<{ ok: true } | { ok: false; reason: string }>;
  toastError: (message: string) => void;
}

/** Injectable seam — production wiring below, tests supply mocks. The
 *  membership path additionally sources bytes by attachment id. */
export interface OpenAttachmentDeps extends OpenAttachmentCoreDeps {
  fetchBytes: (id: string) => Promise<{ bytes: Uint8Array }>;
}

const defaultDeps: OpenAttachmentDeps = {
  fetchBytes: (id) => fetchAttachmentBytes(id),
  open: async (input) => {
    const open = getPlatform().files.openAttachmentExternal;
    return open ? open(input) : { ok: false, reason: t("messages.openingAttachmentsInExternal") };
  },
  toastError: (msg) => {
    toast.error(msg);
  },
};
