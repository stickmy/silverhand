import { TaskCardsSkeleton } from "./task-body-skeleton";
import { taskChannelLabel } from "../model/task-place.js";
import { useRef, useState } from "react";
import { ChevronDownIcon, ChevronRightIcon, ClockIcon, type IconComponent } from "@/icons";
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
import { isTaskDrag, readTaskDrag, writeTaskDrag } from "@/features/tasks/model/task-dnd";
import { absoluteTime, RelativeTime } from "@/features/messages/components/relative-time";
import { ChannelIcon } from "@/components/channel-icon";
import {
  TaskStatusBadge,
  TaskCardContextMenu,
  TASK_STATUS_ICON,
} from "@/features/tasks/components/task-status-badge";
import { TaskDropPlaceholder } from "@/features/tasks/components/task-drop-placeholder";
import { t } from "@/lib/i18n";

/**
 * The task board's five-column kanban BODY — the one authority for column
 * layout, card layout, drag-to-status, collapse and paging sentinel — shared by
 * every board surface (#task:2121). Two surfaces mount it: the global `/tasks`
 * section and the per-conversation inline Tasks tab. They differ only in their
 * SHELL — header, filter bar, create dialog — and in the two fill tokens below;
 * the body itself must read identically everywhere, so column status order, card
 * fields, drop behaviour and collapse can never drift between them.
 *
 * The only surface knob is the fill VOCABULARY ({@link columnClassName} /
 * {@link cardClassName}): both the section and the inline tab put columns on
 * `bg-bg-primary` and cards on `bg-bg-base`. Everything else is fixed.
 */

interface ColumnSpec {
  status: TaskStatus;
  label: string;
  icon: IconComponent;
  /** Icon color — the SAME `--task-status-<status>-solid` token the card chip
   *  reads (TaskStatusBadge via chrome.css `[data-task-status]`), so a column
   *  header and its cards' chips can never disagree (shiki review #task:912). */
  iconClass: string;
}

const columnSpecs = (): ReadonlyArray<ColumnSpec> => [
  {
    status: "todo",
    label: t("tasks.todo"),
    icon: TASK_STATUS_ICON.todo,
    iconClass: "text-[color:var(--task-status-todo-solid)]",
  },
  {
    status: "in_progress",
    label: t("tasks.inProgress"),
    icon: TASK_STATUS_ICON.in_progress,
    iconClass: "text-[color:var(--task-status-in-progress-solid)]",
  },
  {
    status: "in_review",
    label: t("tasks.inReview"),
    icon: TASK_STATUS_ICON.in_review,
    iconClass: "text-[color:var(--task-status-in-review-solid)]",
  },
  {
    status: "done",
    label: t("tasks.done"),
    icon: TASK_STATUS_ICON.done,
    iconClass: "text-[color:var(--task-status-done-solid)]",
  },
  {
    status: "closed",
    label: t("tasks.closed"),
    icon: TASK_STATUS_ICON.closed,
    iconClass: "text-[color:var(--task-status-closed-solid)]",
  },
];

export interface TaskBoardColumnsProps {
  /** The per-status paged reads (`useBoardColumns`). */
  columns: BoardColumns;
  /** The per-status authoritative totals (`useBoardCounts`). */
  counts: BoardAuthority<BoardCounts>;
  /** The render bound this surface owns — the section/tab pass their live
   *  `columnWindow`. */
  window: ColumnWindow;
  isCollapsed: (status: TaskStatus) => boolean;
  onToggleCollapse: (status: TaskStatus) => void;
  onGrow: (status: TaskStatus) => void;
  onOpenTask: (taskId: string) => void;
  onStatusChange: (taskId: string, next: TaskStatus) => void;
  /** Column fill token, e.g. `bg-bg-primary`. */
  columnClassName: string;
  /** Card fill token, e.g. `bg-bg-base`. */
  cardClassName: string;
}

