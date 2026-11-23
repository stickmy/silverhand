import type { KeyboardEvent, MouseEvent } from "react";
import type { AgentDto, TaskAssigneeDto } from "@silverhand/shared";
import { formatUserDisplayName, type UserNameResolver } from "@/features/session/current-user";
import type { CachedMessage } from "@/features/channels/pending-message";
import { StatusPicker, taskStatusLabel } from "@/features/tasks/components/task-status-badge";
import { useTaskRefContext } from "@/features/tasks/components/task-ref-context";
import { EventPerson } from "@/features/tasks/components/task-event-person";
import { useUpdateTaskStatus } from "@/features/tasks/hooks/hooks";
import { cn } from "@/lib/cn";
import { t } from "@/lib/i18n";

/**
 * The task a message is linked to, as a one-line card at the message's bottom
 * edge: status cube, number, title, assignee. The card opens the task; only the
 * cube changes status, and only for a member of an open channel (`canMutate`).
 * Everyone else reads the same cube without a picker.
 */
export function MessageTaskTag({
  message,
  agentsById,
  userResolver,
  canMutate,
}: {
  message: CachedMessage;
  agentsById: Map<string, AgentDto>;
  userResolver?: UserNameResolver;
  canMutate: boolean;
}): JSX.Element | null {
  const updateStatus = useUpdateTaskStatus();
  const onOpenTask = useTaskRefContext()?.onOpenTask;
  const linkedTask = message.linked_task;
  if (linkedTask === null) return null;

  const status = linkedTask.status;
  const label = taskStatusLabel(status);
  const assignee = linkedTask.assignee
    ? formatTaskAssignee(linkedTask.assignee, agentsById, userResolver)
    : null;

  const open = onOpenTask ? () => onOpenTask(linkedTask.task_id) : undefined;
  const handleClick = (event: MouseEvent) => {
    // The row's own selection / jump capture must not also see this click.
    event.stopPropagation();
    open?.();
  };
  const handleKeyDown = (event: KeyboardEvent) => {
    if (event.key !== "Enter" && event.key !== " ") return;
    event.preventDefault();
    open?.();
  };

  const cube = <span data-message-task-cube aria-hidden />;
  const statusNode = canMutate ? (
    <StatusPicker
      current={status}
      onChange={(next) => updateStatus.mutate({ taskId: linkedTask.task_id, status: next })}
    >
      <button
        type="button"
        data-message-task-status
        aria-label={t("tasks.changeStatusCurrent", { status: label })}
        // The card opens the task; the cube only opens the picker.
        onClick={(event) => event.stopPropagation()}
        onKeyDown={(event) => event.stopPropagation()}
        className="cursor-pointer focus-outline"
      >
        {cube}
      </button>
    </StatusPicker>
  ) : (
    <span data-message-task-status title={label}>
      {cube}
      <span className="sr-only">{label}</span>
    </span>
  );

  return (
    <div
      data-message-task-tag
      data-task-ref-tone={status}
      role={open ? "button" : undefined}
      tabIndex={open ? 0 : undefined}
      aria-label={
        open
          ? t("messages.taskTitled", { number: linkedTask.task_number, title: linkedTask.title })
          : undefined
      }
      title={linkedTask.title}
      onClick={open ? handleClick : undefined}
      onKeyDown={open ? handleKeyDown : undefined}
      className={cn(
        "inline-flex h-7.5 max-w-full min-w-0 items-center gap-2 rounded-(--control-radius) border-(length:--control-border-width) border-border bg-bg-base pr-2.5 pl-0.5 text-[13px] leading-normal transition-colors",
        open && "cursor-pointer hover:border-(--border-control-hover) focus-outline",
      )}
    >
      {statusNode}
      <span className="shrink-0 font-medium text-text-muted tabular-nums">
        #{linkedTask.task_number}
      </span>
      <span className="min-w-0 flex-1 truncate text-text-primary">{linkedTask.title}</span>
      {linkedTask.assignee && assignee ? (
        <span className="flex shrink-0 items-center border-l border-border-subtle pl-2 text-xs text-text-secondary">
          <EventPerson
            person={
              linkedTask.assignee.type === "agent"
                ? { kind: "agent", id: linkedTask.assignee.id, name: assignee }
                : { kind: "user", id: linkedTask.assignee.id, name: assignee }
            }
          />
        </span>
      ) : null}
    </div>
  );
}
export function formatTaskAssignee(
  assignee: TaskAssigneeDto,
  agentsById: Map<string, AgentDto>,
  userResolver?: UserNameResolver,
): string {
  if (assignee.type === "user") {
    return formatUserDisplayName(assignee.id, userResolver);
  }
  const agent = agentsById.get(assignee.id);
  const name = (
    agent?.display_name?.trim() ||
    agent?.name ||
    assignee.name.trim() ||
    assignee.id
  ).replace(/^@/, "");
  // In-message task cards are a recognition surface: plain names for both
  // kinds, no leading `@` (the user branch above is already bare). Every task
  // assignee/creator DISPLAY reads the same way now — board cards, the
  // per-conversation Tasks tab,
  // TasksSection, the filter, and the TaskRef popover all show the plain
  // display name (the dto's `name` is the denormalized display_name, never a
  // handle). `@` is reserved for surfaces that show a real handle: mentions,
  // the composer, and the assignee picker — split by surface, not by actor
  // kind (shiki 2026-08-01, #task:1768).
  return name;
}
