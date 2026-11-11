import { useState } from "react";
import * as HoverCard from "@radix-ui/react-hover-card";
import { AddReactionIcon } from "@/icons";
import type { MessageDto, ReactionAggregateDto } from "@silverhand/shared";
import { useMessageReactors } from "@/features/channels/reaction-hooks";
import { ReactionPicker } from "./reaction-picker";
import {
  ReactionChipBody,
  reactionChipClass,
  reactionChipCountLabel,
  REACTION_ROW_CLASS,
} from "./reaction-chip-row";
import { t } from "@/lib/i18n";
import { useMessageAlignEnd } from "./message-row-content";

export interface MessageReactionsProps {
  message: MessageDto;
  /** Toggle the viewer's reaction. `remove` is the current `viewer_reacted`
   *  for an existing chip (re-click removes own), or always `false` from the
   *  picker (picking adds). */
  onReact: (emoji: string, remove: boolean) => void;
  /** Bubbles the inline picker's open state so the host row stays framed. */
  onPickerOpenChange?: (open: boolean) => void;
  /** When false (closed/archived channel), chips render display-only — no
   *  click-to-toggle and no trailing add affordance — but existing counts and
   *  the hover "who reacted" list stay (#task:1122 dogfood follow-up). */
  canReact?: boolean;
}

/**
 * The live, interactive reaction-chip row rendered below a message body
 * (S4, #task:1122).
 *
 * Same component in both the main-line and reply-cluster surfaces — reactions
 * key off `message_id`, so there is no per-surface fork. Renders nothing when
 * the message has no reactions: the only add affordance then is the message's
 * top-right "React" corner button (owned by MessageItem). Once a reaction
 * exists, the row also carries a trailing smiley-plus add chip.
 *
 * The chip VISUAL (fill / radius / sizing / glyph + count) is NOT defined here
 * — it comes from `ReactionChipRow` (`reactionChipClass` / `ReactionChipBody` /
 * `reactionRowClass`), the single source shared with the static export row so
 * reactions can never be styled in chat yet vanish from screenshots (#task:1173).
 * This component only LAYERS interactivity on top: click-to-toggle, the
 * "who reacted" HoverCard, and the add picker.
 */
export function MessageReactions({
  message,
  onReact,
  onPickerOpenChange,
  canReact = true,
}: MessageReactionsProps): JSX.Element | null {
  const alignEnd = useMessageAlignEnd();
  const reactions = message.reactions;
  if (!reactions || reactions.length === 0) return null;
  return (
    <div data-reaction-row className={REACTION_ROW_CLASS}>
      {reactions.map((agg) => (
        <ReactionChip
          key={agg.emoji}
          messageId={message.id}
          agg={agg}
          alignEnd={alignEnd}
          interactive={canReact}
          onClick={() => onReact(agg.emoji, agg.viewer_reacted)}
        />
      ))}
      {canReact ? (
        <ReactionPicker
          align={alignEnd ? "end" : "start"}
          onOpenChange={onPickerOpenChange}
          onPick={(emoji) => onReact(emoji, false)}
          trigger={
            <button
              type="button"
              data-reaction-add
              aria-label={t("messages.addReaction")}
              title={t("messages.addReaction")}
              className="flex h-4.5 items-center rounded-(--chip-radius) bg-(--chip-bg) px-2 text-text-muted transition-colors hover:bg-(--chip-bg-hover) hover:text-text-primary focus-outline"
            >
              <AddReactionIcon className="size-3.5" />
            </button>
          }
        />
      ) : null}
    </div>
  );
}

function ReactionChip({
  messageId,
  agg,
  alignEnd,
  onClick,
  interactive,
}: {
  messageId: string;
  agg: ReactionAggregateDto;
  /** Only to mirror the reactor card's growth direction — see its `align`. */
  alignEnd: boolean;
  onClick: () => void;
  interactive: boolean;
}): JSX.Element {
  const [open, setOpen] = useState(false);
  // Lazy: reactor names load only while hovered (§6 perf).
  const reactors = useMessageReactors(messageId, open);
  const names =
    reactors.data?.find((d) => d.emoji === agg.emoji)?.reactors.map((r) => r.display_name) ?? [];
  // Shared visual (single-sourced in ReactionChipRow); only the interactive
  // chip gets the hover-fill shift and the focus outline — a display-only chip in
  // a read-only channel must not look clickable.
  const chipClass = reactionChipClass(agg, interactive);
  const inner = <ReactionChipBody agg={agg} />;
  const countLabel = reactionChipCountLabel(agg);
  return (
    <HoverCard.Root open={open} onOpenChange={setOpen} openDelay={350} closeDelay={120}>
      <HoverCard.Trigger asChild>
        {interactive ? (
          <button
            type="button"
            data-reaction-chip
            aria-pressed={agg.viewer_reacted}
            aria-label={
              countLabel +
              (agg.viewer_reacted ? t("messages.youReacted") : "") +
              (agg.viewer_reacted ? t("messages.clickToRemove") : t("messages.clickToAdd"))
            }
            onClick={onClick}
            className={chipClass + " focus-outline"}
          >
            {inner}
          </button>
        ) : (
          <div
            data-reaction-chip
            aria-label={countLabel + (agg.viewer_reacted ? t("messages.youReacted") : "")}
            className={chipClass}
          >
            {inner}
          </div>
        )}
      </HoverCard.Trigger>
      <HoverCard.Portal>
        <HoverCard.Content
          // Same rule as the two pickers: the row is mirrored (`justify-end`
          // on an own message), so its chips sit against the row's right
          // edge and the card has to grow leftward from there instead of
          // outward (#task:1840).
          align={alignEnd ? "end" : "start"}
          sideOffset={6}
          collisionPadding={8}
          data-popover-frame="info"
          className="z-(--z-popover) max-w-60 rounded-(--floating-radius) border border-border bg-bg-floating px-2.5 py-1.5 text-xs text-text-primary shadow-popover outline-none"
        >
          <span className="mr-1">{agg.emoji}</span>
          {reactors.isLoading && names.length === 0 ? (
            <span className="text-text-muted">…</span>
          ) : names.length > 0 ? (
            <span>{names.join("、")}</span>
          ) : (
            <span className="text-text-muted">—</span>
          )}
        </HoverCard.Content>
      </HoverCard.Portal>
    </HoverCard.Root>
  );
}
