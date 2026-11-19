import { isGenericMimeHint, mimeFromFilename } from "@silverhand/shared";

export type AttachmentPreviewKind = "markdown" | "html";

/**
 * The previewer kind for an attachment, or `null` when it has no in-app
 * previewer. Never throws — an empty or malformed content type degrades to the
 * filename suffix, and an unknown suffix degrades to `null`.
 */
export function attachmentPreviewKind(
  contentType: string,
  filename: string,
): AttachmentPreviewKind | null {
  const mime = effectiveMime(contentType, filename);
  if (mime === "text/markdown") return "markdown";
  if (mime === "text/html") return "html";
  return null;
}

/**
 * The MIME to classify on: the stored `content_type` (media type only, any
 * `; charset=…` parameter dropped), or — when that is generic/absent — the type
 * implied by the filename suffix. A NON-generic stored type wins even when the
 * suffix disagrees: a file explicitly served as `text/plain` must not be
 * sandbox-rendered as HTML just because it is named `.html`.
 */
function effectiveMime(contentType: string, filename: string): string | null {
  const base = contentType.split(";", 1)[0]?.trim().toLowerCase() ?? "";
  if (!isGenericMimeHint(base)) return base;
  return mimeFromFilename(filename);
}
