import { type FocusEvent, useCallback, useState } from "react";
import { AddReactionIcon, BookmarkIcon, ThreadIcon } from "@/icons";
import { t } from "@/lib/i18n";
import { ReactionPicker } from "./reaction-picker";

/** The row's hover actions (react, thread, save): one framed toolbar on the
 *  row's top-right corner, straddling its top edge so it covers neither the
 *  body nor the row's height. The full-height track keeps it sticky inside
 *  the row: on a message taller than the viewport it stays pinned under the
 *  pane's top while the row is on screen. It is a descendant of the row, so
 *  moving onto the part outside the row keeps the row's hover.
 *
 *  Visibility is React state (row hover / keyboard focus-within / open
 *  picker): Tailwind's `focus-within:` ties with the base `opacity-0` and
 *  loses. An absent handler omits its button. */
export function MessageHoverToolbar({
  shown,
  onPickerOpenChange,
  onReact,
  onStartThread,
  saved,
  onSave,
}: {
  shown: boolean;
  onPickerOpenChange: (open: boolean) => void;
  onReact?: (emoji: string) => void;
  onStartThread?: () => void;
  saved: boolean;
  onSave?: () => void;
}): JSX.Element {
  // Focus-within as state: `onFocus`/`onBlur` bubble in React, so any focus
  // inside the toolbar counts, and it clears only when focus leaves it.
  const [focusWithin, setFocusWithin] = useState(false);

  const handleBlur = useCallback((event: FocusEvent<HTMLDivElement>) => {
    const next = event.relatedTarget as Node | null;
    if (!event.currentTarget.contains(next)) {
      setFocusWithin(false);
    }
  }, []);

  return (
    <div className="pointer-events-none absolute inset-y-0 right-2 z-(--z-raised)">
      <div className="sticky top-[calc(var(--pane-chrome-height,0px)+1rem)] h-0">
        <div
          data-popover-frame="toolbar"
          // Only keyboard focus keeps the toolbar up. A clicked button keeps
          // focus (and the picker hands it back to its trigger on close),
          // which would otherwise leave the toolbar shown after the pointer
          // has left the row.
          onFocus={(event) => {
            if (event.target.matches(":focus-visible")) {
              setFocusWithin(true);
            }
          }}
          onBlur={handleBlur}
          className={
            "absolute right-0 flex -translate-y-1/2 items-center gap-0.5 bg-bg-floating p-0.5 shadow-popover transition-opacity " +
            (shown || focusWithin ? "pointer-events-auto opacity-100" : "opacity-0")
          }
        >
          {onReact ? (
            <ReactionPicker
              align="end"
              onOpenChange={onPickerOpenChange}
              onPick={onReact}
              trigger={
                <button
                  type="button"
                  title={t("messages.react")}
                  aria-label={t("messages.react")}
                  className={TOOLBAR_BUTTON_CLASS + " text-text-muted hover:text-text-primary"}
                >
                  <AddReactionIcon className="size-3.5" />
                </button>
              }
            />
          ) : null}
          {onStartThread ? (
            <ActionButton
              icon={<ThreadIcon className="size-3.5" />}
              label={t("messages.thread2")}
              onClick={onStartThread}
            />
          ) : null}
          {onSave ? (
            <ActionButton
              icon={<BookmarkIcon className={"size-3.5 " + (saved ? "fill-current" : "")} />}
              label={saved ? t("messages.saved") : t("common.save")}
              active={saved}
              onClick={onSave}
            />
          ) : null}
        </div>
      </div>
    </div>
  );
}

// Concentric with the toolbar frame: its corner minus the frame's `p-0.5`.
const TOOLBAR_BUTTON_CLASS =
  "flex size-6 items-center justify-center rounded-[max(0px,calc(var(--floating-radius,var(--radius-md))-2px))] transition-colors hover:bg-(--bg-hover) focus-outline";

function ActionButton({
  icon,
  label,
  onClick,
  active = false,
}: {
  icon: React.ReactNode;
  label: string;
  onClick?: () => void;
  active?: boolean;
}): JSX.Element {
  return (
    <button
      type="button"
      onClick={onClick}
      title={label}
      aria-label={label}
      className={
        TOOLBAR_BUTTON_CLASS +
        " " +
        (active ? "text-accent" : "text-text-muted hover:text-text-primary")
      }
    >
      {icon}
    </button>
  );
}
