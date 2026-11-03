import { motion, useIsPresent, useReducedMotion } from "motion/react";
import { forwardRef, type ReactNode } from "react";
import { AttachmentIcon, CloseIcon, SpinnerIcon } from "@/icons";
import type { CachedPendingAttachment as PendingAttachment } from "@/features/messages/composer-draft-cache";
import { t } from "@/lib/i18n";

export function PendingAttachmentTray({ children }: { children: ReactNode }): JSX.Element {
  const reducedMotion = useReducedMotion();
  const present = useIsPresent();
  return (
    <motion.div
      ref={(node) => node?.toggleAttribute("inert", !present)}
      aria-hidden={!present || undefined}
      initial={{ height: 0, opacity: 0, overflow: "hidden" }}
      animate={{ height: "auto", opacity: 1, transitionEnd: { overflow: "visible" } }}
      exit={{ height: 0, opacity: 0, overflow: "hidden" }}
      transition={{ duration: reducedMotion ? 0 : 0.14, ease: "easeOut" }}
    >
      <div data-composer-tray className="relative flex flex-wrap gap-1.5 pb-2">
        {children}
      </div>
    </motion.div>
  );
}

// Forward the DOM ref so popLayout can remove an exiting chip from flex flow.
export const PendingAttachmentMotion = forwardRef<HTMLDivElement, { children: ReactNode }>(
  function PendingAttachmentMotion({ children }, ref): JSX.Element {
    const present = useIsPresent();
    const reducedMotion = useReducedMotion();
    return (
      <motion.div
        ref={(node) => {
          node?.toggleAttribute("inert", !present);
          if (typeof ref === "function") ref(node);
          else if (ref) ref.current = node;
        }}
        aria-hidden={!present || undefined}
        className="min-w-0 max-w-70"
        style={{ pointerEvents: present ? undefined : "none" }}
        layout={reducedMotion ? false : "position"}
        initial={{ opacity: 0, scale: reducedMotion ? 1 : 0.96 }}
        animate={{ opacity: 1, scale: 1 }}
        exit={{ opacity: 0, scale: reducedMotion ? 1 : 0.96 }}
        transition={{ duration: reducedMotion ? 0 : 0.14, ease: "easeOut" }}
      >
        {children}
      </motion.div>
    );
  },
);

export function AttachmentChip({
  item,
  onRemove,
  removeDisabled = false,
  onInsertLabel,
  onPreview,
}: {
  item: PendingAttachment;
  onRemove: () => void;
  /** Kept in place but inert while the message carrying it is being sent. */
  removeDisabled?: boolean;
  /** Insert this attachment's stable label into the message body. Omitted when
   *  the attachment has no label yet or failed to upload. */
  onInsertLabel?: () => void;
  /** Provided only for image attachments with a local previewUrl. */
  onPreview?: () => void;
}): JSX.Element {
  const sizeLabel = formatBytes(item.size);
  // `error` uses the `--color-error` token at full strength on BOTH frame and
  // ink: there is no `danger` token in any theme (the `status-danger` classes
  // this replaced were never generated, so a failed upload read exactly like a
  // finished one), and a failure the user must notice before sending is where
  // saturation is earned. Full-strength `border-error` on a compact bordered
  // control matches `ConnectionPane`'s failed-endpoint option.
  const stateClass =
    item.state === "error"
      ? "border-error text-error"
      : item.state === "uploading"
        ? "border-border-subtle text-text-muted"
        : "border-border-subtle text-text-secondary";
  const isImage = item.contentType.startsWith("image/");
  // The card shows the moment a file is chosen, before its bytes are read on
  // iOS: until the upload ends a spinner holds the thumbnail's place.
  const uploading = item.state === "uploading";

  // Image chip: small thumbnail + filename + remove. Click thumbnail
  // opens the lightbox. Non-image chip: paperclip icon + filename.
  // Two-line layout (#task:1324): the filename owns the main line and
  // truncates; the `Image #N` label + size sit on a light sub-line so the
  // label never competes with the filename for horizontal width. The label
  // doubles as the "insert reference" button — clicking it drops the label
  // into the message body as plain text — but it's de-emphasized to a subtle
  // text button, not a heavy accent badge.
  return (
    <div
      className={`flex max-w-70 items-center gap-1.5 rounded-md border bg-bg-floating px-2 py-1 text-xs ${stateClass}`}
      title={item.error ?? (uploading ? item.filename : `${item.filename} (${sizeLabel})`)}
    >
      {isImage && item.previewUrl ? (
        <button
          type="button"
          onClick={onPreview}
          disabled={!onPreview}
          className="relative grid size-8 shrink-0 place-items-center overflow-hidden rounded-sm border border-border-subtle focus-outline"
          aria-label={t("messages.previewImage")}
        >
          <img
            src={item.previewUrl}
            alt={item.filename}
            className={`absolute inset-0 size-full object-cover ${uploading ? "opacity-50" : ""}`}
          />
          {uploading ? (
            <SpinnerIcon
              aria-label={t("messages.uploading")}
              className="relative size-3.5 motion-safe:animate-spin"
            />
          ) : null}
        </button>
      ) : uploading ? (
        <span className="grid size-8 shrink-0 place-items-center rounded-sm border border-border-subtle">
          <SpinnerIcon
            aria-label={t("messages.uploading")}
            className="size-3.5 motion-safe:animate-spin"
          />
        </span>
      ) : (
        <AttachmentIcon className="size-3 shrink-0" />
      )}
      <div className="flex min-w-0 flex-1 flex-col">
        <span className="truncate">{item.filename}</span>
        <div className="flex items-center gap-1 text-(length:--text-badge) text-text-muted">
          {item.label ? (
            onInsertLabel ? (
              <button
                type="button"
                onClick={onInsertLabel}
                title={t("messages.insertReferenceIntoMessage")}
                aria-label={t("messages.insertInto", { label: item.label })}
                className="shrink-0 rounded-sm font-mono text-text-muted transition-colors hover:text-text-secondary hover:underline focus-outline"
              >
                {item.label}
              </button>
            ) : (
              <span className="shrink-0 font-mono">{item.label}</span>
            )
          ) : null}
          {uploading ? null : <span className="shrink-0">{sizeLabel}</span>}
        </div>
      </div>
      <button
        type="button"
        onClick={onRemove}
        disabled={removeDisabled}
        className="ml-1 inline-flex size-4 shrink-0 items-center justify-center rounded-sm text-text-muted hover:text-text-primary disabled:pointer-events-none disabled:opacity-50"
        aria-label={t("messages.removeAttachment")}
      >
        <CloseIcon className="size-3" />
      </button>
    </div>
  );
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(1)} GB`;
}
