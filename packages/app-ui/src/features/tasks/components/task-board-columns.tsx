import "./task-board-columns.css";
import { TaskRowsSkeleton } from "./task-body-skeleton";
import { taskChannelLabel } from "../model/task-place.js";
import { useLayoutEffect, useRef, useState, type RefObject } from "react";
import { ChevronDownIcon, ChevronRightIcon } from "@/icons";
import type { TaskStatus, TaskSummaryDto } from "@silverhand/shared";
import { cn } from "@/lib/cn";
import {
  boardColumnTotal,
  boardCountLabel,
  type BoardAuthority,
  type BoardCounts,
} from "@/features/tasks/model/board-authority";
import {
  boardColumnCanGrow,
  boardVisibleItems,
  type BoardColumns,
} from "@/features/tasks/hooks/use-board";
import { useColumnEndSentinel } from "@/features/tasks/hooks/use-column-window";
import type { ColumnWindow } from "@/features/tasks/model/column-window";
import { absoluteTime, RelativeTime } from "@/features/messages/components/relative-time";
import { ChannelIcon } from "@/components/channel-icon";
import { EventPerson } from "@/features/tasks/components/task-event-person";
import {
  TaskCardContextMenu,
  TaskStatusButton,
} from "@/features/tasks/components/task-status-badge";
import { t } from "@/lib/i18n";

/**
 * The task board BODY: one group per status, stacked, each task a full-width
 * row (shiki #3131). It is the one authority for group order, row fields,
 * collapse and the paging sentinel, shared by both board surfaces (the global
 * `/tasks` section and a conversation's Tasks tab, #task:2121), which differ
 * only in their shell (header, filter bar, create dialog).
 *
 * Rows rather than kanban columns: a column narrows every title into many
 * lines, while a row gives the title the whole width and lines the number,
 * assignee and time up in columns a reader can scan down. Status changes on the
 * row's status button or its context menu; there is no drag.
 *
 * Every theme draws the board the same way from its base tokens: no group
 * frames beyond a shaded header bar, and the status hue only on that header's
 * cube and each row's status button cube (the same cube as the message task card
 * and inline task reference). Everything else in a row stays neutral.
 */

interface ColumnSpec {
  status: TaskStatus;
  label: string;
}

const columnSpecs = (): ReadonlyArray<ColumnSpec> => [
  { status: "todo", label: t("tasks.todo") },
  { status: "in_progress", label: t("tasks.inProgress") },
  { status: "in_review", label: t("tasks.inReview") },
  { status: "done", label: t("tasks.done") },
  { status: "closed", label: t("tasks.closed") },
];

export interface TaskBoardColumnsProps {
  /** The per-status paged reads (`useBoardColumns`). */
  columns: BoardColumns;
  /** The per-status authoritative totals (`useBoardCounts`). */
  counts: BoardAuthority<BoardCounts>;
  /** The render bound this surface owns: the section and tab pass their live
   *  `columnWindow`. */
  window: ColumnWindow;
  isCollapsed: (status: TaskStatus) => boolean;
  onToggleCollapse: (status: TaskStatus) => void;
  onGrow: (status: TaskStatus) => void;
  onOpenTask: (taskId: string) => void;
  onStatusChange: (taskId: string, next: TaskStatus) => void;
}

/** The task board region (`[data-tasks-board]`): status groups, top to bottom. */
export function TaskBoardColumns({
  columns,
  counts,
  window,
  isCollapsed,
  onToggleCollapse,
  onGrow,
  onOpenTask,
  onStatusChange,
}: TaskBoardColumnsProps): JSX.Element {
  const boardRef = useRef<HTMLDivElement>(null);

  return (
    <div
      ref={boardRef}
      data-tasks-board
      data-scroll-root
      className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto p-4 scrollbar-hide"
    >
      {columnSpecs().map((col) => (
        <StatusGroup
          key={col.status}
          scrollRootRef={boardRef}
          title={col.label}
          status={col.status}
          items={boardVisibleItems(columns[col.status], window, col.status)}
          total={boardColumnTotal(counts, col.status)}
          hasMore={boardColumnCanGrow(columns[col.status], window, col.status)}
          isLoading={columns[col.status].isLoading}
          isError={columns[col.status].isError}
          collapsed={isCollapsed(col.status)}
          empty={boardColumnTotal(counts, col.status) === 0}
          onToggleCollapse={() => onToggleCollapse(col.status)}
          onReachEnd={() => onGrow(col.status)}
          onOpenTask={onOpenTask}
          onStatusChange={onStatusChange}
        />
      ))}
    </div>
  );
}

