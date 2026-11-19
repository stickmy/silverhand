import * as DialogPrimitive from "./modal";
import { CloseIcon } from "@/icons";
import {
  createContext,
  useContext,
  useCallback,
  forwardRef,
  useEffect,
  useState,
  type CSSProperties,
  type ComponentPropsWithoutRef,
  type ElementRef,
} from "react";
import { cn } from "@/lib/cn";
import { t } from "@/lib/i18n";

const DialogBoundaryContext = createContext<HTMLElement | null>(null);
/** Inline popovers must collide with the dialog's clipping viewport, not the window. */
export function useDialogBoundary() {
  return useContext(DialogBoundaryContext);
}

export const Dialog = DialogPrimitive.Root;
export const DialogTrigger = DialogPrimitive.Trigger;
export const DialogClose = DialogPrimitive.Close;
export const DialogPortal = DialogPrimitive.Portal;

/** The corner-button skin (surface / ghost ink / hover), shared by the modal
 *  close ✕ AND any sibling top-right action (e.g. the task detail's jump) so the
 *  two can't drift. POSITION-AGNOSTIC: the `DialogContent` corner cluster owns
 *  the `absolute` slot and the inter-button gap. poster identity comes from the
 *  `data-sticker` primitive on each button, not from this class.
 *
 *  Rest→hover is a COLOUR step (`--text-muted` → `--text-primary`), never an
 *  opacity step: dropping opacity on an already-muted multi-stroke glyph reads
 *  as smudged, not quiet. `opacity` is left to `disabled:` alone. */
export const dialogCornerButtonClass =
  "cursor-pointer rounded-[var(--control-radius)] p-1 text-text-muted transition-colors hover:bg-bg-hover hover:text-text-primary focus:outline-none";

/**
 * A corner-cluster action — the close ✕'s twin. Use this instead of hand-rolling
 * `dialogCornerButtonClass`: the button carries the `data-sticker` +
 * `data-dialog-corner-action` markers itself, so a peer can't ship as a flat
 * ghost next to the poster-sticker close. The consumer owns behaviour and glyph;
 * identity comes from the theme.
 */
export const DialogCornerButton = forwardRef<HTMLButtonElement, ComponentPropsWithoutRef<"button">>(
  ({ className, ...props }, ref) => (
    <button
      ref={ref}
      type="button"
      data-dialog-corner-action
      data-sticker
      data-sticker-size="md"
      data-sticker-tone="neutral"
      className={cn(dialogCornerButtonClass, className)}
      {...props}
    />
  ),
);
DialogCornerButton.displayName = "DialogCornerButton";

export const DialogOverlay = forwardRef<
  ElementRef<typeof DialogPrimitive.Overlay>,
  ComponentPropsWithoutRef<typeof DialogPrimitive.Overlay>
>(({ className, ...props }, ref) => (
  <DialogPrimitive.Overlay
    ref={ref}
    className={cn(
      "no-drag fixed inset-0 z-(--z-dialog) bg-overlay-backdrop data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0",
      className,
    )}
    {...props}
  />
));
DialogOverlay.displayName = "DialogOverlay";

/** Track the keyboard-reduced visual viewport without moving desktop dialogs. */
function useDialogViewport() {
  const [viewport, setViewport] = useState<CSSProperties>({});
  useEffect(() => {
    const visual = window.visualViewport;
    if (!visual) return;
    const update = () =>
      setViewport({
        "--dialog-viewport-height": `${visual.height}px`,
        "--dialog-viewport-bottom": `${Math.max(0, window.innerHeight - visual.height - visual.offsetTop)}px`,
      } as CSSProperties);
    update();
    visual.addEventListener("resize", update);
    visual.addEventListener("scroll", update);
    return () => {
      visual.removeEventListener("resize", update);
      visual.removeEventListener("scroll", update);
    };
  }, []);
  return viewport;
}

export const DialogContent = forwardRef<
  ElementRef<typeof DialogPrimitive.Content>,
  ComponentPropsWithoutRef<typeof DialogPrimitive.Content> & {
    /** Optional action(s) placed in the top-right corner cluster, LEFT of the
     *  close ✕, sharing its slot + skin. Render them with
     *  `dialogCornerButtonClass` (+ their own theme marker) so they read as
     *  peers of the close; the cluster owns position and the inter-button gap,
     *  so a consumer never guesses `right-*` or per-theme box sizes. */
    cornerActions?: React.ReactNode;
    /** Vertical band the corner cluster sits on.
     *  - `title` (default): centred on the FIRST LINE of a default-padded
     *    (`p-6`) `DialogHeader`, so the cluster reads as part of the title row
     *    instead of floating 12px above it (shiki 2026-07-28: 标题和右边的按钮
     *    不居中). The band is a fixed 24px with `items-center`, so per-theme box
     *    growth (poster's bordered stickers) stays centred on that same line.
     *  - `top`: hugs the content's top-right corner — for a dialog that owns its
     *    own padding (`p-0`) and whose first band is not a `p-6` title line. */
    cornerBand?: "title" | "top";
  }
