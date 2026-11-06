import {
  createContext,
  memo,
  useCallback,
  useContext,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import {
  UNSAFE_LocationContext as LocationContext,
  useLocation,
  useNavigationType,
  type Location,
  type NavigationType,
} from "react-router-dom";
import { modalScope } from "@/lib/modal-scope";
import { useCompactLayout } from "./layout";
import { usePageBarSwipe } from "./page-bar";
import { showsTabBar } from "./presentation";
import {
  NavigationContext,
  NavigationReaderContext,
  useNavigationDriver,
  useNavigationReaderValue,
  type Driver,
} from "./provider";
import { useLocale } from "@/lib/i18n";

const PageLive = createContext(true);

/** Whether this subtree is the page on screen. A kept page stays mounted, so
 * its effects keep running: `PageStack` cuts off only its navigation, leave
 * guards and input (`inert`). An effect with an outside consequence — a read
 * mark, a bar claim, a window or document listener — must stop while this is
 * false. */
export function usePageLive(): boolean {
  return useContext(PageLive);
}

type Kept = { location: Location; element: ReactNode };
type Transition = { kind: "push" | "back"; top: string; under: string; from: number };

const DURATION = 380;
const EASING = "cubic-bezier(0.32, 0.72, 0, 1)";
/** How far the page beneath sits to the left, as a share of its width. */
const PARALLAX = 0.3;
/** A touch starting this close to the left edge can begin Back. */
const EDGE = 24;

/**
 * Compact pages are pushed: the pages beneath stay mounted and hidden, so Back
 * reveals each as it was, with its scroll, images and state. Every Tab keeps
 * its stack the same way. History is the only state: a push slides the new
 * page in over the old, Back slides it off, and an edge swipe drives Back with
 * the finger. Animation moves pages by transform only, so no page lays out
 * again while it moves.
 *
 * A page off screen is inert and receives a driver that cannot navigate or
 * hold a leave guard. App-level logic runs once, outside, for the page on
 * screen; a kept page keeps the props it last had on screen.
 *
 * Regular width shows one page and keeps its instance across pushes. Moving
 * between widths keeps the instance on screen.
 */
export function PageStack({ children }: { children: ReactNode }) {
  const compact = useCompactLayout();
  const driver = useNavigationDriver();
  const { page, backPage, keptPages } = driver;
  const location = useLocation();
  const navigationType = useNavigationType();

  const kept = useRef(new Map<string, Kept>());
  const frames = useRef(new Map<string, HTMLDivElement>());
  const keys = useRef({ compact, regular: page, alias: new Map<string, string>() });
  const [shown, setShown] = useState({ page, backPage });
  const [transition, setTransition] = useState<Transition | null>(null);
  const [dragging, setDragging] = useState(false);
  const progress = useRef(0);
  const barSwipe = usePageBarSwipe();

  // The instance on screen keeps its React key when the width class changes.
  if (compact !== keys.current.compact) {
    if (compact) {
      keys.current.alias = new Map([[page, keys.current.regular]]);
    } else {
      keys.current.regular = keys.current.alias.get(page) ?? page;
      kept.current.clear();
    }
    keys.current.compact = compact;
  }

  // A push lands on a page whose parent was on screen; a back on the parent
  // of the page that was. Anything else (a Tab, a replace) shows at once.
  let moving = transition;
  if (page !== shown.page) {
    const kind = backPage === shown.page ? "push" : shown.backPage === page ? "back" : null;
    moving =
      compact && kind && kept.current.has(shown.page)
        ? {
            kind,
            top: kind === "push" ? page : shown.page,
            under: kind === "push" ? shown.page : page,
            from: kind === "back" ? progress.current : 0,
          }
        : null;
    progress.current = 0;
    setShown({ page, backPage });
    setTransition(moving);
  } else if (backPage !== shown.backPage) {
    setShown({ page, backPage });
  }

  const pages = kept.current;
  if (compact) {
    pages.set(page, { location, element: children });
    for (const id of unmountedPages(pages.keys(), { page, keptPages, leaving: moving?.top })) {
      pages.delete(id);
      keys.current.alias.delete(id);
    }
  }
  const entries: [string, Kept][] = compact
    ? [...pages]
    : [[keys.current.regular, { location, element: children }]];

  useLayoutEffect(() => {
    if (!transition) {
      return;
    }

    const top = frames.current.get(transition.top);
    const under = frames.current.get(transition.under);
    if (!top || !under || reducedMotion()) {
      setTransition(null);
      return;
    }

    const enter = transition.kind === "push";
    const from = enter ? 1 : transition.from;
    const to = enter ? 0 : 1;
    const duration = DURATION * Math.max(0.3, Math.abs(to - from));
    // The last frame holds until the commit that ends the transition, which
    // hides the page that left in the same paint.
    const options = { duration, easing: EASING, fill: "forwards" as const };
    const moves = [
      top.animate([{ transform: slide(from) }, { transform: slide(to) }], options),
      under.animate([{ transform: beneath(from) }, { transform: beneath(to) }], options),
    ];
    void moves[0]!.finished.then(
      () => setTransition(null),
      () => {},
    );
    return () => {
      for (const move of moves) move.cancel();
    };
  }, [transition]);

  // Back by edge swipe: the page follows the finger over the page beneath,
  // and Back runs once the finger lets go far or fast enough.
  const beginSwipe = useCallback(
    (event: React.TouchEvent) => {
      const touch = event.touches[0];
      const top = frames.current.get(page);
      const under = backPage ? frames.current.get(backPage) : undefined;
      if (
        !compact ||
        !touch ||
        event.touches.length > 1 ||
        touch.clientX > EDGE ||
        transition ||
        modalScope.blocked ||
        !top ||
        !under
      ) {
        return;
      }

      const width = top.offsetWidth;
      const startX = touch.clientX;
      const startY = touch.clientY;
      let tracking: boolean | null = null;
      let last = { x: startX, t: performance.now() };
      let velocity = 0;

      const place = (share: number) => {
        progress.current = share;
        barSwipe?.swipeProgress(share);
        top.style.transform = slide(share);
        under.style.transform = beneath(share);
      };
      const release = () => {
        top.style.transform = "";
        under.style.transform = "";
        setDragging(false);
      };

      const move = (e: TouchEvent) => {
        const t = e.touches[0];
        if (!t) return;
        const dx = t.clientX - startX;
        if (tracking === null) {
          if (Math.abs(dx) < 8 && Math.abs(t.clientY - startY) < 8) return;
          tracking = dx > Math.abs(t.clientY - startY);
          if (!tracking) {
            end();
            return;
          }
          setDragging(true);
        }
        e.preventDefault();
        const now = performance.now();
        velocity = (t.clientX - last.x) / Math.max(1, now - last.t);
        last = { x: t.clientX, t: now };
        place(Math.min(1, Math.max(0, dx / width)));
      };
      const end = () => {
        window.removeEventListener("touchmove", move);
        window.removeEventListener("touchend", end);
        window.removeEventListener("touchcancel", end);
        if (!tracking) return;
        const share = progress.current;
        // A finger that stopped before letting go is not flicking.
        const flick = performance.now() - last.t < 100 && velocity > 0.3;
        const commit = share > 0.5 || (flick && share > 0.05);
        release();
        if (commit && driver.back()) {
          barSwipe?.swipeEnd(page);
          return;
        }
        progress.current = 0;
        barSwipe?.swipeEnd(null);
        const options = { duration: DURATION * Math.max(0.3, share), easing: EASING };
        top.animate([{ transform: slide(share) }, { transform: slide(0) }], options);
        under.animate([{ transform: beneath(share) }, { transform: beneath(0) }], options);
      };
      window.addEventListener("touchmove", move, { passive: false });
      window.addEventListener("touchend", end);
      window.addEventListener("touchcancel", end);
    },
    [compact, page, backPage, transition, driver, barSwipe],
  );

  // One keyed list in both widths, so the page on screen keeps its instance
  // when the width class changes. Map order is creation order, so a kept page
  // never moves in the DOM; a moved node would lose its scroll position.
  const shownPair = moving ?? (dragging && backPage ? { top: page, under: backPage } : null);
  return (
    <div
      data-page-stack=""
      className={compact ? "relative flex min-w-0 flex-1 overflow-hidden" : "contents"}
      onTouchStart={compact ? beginSwipe : undefined}
    >
      {entries.map(([id, entry]) => {
        const live = !compact || id === page;
        return (
          <PageFrame
            key={compact ? (keys.current.alias.get(id) ?? id) : id}
            id={id}
            frames={frames.current}
            location={live ? location : entry.location}
            navigationType={live ? navigationType : ("POP" as NavigationType)}
            live={live}
            shown={live || id === shownPair?.top || id === shownPair?.under}
            raised={id === shownPair?.top}
            stacked={compact}
          >
            {entry.element}
          </PageFrame>
        );
      })}
    </div>
  );
}

/** The mounted pages to unmount: every one but the page on screen, the
 * pages the store keeps (each Tab's stack), and a page still sliding off. */
export function unmountedPages(
  mounted: Iterable<string>,
  { page, keptPages, leaving }: { page: string; keptPages: readonly string[]; leaving?: string },
): string[] {
  return [...mounted].filter((id) => id !== page && id !== leaving && !keptPages.includes(id));
}

const slide = (share: number) => `translateX(${share * 100}%)`;
const beneath = (share: number) => `translateX(${(share - 1) * PARALLAX * 100}%)`;
const reducedMotion = () =>
  typeof window.matchMedia === "function" &&
  window.matchMedia("(prefers-reduced-motion: reduce)").matches;

const PageFrame = memo(function PageFrame({
  id,
  frames,
  location,
  navigationType,
  live,
  shown,
  raised,
  stacked,
  children,
}: {
  id: string;
  frames: Map<string, HTMLDivElement>;
  location: Location;
  navigationType: NavigationType;
  live: boolean;
  shown: boolean;
  raised: boolean;
  stacked: boolean;
  children: ReactNode;
}) {
  useLocale();
  const parent = useNavigationDriver();
  const node = useRef<HTMLDivElement | null>(null);
  const value = useMemo(() => ({ location, navigationType }), [location, navigationType]);
  const driver = useMemo<Driver>(() => (live ? parent : inert(parent)), [live, parent]);
  const reader = useNavigationReaderValue(driver, location);
  const ref = useCallback(
    (next: HTMLDivElement | null) => {
      node.current = next;
      if (next) frames.set(id, next);
      else frames.delete(id);
    },
    [frames, id],
  );

  useLayoutEffect(() => {
    node.current?.toggleAttribute("inert", !live);
  }, [live]);

  // Stacked pages fill the same box, so a kept page keeps its measured size
  // and scroll offset while hidden; `display: none` would drop both. A single
  // page adds no box, so the sheet's own row layout applies.
  return (
    <div
      ref={ref}
      data-page=""
      aria-hidden={live ? undefined : true}
      data-tab-bar={showsTabBar(location.pathname, location.search) ? "" : undefined}
      className={stacked ? "absolute inset-0 flex min-w-0 bg-bg-base" : "contents"}
      style={
        stacked
          ? {
              gap: "var(--pane-gap)",
              visibility: shown ? undefined : "hidden",
              zIndex: raised ? 1 : undefined,
            }
          : undefined
      }
    >
      <NavigationContext.Provider value={driver}>
        <NavigationReaderContext.Provider value={reader}>
          <PageLive.Provider value={live}>
            <LocationContext.Provider value={value}>{children}</LocationContext.Provider>
          </PageLive.Provider>
        </NavigationReaderContext.Provider>
      </NavigationContext.Provider>
    </div>
  );
});

/** A page off screen reads navigation state but cannot change it, and its
 * leave guards never hold a navigation back. */
function inert(driver: Driver): Driver {
  const none = () => false;
  return {
    ...driver,
    go: () => {},
    back: none,
    selectTab: () => {},
    openSection: () => {},
    registerLeaveGuard: () => () => {},
  };
}
