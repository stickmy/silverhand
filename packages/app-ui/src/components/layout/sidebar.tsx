import type { ReactNode } from "react";
import { cn } from "@/lib/cn";
import { paneChrome } from "./pane-chrome";

interface SidebarProps {
  /** `data-sidebar` value. The per-theme sidebar painting (carrier ground,
   *  ink ladder, row sticker states) keys on this ancestor, so every section's
   *  list gets it for free — the reason these are real `aside[data-sidebar]`
   *  islands and not look-alikes. */
  name: string;
  /** aria-label for the sidebar landmark, when the section wants one. */
  label?: string;
  /** Sidebar column width as a CSS value (`var(--sidebar-width)` /
   *  `var(--activity-list-width)`). Ignored when `fill` is set. */
  width?: string;
  /** The sidebar fills the whole content area (flex-1) with no page or
   *  splitter beside it — Activity's no-channel state, where the list IS the
   *  section. Off by default: a fixed-width column with the page to its right. */
  fill?: boolean;
  /** Content inside the sidebar `aside` — a `<nav>`, a list. The aside owns the
   *  pane chrome + `overflow: hidden`; scrolling lives on the consumer's inner
   *  `<nav>`. */
  sidebar: ReactNode;
  /** Column divider between sidebar and page. Only Activity has one today
   *  (its list resizes against the conversation); the fixed-width sections
   *  read a width var resized elsewhere, so they pass nothing. */
  splitter?: ReactNode;
  /** The page / detail column(s) to the right of the sidebar. */
  children?: ReactNode;
}

export function Sidebar({
  name,
  label,
  width,
  fill = false,
  sidebar,
  splitter,
  children,
}: SidebarProps): JSX.Element {
  return (
    <>
      <aside
        data-panel="sidebar"
        data-sidebar={name}
        aria-label={label}
        className={cn(
          "flex h-full flex-col px-(--sidebar-inset) py-1.5",
          fill ? "min-w-0 flex-1" : "shrink-0",
        )}
        style={{ ...paneChrome, overflow: "hidden", width: fill ? undefined : width }}
      >
        {sidebar}
      </aside>
      {splitter}
      {children}
    </>
  );
}
