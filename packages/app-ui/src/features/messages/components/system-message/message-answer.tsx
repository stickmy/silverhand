import { readMessageAnswerMetadata, readMessageAnswerWithdrawnMetadata } from "@silverhand/shared";
import { EventPerson } from "@/features/tasks/components/task-event-person";
import { t, tRich } from "@/lib/i18n";
import { GenericNarrationFallback, type RendererProps } from "./shared";

/** "shiki answered Norman's question: A" — one line in the task-event form.
 *  Clicking it brings the question into view. */
export function MessageAnswerRow(props: RendererProps): JSX.Element {
  const meta = readMessageAnswerMetadata(
    (props.message.metadata ?? null) as Record<string, unknown> | null,
  );
  if (!meta) {
    return <GenericNarrationFallback {...props} />;
  }

  const picks = meta.options.map((option) => option.label);
  if (meta.other !== null) {
    picks.push(t("messages.optionOtherAnswer", { text: meta.other }));
  }
  const answerer = <EventPerson person={meta.answerer} />;
  const author = <EventPerson person={meta.question_author} />;

  return (
    <button
      type="button"
      data-task-event-line
      data-message-answer-line
      onClick={() => props.onLocateMessage?.(meta.answer_to)}
      className="w-full min-w-0 justify-start gap-1 text-left text-text-muted"
    >
      <span data-task-event-meta className="min-w-0">
        {meta.changed
          ? tRich("messages.answerEvent.changed", { answerer })
          : tRich("messages.answerEvent.answered", { answerer, author })}
      </span>
      <span className="min-w-0 truncate text-text-secondary">{picks.join(", ")}</span>
    </button>
  );
}

/** "shiki withdrew the answer to Norman's question" — same line form. */
export function MessageAnswerWithdrawnRow(props: RendererProps): JSX.Element {
  const meta = readMessageAnswerWithdrawnMetadata(
    (props.message.metadata ?? null) as Record<string, unknown> | null,
  );
  if (!meta) {
    return <GenericNarrationFallback {...props} />;
  }

  return (
    <button
      type="button"
      data-task-event-line
      onClick={() => props.onLocateMessage?.(meta.answer_to)}
      className="w-full min-w-0 justify-start text-left text-text-muted"
    >
      <span data-task-event-meta className="min-w-0">
        {tRich("messages.answerEvent.withdrawn", {
          answerer: <EventPerson person={meta.answerer} />,
          author: <EventPerson person={meta.question_author} />,
        })}
      </span>
    </button>
  );
}
