import "./task-status-badge.css";
import { useState, type ReactElement, type ReactNode } from "react";
import * as Popover from "@radix-ui/react-popover";
import { ArrowRightIcon, ChevronDownIcon } from "@/icons";
import type { TaskStatus } from "@silverhand/shared";
import { cn } from "@/lib/cn";
import { AppContextMenu, AppContextMenuItem } from "@/components/menus/app-context-menu";
import { t } from "@/lib/i18n";

/**
 * Small inline status chip for task cards (global section + per-conversation Tasks tab).
 *
 * Radius rides `--task-status-radius` — the whole task-status family (this
 * badge, the message task tag, and `TaskStatusPill`) shares ONE capsule form
 * so the same status reads identically wherever it appears (shiki 2026-08-09,
 * overriding the earlier chip/pill split from Cindy review
 * `ME-fJNO4XJRLpSHz`): warm themes give the capsule, poster hard-cuts to 0.
 *
 * Read-only by default. Pass `onChange` to make it click-to-open a tiny
 * popover that moves the task to any other status — the human-side
 * "manage any task, not just in_review" affordance (shiki
 * `x9rIbAIlyUBVneiu`). Status transitions are unrestricted (`POST
 * /api/tasks/:id/status` allows any→any), so we offer every status except
 * the current one — including `closed` (move-to-close) and, on a closed
 * task, the four open statuses (reopen).
 */
const statusLabels = (): Record<TaskStatus, string> => ({
  todo: t("tasks.badge.todo"),
  in_progress: t("tasks.badge.inProgress"),
  in_review: t("tasks.badge.inReview"),
  done: t("tasks.badge.done"),
  closed: t("tasks.badge.closed"),
});

/** Title-case labels for menu/sentence contexts ("Move to In review"). */
const moveLabels = (): Record<TaskStatus, string> => ({
  todo: t("tasks.todo"),
  in_progress: t("tasks.inProgress"),
  in_review: t("tasks.inReview"),
  done: t("tasks.done"),
  closed: t("tasks.closed"),
});

const ALL_STATUSES: TaskStatus[] = ["todo", "in_progress", "in_review", "done", "closed"];

/** Canonical lowercase status label (`todo` / `in progress` / …). Exported so
 *  other surfaces (e.g. the message task tag) render the SAME vocabulary
 *  instead of forking their own label map. */
export function taskStatusLabel(status: TaskStatus): string {
  return statusLabels()[status];
}

const CHIP =
  // `leading-none` (v6.5n, #task:917): the chip is by definition a
  // single-line compact element; without an explicit line-height it
  // inherits the root prose 1.5 and stretches its parent line-box,
  // misaligning with siblings (e.g. card row 1's `#nnn` stamp).
  // Scoped opt-out per Tailwind's intended pattern — prose stays at
  // the loose default, only this chip tightens.
  // `whitespace-nowrap` keeps "in progress" / "in review" cohesive when
  // the card's row 1 wraps under narrow column widths (TasksSection at low
  // viewport + per-conversation Tasks tab at MIN_WIDTH share 5 columns). The
  // chip drops to its own line beneath `#nnn` instead of breaking
  // mid-word.
  "rounded-[var(--task-status-radius)] whitespace-nowrap px-1 py-0.5 text-[length:var(--text-badge)] font-medium uppercase tracking-wide leading-none";

export function TaskStatusBadge({
  status,
  onChange,
  onOpenChange,
  className,
}: {
  status: TaskStatus;
  onChange?: (next: TaskStatus) => void;
  /** Fires when the click-to-change picker opens / closes. Lets a
   *  parent floating surface (e.g. the hover-card task popover) stay
   *  pinned open while the picker is active — otherwise the host
   *  HoverCard closes on pointer-leave and unmounts the picker. */
  onOpenChange?: (open: boolean) => void;
  className?: string;
}): JSX.Element {
  if (!onChange) {
    return (
      <span data-task-status={status} className={cn(CHIP, className)}>
        {statusLabels()[status]}
      </span>
    );
  }
  return (
    <StatusPicker current={status} onChange={onChange} onOpenChange={onOpenChange}>
      <button
        type="button"
        data-task-status={status}
        aria-label={t("tasks.changeStatusCurrent", { status: statusLabels()[status] })}
        title={t("tasks.changeStatus")}
        // Stop the click from reaching the card's stretched jump
        // overlay underneath.
        onClick={(e) => e.stopPropagation()}
        className={cn(
          CHIP,
          "cursor-pointer transition-opacity hover:opacity-80 focus-outline",
          className,
        )}
      >
        {statusLabels()[status]}
      </button>
    </StatusPicker>
  );
}

/**
 * The status control on the task board and in task detail: cube, label and a
 * chevron that opens the {@link StatusPicker}.
 */
