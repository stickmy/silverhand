import "./image-preview.css";
import { getPlatform } from "@/platform";
import { RangeInput } from "@/components/ui/range-input";
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import * as DialogPrimitive from "@/components/ui/modal";
import { CloseIcon, DownloadIcon, ShareIcon, ZoomInIcon, ZoomOutIcon } from "@/icons";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { usePinchZoomLease } from "@/lib/pinch-zoom-lease";
import { SelectionActionButton } from "./selection-toolbar";
import { t } from "@/lib/i18n";

interface LightboxProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  src: string;
  alt: string;
  /** Read original bytes through the resource owner, independently of its display URL. */
  loadExportBytes?: () => Promise<Uint8Array>;
  /** The sent attachment shown, when there is one: a native preview fetches
   *  it itself instead of waiting on the page for its bytes. */
  attachmentId?: string;
  /**
   * Optional save-as callback. When provided, the footer shows a
   * "Download" action alongside "Copy". Omitted for the composer's
   * pre-send preview (the file is on the user's disk already, so
   * save-as is not a meaningful action there).
   */
  onDownload?: () => void | Promise<void>;
}

/** Zoom baseline = fit-to-window (100%). Zoom only grows the image above
 *  fit — below fit is just a tiny image in an empty box, no use — so
 *  MIN is the fit and "zoom out" walks back down toward it. */
const MIN_SCALE = 1;
const MAX_SCALE = 5;
/** Per-click multiplier for the +/− buttons and the +/− keys. */
const STEP = 1.25;
/** Horizontal + vertical padding of the image well (`p-3` = 12px each side),
 *  subtracted from the container's client box to get the fit area. */
const WELL_PADDING = 24;
/** The shell's outer caps. They bound BOTH states: the ceiling the hugging
 *  (`fit-content`) box may grow to, and — because a frozen box is a px value
 *  measured before the window could change — the clamp that keeps a frozen
 *  shell inside a window the user has since made smaller. */
const SHELL_MAX_WIDTH = "min(920px, 88vw)";
const SHELL_MAX_HEIGHT = "82vh";
/** Header + footer + the well's padding, as an approximation used only for the
 *  RESTING image caps (the real chrome measures ~110px). It keeps a
 *  fit-to-window image from overflowing the shell's own `82vh` ceiling, which
 *  would put the well into scroll while still at 100%. */
const SHELL_CHROME = "7rem";
/**
 * Floors for the hug (shiki 2026-08-06「给一个最小尺寸」/「有时候确实太窄了,
 * 观感不好」). The shell hugging the image is right for the resting view, but a
 * hug is also the pan viewport once the image is magnified, and a tall narrow
 * photo, a 32px icon or a 1200×80 banner hugs down to a sliver that reads as a
 * broken card and cannot be panned in.
 *
 * The width floor goes on the SHELL (it is the card's own proportion, and the
 * footer's intrinsic width — ~396px — is not enough of one); the height floor
 * goes on the WELL, since the header and footer add their own height on top.
 *
 * Both are `min(fixed, viewport)` because a floor must yield to a small window:
 * the shell is `overflow-hidden` under `82vh`, so a floor that outlived the
 * window would push the footer out of the frame instead of shrinking the well.
 */
const SHELL_MIN_WIDTH = "min(40rem, 60vw)";
const WELL_MIN_HEIGHT = "min(15rem, 40vh)";

const clamp = (v: number, min: number, max: number): number => Math.min(max, Math.max(min, v));