/** The five-column kanban board region (`[data-tasks-board]`). */
export function TaskBoardColumns({
  columns,
  counts,
  window,
  isCollapsed,
  onToggleCollapse,
  onGrow,
  onOpenTask,
  onStatusChange,
  columnClassName,
  cardClassName,
}: TaskBoardColumnsProps): JSX.Element {
  return (
    <div
      data-tasks-board
      data-scroll-root
      // Columns share the width when it's ample (`flex-1`) but never squeeze
      // below a readable floor (`min-w` on each column): once the five floors
      // exceed the container the board scrolls horizontally instead of crushing
      // the cards (shiki — a squeezed column loses visual efficiency). The
      // scrollbar is hidden (`.scrollbar-hide`); the vertical axis stays owned by
      // each column's own body, so the board itself never scrolls up/down.
      className="flex min-h-0 flex-1 items-start gap-3 overflow-x-auto overflow-y-hidden p-4 scrollbar-hide"
    >
      {columnSpecs().map((col) => (
        <KanbanColumn
          key={col.status}
          title={col.label}
          status={col.status}
          icon={col.icon}
          iconClass={col.iconClass}
          items={boardVisibleItems(columns[col.status], window, col.status)}
          total={boardColumnTotal(counts, col.status)}
          hasMore={boardColumnCanGrow(columns[col.status], window, col.status)}
          isLoading={columns[col.status].isLoading}
          isError={columns[col.status].isError}
          collapsed={isCollapsed(col.status)}
          onToggleCollapse={() => onToggleCollapse(col.status)}
          onReachEnd={() => onGrow(col.status)}
          onCardClick={(t) => onOpenTask(t.id)}
          onStatusChange={onStatusChange}
          columnClassName={columnClassName}
          cardClassName={cardClassName}
        />
      ))}
    </div>
  );
}

// ── Column ─────────────────────────────────────────────────────────

