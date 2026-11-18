import type { ReactNode } from "react";
import { motion, useIsPresent, useReducedMotion, type Variants } from "motion/react";

/** Exit length; a page entering alongside an exiting one waits this long. */
export const PAGE_SWAP_EXIT_S = 0.06;

/** Drill-in page swap inside a pane: a short fade with an 8px shift. The custom
 * value is the direction of travel (1 deeper, -1 back). The exit hides the page
 * without unmounting, so a page kept mounted underneath keeps its scroll. */
export function pageSwapVariants(reducedMotion: boolean, enterDelay = 0): Variants {
  return {
    enter: (direction: number) => ({
      opacity: 0,
      x: reducedMotion ? 0 : direction * 8,
    }),
    visible: {
      opacity: 1,
      x: 0,
      visibility: "visible",
      transition: {
        duration: reducedMotion ? 0 : 0.12,
        delay: reducedMotion ? 0 : enterDelay,
        ease: "easeOut",
      },
    },
    exit: (direction: number) => ({
      opacity: 0,
      x: reducedMotion ? 0 : direction * -8,
      transition: { duration: reducedMotion ? 0 : PAGE_SWAP_EXIT_S, ease: "easeIn" },
      transitionEnd: { visibility: "hidden" },
    }),
  };
}

/** A keyed page under `AnimatePresence`. An exiting page stays visible briefly,
 * but must no longer accept input. */
export function PageSwapTransition({
  children,
  direction,
  className,
  enterDelay,
}: {
  children: ReactNode;
  direction: number;
  className?: string;
  enterDelay?: number;
}): JSX.Element {
  const isPresent = useIsPresent();
  const reducedMotion = useReducedMotion();

  return (
    <motion.div
      ref={(node) => node?.toggleAttribute("inert", !isPresent)}
      aria-hidden={!isPresent || undefined}
      className={className}
      style={{ pointerEvents: isPresent ? undefined : "none" }}
      custom={direction}
      variants={pageSwapVariants(!!reducedMotion, enterDelay)}
      initial="enter"
      animate="visible"
      exit="exit"
    >
      {children}
    </motion.div>
  );
}
