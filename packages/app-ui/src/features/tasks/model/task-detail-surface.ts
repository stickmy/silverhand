import { singlePane } from "@/navigation/panes";
import type { RouteState } from "@/router";

type TaskDetailRoute = Pick<RouteState, "section" | "chatView" | "channelId" | "panes">;

/** The open task and where it shows, or null when none is open or the page
 *  has no place for it. */
export function resolveTaskDetail(
  route: TaskDetailRoute,
): { id: string; surface: "dialog" | "pane" } | null {
  const id = singlePane(route.panes, "task")?.id ?? null;
  if (id === null) {
    return null;
  }

  if (route.section === "tasks") {
    return { id, surface: "dialog" };
  }

  if (
    (route.section === "chat" || route.section === "activity") &&
    route.chatView === "channel" &&
    route.channelId !== null
  ) {
    return { id, surface: "pane" };
  }

  return null;
}
