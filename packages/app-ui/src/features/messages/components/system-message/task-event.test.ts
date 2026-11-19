import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MessageDtoSchema } from "@silverhand/shared";
import { CLUSTERABLE_SUBTYPES } from "@/features/messages/model/group-messages";
import { TaskEventRow } from "./task-event";
import { t } from "@/lib/i18n";

vi.mock("./shared", () => ({ useUserResolver: () => () => "User" }));
vi.mock("@/features/messages/components/task-ref", () => ({
  TaskRef: ({ taskNumber }: { taskNumber: number }) => `#${taskNumber}`,
}));

function text(subtype: "task_edit" | "task_status", enriched: boolean) {
  const metadata = {
    subtype,
    task_number: 42,
    task_status: "in_progress",
    ...(enriched
      ? {
          task_id: "task-a",
          task_title: "Example",
          task_assignee: null,
          actor: { kind: "agent", id: "agent-a", name: "Agent" },
        }
      : {}),
  };
  const message = MessageDtoSchema.parse({
    id: "message",
    conversation_id: "channel",
    from_kind: "system",
    from_agent_id: null,
    from_user_id: null,
    author: null,
    content: "legacy content",
    metadata,
    attachments: null,
    reactions: null,
    seq: 1,
    created_at: 1,
    thread_root_id: null,
    linked_task: null,
  });
  const client = new QueryClient();
  return renderToStaticMarkup(
    createElement(QueryClientProvider, { client }, createElement(TaskEventRow, { message })),
  ).replace(/<[^>]+>/g, " ");
}
describe("task event meaning", () => {
  it.each([false, true])(
    "an edit says edited without claiming a status move (enriched=%s)",
    (enriched) => {
      const content = text("task_edit", enriched);
      expect(content).toContain("#42");
      expect(content).not.toContain(t("messages.taskStatus.inProgress"));
    },
  );
  it.each([false, true])(
    "a status event does report the status transition (enriched=%s)",
    (enriched) => {
      const content = text("task_status", enriched);
      expect(content).toContain(t("messages.taskStatus.inProgress"));
    },
  );
  it("clusters task edits with lifecycle events", () => {
    expect(CLUSTERABLE_SUBTYPES.has("task_edit")).toBe(true);
  });
});
