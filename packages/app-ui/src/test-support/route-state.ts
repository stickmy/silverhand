import { parseRouteState, type RouteState } from "@/router";

/**
 * A complete route state for tests: the parse of `url`, then `overrides`.
 * Deriving from the parser keeps fixtures whole when a route field is added.
 */
export function makeRouteState(url = "/chat", overrides: Partial<RouteState> = {}): RouteState {
  const query = url.indexOf("?");
  const pathname = query === -1 ? url : url.slice(0, query);
  const search = query === -1 ? "" : url.slice(query);
  return { ...parseRouteState(pathname, search), ...overrides };
}
