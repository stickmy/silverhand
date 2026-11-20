import { describe, expect, it } from "vitest";
import { parentUrl } from "./history";
import { resolveNavigationTarget } from "@/router";
describe("navigation targets", () => {
  it.each([
    ["/chat/w/w/c/c?pane=threads.r&pane=files", "/chat/w/w/c/c?pane=threads.r"],
    ["/chat/w/w/c/c?pane=threads.r", "/chat/w/w/c/c?pane=threads"],
    ["/chat/w/w/c/c?pane=threads", "/chat/w/w/c/c"],
    ["/chat/w/w/c/c?pane=settings.members", "/chat/w/w/c/c?pane=settings"],
    ["/chat/w/w/c/c", "/chat"],
    ["/activity/d/c", "/activity"],
    ["/members/u/person", "/members"],
    ["/settings?settingsSection=connection", "/settings"],
    ["/computers?computer=host", "/computers"],
    ["/settings", "/me"],
    ["/me", null],
    ["/chat", null],
  ])("deep link %s has a safe parent", (url, parent) => expect(parentUrl(url!)).toBe(parent));
  it("one semantic target preserves the current activity placement", () => {
    expect(
      resolveNavigationTarget({ kind: "thread", rootMessageId: "r" }, "/activity/d/c", ""),
    ).toBe("/activity/d/c?pane=threads.r");
    expect(resolveNavigationTarget({ kind: "attachments" }, "/chat/d/c", "?pane=threads.r")).toBe(
      "/chat/d/c?pane=threads.r&pane=files",
    );
  });
});