>(({ className, children, cornerActions, cornerBand = "title", style, ...props }, ref) => {
  const viewport = useDialogViewport();
  const [boundary, setBoundary] = useState<HTMLDivElement | null>(null);
  const contentRef = useCallback(
    (node: HTMLDivElement | null) => {
      setBoundary(node);
      if (typeof ref === "function") ref(node);
      else if (ref) ref.current = node;
    },
    [ref],
  );
  return (
    <DialogPortal>
      <DialogOverlay />
      <DialogPrimitive.Content
        ref={contentRef}
        data-dialog-content
        data-modal-shell
        style={{ ...viewport, ...style }}
        data-popover-frame="info"
        className={cn(
          // TOP-ANCHORED, not vertically centred (#task:1829, shiki go). Under
          // `top-1/2 -translate-y-1/2` a dialog's HEIGHT IS ITS POSITION: every
          // content growth moves the whole shell by half the delta, in both
          // directions. That made routine, correct behaviour read as flicker —
          // a one-line validation note appearing and leaving, a member tag row
          // wrapping to a second line. Anchoring the top edge makes growth go
          // DOWNWARD only: the title and the field the user is looking at stay
          // put, and nothing has to reserve space it doesn't need.
          // 15vh (shiki), proportional so a short window keeps the whole shell
          // on screen.
          // `grid-cols-[minmax(0,1fr)]`, not the default `auto` column: an auto
          // track's min sizing function is `auto`, which floors the track at its
          // items' MIN-CONTENT width. One unbreakable string inside — a long
          // display name in the members roster (#task:1836) — then pushed the
          // track past `max-w-*` while the shell itself stayed put, so the
          // content spilled out of the frame and every `w-full` child (search
          // field, footer button) stretched with it. A `0` min sizing function
          // removes that floor, and the items' own `truncate` / `min-w-0` do the
          // rest. Non-overflowing dialogs are unaffected: a stretched auto track
          // and a `1fr` track both resolve to the content box.
          "no-drag fixed left-1/2 top-[15vh] z-(--z-dialog) grid w-full max-w-md grid-cols-[minmax(0,1fr)] -translate-x-1/2 gap-4 bg-bg-floating p-6 shadow-modal focus:outline-none",
          className,
        )}
        {...props}
        // An in-dialog combobox whose list is open owns the first Escape: Radix
        // reads Escape at document capture, before the input's own handler, so
        // without this the dialog (and the form in it) would close instead.
        onEscapeKeyDown={(event) => {
          if (
            event.target instanceof Element &&
            event.target.matches('[role="combobox"][aria-expanded="true"]')
          ) {
            event.preventDefault();
          }
          props.onEscapeKeyDown?.(event);
        }}
      >
        <DialogBoundaryContext.Provider value={boundary}>{children}</DialogBoundaryContext.Provider>
        {/* Top-right corner action cluster: optional peer actions, then the close
          ✕. A fixed 24px band + `items-center` co-aligns glyphs regardless of
          per-theme box size (poster borders the stickers → taller boxes, which
          now overflow the band symmetrically instead of pushing their centres
          down), and `gap-1.5` spaces them — so twins never need pixel/right-*
          math per theme. */}
        <div
          data-dialog-corner
          className={cn(
            "absolute right-3 flex h-6 items-center gap-1.5",
            cornerBand === "title" ? "top-6" : "top-3",
          )}
        >
          {cornerActions}
          <DialogPrimitive.Close
            data-sticker-close
            data-sticker
            data-sticker-size="md"
            data-sticker-tone="neutral"
            className={dialogCornerButtonClass}
          >
            <CloseIcon className="size-4" />
            <span className="sr-only">{t("common.close")}</span>
          </DialogPrimitive.Close>
        </div>
      </DialogPrimitive.Content>
    </DialogPortal>
  );
});
DialogContent.displayName = "DialogContent";

export function DialogHeader({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return <div className={cn("flex flex-col gap-1.5", className)} {...props} />;
}

export function DialogFooter({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn("flex flex-col-reverse gap-2 sm:flex-row sm:justify-end", className)}
      {...props}
    />
  );
}

export const DialogTitle = forwardRef<
  ElementRef<typeof DialogPrimitive.Title>,
  ComponentPropsWithoutRef<typeof DialogPrimitive.Title>
>(({ className, ...props }, ref) => (
  <DialogPrimitive.Title
    ref={ref}
    className={cn("text-base font-semibold text-text-primary", className)}
    {...props}
  />
));
DialogTitle.displayName = "DialogTitle";

export const DialogDescription = forwardRef<
  ElementRef<typeof DialogPrimitive.Description>,
  ComponentPropsWithoutRef<typeof DialogPrimitive.Description>
>(({ className, ...props }, ref) => (
  <DialogPrimitive.Description
    ref={ref}
    className={cn("text-sm text-text-muted", className)}
    {...props}
  />
));
DialogDescription.displayName = "DialogDescription";
