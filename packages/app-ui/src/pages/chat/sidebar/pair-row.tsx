import { forwardRef, type HTMLAttributes, type MouseEvent, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { CloseIcon } from "@/icons";
import type { RowBadge } from "@/features/attention/attention-mode";
import {
  NESTED_GLYPH_INSET,
  UnreadBadge,
  sidebarRowStateClass,
} from "@/components/layout/sidebar-shared";
import { t } from "@/lib/i18n";
import { AppContextMenu } from "@/components/menus/app-context-menu";
import type { SplitPair } from "@/features/conversation-shell/split-pairs";
import { useSidebarSplit } from "./sidebar-split";
import type { RowFace } from "./sidebar-faces";

/** One half of a pair row: the conversation's own glyph and title. */
export interface PairHalf {
  glyph: ReactNode;
  title: string;
  badge: RowBadge | null;
  onClose: () => void;
}

/**
 * A kept pair as a sidebar row, wired to the split view: selected while the
 * pair is on screen, reopens it on click, and each half's close removes that
 * half. `menu` is the left conversation's own row menu.
 */
export function SidebarPairRow({
  pair,
  workspaceId,
  left,
  right,
  menu,
  draggable,
}: {
  pair: SplitPair;
  /** The left conversation's workspace; null for a direct pair. */
  workspaceId: string | null;
  left: RowFace;
  right: RowFace;
  menu: ReactNode;
  draggable?: boolean;
}): JSX.Element | null {
  const split = useSidebarSplit();
  if (!split) return null;
  const row = (
    <PairRow
      selected={split.active?.left === pair.left && split.active.right === pair.right}
      draggable={draggable}
      left={{ ...left, onClose: () => split.closeHalf(pair, "left") }}
      right={{ ...right, onClose: () => split.closeHalf(pair, "right") }}
      onSelect={() => split.openPair(pair, workspaceId)}
    />
  );
  return <AppContextMenu trigger={row}>{menu}</AppContextMenu>;
}

/**
 * Two conversations open side by side, drawn as ONE sidebar row (Chrome's
 * split-tab face): the row is a single selection, clicking anywhere reopens
 * the pair, and neither half highlights on its own. The connector between the
 * halves marks them as joined; on hover each half trades its unread count for
 * a close button that removes that half from the pair.
 */
export const PairRow = forwardRef<
  HTMLDivElement,
  Omit<HTMLAttributes<HTMLDivElement>, "children" | "onSelect"> & {
    selected: boolean;
    draggable?: boolean;
    left: PairHalf;
    right: PairHalf;
    onSelect: (event: MouseEvent<HTMLButtonElement>) => void;
  }
>(function PairRow({ selected, draggable, left, right, onSelect, className, ...rest }, ref) {
  return (
    <div
      ref={ref}
      {...rest}
      data-row-selected={selected || undefined}
      data-sidebar-pair
      className={cn(
        "sidebar-row-border group/pair relative flex h-8 w-full items-stretch border pr-1",
        NESTED_GLYPH_INSET,
        sidebarRowStateClass(selected),
        draggable && "cursor-grab select-none active:cursor-grabbing",
        className,
      )}
    >
      {/* One button under both halves keeps the row a single target; the close
          buttons sit above it. */}
      <button
        type="button"
        onClick={onSelect}
        aria-label={`${left.title} | ${right.title}`}
        className="absolute inset-0"
      />
      <Half half={left} selected={selected} />
      <PairConnector />
      <Half half={right} selected={selected} />
    </div>
  );
});

function Half({ half, selected }: { half: PairHalf; selected: boolean }): JSX.Element {
  return (
    <div className="pointer-events-none flex min-w-0 flex-1 basis-0 items-center gap-1.5 pr-1 text-[13px] leading-4">
      <div className="flex size-5.5 shrink-0 items-center justify-center">{half.glyph}</div>
      <span className={cn("min-w-0 truncate", selected && "font-medium")}>{half.title}</span>
      <span className="relative ml-auto flex h-4 shrink-0 items-center">
        {half.badge ? (
          <UnreadBadge
            count={half.badge.count}
            tier={half.badge.tier}
            className="group-hover/pair:invisible"
          />
        ) : null}
        <button
          type="button"
          onClick={(event) => {
            event.stopPropagation();
            half.onClose();
          }}
          aria-label={t("conversationShell.closeHalf", { name: half.title })}
          title={t("conversationShell.closeHalf", { name: half.title })}
          className={cn(
            "pointer-events-auto absolute right-0 top-1/2 flex size-4 -translate-y-1/2 items-center justify-center rounded-(--control-radius)",
            "invisible text-text-secondary hover:bg-bg-hover hover:text-text-primary group-hover/pair:visible focus-visible:visible",
          )}
        >
          <CloseIcon className="size-3" />
        </button>
      </span>
    </div>
  );
}

/** A 4×16 rounded bar joining the halves: quiet ink at rest, full ink on the
 *  selected row (the row's own text colour). */
function PairConnector(): JSX.Element {
  return (
    <svg
      aria-hidden
      viewBox="0 0 4 16"
      className="pointer-events-none my-auto mr-1.5 h-4 w-1 shrink-0 text-border-control opacity-35 group-data-[row-selected]/pair:opacity-100"
    >
      <rect width="4" height="16" rx="2" fill="currentColor" />
    </svg>
  );
}
