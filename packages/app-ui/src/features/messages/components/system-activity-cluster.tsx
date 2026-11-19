import dayjs from "dayjs";
import { memo } from "react";
import type { MessageDto, UserDto } from "@silverhand/shared";
import { SystemMessage } from "./system-message";
import type { ChannelReference } from "./channel-ref";
import type { WorkspaceChannelReference } from "./conversation-ref";
import { foldTaskRuns } from "../model/fold-task-runs";

/**
 * Activity cluster — renders a contiguous run of low-priority system
 * events (the `task_*` family) as a single muted narration strip rather
 * than as N independent message rows (PR-10). (`rename` / `session_cleared`
 * were removed from the clusterable set in #task:1145 — they render
 * standalone.)
 *
 * The grouping decision lives in `group-messages.ts`; this component
 * only paints the visual frame:
 *  - skip the avatar lane (the cluster's content aligns to the chat
 *    column, just like a real reply body) so the strip doesn't carry
 *    "this is N independent senders' rows" mass.
 *  - left rail (1px subtle) + 10px inset to give the strip a single
 *    anchor edge — this is what makes "this is one activity segment"
 *    legible after the per-row chrome is demoted.
 *  - tight inner stacking (2px row gap) so multi-event clusters look
 *    like one strip, not N stacked rows.
 *  - 7px outer margin so the strip detaches from real messages above
 *    and below it.
 *  - background is intentionally transparent — cards are reserved for
 *    info blocks (`schedule_fire`, context-card series, `mention_cc`,
 *    `onboarding`); a tinted cluster would compete with
 *    those and re-introduce the "everything is a card" density problem
 *    PR-10 is meant to undo.
 *
 * The per-row demotion (actor → muted, chip → transparent + outline,
 * pill → transparent + outline, dot → smaller) is handled entirely via
 * CSS scoped to `.system-activity-cluster` in `chrome.css`. This keeps
 * the per-subtype renderers in `system-message.tsx` unchanged — they
 * still know how to render standalone, and the cluster just borrows
 * their structured output and restyles it from the outside.
 */
export interface SystemActivityClusterProps {
  messages: MessageDto[];
  conversationRefsById?: Map<string, WorkspaceChannelReference>;
  onOpenConversationRef?: (id: string) => void;
  channelsByName?: Map<string, ChannelReference | null>;
  onOpenChannelRef?: (channelId: string) => void;
  humansByName?: Map<string, UserDto>;
}

export const SystemActivityCluster = memo(function SystemActivityCluster({
  messages,
  ...rowProps
}: SystemActivityClusterProps): JSX.Element {
  return (
    <div className="system-activity-cluster" data-system-activity-cluster={String(messages.length)}>
      {foldTaskRuns(messages).map((fold) => (
        <TaskEventFoldRow key={fold.key} events={fold.events} {...rowProps} />
      ))}
    </div>
  );
});

type RowProps = Omit<SystemActivityClusterProps, "messages">;

/** One task's consecutive events as their latest line. The full sequence lives
 *  in the task's History, so the stream never expands. */
function TaskEventFoldRow({ events, ...rowProps }: RowProps & { events: MessageDto[] }) {
  const latest = events[events.length - 1]!;

  return (
    <div data-activity-row className="relative">
      <ActivityTime at={latest.created_at} />
      <SystemMessage message={latest} {...rowProps} />
    </div>
  );
}

function ActivityTime({ at }: { at: number }): JSX.Element {
  const createdAt = dayjs(at);

  return (
    <time
      data-activity-time
      dateTime={createdAt.toISOString()}
      title={createdAt.format("YYYY-MM-DD HH:mm:ss")}
    >
      {createdAt.format("HH:mm")}
    </time>
  );
}
