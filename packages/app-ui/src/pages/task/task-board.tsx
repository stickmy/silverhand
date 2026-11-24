import { memo, useCallback, useMemo, useState } from "react";
import { PlusIcon, TaskIcon } from "@/icons";
import { useCompactLayout } from "@/navigation/layout";
import { useUpdateTaskStatus } from "@/features/tasks/hooks/hooks";
import { useKanbanColumnCollapse } from "@/features/tasks/hooks/use-kanban-column-collapse";
import { useTaskFilterOptions } from "@/features/tasks/hooks/use-task-filter-options";
import { useBoardColumns, useBoardCounts, useBoardFacets } from "@/features/tasks/hooks/use-board";
import { useColumnWindow } from "@/features/tasks/hooks/use-column-window";
import { DEFAULT_TASK_FILTERS, type TaskFilters } from "@/features/tasks/model/task-filters";
import { TaskBoardColumns } from "@/features/tasks/components/task-board-columns";
import { TaskToolbar } from "@/features/tasks/components/task-toolbar";
import {
  CreateTaskDialog,
  useCreateTaskSubmit,
  type CreateTaskTarget,
} from "@/features/tasks/components/create-task-dialog";
import { FloatingPaneChrome, PaneHeader, PaneShell } from "@/components/layout/pane-shell";
import { iconMarkup } from "@/components/layout/native-page-bar";
import { t, useLocale } from "@/lib/i18n";

export interface TaskBoardProps {
  filters: TaskFilters;
  onFiltersChange: (filters: TaskFilters) => void;
  onOpenTask: (taskId: string) => void;
}

export const TaskBoard = memo(function TaskBoard({
  onOpenTask,
  filters,
  onFiltersChange: setFilters,
}: TaskBoardProps): JSX.Element {
  useLocale();
  const [createOpen, setCreateOpen] = useState(false);
  const updateStatus = useUpdateTaskStatus();
  // Per-column render bound, local to THIS surface (#task:1885). Every write that
  // changes what the columns hold goes through `applyFilters` below, which resets
  // the window in the same commit.
  const { columnWindow, grow: growColumn, reset: resetColumnWindow } = useColumnWindow();
  const applyFilters = useCallback(
    (next: TaskFilters) => {
      setFilters(next);
      resetColumnWindow();
    },
    [resetColumnWindow],
  );

  const { workspaceOptions, channelOptions } = useTaskFilterOptions();

  // Collapse is read HERE, not only inside the column: a collapsed column must
  // render no body AND fetch none, so the gate has to reach the queries. The
  // column still receives the resolved bit + its toggle, so the store stays the
  // single source both surfaces share.
  const { isCollapsed, toggle: toggleCollapse } = useKanbanColumnCollapse();

  // The authority read faces (#task:1884): one paged query per column, the
  // per-status totals, and the filter menus' option sets. Every filter axis is
  // narrowed SERVER-side now — nothing here slices a full array, so the board's
  // cost no longer scales with the workspace's task count. Realtime freshness is
  // App's (`useTaskBoardRealtime` — one listener for both surfaces + the badge).
  const columns = useBoardColumns({
    filters,
    enabled: true,
    window: columnWindow,
    isCollapsed,
  });
  const counts = useBoardCounts(filters);
  const people = useBoardFacets(filters);

  const selectedChannelName = filters.channelId
    ? channelOptions.find((o) => o.id === filters.channelId)?.label
    : null;

  // A task must name a channel; seed the create picker from the current channel
  // FILTER only when it resolves to a real member-channel option (a thread-scoped
  // or ALL filter has no channel to pre-select — the actor picks one). On success
  // the dialog closes and the list refreshes in place; the new task is NOT opened
  // in a detail modal (shiki 2026-07-30). The global board always PICKS a channel
  // — it never guesses a DM as the target (contract §5); a DM task is created only
  // from that direct line's own Tasks tab.
  const createTarget = useMemo<CreateTaskTarget>(() => {
    const defaultChannelId = channelOptions.some((o) => o.id === filters.channelId)
      ? filters.channelId
      : null;
    return { kind: "pick", channelOptions, defaultChannelId };
  }, [channelOptions, filters.channelId]);
  const submitCreate = useCreateTaskSubmit();
  const compact = useCompactLayout();

  return (
    <PaneShell>
      <FloatingPaneChrome>
        <PaneHeader
          icon={<TaskIcon data-sticker-size="sm" className="size-4 text-text-muted" />}
          title={
            selectedChannelName
              ? t("taskPage.tasksIn", { channel: selectedChannelName })
              : t("taskPage.tasks")
          }
          trailing={
            compact ? (
              <button
                type="button"
                className="glass-surface glass-round focus-outline"
                onClick={() => setCreateOpen(true)}
                aria-label={t("taskPage.newTask")}
              >
                <PlusIcon size={20} />
              </button>
            ) : undefined
          }
          nativeActions={[
            {
              id: "create",
              label: t("taskPage.newTask"),
              icon: iconMarkup(PlusIcon),
              run: () => setCreateOpen(true),
            },
          ]}
        />

        <TaskToolbar
          onCreate={compact ? undefined : () => setCreateOpen(true)}
          filters={filters}
          onChange={applyFilters}
          baselineFilters={DEFAULT_TASK_FILTERS}
          workspaceOptions={workspaceOptions}
          channelOptions={channelOptions}
          allChannelsLabel={t("taskPage.all")}
          people={people}
        />
      </FloatingPaneChrome>
      <TaskBoardColumns
        columns={columns}
        counts={counts}
        window={columnWindow}
        isCollapsed={isCollapsed}
        onToggleCollapse={toggleCollapse}
        onGrow={growColumn}
        onOpenTask={onOpenTask}
        onStatusChange={(taskId, next) => updateStatus.mutate({ taskId, status: next })}
      />

      <CreateTaskDialog
        open={createOpen}
        onOpenChange={setCreateOpen}
        target={createTarget}
        onSubmit={submitCreate}
      />
    </PaneShell>
  );
});
