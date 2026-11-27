import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { CheckIcon } from "@/icons";
import { cn } from "@/lib/cn";
import { t } from "@/lib/i18n";

const MAX_OTHER_TEXTAREA_HEIGHT = 180;

function resizeOtherTextarea(el: HTMLTextAreaElement | null): void {
  if (!el) {
    return;
  }
  el.style.height = "auto";
  const borderY = el.offsetHeight - el.clientHeight;
  const contentHeight = el.scrollHeight + borderY;
  const nextHeight = Math.min(contentHeight, MAX_OTHER_TEXTAREA_HEIGHT);
  el.style.height = `${nextHeight}px`;
  el.style.overflowY = contentHeight > MAX_OTHER_TEXTAREA_HEIGHT ? "auto" : "hidden";
}

export interface MessageOptionItem {
  id: string;
  label: string;
}

export interface MessageOptionsValue {
  mode: "single" | "multi";
  items: MessageOptionItem[];
}

export interface MessageOptionsAnswer {
  optionIds: string[];
  other: string | null;
  /** Display name of whoever gave the current answer. */
  answererName: string;
  /** The viewer gave the current answer, so they may withdraw it. */
  mine: boolean;
}

export interface MessageOptionsPick {
  optionIds: string[];
  other: string | null;
}

/**
 * The options a message asks its readers to choose from. Single choice answers
 * on one click; multi choice answers on Submit. The current answer stays
 * changeable: picking again replaces it.
 */
export function MessageOptions({
  options,
  answer,
  canAnswer,
  busy = false,
  onAnswer,
  onWithdraw,
}: {
  options: MessageOptionsValue;
  answer: MessageOptionsAnswer | null;
  /** False for the question's author and for readers who cannot post. */
  canAnswer: boolean;
  busy?: boolean;
  onAnswer: (pick: MessageOptionsPick) => void;
  onWithdraw: () => void;
}): JSX.Element {
  const multi = options.mode === "multi";
  const answeredIds = answer?.optionIds ?? NO_IDS;

  const [ticked, setTicked] = useState<ReadonlySet<string> | null>(null);
  const [otherOpen, setOtherOpen] = useState(false);
  const [otherText, setOtherText] = useState("");
  const [hint, setHint] = useState(false);
  const otherRef = useRef<HTMLTextAreaElement | null>(null);

  useLayoutEffect(() => {
    if (otherOpen) {
      resizeOtherTextarea(otherRef.current);
    }
  }, [otherOpen, otherText]);

  // Multi choice edits a draft seeded from the current answer; single choice
  // has no draft, a click is the answer.
  const draft = ticked ?? new Set(answeredIds);
  const interactive = canAnswer && !busy;

  const submit = (pick: MessageOptionsPick): void => {
    onAnswer(pick);
    setTicked(null);
    setOtherOpen(false);
    setOtherText("");
    setHint(false);
  };

  const pickItem = (id: string): void => {
    if (!interactive) {
      return;
    }
    if (!multi) {
      submit({ optionIds: [id], other: null });
      return;
    }
    const next = new Set(draft);
    if (next.has(id)) {
      next.delete(id);
    } else {
      next.add(id);
    }
    setTicked(next);
    setHint(false);
  };

  const submitMulti = (): void => {
    const other = otherOpen ? otherText.trim() : "";
    if (draft.size === 0 && other.length === 0) {
      setHint(true);
      return;
    }
    submit({ optionIds: [...draft], other: other.length > 0 ? other : null });
  };

  const submitOther = (): void => {
    const other = otherText.trim();
    if (other.length === 0) {
      return;
    }
    submit({ optionIds: multi ? [...draft] : [], other });
  };

  const isOn = (id: string): boolean => (multi ? draft.has(id) : answeredIds.includes(id));
  const otherOn = otherOpen || (answer?.other != null && ticked === null);
  const editing = ticked !== null || otherOpen;

  return (
    <div
      data-message-options
      className={cn(
        "mt-2 flex max-w-full flex-col gap-0.5 rounded-(--control-radius) border-(length:--control-border-width) border-border bg-(--bg-base) p-1",
        otherOpen ? "w-full" : "w-fit min-w-[min(260px,100%)]",
      )}
    >
      {options.items.map((item) => (
        <OptionRow
          key={item.id}
          multi={multi}
          on={isOn(item.id)}
          interactive={interactive}
          answererName={
            ticked === null && answeredIds.includes(item.id) ? answer?.answererName : undefined
          }
          onPick={() => pickItem(item.id)}
        >
          {item.label}
        </OptionRow>
      ))}
      <OptionRow
        multi={multi}
        on={otherOn}
        interactive={interactive}
        answererName={
          ticked === null && !otherOpen && answer?.other != null ? answer.answererName : undefined
        }
        onPick={() => {
          if (interactive) {
            setOtherOpen((open) => !open);
          }
        }}
      >
        {answer?.other != null && !otherOpen
          ? t("messages.optionOtherAnswer", { text: answer.other })
          : t("messages.optionOtherEllipsis")}
      </OptionRow>
      {otherOpen ? (
        <form
          className="flex flex-col gap-1.5 px-1 pt-0.5 pb-1"
          onSubmit={(e) => {
            e.preventDefault();
            if (multi) {
              submitMulti();
            } else {
              submitOther();
            }
          }}
        >
          <textarea
            ref={otherRef}
            autoFocus
            rows={2}
            value={otherText}
            onChange={(e) => setOtherText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && !e.nativeEvent.isComposing) {
                e.preventDefault();
                if (multi) {
                  submitMulti();
                } else {
                  submitOther();
                }
              }
            }}
            placeholder={t("messages.optionOtherPlaceholder")}
            className="block w-full resize-none rounded-(--control-radius) border-(length:--control-border-width) border-border bg-transparent px-2.5 py-1.5 text-sm leading-relaxed text-text-primary placeholder:text-text-muted outline-none focus:border-(--border-control-hover,var(--text-muted))"
          />
          {multi ? null : (
            <div className="flex justify-end">
              <button
                type="submit"
                disabled={otherText.trim().length === 0}
                className="rounded-(--control-radius) bg-(--text-primary) px-3 py-1 text-xs font-medium text-(--text-inverse) disabled:opacity-40"
              >
                {t("messages.optionSubmit")}
              </button>
            </div>
          )}
        </form>
      ) : null}
      {interactive && answer?.mine && !editing ? (
        <div className="mt-0.5 flex justify-end border-t border-border-subtle px-2 pt-1.5 pb-1">
          <button
            type="button"
            onClick={onWithdraw}
            className="text-(length:--text-meta) text-text-muted transition-colors hover:text-text-primary"
          >
            {t("messages.optionWithdraw")}
          </button>
        </div>
      ) : null}
      {multi && interactive && (answer === null || editing) ? (
        <div className="mt-0.5 flex items-center justify-end gap-2 border-t border-border-subtle px-1 pt-1.5 pb-0.5">
          {hint ? (
            <span className="text-(length:--text-meta) text-text-muted">
              {t("messages.optionPickFirst")}
            </span>
          ) : null}
          <button
            type="button"
            onClick={submitMulti}
            className="rounded-(--control-radius) bg-(--text-primary) px-3 py-1 text-xs font-medium text-(--text-inverse)"
          >
            {t("messages.optionSubmit")}
          </button>
        </div>
      ) : null}
    </div>
  );
}

