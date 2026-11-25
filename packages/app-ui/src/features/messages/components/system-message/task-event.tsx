import {
  type TaskStatus,
  type TaskSystemMetadata,
  type TaskSystemMetadataPartial,
  readTaskSystemMetadata,
  isTaskAssignmentEvent,
  readTaskSystemMetadataPartial,
} from "@silverhand/shared";
import { formatUserDisplayName } from "@/features/session/current-user";
import { TaskRef } from "@/features/messages/components/task-ref";
import { type RendererProps, useUserResolver } from "./shared";
import { t, tRich } from "@/lib/i18n";
import { EventPerson, type EventPersonRef } from "@/features/tasks/components/task-event-person";

// ─── task event row ───────────────────────────────────────────────────
export function TaskEventRow({ message }: RendererProps): JSX.Element {
  const meta = readTaskSystemMetadata(message.metadata);
  if (meta) return <TaskEventRowEnriched meta={meta} />;
  // Mid-tier degradation for legacy rows that carry the minimal
  // `(subtype, task_number, task_status)` triple but no actor /
  // title / message_id enrichment. Render chip + dot + pill (or
  // chip + verb) so the audit entry still self-identifies. We do
  // NOT promote `message.content` here — it was the old narration
  // string, not structured data.
  const partial = readTaskSystemMetadataPartial(message.metadata);
  if (partial) return <TaskEventRowDegraded meta={partial} />;
  // Total degradation — metadata is missing even subtype/task_number
  // (shouldn't happen for current producers, but bare belt+suspenders
  // for unknown malformed rows).
  return <TaskEventLegacyFallback content={message.content} />;
}

function TaskEventRowEnriched({ meta }: { meta: TaskSystemMetadata }): JSX.Element {
  const userResolver = useUserResolver();

  const actorLabel =
    meta.actor.kind === "user"
      ? formatUserDisplayName(meta.actor.id, userResolver) || meta.actor.name || "user"
      : meta.actor.name;
  const actor: EventPersonRef =
    meta.actor.kind === "user"
      ? { kind: "user", id: meta.actor.id, name: actorLabel }
      : { kind: "agent", id: meta.actor.id, name: actorLabel };
  const cubeKind: TaskEventCubeKind =
    meta.subtype === "task_unclaim" ? "unclaim" : meta.task_status;

  return (
    <div data-task-event-line className="text-text-secondary">
      <TaskEventCube kind={cubeKind} />
      <span data-task-event-task>
        <TaskRef taskNumber={meta.task_number} />
      </span>
      <span data-task-event-meta>
        <TaskEventMeta meta={meta} actor={actor} />
      </span>
    </div>
  );
}

/** The trailing "what happened, by whom" line. The task and its state lead the
 *  row, so the person comes last, with their avatar. */
function TaskEventMeta({
  meta,
  actor,
}: {
  meta: TaskSystemMetadata;
  actor: EventPersonRef;
}): JSX.Element {
  const actorNode = <EventPerson person={actor} />;

  if (meta.subtype === "task_status") {
    return (
      <>
        {tRich("messages.taskEvent.moved", {
          status: <TaskEventStatus status={meta.task_status} />,
          actor: actorNode,
        })}
      </>
    );
  }
  // An assignment names its assignee; saying "claimed by shiki" for an
  // assignment to someone else states the wrong fact.
  if (isTaskAssignmentEvent(meta)) {
    const assignee = meta.task_assignee;
    if (meta.subtype !== "task_unclaim" && assignee) {
      return (
        <>
          {tRich("messages.taskEvent.assigned", {
            assignee: (
              <EventPerson
                person={
                  assignee.type === "agent"
                    ? { kind: "agent", id: assignee.id, name: assignee.name }
                    : { kind: "user", id: assignee.id, name: assignee.name }
                }
              />
            ),
            actor: actorNode,
          })}
        </>
      );
    }
    return (
      <>
        {tRich(
          meta.subtype === "task_unclaim"
            ? "messages.taskEvent.unassigned"
            : "messages.taskEvent.assignedNobody",
          { actor: actorNode },
        )}
      </>
    );
  }
  // `cross_conversation` says an agent filed this task from a DIFFERENT
  // conversation. HARD PRIVACY INVARIANT: the bit carries no origin, so this row
  // says THAT it was filed remotely and never WHERE from; do not add the origin's
  // name, link, or icon, and never call it a "channel" (any conversation, DMs
  // included, can be either end).
  const key =
    meta.subtype === "task_create"
      ? meta.cross_conversation === true
        ? "messages.taskEvent.filed"
        : "messages.taskEvent.created"
      : meta.subtype === "task_claim"
        ? "messages.taskEvent.claimed"
        : meta.subtype === "task_edit"
          ? "messages.taskEvent.edited"
          : "messages.taskEvent.unclaimed";
  return <>{tRich(key, { actor: actorNode })}</>;
}

type TaskEventCubeKind = TaskStatus | "unclaim";

/** The event's after-state, in the same cube as the inline task reference
 *  (`--task-status-*-solid`). `unclaim` is not a status: a dashed hollow cube. */
function TaskEventCube({ kind }: { kind: TaskEventCubeKind }): JSX.Element {
  return <span aria-hidden data-task-event-dot={kind} />;
}

function TaskEventStatus({ status }: { status: TaskStatus }): JSX.Element {
  const labels: Record<TaskStatus, string> = {
    todo: t("messages.taskStatus.todo"),
    in_progress: t("messages.taskStatus.inProgress"),
    in_review: t("messages.taskStatus.inReview"),
    done: t("messages.taskStatus.done"),
    closed: t("messages.taskStatus.closed"),
  };
  return <span data-task-event-status={status}>{labels[status]}</span>;
}

function TaskEventLegacyFallback({ content }: { content: string }): JSX.Element {
  return (
    <div className="px-4 py-1.5 text-center italic text-text-muted" data-task-event-legacy="1">
      {content}
    </div>
  );
}

/**
 * Mid-tier degraded row for legacy task system messages (pre-PR-2 DB rows):
 * metadata only carries `(subtype, task_number, task_status)`, so the line is
 * cube + bare `#task:N` + the status word (status events) or the verb.
 */
function TaskEventRowDegraded({ meta }: { meta: TaskSystemMetadataPartial }): JSX.Element {
  const cubeKind: TaskEventCubeKind =
    meta.subtype === "task_unclaim" ? "unclaim" : meta.task_status;
  return (
    <div data-task-event-line data-task-event-degraded="1" className="text-text-secondary">
      <TaskEventCube kind={cubeKind} />
      <span data-task-event-task>
        <TaskRef taskNumber={meta.task_number} />
      </span>
      <span data-task-event-meta>
        {meta.subtype === "task_status" ? (
          <TaskEventStatus status={meta.task_status} />
        ) : meta.subtype === "task_create" ? (
          t("messages.taskVerb.created")
        ) : meta.subtype === "task_claim" ? (
          t("messages.taskVerb.claimed")
        ) : meta.subtype === "task_assign" ? (
          t("messages.taskVerb.assigned")
        ) : meta.subtype === "task_edit" ? (
          t("messages.taskVerb.edited")
        ) : (
          t("messages.taskVerb.unclaimed")
        )}
      </span>
    </div>
  );
}
