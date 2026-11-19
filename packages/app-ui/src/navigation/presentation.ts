import { SECONDARY_PANE_ORDER } from "./secondary-panes";

/** Workspace selection remains a Chats root; panel targets are detail pages. */
export function showsTabBar(path: string, search = ""): boolean {
  const root =
    ["/chat", "/activity", "/tasks", "/me"].includes(path) || /^\/chat\/w\/[^/]+$/.test(path);
  const query = new URLSearchParams(search);
  return root && !SECONDARY_PANE_ORDER.some((key) => query.has(key));
}
