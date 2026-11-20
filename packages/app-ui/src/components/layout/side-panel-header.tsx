import { createContext, useContext, type HTMLAttributes, type KeyboardEvent } from "react";
import { useCompactLayout } from "@/navigation/layout";
import { MobileNavigationSlot } from "@/components/layout/mobile-header-slot";
import {
  useNativeBar,
  type PageBarDeclaration,
  type PageBarHandlers,
} from "@/components/layout/native-page-bar";
import { ArrowLeftIcon, CloseIcon, DragHandleIcon } from "@/icons";
import { t } from "@/lib/i18n";

/**
 * The fixed-height header row shared by every right-pane side panel — the agent
 * profile panel, the thread panel, the threads list, and the attachments list.
 *
 * Locks the header-height contract to the channel header's `h-12` (48px)
 * *structurally*: height, the bottom hairline, the horizontal padding, and the
 * close affordance live here once, so a panel's content can't grow the outer
 * box and drift off the channel's 48px.
 *
 * Only the header is shared, not the frame: each panel keeps its own outer
 * element because those genuinely differ (the thread panel is a
 * `<aside ref … data-pane>` with a positioned find-bar overlay; the agent panel
 * is a card `<section>` with `--card-radius` / `--card-shadow`). Forcing one
 * frame component would degrade into a soup of element / ref / bg / card props;
 * the drift-prone thing is the header row, so that is what we hoist.
 *
 * On compact iOS a panel that declares `nativeBar` has its header drawn in the
 * native bar, and a clearance takes the web row's place. Native Back leaves
 * the panel, so the close button is not declared.
 */
export function SidePanelHeader({
  children,
  actions,
  onClose,
  onBack,
  nativeBar = null,
  nativeHandlers = {},
}: {
  /** Leading/main header content — usually an icon + `<SidePanelHeaderTitle>`,
   *  or a custom identity block (avatar + name/description). It carries the
   *  row's free width (`flex-1`), so the trailing controls pin right. */
  children: React.ReactNode;
  /** Extra trailing controls rendered before the close button (search, follow,
   *  view-in-channel…). When present they + the close button sit in the
   *  `data-channel-actions` layout cluster, matching the channel header; each
   *  control carries its own poster `data-sticker` hook. */
  actions?: React.ReactNode;
  onBack?: () => void;
  onClose?: () => void;
  /** The header as the native bar draws it; null keeps the web row. */
  nativeBar?: PageBarDeclaration | null;
  nativeHandlers?: PageBarHandlers;
}): JSX.Element | null {
  const compact = useCompactLayout();
  const closeButton = onClose ? <SidePanelCloseButton onClose={onClose} /> : null;
  const native = useNativeBar(nativeBar, nativeHandlers);

  if (native.active) {
    return native.clearance;
  }

  return (
    <>
      <SidePanelHeaderRow compact={compact} onBack={onBack}>
        {children}
        {actions ? (
          // `data-channel-actions` lays the trailing controls out only; each
          // button declares its own poster sticker axes.
          <div data-channel-actions className="ml-auto flex shrink-0 items-center gap-1.5">
            {actions}
            {closeButton}
          </div>
        ) : onClose ? (
          // Close-only panels keep the button standalone (no `data-channel-actions`
          // wrapper); `ml-auto` pins it right when there is no `flex-1` content.
          <SidePanelCloseButton onClose={onClose} className="ml-auto" />
        ) : null}
      </SidePanelHeaderRow>
    </>
  );
}

function SidePanelHeaderRow({
  compact,
  onBack,
  children,
}: {
  compact: boolean;
  onBack?: () => void;
  children: React.ReactNode;
}): JSX.Element {
  return (
    <header
      data-side-panel-header
      className="flex h-12 shrink-0 items-center gap-2 border-b border-border-subtle px-3"
    >
      <MobileNavigationSlot />
      <PaneGrip />
      {!compact && onBack ? (
        <button
          type="button"
          aria-label={t("common.back")}
          onClick={onBack}
          className="flex size-7 shrink-0 items-center justify-center rounded-(--control-radius) text-text-secondary hover:bg-bg-hover focus-outline"
        >
          <ArrowLeftIcon className="size-4" />
        </button>
      ) : null}
      {children}
    </header>
  );
}

function SidePanelCloseButton({
  onClose,
  className = "",
}: {
  onClose: () => void;
  className?: string;
}): JSX.Element {
  return (
    <button
      type="button"
      aria-label={t("common.close")}
      title={t("common.close")}
      onClick={onClose}
      data-sticker-close
      data-sticker
      data-sticker-size="md"
      data-sticker-tone="neutral"
      className={
        "flex size-7 shrink-0 items-center justify-center rounded-(--control-radius) text-text-secondary transition-colors hover:bg-bg-hover hover:text-text-primary focus-outline " +
        className
      }
    >
      <CloseIcon className="size-3.5" />
    </button>
  );
}

/** The header of a pane the viewer can reorder: the grip is its keyboard
 *  handle. Null for a pane that does not move. */
export interface PaneMovable {
  ref: (node: HTMLElement | null) => void;
  attributes: HTMLAttributes<HTMLElement>;
  onKeyDown: ((event: KeyboardEvent) => void) | undefined;
}

const PaneMovableContext = createContext<PaneMovable | null>(null);
export const PaneMovableProvider = PaneMovableContext.Provider;

/** The drag hint on a movable pane's header. The whole header drags with the
 *  pointer; the grip is where the keyboard picks the pane up. */
export function PaneGrip(): JSX.Element | null {
  const movable = useContext(PaneMovableContext);
  if (!movable) return null;
  return (
    <span
      ref={movable.ref}
      {...movable.attributes}
      onKeyDown={movable.onKeyDown}
      data-pane-grip
      className="-ml-1 flex shrink-0 cursor-grab items-center rounded-(--control-radius) text-text-muted focus-outline"
    >
      <DragHandleIcon className="size-3.5" />
    </span>
  );
}

/** The conversation an attached pane belongs to, named in its header when it
 *  is not the conversation the stage is showing first; null names none. */
const PaneOwnerContext = createContext<string | null>(null);
export const PaneOwnerProvider = PaneOwnerContext.Provider;

/**
 * The icon + title + context idiom shared by the thread, threads-list, and
 * attachments panels: one line, the context muted after the title. The context
 * is short and stays whole up to a cap; a long title truncates first. Pass this
 * as `SidePanelHeader`'s children.
 */
export function SidePanelHeaderTitle({
  icon,
  title,
  subtitle,
}: {
  icon?: React.ReactNode;
  title: React.ReactNode;
  subtitle?: React.ReactNode;
}): JSX.Element {
  const owner = useContext(PaneOwnerContext);

  return (
    <>
      {icon}
      <div className="flex min-w-0 flex-1 items-center gap-2">
        <div className="min-w-0 shrink truncate text-sm font-medium text-text-primary">{title}</div>
        {subtitle ? (
          <div className="max-w-[45%] shrink-0 truncate text-xs text-text-muted">{subtitle}</div>
        ) : null}
        {owner ? (
          <span
            data-pane-owner
            title={owner}
            className="max-w-[45%] shrink-0 truncate rounded-(--control-radius) bg-bg-hover px-1.5 py-0.5 text-xs text-text-secondary"
          >
            {owner}
          </span>
        ) : null}
      </div>
    </>
  );
}