// ── Group ──────────────────────────────────────────────────────────

function StatusGroup({
  scrollRootRef,
  title,
  status,
  items,
  total,
  hasMore,
  isLoading,
  isError,
  collapsed: collapsedByViewer,
  empty,
  onToggleCollapse,
  onReachEnd,
  onOpenTask,
  onStatusChange,
}: {
  /** The board scroller: groups no longer scroll on their own, so the paging
   *  sentinel is observed against the board. */
  scrollRootRef: RefObject<HTMLDivElement | null>;
  title: string;
  status: TaskStatus;
  /** The rows this surface may render: the loaded prefix under its own window
   *  bound, never the whole group. */
  items: TaskSummaryDto[];
  /** The group's authoritative total under the current filters (counts face),
   *  which is what the header shows: bounding the body must not shrink the
   *  number. `null` while the counts face has not answered. */
  total: number | null;
  /** Anything left past the rendered rows, loaded or still on the server. */
  hasMore: boolean;
  isLoading: boolean;
  isError: boolean;
  collapsed: boolean;
  /** The authoritative total is zero: the group starts collapsed, but still
   *  opens on request to show its empty state. */
  empty: boolean;
  onToggleCollapse: () => void;
  onReachEnd: () => void;
  onOpenTask: (taskId: string) => void;
  onStatusChange: (taskId: string, next: TaskStatus) => void;
}): JSX.Element {
  // A collapsed group renders only its header and reads nothing (the queries
  // are gated on the same store above the board, #task:1169), so it also arms
  // no observer.
  // An empty group starts collapsed whatever the stored bit says, but stays
  // openable: a reader who opens it sees the empty state instead of a dead
  // header. Once a task arrives the stored bit takes over again.
  const [openWhileEmpty, setOpenWhileEmpty] = useState(false);
  const collapsed = empty ? !openWhileEmpty : collapsedByViewer;
  const sectionRef = useRef<HTMLElement>(null);
  const headerRef = useRef<HTMLButtonElement>(null);
  const stuckOffsetRef = useRef<number | null>(null);
  const toggle = () => {
    // A header pinned over its own rows (compact sticky) has scrolled past the
    // group's top; collapsing would leave the reader far below the group.
    // Remember where the header sits so the group can be put back under it.
    const section = sectionRef.current;
    const header = headerRef.current;
    stuckOffsetRef.current =
      section && header && header.getBoundingClientRect().top > section.getBoundingClientRect().top
        ? header.getBoundingClientRect().top
        : null;
    if (empty) {
      setOpenWhileEmpty((open) => !open);
    } else {
      onToggleCollapse();
    }
  };
  const Chevron = collapsed ? ChevronRightIcon : ChevronDownIcon;
  const canGrow = !collapsed && hasMore;
  const sentinelRef = useRef<HTMLLIElement>(null);

  useLayoutEffect(() => {
    const offset = stuckOffsetRef.current;
    const section = sectionRef.current;
    const board = scrollRootRef.current;
    stuckOffsetRef.current = null;
    if (offset === null || !section || !board) {
      return;
    }
    board.scrollTop += section.getBoundingClientRect().top - offset;
  }, [collapsed, scrollRootRef]);

  useColumnEndSentinel({
    rootRef: scrollRootRef,
    sentinelRef,
    hasMore: canGrow,
    renderedCount: items.length,
    onReachEnd,
  });

  return (
    <section
      ref={sectionRef}
      data-tasks-column
      data-status={status}
      className="flex flex-none flex-col"
    >
      <button
        ref={headerRef}
        type="button"
        data-tasks-column-header
        onClick={toggle}
        aria-expanded={!collapsed}
        aria-label={t(collapsed ? "tasks.expandColumn" : "tasks.collapseColumn", { title })}
        className="group/header flex h-8 w-full shrink-0 cursor-pointer items-center gap-2 rounded-(--control-radius) bg-bg-elevated px-3 text-left transition-colors hover:bg-bg-hover"
      >
        <span data-task-cube data-task-ref-tone={status} aria-hidden />
        <span
          className={cn(
            "text-(length:--text-meta) font-semibold",
            // Terminal and empty groups recede so the live ones lead.
            empty || status === "done" || status === "closed"
              ? "text-text-muted"
              : "text-text-primary",
          )}
        >
          {title}
        </span>
        <span className="text-(length:--text-meta) tabular-nums text-text-muted">
          {boardCountLabel(total)}
        </span>
        <Chevron className="ml-auto size-3.5 shrink-0 text-text-muted" aria-hidden />
      </button>
      {collapsed ? null : (
        <div data-tasks-column-body className="pt-0.5">
          {isLoading ? (
            <div role="status" aria-label={t("tasks.loadingTasks")}>
              <TaskRowsSkeleton />
            </div>
          ) : isError ? (
            <p className="px-3 py-3 text-center text-(length:--text-meta) text-text-muted">
              {t("tasks.couldntLoadTasks")}
            </p>
          ) : items.length === 0 ? (
            <p className="flex h-9 items-center px-3 text-(length:--text-meta) text-text-muted">
              {t("tasks.emptyColumn", { title: title.toLowerCase() })}
            </p>
          ) : (
            <ul>
              {items.map((task) => (
                <li key={task.id}>
                  <TaskRow
                    task={task}
                    onOpen={() => onOpenTask(task.id)}
                    onStatusChange={(next) => onStatusChange(task.id, next)}
                  />
                </li>
              ))}
              {/* Tail sentinel: grows the window as it comes into view, which is
                  also what asks for the next page. No visual of its own; a
                  spinner pinned to the tail would flicker on every local growth. */}
              {canGrow ? <li ref={sentinelRef} aria-hidden className="h-px" /> : null}
            </ul>
          )}
        </div>
      )}
    </section>
  );
}

