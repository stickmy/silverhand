import { useEffect, useId, useRef, useState } from "react";
import type { JSX, ReactNode } from "react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import { CheckIcon, CloseIcon } from "@/icons";
import { cn } from "@/lib/cn";
import { t } from "@/lib/i18n";

const EASE = [0.32, 0.72, 0, 1] as const;
const PANEL_WIDTH = 84;

export type DeleteButtonProps = {
  /** Trigger content, not another button: this control owns all three buttons. */
  children: ReactNode;
  onConfirm: () => void;
  onCancel?: () => void;
  variant?: "destructive" | "neutral";
  disabled?: boolean;
  confirmLabel?: string;
  className?: string;
};

/**
 * rare-ui's expanding surface with a content-sized trigger. The outer surface
 * stays painted. The fixed-width recess sits on that surface, with BOTH ends
 * rounded and a notch pointing left. Only the outer space expands; the recess
 * itself is never squeezed or clipped.
 *
 * The colour lives on the CONTINUOUS outer surface, never on the trigger, so
 * the control always reads as one capsule: destructive paints the whole surface
 * `--error` (its pre-DeleteButton red), neutral paints `--bg-elevated`. The
 * trigger is always transparent (destructive → `text-on-error` on the red,
 * neutral → `text-primary`). The confirm zone is a `--bg-floating` capsule
 * covering the right of that surface (full height, same corners). Confirm /
 * Cancel are plain text-glyph buttons (no fill, no hover background — hover
 * only shifts colour): Confirm is `--success` (the go/affirm hue — never
 * `--error`, which is the delete action itself), Cancel is `--text-secondary`.
 * Corners follow `--control-radius` (poster sharp, iris 8px, slack/studio 6px).
 */
export function DeleteButton({
  children,
  onConfirm,
  onCancel,
  variant = "destructive",
  disabled = false,
  confirmLabel = t("ui.confirm"),
  className,
}: DeleteButtonProps): JSX.Element {
  const reduced = useReducedMotion() ?? false;
  const [open, setOpen] = useState(false);
  const expanded = open && !disabled;
  const triggerRef = useRef<HTMLButtonElement>(null);
  const confirmRef = useRef<HTMLButtonElement>(null);
  const panelId = useId();
  // Reject repeated clicks even before React has rendered the closed state.
  const admitted = useRef(false);

  useEffect(() => {
    if (expanded) confirmRef.current?.focus();
  }, [expanded]);

  useEffect(() => {
    if (disabled) {
      admitted.current = false;
      setOpen(false);
    }
  }, [disabled]);

  const resolve = (confirmed: boolean): void => {
    if (disabled || !admitted.current) return;
    admitted.current = false;
    setOpen(false);
    triggerRef.current?.focus();
    (confirmed ? onConfirm : onCancel)?.();
  };

  const glyphButton = cn(
    "grid size-7 shrink-0 place-items-center rounded-full",
    "focus-outline disabled:pointer-events-none",
  );

  return (
    <motion.div
      data-slot="delete-button"
      data-state={expanded ? "open" : "closed"}
      className={cn(
        "relative inline-flex h-10 shrink-0 items-center rounded-(--control-radius)",
        variant === "destructive" ? "bg-error" : "bg-bg-elevated",
        disabled && "opacity-50",
        className,
      )}
      initial={false}
      animate={{ paddingRight: expanded ? PANEL_WIDTH : 0 }}
      transition={{ duration: reduced ? 0 : 0.62, ease: EASE }}
      onKeyDown={(event) => {
        if (event.key === "Escape" && expanded) {
          event.preventDefault();
          event.stopPropagation();
          resolve(false);
        }
      }}
    >
      <button
        ref={triggerRef}
        type="button"
        disabled={disabled}
        aria-expanded={expanded}
        aria-controls={expanded ? panelId : undefined}
        onClick={() => {
          if (disabled) return;
          if (admitted.current) return resolve(false);
          admitted.current = true;
          setOpen(true);
        }}
        className={cn(
          "relative z-10 inline-flex h-10 items-center justify-center gap-2 whitespace-nowrap rounded-(--control-radius) bg-transparent px-3 text-sm font-medium focus-outline disabled:pointer-events-none",
          variant === "destructive" ? "text-text-on-error" : "text-text-primary",
        )}
      >
        {children}
      </button>

      <AnimatePresence>
        {expanded && (
          <motion.div
            key="confirmation"
            id={panelId}
            role="group"
            aria-label={confirmLabel}
            style={{ width: PANEL_WIDTH }}
            className="absolute inset-y-0 right-0 flex items-center justify-center gap-2 rounded-(--control-radius) bg-bg-floating"
            initial={reduced ? false : "hidden"}
            animate="shown"
            exit="hidden"
            variants={{
              hidden: {
                opacity: 0,
                x: reduced ? 0 : -6,
                transition: { duration: reduced ? 0 : 0.3 },
              },
              shown: {
                opacity: 1,
                x: 0,
                transition: {
                  duration: reduced ? 0 : 0.44,
                  ease: EASE,
                  delay: reduced ? 0 : 0.14,
                  staggerChildren: reduced ? 0 : 0.07,
                },
              },
            }}
          >
            {[
              { label: confirmLabel, confirmed: true, icon: CheckIcon },
              { label: t("common.cancel"), confirmed: false, icon: CloseIcon },
            ].map(({ label, confirmed, icon: Icon }) => (
              <motion.div
                key={label}
                variants={{
                  hidden: {
                    opacity: 0,
                    scale: reduced ? 1 : 0.9,
                    transition: { duration: reduced ? 0 : 0.3 },
                  },
                  shown: {
                    opacity: 1,
                    scale: 1,
                    transition: { duration: reduced ? 0 : 0.44, ease: EASE },
                  },
                }}
              >
                <motion.button
                  ref={confirmed ? confirmRef : undefined}
                  type="button"
                  aria-label={label}
                  disabled={disabled || !expanded}
                  onClick={() => resolve(confirmed)}
                  whileHover={reduced ? undefined : "hover"}
                  whileTap={reduced ? undefined : { scale: 0.84 }}
                  className={cn(
                    glyphButton,
                    confirmed
                      ? "text-success hover:opacity-80"
                      : "text-text-secondary hover:text-text-primary",
                  )}
                >
                  <motion.span
                    className="grid place-items-center"
                    variants={reduced ? undefined : { hover: { scale: 1.2 } }}
                    transition={{ duration: 0.18, ease: EASE }}
                  >
                    <Icon aria-hidden="true" className="size-3.5" />
                  </motion.span>
                </motion.button>
              </motion.div>
            ))}
          </motion.div>
        )}
      </AnimatePresence>
    </motion.div>
  );
}

export default DeleteButton;
