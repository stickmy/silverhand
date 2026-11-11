import { useEffect, useRef, useState } from "react";
import type { JSX, ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { SpinnerIcon } from "@/icons";
import { cn } from "@/lib/cn";
import { t } from "@/lib/i18n";

export type ConfirmButtonProps = {
  /** Trigger content. */
  children: ReactNode;
  /** The commit button's label; it names the consequence ("Delete permanently"). */
  confirmLabel: string;
  /** Leading glyph on the commit; a neutral commit needs it to read apart from Cancel. */
  confirmIcon?: ReactNode;
  onConfirm: () => void;
  variant?: "destructive" | "neutral";
  disabled?: boolean;
  /** Keeps the confirm step open with a spinner until the action settles, so a
   *  failure lands back on the same step. */
  pending?: boolean;
  pendingLabel?: string;
  className?: string;
};

/**
 * Two-step commit for settings panels: the trigger turns into Cancel plus a
 * labelled commit in place. Red appears only on the destructive commit, the one
 * moment the panel has a primary action; a neutral commit (Leave) stays ink. Opening focuses Cancel so Enter never
 * commits by accident; Escape cancels.
 */
export function ConfirmButton({
  children,
  confirmLabel,
  confirmIcon,
  onConfirm,
  variant = "destructive",
  disabled = false,
  pending = false,
  pendingLabel,
  className,
}: ConfirmButtonProps): JSX.Element {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const wasOpen = useRef(false);

  useEffect(() => {
    if (open) {
      cancelRef.current?.focus();
    } else if (wasOpen.current) {
      triggerRef.current?.focus();
    }
    wasOpen.current = open;
  }, [open]);

  if (!open) {
    return (
      <Button
        ref={triggerRef}
        type="button"
        variant={variant === "destructive" ? "destructive-outline" : "neutral"}
        disabled={disabled}
        className={className}
        onClick={() => setOpen(true)}
      >
        {children}
      </Button>
    );
  }

  return (
    <div
      role="group"
      aria-label={confirmLabel}
      className={cn("flex shrink-0 items-center gap-2", className)}
      onKeyDown={(event) => {
        if (event.key === "Escape" && !pending) {
          event.preventDefault();
          event.stopPropagation();
          setOpen(false);
        }
      }}
    >
      <Button
        ref={cancelRef}
        type="button"
        variant="cancel"
        disabled={pending}
        onClick={() => setOpen(false)}
      >
        {t("common.cancel")}
      </Button>
      <Button type="button" variant={variant} disabled={disabled || pending} onClick={onConfirm}>
        {pending ? <SpinnerIcon className="size-3.5 animate-spin" /> : confirmIcon}
        {pending && pendingLabel ? pendingLabel : confirmLabel}
      </Button>
    </div>
  );
}
