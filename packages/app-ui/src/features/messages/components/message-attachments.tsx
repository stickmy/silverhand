import { getPlatform } from "@/platform";
import { fetchAttachmentBytes } from "@/lib/api";
import { useEffect, useRef, useState } from "react";
import { DocumentIcon, ViewIcon } from "@/icons";
import type { AttachmentSnapshot } from "@silverhand/shared";
import { resolveAttachmentLabels } from "@silverhand/shared";
import {
  getAttachmentBlobUrl,
  loadAttachmentBlobUrl,
  useAttachmentBlobToken,
} from "@/features/attachments/attachment-blob-cache";
import { Lightbox } from "./lightbox";
import { AttachmentDownloadGlyph } from "./attachment-download-glyph";
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
  // Generation token for this attachment (#task:1769). Bumps when the creator
  // updates the bytes in place (`attachment_changed`) or a reconnect gap forces
  // a coarse epoch bump — either way we re-fetch the latest bytes and swap them
  // in below without unmounting the current <img> (no loading flash).
  const token = useAttachmentBlobToken(attachment.id);
  // Synchronous cache hit covers both the optimistic-message render
  // (Composer pre-populated the cache from the local File) and any
  // later viewer who has already fetched this attachment in this
  // session — so the swap from optimistic → real message never goes
  // through a "loading" frame.
  const [imageUrl, setImageUrl] = useState<string | null>(() =>
    isImage ? (getAttachmentBlobUrl(attachment.id) ?? null) : null,
  );
  // The generation `imageUrl` was fetched under; a token change past this means
  // the displayed bytes are stale and the load effect must refetch.
  const loadedTokenRef = useRef(token);
  const [error, setError] = useState<string | null>(null);
  const [lightboxOpen, setLightboxOpen] = useState(false);
  // Terminal "this image can't be decoded" flag. A first `onError` is treated
  // as a stale/evicted blob url and retried ONCE (see onImgError); a second is
  // taken as "the bytes themselves won't decode" and lands here so we stop
  // retrying. `retriedRef` carries the once-flag across renders without
  // triggering one. Both are GENERATION-scoped: they reset when the tile is
  // pointed at a new attachment OR when the bytes are updated in place (#task:1769)
  // — an undecodable intermediate revision must not permanently poison a later
  // fixed revision.
  const [decodeFailed, setDecodeFailed] = useState(false);
  const retriedRef = useRef(false);

  useEffect(() => {
    // Re-allow load/retry for a new generation without dropping the visible old
    // image (no white flash) — a successful reload swaps it in place below.
    retriedRef.current = false;
    setDecodeFailed(false);
    setError(null);
  }, [attachment.id, token]);

  useEffect(() => {
    if (!isImage || decodeFailed) return;
    // Already showing bytes for the current generation — nothing to do.
    if (imageUrl && loadedTokenRef.current === token) return;
    let cancelled = false;
    loadAttachmentBlobUrl(attachment.id)
      .then((url) => {
        if (!cancelled) {
          loadedTokenRef.current = token;
          setImageUrl(url);
          setError(null);
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : String(err));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [attachment.id, isImage, imageUrl, decodeFailed, token]);

  // Bound the image-error recovery. The FIRST error may be a stale blob url
  // that was evicted + revoked out from under us — drop it and let the effect
  // re-fetch a fresh one, ONCE. A SECOND error (after a known-fresh url) means
  // the bytes are undecodable by Chromium (e.g. a `.tiff`), so we go terminal
  // instead of looping fetch → error → fetch forever — the unbounded version
  // repainted a broken <img> every frame and flickered the whole UI
  // (#task:1581). A decode-failed image then degrades to the downloadable
  // file chip below rather than a dead preview box.
  function onImgError(): void {
    if (retriedRef.current) {
      setDecodeFailed(true);
      return;
    }
    retriedRef.current = true;
    setImageUrl(null);
  }

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
      onInlineError: (msg) => setError(msg),
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
              onError={onImgError}
              className="block max-h-64 max-w-full cursor-zoom-in object-contain"
            />
          ) : (
            <div className="flex h-32 w-48 items-center justify-center text-xs text-text-muted">
              {error ?? t("messages.loading")}
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
      <div className="flex min-w-0 max-w-full flex-col items-start gap-1">
        <div
          className="flex max-w-[min(320px,100%)] items-center gap-2 overflow-hidden rounded-(--chip-radius) border border-border-subtle bg-bg-floating px-2 py-1.5 text-xs text-text-secondary"
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
    <div className="flex min-w-0 max-w-full flex-col items-start gap-1">
      <div className="flex max-w-[min(320px,100%)] items-stretch overflow-hidden rounded-(--chip-radius) border border-border-subtle bg-bg-floating focus-outline-within">
        <button
          type="button"
          disabled={!previewable && !getPlatform().files.openAttachmentExternal}
          onClick={previewable ? () => onOpenAttachment?.(attachment) : downloadAndOpen}
          className="group flex min-w-0 flex-1 items-center gap-2 px-2 py-1.5 text-xs text-text-secondary hover:text-text-primary focus:outline-none"
          title={error ?? `${attachment.filename} (${formatBytes(attachment.size)})`}
        >
          <DocumentIcon className="size-4 shrink-0 text-text-muted group-hover:text-text-primary" />
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