const NO_IDS: string[] = [];

function OptionRow({
  multi,
  on,
  interactive,
  answererName,
  onPick,
  children,
}: {
  multi: boolean;
  on: boolean;
  interactive: boolean;
  answererName?: string;
  onPick: () => void;
  children: ReactNode;
}): JSX.Element {
  return (
    <button
      type="button"
      data-message-option-row
      role={multi ? "checkbox" : "radio"}
      aria-checked={on}
      disabled={!interactive}
      onClick={onPick}
      className={cn(
        "flex w-full items-start gap-2 rounded-[max(0px,calc(var(--control-radius)-2px))] px-2 py-1.5 text-left text-sm text-text-primary transition-colors focus-outline disabled:cursor-default",
        on ? "bg-(--chip-bg)" : "enabled:hover:bg-bg-hover",
      )}
    >
      <OptionMark multi={multi} on={on} />
      <span className="min-w-0 flex-1 whitespace-pre-wrap break-words">{children}</span>
      {answererName ? (
        <span
          data-message-option-answerer
          className="mt-px shrink-0 text-(length:--text-meta) text-text-muted"
        >
          {answererName}
        </span>
      ) : null}
    </button>
  );
}

function OptionMark({ multi, on }: { multi: boolean; on: boolean }): JSX.Element {
  if (multi) {
    return (
      <span
        aria-hidden
        data-message-option-mark="multi"
        className={cn(
          "mt-[3px] flex size-3.5 shrink-0 items-center justify-center rounded-[min(3px,var(--control-radius))] border-[1.5px]",
          on ? "border-text-primary bg-(--text-primary) text-(--bg-base)" : "border-text-muted",
        )}
      >
        {on ? <CheckIcon size={10} strokeWidth={3} /> : null}
      </span>
    );
  }

  return (
    <span
      aria-hidden
      data-message-option-mark="single"
      className={cn(
        "mt-[3px] flex size-3.5 shrink-0 items-center justify-center rounded-full border-[1.5px]",
        on ? "border-text-primary" : "border-text-muted",
      )}
    >
      {on ? (
        <span data-message-option-dot className="size-1.5 rounded-full bg-(--text-primary)" />
      ) : null}
    </span>
  );
}
