import { getPlatform } from "@/platform";
import { fetchAttachmentBytes } from "@/lib/api";
import { useState } from "react";
import { DocumentIcon, ViewIcon } from "@/icons";
import type { AttachmentSnapshot } from "@silverhand/shared";
import { resolveAttachmentLabels } from "@silverhand/shared";
import {
  attachmentImageUrl,
  useAttachmentImage,
} from "@/features/attachments/use-attachment-image";
import { Lightbox } from "./lightbox";
import { AttachmentDownloadGlyph, AttachmentTaskGlyph } from "./attachment-task-glyph";
import { saveAttachmentToDisk } from "../model/save-attachment";
import { openAttachmentExternally } from "../model/open-attachment";
import { attachmentPreviewKind } from "@/features/attachments/attachment-preview";
import { t } from "@/lib/i18n";

export function AttachmentList({
  attachments,
  alignRight,
  canExport = true,
  onOpenAttachment,
}: {
  attachments: AttachmentSnapshot[];
  alignRight: boolean;
  /** Whether the viewer may export (download / open-external) an attachment.
   *  `false` for a non-member browsing a public channel: inline
   *  PREVIEW stays, but the download / open affordances are withheld. */
  canExport?: boolean;
  /** Open a md/html attachment in its previewer tab (#task:2122). Absent → the
   *  tile keeps the open-with-OS main click for every type. */
  onOpenAttachment?: (attachment: AttachmentSnapshot) => void;
}): JSX.Element {
  // Resolve every tile's label once at the list level so ordinals match the
  // agent-facing stdin suffix exactly (same shared resolver). This lets a
  // reader map an agent's "Image #1" back to the actual tile (#task:1324).
  const labels = resolveAttachmentLabels(attachments);
  return (
    <div className={"mt-2 flex flex-wrap gap-2 " + (alignRight ? "justify-end" : "justify-start")}>
      {attachments.map((att, i) => (
        <AttachmentTile
          key={att.id}
          attachment={att}
          label={labels[i]}
          canExport={canExport}
          onOpenAttachment={onOpenAttachment}
        />
      ))}
    </div>
  );
}

