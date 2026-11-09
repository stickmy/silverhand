import type { AgentDto, UserDto } from "@silverhand/shared";
import { HashIcon } from "@/icons";
import { t } from "@/lib/i18n";
import type { ChannelReference } from "./channel-ref";
import {
  conversationIcon,
  faceLabel,
  shortConversationId,
  type WorkspaceChannelReference,
} from "./conversation-ref";

// Non-interactive chips for export and screenshot trees, which must not mount
// the live chips' hooks and popovers.

/** The label a mention chip shows: `display_name` when the principal has one,
 *  else the handle (shiki's ruling — a chip always reads as the person, not the
 *  address). Shared by both spellings so an alias and a canonical reference to
 *  the same principal are indistinguishable on screen. Kind scoping happens at
 *  the call site; at most one of the two is ever present. */
export function mentionDisplayLabel(
  agent: AgentDto | undefined,
  human: UserDto | undefined,
): string | null {
  return (
    agent?.display_name?.trim() || agent?.name || human?.display_name?.trim() || human?.name || null
  );
}

export function StaticMentionRef({
  name,
  label,
  resolved,
  humanId,
}: {
  name: string;
  label: string | null;
  resolved: boolean;
  humanId?: string;
}): JSX.Element {
  if (!resolved) {
    return <span className="text-text-muted">@{name}</span>;
  }
  return (
    <span
      className="rounded-(--chip-radius) px-1 -mx-0.5 font-medium text-accent"
      aria-label={`@${name}`}
      data-mention-kind={humanId ? "human" : "agent"}
      data-mention-id={humanId}
    >
      @{label ?? name}
    </span>
  );
}

export function StaticChannelRef({
  name,
  channel,
}: {
  name: string;
  channel: ChannelReference | null | undefined;
}): JSX.Element {
  if (!channel) {
    return <>{`#${name}`}</>;
  }
  return (
    <span
      className={
        "inline-flex items-baseline gap-0.5 rounded-(--chip-radius) px-1 -mx-0.5 font-medium text-accent" +
        (channel.closed_at != null ? " opacity-60" : "")
      }
      title={`#${channel.title}`}
      data-channel-ref-state="resolved-no-handler"
      data-channel-ref-id={channel.id}
    >
      <HashIcon className="size-3 shrink-0 self-center" />
      {channel.title}
    </span>
  );
}

export function StaticConversationRef({
  id,
  conversation,
}: {
  id: string;
  conversation: WorkspaceChannelReference | undefined;
}): JSX.Element {
  if (!conversation) {
    return <>{`#conv:${id}`}</>;
  }

  const label = conversation
    ? conversationDisplayName(conversation)
    : `#conv:${shortConversationId(id)}`;
  // `channel` is not a guess: the parameter's TYPE says this ref came from a
  // workspace-scoped list ({@link WorkspaceChannelReference} carries the
  // argument), so a direct pair never reaches it. Same face, same word, same
  // glyph as the live chip — one authority (#task:1373).
  const icon = conversationIcon(conversation ? "channel" : undefined);
  return (
    <span
      className="inline-flex max-w-55 items-center gap-0.5 rounded-(--chip-radius) bg-accent-soft px-1.5 -mx-0.5 font-medium text-accent"
      aria-label={
        conversation ? `${faceLabel("channel")} ${label}` : t("messages.conversationId", { id })
      }
      data-conversation-ref={id}
    >
      {icon}
      <span className={conversation ? "truncate" : "truncate font-mono"}>{label}</span>
    </span>
  );
}

export function conversationDisplayName(conversation: WorkspaceChannelReference): string {
  return conversation.title;
}
