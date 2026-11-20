import { describe, expect, it } from "vitest";
import { parseRouteState } from "@/router";
import { resolveTaskDetail } from "./task-detail-surface";

function parseRoute(path: string) {
  const url = new URL(path, "https://example.test");
  return parseRouteState(url.pathname, url.search);
}

describe("task detail surface", () => {
  it("uses a dialog only on the Tasks page", () => {
    expect(resolveTaskDetail(parseRoute("/tasks?pane=task.t"))?.surface).toBe("dialog");
    expect(resolveTaskDetail(parseRoute("/tasks"))).toBeNull();
  });

  it("uses a pane in chat and activity conversations", () => {
    expect(resolveTaskDetail(parseRoute("/chat/w/w/c/c?pane=task.t"))?.surface).toBe("pane");
    expect(resolveTaskDetail(parseRoute("/chat/d/d?pane=task.t"))?.surface).toBe("pane");
    expect(resolveTaskDetail(parseRoute("/activity/w/w/c/c?pane=task.t"))?.surface).toBe("pane");
  });

  it("does not invent a modal for other pages or an empty conversation", () => {
    for (const path of [
      "/members?pane=task.t",
      "/settings?pane=task.t",
      "/chat?pane=task.t",
      "/chat/saved?pane=task.t",
      "/activity?pane=task.t",
    ]) {
      expect(resolveTaskDetail(parseRoute(path))).toBeNull();
    }
  });
});