export function TaskStatusButton({
  status,
  onChange,
  disabled = false,
  className,
}: {
  status: TaskStatus;
  onChange: (next: TaskStatus) => void;
  /** Another edit on the task is in flight; the control holds still. */
  disabled?: boolean;
  className?: string;
}): JSX.Element {
  return (
    <StatusPicker current={status} onChange={onChange}>
      <button
        type="button"
        disabled={disabled}
        aria-label={t("tasks.changeStatusCurrent", { status: statusLabels()[status] })}
        className={cn(
          "inline-flex h-6 cursor-pointer items-center gap-1.5 rounded-(--control-radius) border-[length:var(--control-border-width)] border-border-subtle px-2 text-text-secondary transition-colors hover:border-border hover:bg-bg-hover hover:text-text-primary focus-outline disabled:cursor-default",
          className,
        )}
      >
        <span data-task-cube data-task-ref-tone={status} aria-hidden />
        <span className="min-w-0 flex-1 truncate text-left">{statusLabels()[status]}</span>
        <ChevronDownIcon className="size-3 shrink-0 text-text-muted" aria-hidden />
      </button>
    </StatusPicker>
  );
}

/**
 * The click-to-change status picker, extracted as the trigger-wrapping
 * primitive so any element can become a "click to move task to another
 * status" affordance — `TaskStatusBadge` (the chip), and the message task
 * tag (the whole tag, #task:1185). `children` is the trigger (Radix
 * `asChild`), so the consumer owns the trigger's look.
 */
export function StatusPicker({
  current,
  onChange,
  onOpenChange,
  children,
}: {
  current: TaskStatus;
  onChange: (next: TaskStatus) => void;
  onOpenChange?: (open: boolean) => void;
  children: ReactNode;
}): JSX.Element {
  const [open, setOpen] = useState(false);
  // Single close path that ALSO notifies the parent. Radix only fires
  // its own `onOpenChange` for interactions it owns (escape / outside /
  // trigger toggle); a programmatic `setOpen(false)` from the item click
  // would close the popover via the controlled prop WITHOUT notifying,
  // leaving the host hover-card's `keepOpen` pin stuck true → popover
  // never dismisses (#task:923). Route every close through here.
  const close = () => {
    setOpen(false);
    onOpenChange?.(false);
  };
  return (
    <Popover.Root
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        onOpenChange?.(next);
      }}
    >
      <Popover.Trigger asChild>{children}</Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          align="start"
          sideOffset={4}
          onClick={(e) => e.stopPropagation()}
          // Don't bounce focus back to the trigger badge on close. When
          // this picker lives inside a hover-card popover, returning
          // focus to the (hover-card-owned) badge re-arms the card's
          // focus-open and fights the card's dismissal (#task:923 bug:
          // 点击 status 后卡片无法失焦消失).
          onCloseAutoFocus={(e) => e.preventDefault()}
          data-popover-frame="info"
          // z-[var(--z-popover-nested)] clears the hover-card task popover (z-[var(--z-popover)] in
          // HoverPopover) so the picker isn't trapped behind its own
          // host; stays under the modal layer (z-[var(--z-lightbox)]+). Harmless in the
          // task-card context where nothing floats this high. #task:923
          className="z-(--z-popover-nested) min-w-34 bg-bg-floating p-1 shadow-popover outline-none"
        >
          {ALL_STATUSES.filter((s) => s !== current).map((s) => (
            <button
              key={s}
              type="button"
              onClick={() => {
                onChange(s);
                close();
              }}
              className="flex w-full items-center rounded-(--floating-item-radius) px-2 py-1.5 text-left hover:bg-bg-hover"
            >
              <span data-task-status={s} className={CHIP}>
                {statusLabels()[s]}
              </span>
            </button>
          ))}
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

/**
 * Right-click context menu for task cards — direct "Move to {status}"
 * items (shiki `wCrBduGkOdT7EBTt`), now including `closed` and reopen
 * (any→any via `ALL_STATUSES`). Mirrors the StatusPicker but reachable
 * without first hitting the small badge, so the whole card surface
 * becomes a status-management affordance.
 *
 * `children` must be a single element (Radix ContextMenu.Trigger asChild)
 * — pass the card's root <div>.
 */
export function TaskCardContextMenu({
  status,
  onStatusChange,
  children,
}: {
  status: TaskStatus;
  onStatusChange: (next: TaskStatus) => void;
  children: ReactElement;
}): JSX.Element {
  return (
    <AppContextMenu trigger={children}>
      {ALL_STATUSES.filter((s) => s !== status).map((s) => (
        <AppContextMenuItem
          key={s}
          icon={<ArrowRightIcon className="size-3.5" />}
          onSelect={() => onStatusChange(s)}
        >
          {t("tasks.moveTo", { status: moveLabels()[s] })}
        </AppContextMenuItem>
      ))}
    </AppContextMenu>
  );
}