function KanbanColumn({
  title,
  status,
  icon: Icon,
  iconClass,
  items,
  total,
  hasMore,
  isLoading,
  isError,
  collapsed,
  onToggleCollapse,
  onReachEnd,
  onCardClick,
  onStatusChange,
  columnClassName,
  cardClassName,
}: {
  title: string;
  status: TaskStatus;
  icon: IconComponent;
  iconClass: string;
  /** The rows this surface may render — the loaded prefix under its own window
   *  bound, never the whole column. */
  items: TaskSummaryDto[];
  /** The column's authoritative total under the current filters (counts face),
   *  which is what the header shows: bounding the body must not shrink the
   *  number. `null` while the counts face has not answered — independent of this
   *  column's own body read, so a loaded body can show its rows with no number,
   *  and a known total can stand over a failed body. */
  total: number | null;
  /** Anything left past the rendered rows — loaded or still on the server. */
  hasMore: boolean;
  isLoading: boolean;
  isError: boolean;
  collapsed: boolean;
  onToggleCollapse: () => void;
  onReachEnd: () => void;
  onCardClick: (t: TaskSummaryDto) => void;
  onStatusChange: (taskId: string, next: TaskStatus) => void;
  columnClassName: string;
  cardClassName: string;
}): JSX.Element {
  // Drag-over affordance (#task:779). Any task drag entering the column arms it
  // (accent edge + soft wash, painted in poster-components.css off
  // `[data-drop-target]`) and shows a "Drop here" slot. The source column is
  // not special-cased — its own drop is a no-op (below), so it still highlights.
  const [dragOver, setDragOver] = useState(false);
  // Per-column collapse (#task:1169). Collapsed → render only the header bar and
  // read nothing (the queries are gated on the SAME store above the column), so
  // all five stay visible but a heavy terminal column (Closed) costs nothing.
  const Chevron = collapsed ? ChevronRightIcon : ChevronDownIcon;
  // A collapsed column renders no body, so it also arms no observer.
  const canGrow = !collapsed && hasMore;
  const bodyRef = useRef<HTMLDivElement>(null);
  const sentinelRef = useRef<HTMLLIElement>(null);
  useColumnEndSentinel({
    rootRef: bodyRef,
    sentinelRef,
    hasMore: canGrow,
    renderedCount: items.length,
    onReachEnd,
  });
  return (
    <section
      data-tasks-column
      data-status={status}
      data-drop-target={dragOver || undefined}
      onDragOver={(e) => {
        if (!isTaskDrag(e.dataTransfer)) return;
        e.preventDefault();
        e.dataTransfer.dropEffect = "move";
        if (!dragOver) setDragOver(true);
      }}
      onDragLeave={(e) => {
        // Ignore leave events bubbling from children — only clear when the
        // pointer actually exits the column box.
        if (e.currentTarget.contains(e.relatedTarget as Node | null)) return;
        setDragOver(false);
      }}
      onDrop={(e) => {
        const payload = readTaskDrag(e.dataTransfer);
        setDragOver(false);
        if (!payload) return;
        e.preventDefault();
        // Drop onto the same status = no-op (Cindy review: no meaningless
        // mutation).
        if (payload.from === status) return;
        onStatusChange(payload.taskId, status);
      }}
      className={cn(
        // Each column hugs its own content height (cross-axis auto via
        // `items-start` on the row) but caps at the parent height with
        // `max-h-full`; the body scrolls inside so the page never scrolls. All
        // five keep an EQUAL flex width whether collapsed or not (#task:1169
        // follow-up, shiki): collapsing only drops the body, never resizes a
        // column, so the live columns don't jitter when Closed folds/unfolds.
        // `min-w-[12rem]` is the readable floor — below it the board scrolls
        // (parent `overflow-x-auto`) rather than letting a card squeeze.
        "flex min-h-0 max-h-full min-w-48 flex-1 flex-col overflow-hidden rounded-(--card-radius) border border-border-subtle transition-colors",
        columnClassName,
        // Done + Closed are terminal — dim slightly so the live columns read as
        // the focus.
        (status === "done" || status === "closed") && "opacity-80",
      )}
    >
      <button
        type="button"
        data-tasks-column-header
        onClick={onToggleCollapse}
        aria-expanded={!collapsed}
        aria-label={t(collapsed ? "tasks.expandColumn" : "tasks.collapseColumn", { title })}
        className="flex w-full shrink-0 cursor-pointer items-center gap-2 border-b border-border-subtle px-3 py-2 text-left transition-colors hover:bg-bg-hover"
      >
        <Chevron className="size-3.5 shrink-0 text-text-muted" aria-hidden />
        <Icon data-tasks-column-icon className={cn("size-3.5 shrink-0", iconClass)} aria-hidden />
        <span className="text-(length:--text-meta) font-medium uppercase tracking-wider text-text-secondary">
          {title}
        </span>
        <span className="text-(length:--text-badge) tabular-nums text-text-muted">
          {boardCountLabel(total)}
        </span>
      </button>
      {collapsed ? null : (
        <div
          ref={bodyRef}
          data-tasks-column-body
          className="min-h-0 flex-auto overflow-y-auto p-2 scrollbar-hide"
        >
          {isLoading ? (
            <div role="status" aria-label={t("tasks.loadingTasks")}>
              <TaskCardsSkeleton />
            </div>
          ) : isError ? (
            <p className="px-2 py-3 text-center text-(length:--text-meta) text-text-muted">
              {t("tasks.couldntLoadTasks")}
            </p>
          ) : items.length === 0 && !dragOver ? (
            <p className="px-2 py-3 text-center text-(length:--text-meta) italic text-text-muted">
              {t("tasks.emptyColumn", { title: title.toLowerCase() })}
            </p>
          ) : (
            <ul className="space-y-1.5">
              {dragOver ? <TaskDropPlaceholder /> : null}
              {items.map((t) => (
                <li key={t.id}>
                  <TaskCard
                    task={t}
                    onClick={() => onCardClick(t)}
                    onStatusChange={(next) => onStatusChange(t.id, next)}
                    cardClassName={cardClassName}
                  />
                </li>
              ))}
              {/* Tail sentinel — grows the window as it comes into view, which is
                  also what asks for the next page (the window drives fetching).
                  It has no visual of its own: the rows either arrive on the next
                  commit (already cached) or with the page, and a spinner pinned
                  to the tail would flicker on every local growth. */}
              {canGrow ? <li ref={sentinelRef} aria-hidden className="h-px" /> : null}
            </ul>
          )}
        </div>
      )}
    </section>
  );
}

// ── Card ───────────────────────────────────────────────────────────

