import { forwardRef, type ComponentProps, type HTMLAttributes, type MouseEvent } from "react";
import { cn } from "@/lib/cn";
import { ChannelIcon } from "@/components/channel-icon";
import type { RowBadge } from "@/features/attention/attention-mode";
import {
  ConversationRowLayout,
  MutedMarker,
  UnreadBadge,
} from "@/components/layout/sidebar-shared";

interface ChannelRowProps extends Omit<HTMLAttributes<HTMLDivElement>, "children" | "onSelect"> {
  selected: boolean;
  column: "outer" | "nested";
  draggable?: boolean;
  onSelect: (event: MouseEvent<HTMLButtonElement>) => void;
  channelTitle: string;
  visibility?: ComponentProps<typeof ChannelIcon>["visibility"];
  muted: boolean;
  badge: RowBadge | null;
}

export const ChannelRow = forwardRef<HTMLDivElement, ChannelRowProps>(function ChannelRow(
  { channelTitle, visibility, muted, badge, ...layout },
  ref,
): JSX.Element {
  return (
    <ConversationRowLayout ref={ref} {...layout}>
      <div className="flex size-5.5 shrink-0 items-center justify-center">
        <ChannelIcon variant="sticker" visibility={visibility} />
      </div>
      <span className={cn("min-w-0 truncate", layout.selected && "font-medium")}>
        {channelTitle}
      </span>
      {muted ? (
        <span className="flex shrink-0 items-center">
          <MutedMarker />
        </span>
      ) : null}
      {badge ? <UnreadBadge count={badge.count} tier={badge.tier} className="ml-auto" /> : null}
    </ConversationRowLayout>
  );
});
