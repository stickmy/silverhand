import type { ReactNode } from "react";
import { DownloadIcon, SpinnerIcon } from "@/icons";
import { cn } from "@/lib/cn";
import { type AttachmentTask, useAttachmentTaskRunning } from "../model/attachment-busy";

/** A control's glyph that turns into a spinner from the tap until the save or
 *  open finishes, so a slow fetch never reads as an ignored tap. */
export function AttachmentTaskGlyph({
  task,
  attachmentId,
  className = "size-3.5",
  children,
}: {
  task: AttachmentTask;
  attachmentId: string;
  className?: string;
  children: ReactNode;
}): JSX.Element {
  const running = useAttachmentTaskRunning(task, attachmentId);

  if (running) {
    return <SpinnerIcon className={cn(className, "shrink-0 animate-spin")} />;
  }
  return <>{children}</>;
}

export function AttachmentDownloadGlyph({ attachmentId }: { attachmentId: string }): JSX.Element {
  return (
    <AttachmentTaskGlyph task="save" attachmentId={attachmentId}>
      <DownloadIcon className="size-3.5" />
    </AttachmentTaskGlyph>
  );
}
