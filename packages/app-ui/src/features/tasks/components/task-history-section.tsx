import { useMemo, useState, useSyncExternalStore } from "react";
import dayjs from "dayjs";
import type { TaskStatus, TaskSystemMetadata } from "@silverhand/shared";
import { ChevronDownIcon, ChevronRightIcon } from "@/icons";
import { formatUserDisplayName } from "@/features/session/current-user";
import { makeUserNameResolver, useUsersById } from "@/features/users/hooks";
import { EventPerson, type EventPersonRef } from "@/features/tasks/components/task-event-person";
import { type TaskHistoryEntry, useTaskHistory } from "@/features/tasks/hooks/use-task-history";
import { t, tRich } from "@/lib/i18n";

const PREVIEW_COUNT = 5;

// Collapse is a session preference shared by every task's History, so folding
// it once keeps it folded while moving between tasks.
let collapsed = false;
const listeners = new Set<() => void>();
function subscribe(onChange: () => void): () => void {
  listeners.add(onChange);
  return () => listeners.delete(onChange);
}
function setCollapsed(next: boolean): void {
  collapsed = next;
  for (const listener of listeners) listener();
}

/** The task's changes, newest first, as a timeline. Hidden until the server
 *  answers with at least one event (older servers have no history route). */
export function TaskHistorySection({ taskId }: { taskId: string }): JSX.Element | null {
  const { data: entries } = useTaskHistory(taskId);
  const isCollapsed = useSyncExternalStore(subscribe, () => collapsed);
  const [showAll, setShowAll] = useState(false);

  if (!entries || entries.length === 0) return null;

  const visible = showAll ? entries : entries.slice(0, PREVIEW_COUNT);
  const Chevron = isCollapsed ? ChevronRightIcon : ChevronDownIcon;

  return (
    <section data-task-history className="flex min-w-0 flex-col gap-2">
      <button
        type="button"
        aria-expanded={!isCollapsed}
        onClick={() => setCollapsed(!isCollapsed)}
        className="-mx-1 flex w-fit cursor-pointer items-center gap-1 rounded px-1 text-text-muted hover:text-text-secondary focus-outline"
      >
        <span className="chrome-label text-(length:--text-badge) font-medium">
          {t("tasks.history.title")}
        </span>
        <Chevron className="size-3" aria-hidden />
      </button>
      {isCollapsed ? null : (
        <>
          <ol data-task-history-list>
            {visible.map((entry) => (
              <TaskHistoryItem key={entry.id} entry={entry} />
            ))}
          </ol>
          {entries.length > PREVIEW_COUNT ? (
            <button
              type="button"
              onClick={() => setShowAll(!showAll)}
              className="w-fit cursor-pointer text-(length:--text-meta) text-text-muted underline-offset-2 hover:text-text-secondary hover:underline focus-outline"
            >
              {showAll
                ? t("tasks.history.showLess")
                : t("tasks.history.showAll", { count: entries.length })}
            </button>
          ) : null}
        </>
      )}
    </section>
  );
}

function TaskHistoryItem({ entry }: { entry: TaskHistoryEntry }): JSX.Element {
  const usersById = useUsersById();
  const resolver = useMemo(() => makeUserNameResolver(usersById), [usersById]);
  const { meta } = entry;
  const actor: EventPersonRef =
    meta.actor.kind === "user"
      ? {
          kind: "user",
          id: meta.actor.id,
          name: formatUserDisplayName(meta.actor.id, resolver) || meta.actor.name,
        }
      : { kind: "agent", id: meta.actor.id, name: meta.actor.name };
  const at = dayjs(entry.createdAt);

  return (
    <li
      data-task-history-item
      data-task-history-kind={meta.subtype === "task_status" ? meta.task_status : "change"}
    >
      <span data-task-history-dot aria-hidden />
      <span data-task-history-text>{describe(meta, <EventPerson person={actor} />)}</span>
      <time dateTime={at.toISOString()} title={at.format("YYYY-MM-DD HH:mm:ss")}>
        {at.format(at.isSame(dayjs(), "year") ? "MM-DD HH:mm" : "YYYY-MM-DD")}
      </time>
    </li>
  );
}

function describe(meta: TaskSystemMetadata, actor: JSX.Element): React.ReactNode {
  switch (meta.subtype) {
    case "task_status":
      return tRich("tasks.history.moved", {
        actor,
        status: <span data-task-history-status>{statusLabel(meta.task_status)}</span>,
      });
    case "task_create":
      return tRich(
        meta.cross_conversation === true ? "tasks.history.filed" : "tasks.history.created",
        { actor },
      );
    case "task_edit":
      return tRich("tasks.history.edited", { actor });
    case "task_claim":
    case "task_unclaim": {
      // A human claim/unclaim is an assignment from the board, not taking the
      // job (`docs/tasks-design.md` §4.4).
      if (meta.actor.kind !== "user") {
        return tRich(
          meta.subtype === "task_claim" ? "tasks.history.claimed" : "tasks.history.unclaimed",
          { actor },
        );
      }
      const assignee = meta.task_assignee;
      if (meta.subtype === "task_claim" && assignee) {
        return tRich("tasks.history.assigned", {
          actor,
          assignee: (
            <EventPerson
              person={
                assignee.type === "agent"
                  ? { kind: "agent", id: assignee.id, name: assignee.name }
                  : { kind: "user", id: assignee.id, name: assignee.name }
              }
            />
          ),
        });
      }
      return tRich(
        meta.subtype === "task_claim" ? "tasks.history.assignedNobody" : "tasks.history.unassigned",
        { actor },
      );
    }
  }
}

function statusLabel(status: TaskStatus): string {
  switch (status) {
    case "todo":
      return t("tasks.todo");
    case "in_progress":
      return t("tasks.inProgress");
    case "in_review":
      return t("tasks.inReview");
    case "done":
      return t("tasks.done");
    case "closed":
      return t("tasks.closed");
  }
}
