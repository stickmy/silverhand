import { useCallback, useLayoutEffect, useRef } from "react";

/**
 * A function whose identity never changes and that calls the latest `handler`.
 * For handlers passed to memoized children: a parent that rebuilds its
 * handlers every render would otherwise render every child again. Call it from
 * events and effects, not during render.
 */
export function useEventCallback<Args extends unknown[], Result>(
  handler: (...args: Args) => Result,
): (...args: Args) => Result {
  const latest = useRef(handler);
  useLayoutEffect(() => {
    latest.current = handler;
  });
  return useCallback((...args: Args) => latest.current(...args), []);
}
