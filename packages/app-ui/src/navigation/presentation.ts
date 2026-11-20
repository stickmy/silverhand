import { PANE_PARAMS } from "./panes";

/** Workspace selection remains a Chats root; panel targets are detail pages. */
export function showsTabBar(path: string, search = ""): boolean {
  const root =
    ["/chat", "/activity", "/tasks", "/me"].includes(path) || /^\/chat\/w\/[^/]+$/.test(path);
  const query = new URLSearchParams(search);
  return root && !PANE_PARAMS.some((key) => query.has(key));
}
