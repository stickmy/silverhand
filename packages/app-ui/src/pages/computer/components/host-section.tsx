import { ChevronRightIcon } from "@/icons";
import { useState, type JSX, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { type HostStatusTone } from "../model/host-status-presentation";

export const TONE_BANNER_CLASS: Record<HostStatusTone, string> = {
  accent: "border-accent text-accent",
  success: "border-success text-success",
  warning: "border-warning text-warning",
  error: "border-error text-error",
  muted: "border-border-subtle text-text-secondary",
};

/* ─── HostSection (local) ───────────────────────────────────────── */

/**
 * Section primitive for this surface. shiki #task:1360 redesign: the
 * previous version wrapped each section's body in a solid white card
 * (`data-panel-card="profile-section"` border + `bg-bg-floating` tile +
 * `p-4` + hover border, amplified to a 2px frame in poster). Multiple
 * equal-weight boxes read as "obvious boxes" and diluted the hierarchy —
 * the loudness budget went to containers instead of content.
 *
 * Now a section is just a **title + flush-left content**, separated from
 * the previous section by a single hairline (`border-t border-border-subtle`,
 * skipped on the first child). No box, no inner padding inset, so the title
 * and its content share one left edge and the whole pane reads as one
 * continuous document rather than an admin dashboard. Controls inside a
 * section (proxy input, runtime tiles, agent row cards) keep their own
 * borders — they are controls, not section boxes.
 *
 * The title stays a quiet uppercase caps label (the box was the noise, not
 * the title). Vertical rhythm is `py-6` per section; the hairline lands in
 * the gap between two sections.
 */
export function HostSection({
  title,
  action,
  collapsible = false,
  defaultOpen = true,
  children,
}: {
  /** ReactNode so the caller can compose inline trailing content
   *  (e.g. an agent count number after the title text). Counts stay on
   *  the title rather than at the far edge (shiki dogfood
   *  `6QIZeGVS8u8yJ137`). */
  title: ReactNode;
  /** A control at the header's right edge, for the one action that adds to
   *  this section (not for counts, which stay on the title). */
  action?: ReactNode;
  /** When set, the header becomes a disclosure toggle and the body
   *  mounts only while open. Used by the network-proxy section, which
   *  shiki wants collapsed by default so the page leads with identity +
   *  agents, not a rarely-touched form. */
  collapsible?: boolean;
  defaultOpen?: boolean;
  children: ReactNode;
}): JSX.Element {
  const [open, setOpen] = useState(defaultOpen);
  const heading = (
    <h3 className="text-(length:--text-meta) font-semibold uppercase tracking-[0.14em] text-text-muted">
      {title}
    </h3>
  );
  return (
    <section className="flex flex-col border-t border-border-subtle py-6 first:border-t-0">
      {collapsible ? (
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          aria-expanded={open}
          className="group flex w-full items-center gap-2 text-left"
        >
          <ChevronRightIcon
            className={cn(
              "size-3.5 shrink-0 text-text-muted transition-transform",
              open && "rotate-90",
            )}
            aria-hidden
          />
          {heading}
        </button>
      ) : (
        <header className="mb-3.5 flex min-h-6 items-center justify-between gap-3">
          {heading}
          {action}
        </header>
      )}
      {(!collapsible || open) && <div className={cn(collapsible && "mt-3.5")}>{children}</div>}
    </section>
  );
}

/* ─── shared bits ───────────────────────────────────────────────── */

export function Banner({
  tone,
  children,
}: {
  tone: HostStatusTone;
  children: ReactNode;
}): JSX.Element {
  return (
    <div
      className={cn("border bg-bg-floating px-3 py-2 text-sm", TONE_BANNER_CLASS[tone])}
      role="status"
    >
      {children}
    </div>
  );
}