// ── Row ────────────────────────────────────────────────────────────

function TaskRow({
  task,
  onOpen,
  onStatusChange,
}: {
  task: TaskSummaryDto;
  onOpen: () => void;
  onStatusChange: (next: TaskStatus) => void;
}): JSX.Element {
  const channelLabel = taskChannelLabel(task);

  return (
    <TaskCardContextMenu status={task.status} onStatusChange={onStatusChange}>
      <div
        data-task-row
        data-task-number={task.task_number}
        data-task-id={task.id}
        className="relative flex h-9 min-w-0 items-center gap-4 rounded-(--control-radius) px-3 text-(length:--text-meta) text-text-muted transition-colors hover:bg-bg-hover"
      >
        {/* Stretched open target under the content, so a click anywhere opens
            the task while the status badge above it stays its own control. */}
        <button
          type="button"
          onClick={onOpen}
          aria-label={t("tasks.openTaskDetail")}
          className="absolute inset-0 z-0 focus-outline"
        />
        <span data-task-row-number className="pointer-events-none relative">
          #{task.task_number}
        </span>
        {/* The title is the row's one loud element and its only flexible
            column: it truncates, so the row height never changes. */}
        <span className="pointer-events-none relative min-w-0 flex-1 truncate text-[13px] font-medium text-text-primary">
          {task.title}
        </span>
        <span data-task-row-assignee className="pointer-events-none relative">
          {task.assignee ? (
            <EventPerson
              person={
                task.assignee.type === "agent"
                  ? { kind: "agent", id: task.assignee.id, name: task.assignee.name }
                  : { kind: "user", id: task.assignee.id, name: task.assignee.name }
              }
            />
          ) : (
            <span data-task-row-unassigned className="italic">
              {t("tasks.unassigned")}
            </span>
          )}
        </span>
        <span
          data-task-row-channel
          className="pointer-events-none relative"
          title={channelLabel ?? undefined}
        >
          {channelLabel !== null ? (
            <>
              <ChannelIcon variant="utility" density="sm" className="text-text-muted" />
              <span className="min-w-0 truncate">{channelLabel}</span>
            </>
          ) : null}
        </span>
        <span
          data-task-row-time
          className="pointer-events-none relative"
          title={absoluteTime(task.created_at)}
        >
          <RelativeTime epochMs={task.created_at} />
        </span>
        <span data-task-row-status className="relative z-(--z-raised) flex">
          <TaskStatusButton status={task.status} onChange={onStatusChange} className="w-full" />
        </span>
      </div>
    </TaskCardContextMenu>
  );
}