export function AttachmentTile({
  attachment,
  label,
  canExport = true,
  onOpenAttachment,
}: {
  attachment: AttachmentSnapshot;
  label?: string;
  canExport?: boolean;
  onOpenAttachment?: (attachment: AttachmentSnapshot) => void;
}): JSX.Element {
  const isImage = attachment.content_type.startsWith("image/");
  // The owner supplies the destination tab controller, directly or via a sibling
  // pane relay. Legacy generic MIME types still classify by filename suffix.
  const previewable =
    onOpenAttachment != null &&
    attachmentPreviewKind(attachment.content_type, attachment.filename) != null;
  // A cache hit (the composer's local file, or an earlier fetch) shows without a
  // loading frame; an in-place update (#task:1769) swaps the picture without
  // unmounting it.
  const { load, onImageError } = useAttachmentImage(attachment.id, isImage);
  const imageUrl = attachmentImageUrl(load);
  const decodeFailed = load.status === "failed" && load.cause === "decode";
  const loadError = load.status === "failed" && load.cause === "fetch" ? load.message : null;
  const [openError, setOpenError] = useState<string | null>(null);
  const [lightboxOpen, setLightboxOpen] = useState(false);

  async function downloadAndOpen(): Promise<void> {
    // Non-image attachments must NOT be opened via `window.open(blob_url)` —
    // Electron's setWindowOpenHandler hands the URL to
    // `shell.openExternal`, which can't resolve `blob:` URLs (they're
    // scoped to the renderer process). The result was that clicking
    // any non-image attachment silently did nothing — the symptom that
    // surfaced as the original `.html` bug (`#task:383`).
    //
    // Instead: fetch the bytes through the authenticated backend route,
    // ship them to main over IPC, and let main write a temp file +
    // `shell.openPath`. The OS default handler picks the right app from the
    // extension. HTML therefore opens in the user's real browser, not in
    // any Electron-controlled WebContents — which keeps untrusted HTML
    // outside the app's process tree.
    //
    // Main applies a strict extension allowlist (`isSafeOpenExtension`);
    // `unsafe_extension` and IPC failures surface through the shared
    // `open-attachment` helper — this tile shows them as an inline error
    // (the chip has its own error slot); the Attachments panel opts into
    // toasts.
    await openAttachmentExternally(attachment, {
      onInlineError: (msg) => setOpenError(msg),
    });
  }

  if (isImage && !decodeFailed) {
    return (
      // The width cap sits on the tile, where the percentage resolves against the
      // row. On the <img> it is cyclic while the tile sizes itself, so the tile
      // took the image's natural width and the frame ran far past the picture.
      <div className="flex min-w-0 max-w-[min(320px,100%)] flex-col gap-1">
        <button
          type="button"
          onClick={() => imageUrl && setLightboxOpen(true)}
          disabled={!imageUrl}
          className="block max-w-full overflow-hidden rounded-(--chip-radius) border border-border-subtle bg-bg-floating focus-outline"
          title={`${attachment.filename} (${formatBytes(attachment.size)})`}
        >
          {imageUrl ? (
            <img
              src={imageUrl}
              alt={attachment.filename}
              onError={onImageError}
              className="block max-h-64 max-w-full cursor-zoom-in object-contain"
            />
          ) : (
            <div className="flex h-32 w-48 items-center justify-center text-xs text-text-muted">
              {loadError ?? t("messages.loading")}
            </div>
          )}
        </button>
        {label ? <AttachmentLabelBadge label={label} /> : null}
        {imageUrl ? (
          <Lightbox
            open={lightboxOpen}
            onOpenChange={setLightboxOpen}
            src={imageUrl}
            loadExportBytes={async () => (await fetchAttachmentBytes(attachment.id)).bytes}
            attachmentId={attachment.id}
            alt={attachment.filename}
            // Preview stays; download is a member export. Omitting
            // `onDownload` hides the Lightbox's download action (Copy remains).
            onDownload={canExport ? () => saveAttachmentToDisk(attachment) : undefined}
          />
        ) : null}
      </div>
    );
  }

  if (!canExport) {
    // Non-member preview: a non-image attachment has no inline
    // render, so its "preview" is the file reference itself — a static,
    // non-interactive chip. Clicking would download-and-open (an export), so
    // the whole tile is inert and the explicit Download button is omitted.
    return (
      <div className="flex min-w-0 max-w-[min(320px,100%)] flex-col gap-1">
        <div
          className="flex items-center gap-2 overflow-hidden rounded-(--chip-radius) border border-border-subtle bg-bg-floating px-2 py-1.5 text-xs text-text-secondary"
          title={`${attachment.filename} (${formatBytes(attachment.size)})`}
        >
          <DocumentIcon className="size-4 shrink-0 text-text-muted" />
          <span className="min-w-0 truncate">{attachment.filename}</span>
          <span className="shrink-0 text-(length:--text-badge) text-text-muted">
            {formatBytes(attachment.size)}
          </span>
        </div>
        {label ? <AttachmentLabelBadge label={label} /> : null}
      </div>
    );
  }

  return (
    // The cap sits on the tile, not the chip: on the chip the percentage is cyclic
    // while the tile sizes itself, so the tile kept the full filename's width and
    // the chip sat at its left, off the row's right edge on an own message.
    <div className="flex min-w-0 max-w-[min(320px,100%)] flex-col gap-1">
      <div className="flex items-stretch overflow-hidden rounded-(--chip-radius) border border-border-subtle bg-bg-floating focus-outline-within">
        <button
          type="button"
          disabled={!previewable && !getPlatform().files.openAttachmentExternal}
          onClick={previewable ? () => onOpenAttachment?.(attachment) : downloadAndOpen}
          className="group flex min-w-0 flex-1 items-center gap-2 px-2 py-1.5 text-xs text-text-secondary hover:text-text-primary focus:outline-none"
          title={openError ?? `${attachment.filename} (${formatBytes(attachment.size)})`}
        >
          <AttachmentTaskGlyph task="open" attachmentId={attachment.id} className="size-4">
            <DocumentIcon className="size-4 shrink-0 text-text-muted group-hover:text-text-primary" />
          </AttachmentTaskGlyph>
          <span className="min-w-0 truncate">{attachment.filename}</span>
          <span className="shrink-0 text-(length:--text-badge) text-text-muted group-hover:text-text-primary">
            {formatBytes(attachment.size)}
          </span>
        </button>
        {previewable ? (
          <button
            type="button"
            onClick={() => onOpenAttachment?.(attachment)}
            className="flex shrink-0 items-center justify-center px-2 text-text-muted hover:text-text-primary focus-outline"
            title={t("messages.view")}
            aria-label={t("messages.viewFile", { file: attachment.filename })}
          >
            <ViewIcon className="size-3.5" />
          </button>
        ) : null}
        <button
          type="button"
          onClick={() => void saveAttachmentToDisk(attachment)}
          className="flex shrink-0 items-center justify-center px-2 text-text-muted hover:text-text-primary focus-outline"
          title={t("messages.download")}
          aria-label={t("messages.downloadFile", { file: attachment.filename })}
        >
          <AttachmentDownloadGlyph attachmentId={attachment.id} />
        </button>
      </div>
      {label ? <AttachmentLabelBadge label={label} /> : null}
    </div>
  );
}

/** The `Image #N` / `File #N` reference caption under a received attachment
 *  tile (#task:1324). It's a reference identifier, not a status/action badge —
 *  so it's rendered as light, document-caption-style text centered under the
 *  tile, NOT an accent pill (which would over-read as a chip and steal the
 *  saturated-hue budget). Non-interactive here (unlike the composer's insert
 *  button): on a sent message the label is already fixed. */
export function AttachmentLabelBadge({ label }: { label: string }): JSX.Element {
  return (
    <span className="block w-full text-center font-mono text-(length:--text-badge) text-text-muted">
      {label}
    </span>
  );
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(1)} GB`;
}
