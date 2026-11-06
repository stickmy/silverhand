import { useNativeNavigation } from "./native";
import { ViewMemoryProvider } from "./view-memory";
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type ReactNode,
} from "react";
import { Router, createPath, type Location, type To } from "react-router-dom";
import { connectAddress } from "./address";
import type { NavigationLaunch } from "./launch";
import { NavigationStore } from "./store";
import type { MainTab } from "./history";

export type Driver = {
  session: string;
  go: (url: string, options?: { replace?: boolean; pane?: boolean }) => void;
  /** Whether Back left the page; a leave guard can hold it. */
  back: () => boolean;
  selectTab: (tab: MainTab) => void;
  openSection: (tab: MainTab, root?: string) => void;
  tab: MainTab;
  canBack: boolean;
  /** The page on screen, the one Back returns to, and every page kept
   * mounted: each Tab's stack. See `PageStack`. */
  page: string;
  backPage: string | null;
  keptPages: readonly string[];
  registerLeaveGuard: (guard: () => boolean) => () => void;
};
export const NavigationContext = createContext<Driver | null>(null);

/** The driver and location in effect, read when an action runs. Its identity
 * never changes, so a consumer that only acts on navigation (a message row's
 * avatar, a mention) does not render again on every navigation. */
export type NavigationReader = () => { driver: Driver; location: Location };
export const NavigationReaderContext = createContext<NavigationReader | null>(null);

export function useNavigationReaderValue(driver: Driver, location: Location): NavigationReader {
  const current = useRef({ driver, location });
  current.current = { driver, location };
  return useCallback(() => current.current, []);
}

export function useNavigationReader(): NavigationReader {
  const reader = useContext(NavigationReaderContext);
  if (!reader) throw new Error("NavigationProvider required");
  return reader;
}

export function NavigationProvider({
  children,
  launch,
  dark = false,
}: {
  children: ReactNode;
  launch: NavigationLaunch;
  /** The theme is dark; native chrome follows it. */
  dark?: boolean;
}) {
  const [store] = useState(() => new NavigationStore(launch.initialUrl));
  const snapshot = useSyncExternalStore(store.subscribe, store.getSnapshot);
  const [guards] = useState(() => new Set<() => boolean>());
  const allowed = useCallback(() => [...guards].every((guard) => guard()), [guards]);
  const run = useNativeNavigation(store, allowed, dark);
  const registerLeaveGuard = useCallback(
    (guard: () => boolean) => {
      guards.add(guard);
      return () => {
        guards.delete(guard);
      };
    },
    [guards],
  );
  const value = useMemo<Driver>(
    () => ({
      session: store.session,
      go: (url, options) => {
        run(() => store.go(url, { replace: options?.replace && !options.pane }));
      },
      back: () => run(store.back),
      selectTab: (tab) => {
        run(() => store.selectTab(tab));
      },
      openSection: (tab, root) => {
        run(() => store.openSection(tab, root));
      },
      tab: snapshot.tab,
      canBack: snapshot.canBack,
      page: snapshot.page,
      backPage: snapshot.back?.page ?? null,
      keptPages: snapshot.keptPages,
      registerLeaveGuard,
    }),
    [store, run, registerLeaveGuard, snapshot],
  );

  useEffect(() => {
    launch.consume();
    store.activate();
    const disconnect = connectAddress(
      store,
      {
        read: () => window.location.hash.slice(1),
        replace: (url) => window.history.replaceState(null, "", `#${url}`),
        subscribe: (listener) => {
          window.addEventListener("hashchange", listener);
          return () => window.removeEventListener("hashchange", listener);
        },
      },
      allowed,
    );
    return () => {
      disconnect();
      store.dispose();
      window.history.replaceState(null, "", "#/chat");
    };
  }, [store, allowed, launch]);

  const navigator = useMemo(
    () => ({
      createHref: (to: To) => (typeof to === "string" ? to : createPath(to)),
      go: (delta: number) => {
        run(() => store.traverse(delta));
      },
      push: (to: To) => value.go(typeof to === "string" ? to : createPath(to)),
      replace: (to: To) =>
        value.go(typeof to === "string" ? to : createPath(to), { replace: true }),
    }),
    [value, run, store],
  );
  const reader = useNavigationReaderValue(value, snapshot.location);
  return (
    <NavigationContext.Provider value={value}>
      <NavigationReaderContext.Provider value={reader}>
        <Router location={snapshot.location} navigationType={snapshot.action} navigator={navigator}>
          <ViewMemoryProvider>{children}</ViewMemoryProvider>
        </Router>
      </NavigationReaderContext.Provider>
    </NavigationContext.Provider>
  );
}

export function useNavigationDriver(): Driver {
  const context = useContext(NavigationContext);
  if (!context) throw new Error("NavigationProvider required");
  return context;
}
