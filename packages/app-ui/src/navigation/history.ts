export type MainTab = "chat" | "activity" | "tasks" | "me";
export const MAIN_TABS: readonly MainTab[] = ["chat", "activity", "tasks", "me"];

/** Members, Computers and Settings are entered from Me, so a deep link to one of
 * them lands in the Me history. */
export function mainTab(url: string): MainTab | null {
  const section = url.split(/[/?]/)[1];
  if (section === "members" || section === "computers" || section === "settings") {
    return "me";
  }
  return MAIN_TABS.find((tab) => tab === section) ?? null;
}
/** Deep links have no known caller. Peel the semantic URL, never leave the app. */
export function parentUrl(url: string): string | null {
  const [path = "/chat", search = ""] = url.split("?");
  const query = new URLSearchParams(search);
  for (const key of [
    "settingsPage",
    "side",
    "agent",
    "human",
    "task",
    "attachment",
    "files",
    "settings",
    "thread",
    "threads",
    "computer",
    "settingsSection",
  ]) {
    if (!query.has(key)) continue;
    query.delete(key);
    query.delete("fullscreen");
    const rest = query.toString();
    return path + (rest ? `?${rest}` : "");
  }
  if (path !== "/chat" && path.startsWith("/chat/")) return "/chat";
  if (path !== "/activity" && path.startsWith("/activity/")) return "/activity";
  if (path !== "/members" && path.startsWith("/members/")) return "/members";
  if (["/members", "/computers", "/settings"].includes(path)) return "/me";
  return null;
}
