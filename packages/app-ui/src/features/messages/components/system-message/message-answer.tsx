import { readMessageAnswerMetadata } from "@silverhand/shared";
import { EventPerson } from "@/features/tasks/components/task-event-person";
import { t, tRich } from "@/lib/i18n";
import { GenericNarrationFallback, type RendererProps } from "./shared";

/** "shiki answered Norman's question: A" — one line in the task-event form.
 *  Clicking it brings the question into view when it is in the loaded window. */
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
      onClick={() =>
        document
          .getElementById(`msg-${meta.answer_to}`)
          ?.scrollIntoView({ block: "center", behavior: "smooth" })
      }
      className="w-full min-w-0 text-left text-text-muted"
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
