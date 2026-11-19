import { useCallback, type ReactNode } from "react";
import { useCompactLayout } from "@/navigation/layout";
import { SplitPane } from "@/components/layout/split-pane";
import { registerSplitDropStage, useSplitDropState } from "@/components/layout/split-drop";
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
    <SplitPane
      left={content.primary}
      right={content.split}
      storageKey={`${storageKey}.columns`}
      minRatio={0.3}
      maxRatio={0.7}
    />
  );
  return (
    <SplitDropStage enabled={acceptsSplitDrop}>
      <SplitPane left={columns} right={content.secondary} storageKey={storageKey} />
    </SplitDropStage>
  );
}

/** The stage's box is the drop target for a sidebar row; while a row is dragged
 *  the right half shows where it will open. */
function SplitDropStage({ enabled, children }: { enabled: boolean; children: ReactNode }) {
  const state = useSplitDropState();
  const ref = useCallback(
    (node: HTMLDivElement | null) => registerSplitDropStage(enabled ? node : null),
    [enabled],
  );

  return (
    <div ref={ref} className="relative flex h-full min-w-0 flex-1">
      {children}
      {enabled && state !== "idle" ? (
        <div
          aria-hidden
          data-split-drop={state}
          className={cn(
            "pointer-events-none absolute inset-y-0 right-0 z-30 flex w-1/2 items-center justify-center border-2 border-dashed transition-colors",
            state === "over"
              ? "border-border-control bg-bg-hover text-text-primary"
              : "border-border-subtle text-text-muted",
          )}
        >
          <span className="flex items-center gap-1.5 text-[13px] font-medium">
            <SplitRightIcon className="size-4" />
            {t("conversationShell.openOnRight")}
          </span>
        </div>
      ) : null}
    </div>
  );
}
