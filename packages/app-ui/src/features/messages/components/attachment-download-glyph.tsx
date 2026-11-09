import { DownloadIcon, SpinnerIcon } from "@/icons";
import { useAttachmentSaving } from "../model/save-attachment";

/** The download control's glyph: a spinner from the tap until the save
 *  finishes, so a slow fetch never reads as an ignored tap. */
export function AttachmentDownloadGlyph({ attachmentId }: { attachmentId: string }): JSX.Element {
  const saving = useAttachmentSaving(attachmentId);

  if (saving) {
    return <SpinnerIcon className="size-3.5 animate-spin" />;
  }
  return <DownloadIcon className="size-3.5" />;
}
