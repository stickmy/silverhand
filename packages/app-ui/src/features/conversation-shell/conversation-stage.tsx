import { useCallback, useLayoutEffect, useRef, type ReactNode } from "react";
import { useCompactLayout } from "@/navigation/layout";
import { SplitPane } from "@/components/layout/split-pane";
import { PANE_BG } from "@/components/layout/pane-chrome";
import { createPortal } from "react-dom";
import {
  registerSplitDropStage,
  splitDropTarget,
  useSplitDragGhost,
  useSplitDropState,
} from "@/components/layout/split-drop";
import { ConversationViewSkeleton } from "@/features/conversation-shell/components/conversation-view-skeleton";
import { SplitRightIcon } from "@/icons";
import { cn } from "@/lib/cn";
import { t } from "@/lib/i18n";
import type { ConversationContent } from "./conversation-content";

export function ConversationStage({
  content,
  ready,
  fullscreen,
  storageKey,
  acceptsSplitDrop = false,
}: {
  content: ConversationContent;
  ready: boolean;
  fullscreen: boolean;
  storageKey: string;
  /** Dragging a sidebar row onto the right half opens it as a second column. */
  acceptsSplitDrop?: boolean;
}) {
  const compact = useCompactLayout();
  if (!ready) {
    return (
      <ConversationViewSkeleton
        showTabs={content.loadingTabs && !(content.secondary && fullscreen)}
      />
    );
  }
  if (content.secondary && (fullscreen || compact)) {
    return content.secondary;
  }
  if (compact) return content.primary;
  // Always mounted, so opening or closing the second column never remounts the
  // first (its scroll position and draft survive).
  const columns = (
    <div data-split-columns className="flex h-full min-w-0 flex-1">
      <SplitPane
        left={content.primary}
        right={content.split}
        storageKey={`${storageKey}.columns`}
        minRatio={0.3}
        maxRatio={0.7}
      />
    </div>
  );
  return (
    <SplitDropStage enabled={acceptsSplitDrop}>
      <SplitPane left={columns} right={content.secondary} storageKey={storageKey} />
    </SplitDropStage>
  );
}

/** The conversation area is the drop target for a sidebar row. Once a held
 *  row is lifted out of its list, a ghost of it follows the pointer and the
 *  column it would open into shows, filled so nothing under it competes. */
function SplitDropStage({ enabled, children }: { enabled: boolean; children: ReactNode }) {
  const state = useSplitDropState();
  const box = useRef<HTMLDivElement | null>(null);
  const ref = useCallback(
    (node: HTMLDivElement | null) => {
      box.current = node;
      registerSplitDropStage(enabled ? node : null);
    },
    [enabled],
  );
  const lifted = enabled && (state === "lifted" || state === "over");
  const target = lifted ? splitDropTarget() : null;
  const origin = lifted ? box.current?.getBoundingClientRect() : undefined;

  return (
    <div ref={ref} className="relative flex h-full min-w-0 flex-1">
      {children}
      {target && origin ? (
        <div
          aria-hidden
          data-split-drop={state}
          className="pointer-events-none absolute z-30 p-2"
          style={{
            left: target.left - origin.left,
            top: target.top - origin.top,
            width: target.width,
            height: target.height,
            background: PANE_BG,
          }}
        >
          <div
            className={cn(
              "flex h-full items-center justify-center rounded-(--floating-radius) border-2 border-dashed border-border-control transition-colors",
              // Utility state: neutral ink, never the theme's selection hue.
              state === "over"
                ? "bg-[color-mix(in_oklab,var(--text-primary)_9%,transparent)] text-text-primary"
                : "bg-[color-mix(in_oklab,var(--text-primary)_4%,transparent)] text-text-secondary",
            )}
          >
            <span className="flex items-center gap-1.5 text-[13px] font-medium">
              <SplitRightIcon className="size-4" />
              {t("conversationShell.openOnRight")}
            </span>
          </div>
        </div>
      ) : null}
      {lifted ? <DragGhost /> : null}
    </div>
  );
}

/** The held row lifted out of the sidebar: a copy of the row itself, same
 *  width and paint, in the same dragging state a reorder shows. Mounted in an
 *  `aside[data-sidebar]` so it keeps the sidebar's theme scope. */
function DragGhost(): JSX.Element | null {
  const ghost = useSplitDragGhost();
  const host = useRef<HTMLElement | null>(null);
  const row = ghost?.row ?? null;

  useLayoutEffect(() => {
    const node = host.current;
    if (!node || !row) return;
    node.replaceChildren(row);
    return () => {
      node.replaceChildren();
    };
  }, [row]);

  if (!ghost) return null;
  return createPortal(
    <aside
      ref={host}
      aria-hidden
      data-sidebar="drag-ghost"
      data-sidebar-sort-dragging="true"
      className="pointer-events-none fixed top-0 left-0 z-(--z-popover)"
      style={{
        transform: `translate3d(${ghost.left}px, ${ghost.top}px, 0)`,
      }}
    />,
    document.body,
  );
}
