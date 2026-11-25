import { useMemo, type ComponentType } from "react";
import type { MessageDto, UserDto } from "@silverhand/shared";
import { useUsersById, makeUserNameResolver } from "@/features/users/hooks";
import { Markdown } from "@/features/messages/components/markdown";
import type { WorkspaceChannelReference } from "@/features/messages/components/conversation-ref";
import type { ChannelReference } from "@/features/messages/components/channel-ref";

/**
 * Shared hook: build a resolver that maps `user_id` → display name.
 *
 * /api/users query so subscribing from multiple sub-components is free.
 */
export function useUserResolver(): ReturnType<typeof makeUserNameResolver> {
  const usersById = useUsersById();
  return useMemo(() => makeUserNameResolver(usersById), [usersById]);
}

/**
 * Props every per-subtype renderer receives. Mirrors `SystemMessageProps`
 * minus the public-facing prop name; carrying the same shape keeps refs
 * (channel / conversation links, mention pills) wired through markdown
 * inside any renderer.
 */
export interface RendererProps {
  message: MessageDto;
  conversationRefsById?: Map<string, WorkspaceChannelReference>;
  onOpenConversationRef?: (id: string) => void;
  channelsByName?: Map<string, ChannelReference | null>;
  onOpenChannelRef?: (channelId: string) => void;
  humansByName?: Map<string, UserDto>;
  /** Brings a message of this stream into view, loading it when off-window. */
  onLocateMessage?: (messageId: string) => void;
}

export type SystemMessageRenderer = ComponentType<RendererProps>;

// ─── Generic fallback ──────────────────────────────────────────────────

/** IM-convention narration for unknown subtypes and any current
 *  subtype that doesn't have a custom renderer yet
 * */
export function GenericNarrationFallback({
  message,
  conversationRefsById,
  onOpenConversationRef,
  channelsByName,
  onOpenChannelRef,
  humansByName,
}: RendererProps): JSX.Element {
  return (
    <div className="px-4 py-1.5 text-center italic text-text-muted">
      <Markdown
        content={message.content}
        conversationRefsById={conversationRefsById}
        onOpenConversationRef={onOpenConversationRef}
        channelsByName={channelsByName}
        onOpenChannelRef={onOpenChannelRef}
        humansByName={humansByName}
      />
    </div>
  );
}

// ─── Text helpers ──────────────────────────────────────────────────────

export function firstLine(text: string): string {
  const newline = text.indexOf("\n");
  return newline === -1 ? text : text.slice(0, newline);
}
