import { useState, type ReactNode } from "react";
import { CheckIcon } from "@/icons";
import { cn } from "@/lib/cn";
import { t } from "@/lib/i18n";

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
    <div data-message-options className="mt-2 flex max-w-[min(360px,100%)] flex-col gap-1.5">
      <div className="flex flex-col rounded-(--control-radius) border-(length:--control-border-width) border-border bg-(--bg-base) p-1">
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
            className="flex gap-1.5 px-1 pt-0.5 pb-1"
            onSubmit={(e) => {
              e.preventDefault();
              if (multi) {
                submitMulti();
              } else {
                submitOther();
              }
            }}
          >
            <input
              autoFocus
              value={otherText}
              onChange={(e) => setOtherText(e.target.value)}
              placeholder={t("messages.optionOtherPlaceholder")}
              className="min-w-0 flex-1 rounded-(--control-radius) border-(length:--control-border-width) border-border bg-transparent px-2.5 py-1.5 text-sm outline-none focus:border-(--border-control-hover,var(--text-muted))"
            />
            {multi ? null : (
              <button
                type="submit"
                className="rounded-(--control-radius) bg-(--text-primary) px-3 text-xs text-(--bg-primary)"
              >
                {t("messages.optionSubmit")}
              </button>
            )}
          </form>
        ) : null}
      </div>
      {interactive && answer?.mine && !editing ? (
        <div className="flex justify-end">
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
        <div className="flex items-center justify-end gap-2">
          {hint ? (
            <span className="text-(length:--text-meta) text-text-muted">
              {t("messages.optionPickFirst")}
            </span>
          ) : null}
          <button
            type="button"
            onClick={submitMulti}
            className="rounded-(--control-radius) bg-(--text-primary) px-3 py-1 text-xs text-(--bg-primary)"
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
      role={multi ? "checkbox" : "radio"}
      aria-checked={on}
      disabled={!interactive}
      onClick={onPick}
      className={cn(
        "flex w-full items-center gap-2 rounded-[max(0px,calc(var(--control-radius)-2px))] px-2 py-1.5 text-left text-sm transition-colors focus-outline disabled:cursor-default",
        on
          ? "text-text-primary"
          : "text-text-secondary enabled:hover:bg-bg-hover enabled:hover:text-text-primary",
      )}
    >
      <OptionMark multi={multi} on={on} />
      <span className="min-w-0 flex-1">{children}</span>
      {answererName ? (
        <span className="shrink-0 text-(length:--text-meta) text-text-muted">{answererName}</span>
      ) : null}
    </button>
  );
}

function OptionMark({ multi, on }: { multi: boolean; on: boolean }): JSX.Element {
  if (multi) {
    return (
      <span
        aria-hidden
        className={cn(
          "flex size-3.5 shrink-0 items-center justify-center rounded-[min(3px,var(--control-radius))] border-[1.5px]",
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
      className={cn(
        "flex size-3.5 shrink-0 items-center justify-center rounded-full border-[1.5px]",
        on ? "border-text-primary" : "border-text-muted",
      )}
    >
      {on ? <span className="size-1.5 rounded-full bg-(--text-primary)" /> : null}
    </span>
  );
}
