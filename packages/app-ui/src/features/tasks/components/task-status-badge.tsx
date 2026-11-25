import "./task-status-badge.css";
import { useState, type ReactElement, type ReactNode } from "react";
import * as Popover from "@radix-ui/react-popover";
import { ArrowRightIcon, ChevronDownIcon } from "@/icons";
import type { TaskStatus } from "@silverhand/shared";
import { cn } from "@/lib/cn";
import { AppContextMenu, AppContextMenuItem } from "@/components/menus/app-context-menu";
import { t } from "@/lib/i18n";

/**
 * Task status reads the same everywhere: a status cube and its label. The
 * cube's hue comes from the shared `--task-ref-tone` map; the label stays
 * neutral text.
 *
 * Status transitions are unrestricted (`POST /api/tasks/:id/status` allows
 * any→any), so the picker offers every status except the current one,
 * including `closed` and, on a closed task, reopening.
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
 *  other surfaces render the SAME vocabulary instead of forking a label map. */
export function taskStatusLabel(status: TaskStatus): string {
  return statusLabels()[status];
}

/** Cube + label, the one way a status is drawn. */
function StatusMark({ status }: { status: TaskStatus }): JSX.Element {
  return (
    <>
      <span data-task-cube data-task-ref-tone={status} aria-hidden />
      <span className="min-w-0 flex-1 truncate text-left">{statusLabels()[status]}</span>
    </>
  );
}

const MARK = "inline-flex items-center gap-1.5 whitespace-nowrap text-text-secondary";

/**
 * Inline status (message task hover card). Read-only by default; pass
 * `onChange` to open the {@link StatusPicker} on click.
 */
export function TaskStatusBadge({
  status,
  onChange,
  onOpenChange,
  className,
}: {
  status: TaskStatus;
  onChange?: (next: TaskStatus) => void;
  /** Fires when the picker opens / closes, so a host floating surface (the
   *  hover-card task popover) stays pinned open while it is active. */
  onOpenChange?: (open: boolean) => void;
  className?: string;
}): JSX.Element {
  if (!onChange) {
    return (
      <span className={cn(MARK, className)}>
        <StatusMark status={status} />
      </span>
    );
  }
  return (
    <StatusPicker current={status} onChange={onChange} onOpenChange={onOpenChange}>
      <button
        type="button"
        aria-label={t("tasks.changeStatusCurrent", { status: statusLabels()[status] })}
        title={t("tasks.changeStatus")}
        // Keep the click off the card's jump overlay underneath.
        onClick={(e) => e.stopPropagation()}
        className={cn(
          MARK,
          "cursor-pointer rounded-(--control-radius) transition-colors hover:text-text-primary focus-outline",
          className,
        )}
      >
        <StatusMark status={status} />
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
        <StatusMark status={status} />
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
              className="flex w-full items-center gap-1.5 rounded-(--floating-item-radius) px-2 py-1.5 text-left text-xs text-text-secondary hover:bg-bg-hover hover:text-text-primary"
            >
              <StatusMark status={s} />
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