/**
 * Image preview modal. A modal shell (elevation via `shadow-xl`) that wears
 * poster's shared ink modal frame + poster drop shadow; other themes render it
 * frameless (elevation only). It has a header ("Image Preview" title + a close
 * icon on the right), the image well, and a bottom action bar (zoom controls
 * left, Copy/Download right).
 *
 * The shell hugs the fitted image — `fit-content` under the
 * `min(920px, 88vw)` / `82vh` caps — so a small image gets a compact card
 * instead of sitting in an 82vh-tall empty well (shiki 2026-08-06
 * 「图片很小的时候会显得很高」). It hugs no tighter than its floors —
 * `SHELL_MIN_WIDTH` and `WELL_MIN_HEIGHT` — so the card keeps a readable
 * proportion and the zoom + Copy/Download controls are never squeezed.
 *
 * Its size is a function of the IMAGE, never of zoom. The frame the user is
 * looking through does not move when they magnify what is inside it — the
 * desktop image-viewer idiom, and the two rejected alternatives were both
 * shiki's: a shell that grew step after step with the image and then stalled at
 * the cap (2026-07-22), and one that jumped to the full capped viewport the
 * moment zoom left 100% (2026-08-06 「放大图片的时候,previewer 突然放大很奇怪」).
 * So on the way into zoom the box is FROZEN at the size it is already rendered
 * at — a measured px value, which makes the freeze visually a no-op — and the
 * magnified image overflows into the `overflow-auto` well instead. Reset-to-fit
 * thaws it back to hugging.
 *
 * Zoom (pinch / slider / `+` `−` buttons / `+` `−` `0` keys) sets the image's
 * *actual* rendered size to `fit × scale`, so past 100% it overflows the
 * well and the user scrolls both axes (or drags) to inspect detail. Zoom keeps
 * the viewport centre fixed. Small images are never scaled UP to fill the well
 * (`k ≤ 1`): these are mostly screenshots, where 1:1 pixels are the point.
 * Trackpad pinch is enabled per-frame only while open (macOS Electron disables
 * it by default); a plain wheel / swipe is left to scroll the well. Backdrop
 * click + Escape close (Radix defaults).
 *
 * Copy reuses the `export:copyPng` IPC (`copyPngExport`): the resource owner
 * supplies bytes (local previews fall back to `src`) and main builds an NSImage + writes the OS
 * pasteboard — the same path the screenshot export uses, which is far
 * more reliable than renderer-side `navigator.clipboard.write` for
 * images on Electron/macOS. nativeImage decodes PNG/JPEG; an
 * undecodable format surfaces a toast rather than failing silently.
 */