function TaskCard({
  task,
  onClick,
  onStatusChange,
  cardClassName,
}: {
  task: TaskSummaryDto;
  onClick: () => void;
  onStatusChange: (next: TaskStatus) => void;
  cardClassName: string;
}): JSX.Element {
  const [dragging, setDragging] = useState(false);
  const channelLabel = taskChannelLabel(task);
  return (
    <TaskCardContextMenu status={task.status} onStatusChange={onStatusChange}>
      <div
        data-task-number={task.task_number}
        data-task-id={task.id}
        draggable
        onDragStart={(e) => {
          writeTaskDrag(e.dataTransfer, { taskId: task.id, from: task.status });
          setDragging(true);
        }}
        onDragEnd={() => setDragging(false)}
        className={cn(
          "group relative min-w-0 cursor-grab rounded-(--card-radius) border border-border-subtle transition-all hover:border-(--border-row-hover) hover:bg-bg-hover active:cursor-grabbing",
          cardClassName,
          dragging && "opacity-50",
        )}
      >
        {/* Stretched jump overlay — covers the whole card so a click anywhere
            (except the status badge layered above) opens the detail modal. Lets
            the badge stay an independently clickable control without a
            button-in-button. `draggable=false` so a press-drag starting here
            initiates the card's drag (the draggable ancestor), not the button. */}
        <button
          type="button"
          draggable={false}
          onClick={onClick}
          aria-label={t("tasks.openTaskDetail")}
          className="absolute inset-0 z-0 rounded-(--card-radius) focus-outline"
        />
        {/* Content layer. `pointer-events-none` so text clicks fall through to
            the jump overlay; only the status badge re-enables pointer events.
            `min-w-0` propagates the row's narrow-width shrink to the rows below
            (#task:918). */}
        <div className="pointer-events-none relative z-(--z-raised) min-w-0 p-2">
          {/* Row 1: #number + interactive status badge (reads "Closed" for a
              closed task — no separate chip). 2-row layout (#task:917): #nnn +
              status own a line, content below, so a long title isn't compressed.
              `flex-wrap` (#task:918): in a narrow column the chip drops beneath
              `#nnn` instead of overflowing the card edge. */}
          <div className="flex flex-wrap items-center gap-x-1.5 gap-y-0.5 pb-0.5 leading-none">
            <span
              data-tasks-num
              className="shrink-0 font-mono text-(length:--text-badge) text-text-muted"
            >
              #{task.task_number}
            </span>
            <TaskStatusBadge
              status={task.status}
              onChange={onStatusChange}
              className="ml-auto pointer-events-auto"
            />
          </div>
          {/* Row 2: title — `[overflow-wrap:anywhere]` so a long URL / hash with
              no spaces still breaks inside a narrow card (#task:918). */}
          <span className="block min-w-0 text-xs text-text-primary wrap-anywhere">
            {task.title}
          </span>
          {/* Row 3: assignee · channel · created — inline meta, single line.
              Separators are identity icons, not `·` glyphs (shiki 2026-07-21):
              the channel gets a `#`, the timestamp a clock. NO `flex-wrap` here
              (unlike Row 1): the conversation span is the only shrink target
              (`min-w-0 truncate`) so a long channel title ellipsizes while
              assignee + time stay pinned, and the card height never grows
              (shiki 2026-07-23). 稳定 > 自适应. */}
          <div className="mt-1 flex items-center gap-x-2 text-(length:--text-badge) text-text-muted">
            <span className="shrink-0 text-text-secondary">
              {task.assignee ? (
                task.assignee.name
              ) : (
                <span className="italic">{t("tasks.unassigned")}</span>
              )}
            </span>
            {channelLabel !== null ? (
              <span
                className="inline-flex min-w-0 items-center gap-0.5 text-text-secondary"
                title={channelLabel}
              >
                <ChannelIcon variant="utility" density="sm" className="text-text-muted" />
                <span className="min-w-0 truncate">{channelLabel}</span>
              </span>
            ) : null}
            <span
              className="ml-auto inline-flex shrink-0 items-center gap-0.5"
              title={absoluteTime(task.created_at)}
            >
              <ClockIcon className="size-3 shrink-0" />
              <RelativeTime epochMs={task.created_at} />
            </span>
          </div>
        </div>
      </div>
    </TaskCardContextMenu>
  );
}
