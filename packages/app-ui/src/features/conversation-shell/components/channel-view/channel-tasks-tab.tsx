import { useCallback, useMemo, useRef, useState } from "react";
import type { TaskFilters } from "@/features/tasks/model/task-filters";
import { cn } from "@/lib/cn";
import { useUpdateTaskStatus } from "@/features/tasks/hooks/hooks";
import { useKanbanColumnCollapse } from "@/features/tasks/hooks/use-kanban-column-collapse";
import { useTaskFilterOptions } from "@/features/tasks/hooks/use-task-filter-options";
import { useBoardColumns, useBoardCounts, useBoardFacets } from "@/features/tasks/hooks/use-board";
import { useColumnWindow } from "@/features/tasks/hooks/use-column-window";
import {
  taskContainerBaseline,
  taskContainerKey,
  type TaskBoardContainer,
} from "@/features/tasks/model/task-container";
import { TaskBoardColumns } from "@/features/tasks/components/task-board-columns";
import { TaskToolbar } from "@/features/tasks/components/task-toolbar";
import {
  CreateTaskDialog,
  useCreateTaskSubmit,
  type CreateTaskTarget,
} from "@/features/tasks/components/create-task-dialog";
import { t } from "@/lib/i18n";

/**
 * The conversation's Tasks tab (#task:2121) — the five-column status kanban
 * rendered INLINE in the main pane, scoped to the conversation it opens in.
 *
 * The board BODY is the shared {@link TaskBoardColumns} — the same columns,
 * cards, drag/drop, collapse and paging the global section renders, so the two
 * surfaces cannot drift. This tab only supplies the SHELL: the filter bar
 * pre-focused on this conversation's CONTAINER
 * (`taskContainerBaseline` — a workspace channel anchors workspace + channel, a
 * direct line anchors its own conversation and mints no workspace), the create
 * entrance, and the main-pane fill vocabulary (columns `bg-bg-primary`, cards
 * `bg-bg-base`). The bar hides the board scope and channel axes here (the tab is
 * this conversation's tasks); Assignee / From narrow, and Clear restores the
 * baseline, not global All.
 *
 * Render-window bound is surface-local: this board owns its own
 * (`useColumnWindow`) so its scrolling never inflates the global section's
 * mount cost.
 */
export interface ChannelTasksTabProps {
  container: TaskBoardContainer;
  /** Opens a task's detail in the side pane (App writes `?task=<id>`) — the same
   *  host an inline `#task` chip opens, so a card and a chip land identically. */
  onOpenTask: (taskId: string) => void;
}

export function ChannelTasksTab({ container, onOpenTask }: ChannelTasksTabProps): JSX.Element {
  const baselineFilters = useMemo<TaskFilters>(
    () => taskContainerBaseline(container),
    // Keyed on the container's VALUE, not the prop's identity — a carrier
    // refetch returns equal facts as fresh objects (see taskContainer.ts).
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [taskContainerKey(container)],
  );
  const [filters, setFilters] = useState<TaskFilters>(baselineFilters);
  // Re-seed the filters when the container changes (switching conversations
  // remounts this tab, but keep the reset explicit for the in-place case).
  const prevKey = useRef(taskContainerKey(container));
  const currentKey = taskContainerKey(container);
  if (prevKey.current !== currentKey) {
    prevKey.current = currentKey;
    setFilters(baselineFilters);
  }

  const { columnWindow, grow: growColumn, reset: resetColumnWindow } = useColumnWindow();
  const applyFilters = useCallback(
    (next: TaskFilters) => {
      setFilters(next);
      resetColumnWindow();
    },
    [resetColumnWindow],
  );

  const { workspaceOptions, channelOptions } = useTaskFilterOptions();
  const { isCollapsed, toggle: toggleCollapse } = useKanbanColumnCollapse();

  const columns = useBoardColumns({
    filters,
    enabled: true,
    window: columnWindow,
    isCollapsed,
  });
  const counts = useBoardCounts(filters);
  const people = useBoardFacets(filters);
  const updateStatus = useUpdateTaskStatus();

  const [createOpen, setCreateOpen] = useState(false);
  const submitCreate = useCreateTaskSubmit();
  // A direct line creates ONTO the pair conversation (no channel to pick,
  // contract §5); a workspace board offers the picker seeded from the current
  // channel filter when it resolves to a real member-channel option.
  const createTarget = useMemo<CreateTaskTarget>(() => {
    if (container.kind === "direct_pair") {
      return { kind: "locked", conversationId: container.conversationId };
    }
    const defaultChannelId = channelOptions.some((o) => o.id === filters.channelId)
      ? filters.channelId
      : null;
    return { kind: "pick", channelOptions, defaultChannelId };
  }, [container, channelOptions, filters.channelId]);

  return (
    <div className={cn("flex h-full min-h-0 flex-col bg-(--pane-bg,var(--bg-base))")}>
      <TaskToolbar
        onCreate={() => setCreateOpen(true)}
        filters={filters}
        onChange={applyFilters}
        baselineFilters={baselineFilters}
        workspaceOptions={workspaceOptions}
        channelOptions={channelOptions}
        allChannelsLabel={t("conversationShell.all")}
        people={people}
      />

      <TaskBoardColumns
        columns={columns}
        counts={counts}
        window={columnWindow}
        isCollapsed={isCollapsed}
        onToggleCollapse={toggleCollapse}
        onGrow={growColumn}
        onOpenTask={onOpenTask}
        onStatusChange={(taskId, status) => updateStatus.mutate({ taskId, status })}
        columnClassName="bg-bg-primary"
        cardClassName="bg-bg-base"
      />

      <CreateTaskDialog
        open={createOpen}
        onOpenChange={setCreateOpen}
        target={createTarget}
        onSubmit={submitCreate}
      />
    </div>
  );
}
