import { taskChannelLabel } from "@/features/tasks/model/task-place";
import { useEffect, useState } from "react";
import { ClockIcon } from "@/icons";
import { useTaskByNumber } from "@/features/tasks/hooks/use-task-by-number";
import { useTaskRefContext } from "@/features/tasks/components/task-ref-context";
import { useUpdateTaskStatus } from "@/features/tasks/hooks/hooks";
import {
  StatusPicker,
  TaskStatusBadge,
  taskStatusLabel,
} from "@/features/tasks/components/task-status-badge";
import { HoverPopover } from "./hover-popover";
import { useRelativeTime } from "../hooks/use-relative-time";
import type { TaskSummaryDto, TaskStatus } from "@silverhand/shared";
import { t } from "@/lib/i18n";

interface TaskRefProps {
  taskNumber: number;
  bare?: boolean;
}

export interface StaticTaskRefSummary {
  task_number: number;
  title: string;
  status?: TaskStatus;
}

/** Leading status cube + number + title (truncated by CSS, `markdown.css`).
 *  The cube's colour comes from `data-task-ref-tone`; the status label lives in
 *  the hover card. Event rows hide the cube because the row already states it. */
function TaskRefContent({
  taskNumber,
  title,
  status,
  onStatusChange,
  onStatusActiveChange,
}: {
  taskNumber: number;
  title: string;
  status?: TaskStatus;
  onStatusChange?: (next: TaskStatus) => void;
  onStatusActiveChange?: (active: boolean) => void;
}): JSX.Element {
  return (
    <>
      {status ? (
        <TaskRefStatus
          status={status}
          onChange={onStatusChange}
          onActiveChange={onStatusActiveChange}
        />
      ) : null}
      <span data-task-ref-label>
        <span data-task-ref-num>#{taskNumber}</span> {title}
      </span>
    </>
  );
}

/** `onActiveChange` reports while the pointer is on the status or its picker is
 *  open, so the host can keep the task hover card out of the way. */
function TaskRefStatus({
  status,
  onChange,
  onActiveChange,
}: {
  status: TaskStatus;
  onChange?: (next: TaskStatus) => void;
  onActiveChange?: (active: boolean) => void;
}): JSX.Element {
  const [hovered, setHovered] = useState(false);
  const [pickerOpen, setPickerOpen] = useState(false);

  useEffect(() => {
    onActiveChange?.(hovered || pickerOpen);
  }, [hovered, pickerOpen, onActiveChange]);

  const label = taskStatusLabel(status);
  const cube = <span data-task-ref-cube aria-hidden />;

  if (!onChange) {
    return (
      <span data-task-ref-status={status} title={label}>
        {cube}
        <span className="sr-only">{label}</span>
      </span>
    );
  }
  return (
    <StatusPicker current={status} onChange={onChange} onOpenChange={setPickerOpen}>
      <button
        type="button"
        data-task-ref-status={status}
        aria-label={t("tasks.changeStatusCurrent", { status: label })}
        // The chip itself opens the task; the status only opens the picker.
        onClick={(event) => event.stopPropagation()}
        onKeyDown={(event) => event.stopPropagation()}
        onPointerEnter={() => setHovered(true)}
        onPointerLeave={() => setHovered(false)}
        className="cursor-pointer focus-outline"
      >
        {cube}
      </button>
    </StatusPicker>
  );
}

/** Static references retain raw text until metadata is available. */
export function TaskRefStaticChip({
  taskNumber,
  task,
}: TaskRefProps & { task?: StaticTaskRefSummary | null }): JSX.Element {
  const inertLabel = `#task:${taskNumber}`;
  if (!task) {
    return <>{inertLabel}</>;
  }
  return (
    <span
      title={t("messages.taskTitled", { number: task.task_number, title: task.title })}
      data-task-ref={String(taskNumber)}
      data-task-ref-state="resolved"
      data-task-ref-tone={task.status}
    >
      <TaskRefContent taskNumber={task.task_number} title={task.title} status={task.status} />
    </span>
  );
}

