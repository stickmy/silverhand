import { expect, it } from "vitest";
import { NavigationStore } from "./store";
const url = (s: NavigationStore) =>
  s.getSnapshot().location.pathname + s.getSnapshot().location.search;
it("restores independent tab histories after a new push in the previous tab", () => {
  const s = new NavigationStore("/chat");
  s.go("/chat/d/c");
  s.selectTab("tasks");
  s.go("/tasks?pane=task.t");
  s.go("/chat/d/c");
  s.selectTab("chat");
  s.go("/chat/d/c?pane=settings");
  s.selectTab("tasks");
  expect(url(s)).toBe("/chat/d/c");
  s.back();
  expect(url(s)).toBe("/tasks?pane=task.t");
});
it("uses the same history for members, overview and conversation", () => {
  const s = new NavigationStore("/chat/d/c");
  s.go("/chat/d/c?pane=settings");
  s.go("/chat/d/c?pane=settings.members");
  s.back();
  expect(url(s)).toBe("/chat/d/c?pane=settings");
  s.back();
  expect(url(s)).toBe("/chat/d/c");
});
it("Close replaces the current pane instead of going back to an earlier thread", () => {
  const s = new NavigationStore("/chat/d/c");
  s.go("/chat/d/c?pane=threads.a");
  s.go("/chat/d/c?pane=threads.b");
  s.go("/chat/d/c", { replace: true });
  expect(url(s)).toBe("/chat/d/c");
});
it("More belongs to its source tab and same URL tab switches publish state", () => {
  const s = new NavigationStore("/chat");
  s.selectTab("activity");
  s.go("/members");
  s.selectTab("chat");
  s.go("/members");
  s.selectTab("activity");
  expect(s.getSnapshot().tab).toBe("activity");
  s.back();
  expect(url(s)).toBe("/activity");
});
it("external links choose their tab, deep links have a parent, disposed sessions ignore callbacks", () => {
  const s = new NavigationStore("/chat/d/c?pane=settings.members");
  s.back();
  expect(url(s)).toBe("/chat/d/c?pane=settings");
  s.external("/tasks?pane=task.t");
  expect(s.getSnapshot().tab).toBe("tasks");
  s.dispose();
  s.external("/chat/private");
  expect(url(s)).toBe("/tasks?pane=task.t");
});
it("keeps library forward traversal within the active tab", () => {
  const s = new NavigationStore("/chat");
  s.go("/chat/d/c");
  s.go("/chat/d/c?pane=settings");
  s.traverse(-1);
  expect(url(s)).toBe("/chat/d/c");
  s.traverse(1);
  expect(url(s)).toBe("/chat/d/c?pane=settings");
  s.selectTab("tasks");
  s.traverse(1);
  expect(url(s)).toBe("/tasks");
});
it("Closing an overlay returns to the entry that opened it", () => {
  const s = new NavigationStore("/tasks");
  s.go("/tasks?pane=task.t");
  s.go("/tasks", { replace: true });
  expect(url(s)).toBe("/tasks");
  expect(s.getSnapshot().canBack).toBe(false);
});
it("a push starts a page, a replace keeps it, and Back returns to the kept page", () => {
  const s = new NavigationStore("/chat/d/c");
  const channel = s.getSnapshot().page;
  s.go("/chat/d/c?pane=threads.a");
  const thread = s.getSnapshot();
  expect(thread.page).not.toBe(channel);
  expect(thread.keptPages[0]).toBe(channel);
  s.go("/chat/d/c?pane=threads.b", { replace: true });
  expect(s.getSnapshot().page).toBe(thread.page);
  s.back();
  expect(s.getSnapshot().page).toBe(channel);
});
it("keeps every page of every Tab's stack", () => {
  const s = new NavigationStore("/chat");
  const chats = s.getSnapshot().page;
  s.go("/chat/d/c");
  const channel = s.getSnapshot().page;
  s.go("/chat/d/c?pane=settings");
  const settings = s.getSnapshot().page;
  s.go("/chat/d/c?pane=settings.members");
  expect(s.getSnapshot().keptPages).toEqual(
    expect.arrayContaining([chats, channel, settings, s.getSnapshot().page]),
  );
  s.selectTab("tasks");
  expect(s.getSnapshot().keptPages).toEqual(expect.arrayContaining([chats, channel, settings]));
  s.selectTab("chat");
  s.back();
  s.back();
  expect(s.getSnapshot().page).toBe(channel);
  expect(s.getSnapshot().keptPages).not.toContain(settings);
});