export function Lightbox({
  open,
  onOpenChange,
  src,
  alt,
  onDownload,
  loadExportBytes,
  attachmentId,
}: LightboxProps): JSX.Element {
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);
  const [downloading, setDownloading] = useState(false);

  // `scale` = 1 is fit-to-window. `fit` is the on-screen size the image
  // takes at scale 1 (computed from its natural size + the well): the
  // rendered size is `fit × scale`, so at scale 1 it fits exactly and above
  // it overflows into native scroll. `dragging` toggles the grab cursor.
  const [scale, setScale] = useState(1);
  const [fit, setFit] = useState<{ w: number; h: number } | null>(null);
  const [dragging, setDragging] = useState(false);
  // The shell's frozen box while zoomed, in px, measured off the hugging shell
  // on the way in (see the docblock). `null` = hugging.
  const [frozenBox, setFrozenBox] = useState<{ w: number; h: number } | null>(null);
  const shellRef = useRef<HTMLDivElement | null>(null);
  const wellRef = useRef<HTMLDivElement | null>(null);
  const imgRef = useRef<HTMLImageElement | null>(null);
  const scaleRef = useRef(1);
  // Viewport-centre fraction captured just before a zoom, re-applied after
  // the image resizes so zoom stays anchored on the middle of the view.
  const pendingCenterRef = useRef<{ fx: number; fy: number } | null>(null);
  // Drag-to-pan origin: pointer + scroll offset captured at pointerdown.
  const exportGeneration = useRef(0);
  const dragRef = useRef<{ px: number; py: number; sl: number; st: number } | null>(null);

  useEffect(() => {
    scaleRef.current = scale;
  }, [scale]);

  // Reset zoom whenever the modal opens or the image changes — a stale zoom
  // carried into a different image would open it mid-scroll.
  useEffect(() => {
    exportGeneration.current += 1;
    setBusy(false);
    setScale(1);
    setFit(null);
    setFrozenBox(null);
    pendingCenterRef.current = null;
    return () => {
      exportGeneration.current += 1;
    };
  }, [open, src]);

  // Fit = the image's natural size scaled down (never up) to sit inside the
  // well. Recomputed on load + on well resize so it tracks the window. macOS
  // overlay scrollbars don't consume layout width, so a zoomed-in scrollbar
  // doesn't shrink the well and feed back into fit.
  const computeFit = useCallback((): void => {
    const c = wellRef.current;
    const im = imgRef.current;
    if (!c || !im || !im.naturalWidth || !im.naturalHeight) return;
    const availW = Math.max(1, c.clientWidth - WELL_PADDING);
    const availH = Math.max(1, c.clientHeight - WELL_PADDING);
    const k = Math.min(availW / im.naturalWidth, availH / im.naturalHeight, 1);
    setFit({ w: im.naturalWidth * k, h: im.naturalHeight * k });
  }, []);

  useEffect(() => {
    const c = wellRef.current;
    if (!c) return;
    computeFit(); // cover an already-cached image whose onLoad won't refire
    const ro = new ResizeObserver(() => computeFit());
    ro.observe(c);
    return () => ro.disconnect();
  }, [open, computeFit]);

  const setZoom = useCallback((raw: number): void => {
    const next = clamp(raw, MIN_SCALE, MAX_SCALE);
    // Freeze / thaw the shell BEFORE the new scale renders, in the same batch,
    // so the larger image never gets a frame in which it could grow the hugging
    // box. The size is READ from the live box rather than computed, which is
    // what makes the freeze invisible: the shell keeps exactly the geometry it
    // already had. Rounded to whole device pixels — `margin: auto` centering of
    // a fractional width would land the raster image mid-pixel and blur it.
    if (next > MIN_SCALE) {
      const shell = shellRef.current;
      if (shell) {
        const r = shell.getBoundingClientRect();
        // First zoom step of a session wins; later steps must not re-measure a
        // box that scroll/overflow has since had a say in.
        setFrozenBox((prev) => prev ?? { w: Math.round(r.width), h: Math.round(r.height) });
      }
    } else {
      setFrozenBox(null);
    }
    const c = wellRef.current;
    if (c) {
      const sw = c.scrollWidth || 1;
      const sh = c.scrollHeight || 1;
      pendingCenterRef.current = {
        fx: (c.scrollLeft + c.clientWidth / 2) / sw,
        fy: (c.scrollTop + c.clientHeight / 2) / sh,
      };
    }
    setScale(next);
  }, []);

  // After the image resizes (scale or fit changed), re-centre the scroll on
  // the fraction captured before the zoom. The browser clamps out-of-range
  // scroll offsets, so shrinking back to fit lands at 0/0 (centred by the
  // flex `m-auto`).
  useLayoutEffect(() => {
    const c = wellRef.current;
    const p = pendingCenterRef.current;
    if (!c || !p) return;
    pendingCenterRef.current = null;
    c.scrollLeft = p.fx * c.scrollWidth - c.clientWidth / 2;
    c.scrollTop = p.fy * c.scrollHeight - c.clientHeight / 2;
  }, [scale, fit]);

  // Trackpad pinch, bound through a callback ref (not a useEffect keyed off
  // the ref) so the native, non-passive listener attaches the instant the
  // node mounts — no window where a missed effect run leaves pinch dead.
  // Non-passive is required to `preventDefault`. Only pinch (`wheel` +
  // `ctrlKey`, which macOS emits once pinch is enabled below) zooms; a plain
  // wheel / two-finger swipe is left untouched so it scrolls the well.
  const wheelCleanupRef = useRef<(() => void) | null>(null);
  const attachWell = useCallback(
    (node: HTMLDivElement | null): void => {
      wellRef.current = node;
      wheelCleanupRef.current?.();
      wheelCleanupRef.current = null;
      if (!node) return;
      const onWheel = (e: WheelEvent): void => {
        if (!e.ctrlKey) return; // plain scroll → let the well scroll
        e.preventDefault();
        setZoom(scaleRef.current * Math.exp(-e.deltaY / 100));
      };
      node.addEventListener("wheel", onWheel, { passive: false });
      wheelCleanupRef.current = () => node.removeEventListener("wheel", onWheel);
    },
    [setZoom],
  );

  // Enable trackpad pinch only while open (preload `setPinchZoom` →
  // `webFrame.setVisualZoomLevelLimits`). macOS Electron disables pinch by
  // default, and a disabled pinch is never dispatched to the page as
  // `ctrl+wheel` at all. Refcounted so a peer previewer mounted underneath
  // keeps pinch alive when this one closes.
  usePinchZoomLease(open);

  // Drag-to-pan: when the image overflows the well, grabbing it scrolls the
  // well (the hand-tool idiom), complementing native scroll. Capture the
  // pointer so a drag that leaves the image keeps panning.
  const onPointerDown = (e: React.PointerEvent<HTMLImageElement>): void => {
    const c = wellRef.current;
    if (!c) return;
    const overflowing = c.scrollWidth > c.clientWidth || c.scrollHeight > c.clientHeight;
    if (!overflowing) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    dragRef.current = { px: e.clientX, py: e.clientY, sl: c.scrollLeft, st: c.scrollTop };
    setDragging(true);
  };
  const onPointerMove = (e: React.PointerEvent<HTMLImageElement>): void => {
    const c = wellRef.current;
    const d = dragRef.current;
    if (!c || !d) return;
    c.scrollLeft = d.sl - (e.clientX - d.px);
    c.scrollTop = d.st - (e.clientY - d.py);
  };
  const endDrag = (e: React.PointerEvent<HTMLImageElement>): void => {
    if (!dragRef.current) return;
    dragRef.current = null;
    setDragging(false);
    try {
      e.currentTarget.releasePointerCapture(e.pointerId);
    } catch {
      /* capture may already be gone (e.g. pointercancel) */
    }
  };

  const onKeyDown = (e: React.KeyboardEvent): void => {
    if (e.key === "+" || e.key === "=") {
      e.preventDefault();
      setZoom(scale * STEP);
    } else if (e.key === "-" || e.key === "_") {
      e.preventDefault();
      setZoom(scale / STEP);
    } else if (e.key === "0") {
      e.preventDefault();
      setZoom(1);
    }
  };

  const shareImage = getPlatform().files.sharePngExport;
  // A native host shows the image in its own sheet (zoom, share), opened at
  // once. The displayed image is a local blob, so its bytes are handed over;
  // only when that read fails does native download the attachment itself,
  // inside the already-open sheet.
  const nativePreview = getPlatform().files.previewPngExport;
  useEffect(() => {
    if (!open || !nativePreview) {
      return;
    }
    const generation = exportGeneration.current;
    const labels = {
      title: alt,
      shareLabel: t("messages.share"),
      closeLabel: t("common.close"),
    };
    void (async () => {
      try {
        const local = await fetch(src)
          .then(async (res) => new Uint8Array(await res.arrayBuffer()))
          .catch((err: unknown) => {
            if (!attachmentId) {
              throw err;
            }
            return null;
          });
        const image = local ? { pngBytes: local } : { attachmentId: attachmentId! };
        if (generation !== exportGeneration.current) {
          return;
        }
        await nativePreview({ image, ...labels });
      } catch (err) {
        toast.error(err instanceof Error ? err.message : String(err));
      }
      if (generation === exportGeneration.current) {
        onOpenChange(false);
      }
    })();
  }, [open, src]);

  useEffect(() => {
    if (!copied) {
      return;
    }
    const timer = window.setTimeout(() => setCopied(false), 1500);
    return () => window.clearTimeout(timer);
  }, [copied]);

  const onExport = async (): Promise<void> => {
    if (busy) return;
    const generation = exportGeneration.current;
    setBusy(true);
    try {
      // Attachment display URLs belong to an evictable cache, not to this modal.
      // Use its authenticated byte reader instead of fetching the preview URL again.
      const bytes = loadExportBytes
        ? await loadExportBytes()
        : new Uint8Array(await (await fetch(src)).arrayBuffer());
      if (generation !== exportGeneration.current) return;
      const result = await (shareImage ?? getPlatform().files.copyPngExport)({ pngBytes: bytes });
      if (generation !== exportGeneration.current) return;
      if (result.ok) {
        setCopied(!shareImage);
      } else if ("error" in result) {
        toast.error(
          shareImage ? t("messages.shareFailed", { reason: result.error }) : t("common.copyFailed"),
        );
      }
    } catch (err) {
      if (generation === exportGeneration.current)
        toast.error(
          shareImage
            ? t("messages.shareFailed", {
                reason: err instanceof Error ? err.message : String(err),
              })
            : t("common.copyFailed"),
        );
    } finally {
      if (generation === exportGeneration.current) setBusy(false);
    }
  };

  const onSave = async (): Promise<void> => {
    if (downloading || !onDownload) return;
    setDownloading(true);
    try {
      await onDownload();
    } finally {
      setDownloading(false);
    }
  };

  const zoomed = scale > MIN_SCALE;

  if (nativePreview) {
    return <></>;
  }

  return (
    <DialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay className="fixed inset-0 z-(--z-lightbox) bg-overlay-backdrop data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0" />
        <DialogPrimitive.Content
          ref={shellRef}
          data-modal-shell
          data-image-lightbox
          onKeyDown={onKeyDown}
          // The shell's size is a function of the IMAGE, never of zoom (see the
          // component docblock): it hugs the fitted image under the caps, and
          // zoom freezes that exact box in px rather than resizing it.
          // Centered with `fixed inset-0 + margin:auto` (not translate-50%):
          // margin-auto keeps the box on whole device pixels so the raster image
          // stays crisp, whereas a 50% translate on an odd size lands mid-pixel
          // and blurs it (shiki "有时候图片不清晰"). `fit-content` is what makes
          // that trick shrink-to-fit — plain `auto` would stretch to `inset: 0`.
          className="fixed inset-0 z-(--z-lightbox-chrome) m-auto flex flex-col overflow-hidden rounded-(--floating-radius,var(--radius-md)) bg-bg-floating shadow-xl focus:outline-none"
          style={{
            width: frozenBox ? `${frozenBox.w}px` : "fit-content",
            height: frozenBox ? `${frozenBox.h}px` : "fit-content",
            minWidth: SHELL_MIN_WIDTH,
            maxWidth: SHELL_MAX_WIDTH,
            maxHeight: SHELL_MAX_HEIGHT,
          }}
        >
          <div
            data-modal-divider
            className="flex shrink-0 items-center gap-2 border-b-(length:--control-border-width) border-border-subtle px-3 py-2"
          >
            <DialogPrimitive.Title className="min-w-0 flex-1 truncate text-sm font-semibold text-text-primary">
              {t("messages.imagePreview")}
            </DialogPrimitive.Title>
            <DialogPrimitive.Close
              aria-label={t("common.close")}
              data-sticker-close
              data-sticker
              data-sticker-size="md"
              data-sticker-tone="neutral"
              className="flex size-7 shrink-0 items-center justify-center rounded-(--control-radius) text-text-secondary hover:bg-bg-hover hover:text-text-primary focus-outline"
            >
              <CloseIcon className="size-4" />
            </DialogPrimitive.Close>
          </div>

          {/* Image well. `flex` + child `m-auto` (NOT justify/items-center)
              centers the image in both axes when it fits, yet lets scroll
              reach every edge once it overflows — the standard fix for
              "flex centering clips overflow". Which side sizes which flips with
              the shell: while hugging, the well's CONTENT sizes the shell (the
              grow factor is inert there, since there is no free space to
              distribute) but never below the floors; once the shell is
              frozen for zoom it sizes the well instead, and the magnified image
              overflows into scroll. The explicit `minHeight` also does the job
              `min-h-0` used to: it replaces this flex item's automatic minimum
              size, so the well shrinks to its floor under a magnified image
              rather than pushing the footer out of the frame. */}
          <div
            ref={attachWell}
            data-lightbox-well
            className="flex flex-1 overflow-auto bg-bg-elevated p-3 scrollbar-hide"
            style={{ minHeight: WELL_MIN_HEIGHT }}
          >
            <img
              ref={imgRef}
              src={src}
              alt={alt}
              draggable={false}
              onLoad={computeFit}
              onPointerDown={onPointerDown}
              onPointerMove={onPointerMove}
              onPointerUp={endDrag}
              onPointerCancel={endDrag}
              // Explicit `fit × scale` size (once measured) so the image
              // genuinely overflows the well past 100% and native scroll
              // engages. Before the first measure, fall back to the viewport
              // caps + object-contain so the first paint already fits — and at
              // rest that is also what the hugging shell measures itself
              // against, so a fit-to-window image can never put the well into
              // scroll while still at 100%.
              className="m-auto block shrink-0 select-none object-contain"
              style={
                fit
                  ? {
                      width: `${fit.w * scale}px`,
                      height: `${fit.h * scale}px`,
                      // Override Tailwind Preflight's `img { max-width: 100% }`
                      // — without this the explicit width is clamped to the
                      // well's width, so zoom past fit couldn't widen the image
                      // (it just stalled at 100% width; shiki 2026-07-22).
                      maxWidth: "none",
                      maxHeight: "none",
                      cursor: zoomed ? (dragging ? "grabbing" : "grab") : "default",
                    }
                  : {
                      maxWidth: `calc(${SHELL_MAX_WIDTH} - ${WELL_PADDING}px)`,
                      maxHeight: `calc(${SHELL_MAX_HEIGHT} - ${SHELL_CHROME})`,
                    }
              }
            />
          </div>

          <div
            data-modal-divider
            data-lightbox-footer
            className="flex shrink-0 items-center justify-between gap-2 border-t-(length:--control-border-width) border-border-subtle px-3 py-2 text-xs"
          >
            <div className="flex items-center gap-2">
              <Button
                type="button"
                variant="neutral"
                size="icon"
                className="size-[22px] shrink-0"
                aria-label={t("messages.zoomOut")}
                disabled={scale <= MIN_SCALE}
                onClick={() => setZoom(scale / STEP)}
                data-zoom-stepper
              >
                <ZoomOutIcon className="size-4" />
              </Button>
              <RangeInput
                data-zoom-range
                className="w-28"
                min={MIN_SCALE * 100}
                max={MAX_SCALE * 100}
                step="any"
                value={scale * 100}
                aria-label={t("messages.zoomLevel")}
                onChange={(e) => setZoom(Number(e.target.value) / 100)}
              />
              <Button
                type="button"
                variant="neutral"
                size="icon"
                className="size-[22px] shrink-0"
                aria-label={t("messages.zoomIn")}
                disabled={scale >= MAX_SCALE}
                onClick={() => setZoom(scale * STEP)}
                data-zoom-stepper
              >
                <ZoomInIcon className="size-4" />
              </Button>
              {/* Zoom level is a READOUT, not a control: no click / reset, no
                  button or sticker chrome. Reset is the `0` key (and stepping
                  with −). A visually-hidden "Zoom " prefix gives screen readers
                  "Zoom 150%", not a bare percentage. */}
              <span className="flex h-7 min-w-13 items-center justify-center px-1.5 tabular-nums text-text-secondary">
                <span className="sr-only">{t("messages.zoom")} </span>
                {Math.round(scale * 100)}%
              </span>
            </div>
            <div className="flex items-center gap-2">
              <SelectionActionButton
                rank={onDownload && !shareImage ? "secondary" : "primary"}
                label={
                  shareImage
                    ? t("messages.share")
                    : copied
                      ? t("messages.copied")
                      : t("messages.copy")
                }
                icon={shareImage ? <ShareIcon className="size-3.5" /> : undefined}
                disabled={busy}
                onClick={onExport}
              />
              {onDownload && !shareImage ? (
                <SelectionActionButton
                  rank="primary"
                  icon={<DownloadIcon className="size-3.5" />}
                  label={t("messages.download")}
                  disabled={downloading}
                  onClick={() => void onSave()}
                />
              ) : null}
            </div>
          </div>
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