/** Only resolved tasks receive reference styling and a metadata popover. */
export function TaskRef({ taskNumber, bare = false }: TaskRefProps): JSX.Element {
  const ctx = useTaskRefContext();
  const conversationId = ctx?.conversationId ?? null;
  const result = useTaskByNumber(
    ctx ? { workspaceId: ctx.workspaceId, conversationId: ctx.conversationId } : null,
    taskNumber,
  );
  const task = result.data ?? null;
  const onOpenTask = ctx?.onOpenTask;
  const inertLabel = `#task:${taskNumber}`;

  const [pickerOpen, setPickerOpen] = useState(false);
  const [statusActive, setStatusActive] = useState(false);
  const updateStatus = useUpdateTaskStatus();

  if (!conversationId || result.isLoading || !task) {
    return <>{bare ? `#${taskNumber}` : inertLabel}</>;
  }

  const label = `#${task.task_number} ${task.title}`;
  const tooltip = t("messages.taskLabel", { label });

  const chipContent = (
    <TaskRefContent
      taskNumber={task.task_number}
      title={task.title}
      status={task.status}
      onStatusChange={(next) => updateStatus.mutate({ taskId: task.id, status: next })}
      onStatusActiveChange={setStatusActive}
    />
  );

  const inertChip = (
    <span
      // Wrapped in HoverPopover below — `aria-label` instead of the
      // native `title` attribute (see loading branch comment above
      // for the flicker rationale).
      aria-label={tooltip}
      data-task-ref={String(taskNumber)}
      data-task-ref-state="resolved-no-handler"
      data-task-ref-tone={task.status}
    >
      {chipContent}
    </span>
  );

  const interactiveChip = (
    <span
      role="button"
      tabIndex={0}
      className="cursor-pointer focus-outline"
      aria-label={tooltip}
      data-task-ref={String(taskNumber)}
      data-task-ref-state="resolved"
      data-task-ref-tone={task.status}
      onClick={(event) => {
        event.preventDefault();
        event.stopPropagation();
        onOpenTask?.(task.id);
      }}
      onKeyDown={(event) => {
        if (event.key !== "Enter" && event.key !== " ") return;
        event.preventDefault();
        event.stopPropagation();
        onOpenTask?.(task.id);
      }}
    >
      {chipContent}
    </span>
  );

  // Both interactive and inert resolved-state chips get the popover —
  // the popover is purely informational (status / assignee /
  // conversation), not a click affordance, so it makes sense even
  // when the click handler is unavailable (e.g. saved/preview
  // surfaces with workspace context but no task-open host).
  return (
    <HoverPopover
      trigger={onOpenTask ? interactiveChip : inertChip}
      content={
        <TaskPopoverBody
          task={task}
          onStatusChange={(next) => updateStatus.mutate({ taskId: task.id, status: next })}
          onPickerOpenChange={setPickerOpen}
        />
      }
      isContentReady={true}
      keepOpen={pickerOpen}
      suppressed={statusActive}
    />
  );
}

/**
 * Popover body — full title (no clamp) with a single inline meta row
 * for assignee · created · conversation. Earlier this was a `dl` with
 * three labelled rows; @shiki feedback (`WlIwDhmrbu6_azy1`) was that
 * the form-style layout felt heavy in such a small surface. Inline
 * meta keeps the popover scannable: title dominates, meta sits as one
 * subtle muted line that wraps if it has to.
 */
function TaskPopoverBody({
  task,
  onStatusChange,
  onPickerOpenChange,
}: {
  task: TaskSummaryDto;
  onStatusChange: (next: TaskStatus) => void;
  onPickerOpenChange: (open: boolean) => void;
}): JSX.Element {
  const { relative, absolute } = useRelativeTime(task.created_at);
  const channelLabel = taskChannelLabel(task);
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between gap-2">
        <span data-tasks-num className="font-mono text-(length:--text-badge) text-text-muted">
          #{task.task_number}
        </span>
        {/* Same interactive chip as the task-pane card: click → "Move to"
            picker → POST /status. `onPickerOpenChange` pins the host
            hover-card open while the picker is up. */}
        <TaskStatusBadge
          status={task.status}
          onChange={onStatusChange}
          onOpenChange={onPickerOpenChange}
        />
      </div>
      <p className="wrap-break-word text-xs font-medium text-text-primary">{task.title}</p>
      <p className="flex items-center gap-1.5 text-(length:--text-meta) text-text-muted">
        <span className="shrink-0 text-text-secondary">
          {task.assignee ? task.assignee.name : <em>{t("messages.unassigned")}</em>}
        </span>
        {channelLabel !== null ? (
          <span className="min-w-0 truncate text-text-secondary" title={channelLabel}>
            {t("messages.inChannel", { channel: channelLabel })}
          </span>
        ) : null}
        <span className="inline-flex shrink-0 items-center gap-1" title={absolute}>
          <ClockIcon className="size-3 shrink-0" aria-hidden />
          <span>{relative}</span>
        </span>
      </p>
    </div>
  );
}
