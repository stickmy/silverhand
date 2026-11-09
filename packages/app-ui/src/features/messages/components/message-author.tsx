import type { AgentDto, UserDto } from "@silverhand/shared";
import { AVATAR_PX } from "@/components/avatar-scale";
import { AgentInfoPopover } from "@/features/agents/components/agent-info-popover";
import { HumanInfoPopover } from "@/features/profile/human-info-popover";
import type { AuthorPresentation } from "../model/author-presentation";
import { AuthorAvatarView, AuthorName } from "./author-avatar-view";

export function MessageAuthorAvatar({
  presentation,
  avatarObjectUrl,
  liveAgent,
  liveHuman,
  conversationId,
  onNavigateAgent,
  onNavigateHuman,
}: {
  presentation: AuthorPresentation;
  /** The author's resolved avatar bytes (active arms only); neutral arms ignore
   *  it and draw the runtime-free placeholder. */
  avatarObjectUrl: string | null;
  /** Live actor rows, present only for an active author — they wrap the avatar
   *  in the hover identity card. A deleted / unresolved author renders the bare
   *  neutral square: there is no profile to open. */
  liveAgent: AgentDto | undefined;
  liveHuman: UserDto | undefined;
  conversationId: string;
  onNavigateAgent: (agentId: string) => void;
  onNavigateHuman: (userId: string) => void;
}): JSX.Element {
  // Make the avatar's outermost element (button when wrapped, span when not)
  // carry the explicit size box so the focus outline stays square regardless of the
  // image's natural aspect ratio (@shiki `4TU4EBHr70R-YMgc`).
  const avatar = (
    <AuthorAvatarView
      avatar={presentation.avatar}
      size={AVATAR_PX.lg}
      label={presentation.label}
      className="border-(length:--border-avatar-width) border-border-avatar"
      objectUrlOverride={avatarObjectUrl}
    />
  );
  if (presentation.status === "active" && liveAgent) {
    return (
      <AgentInfoPopover
        agent={liveAgent}
        conversationId={conversationId}
        trigger="hover"
        avatarObjectUrl={avatarObjectUrl}
      >
        <button
          type="button"
          aria-label={`@${liveAgent.name}`}
          onClick={(event) => {
            event.preventDefault();
            onNavigateAgent(liveAgent.id);
          }}
          className="mt-0.5 rounded-(--avatar-radius) transition-[filter] duration-100 hover:brightness-90 focus-outline"
        >
          {avatar}
        </button>
      </AgentInfoPopover>
    );
  }
  if (presentation.status === "active" && liveHuman) {
    return (
      <HumanInfoPopover
        human={liveHuman}
        userId={liveHuman.id}
        trigger="hover"
        avatarObjectUrl={avatarObjectUrl}
      >
        <button
          type="button"
          aria-label={`@${liveHuman.name}`}
          onClick={(event) => {
            event.preventDefault();
            onNavigateHuman(liveHuman.id);
          }}
          className="mt-0.5 rounded-(--avatar-radius) transition-[filter] duration-100 hover:brightness-90 focus-outline"
        >
          {avatar}
        </button>
      </HumanInfoPopover>
    );
  }
  // Deleted / unresolved (or an active author whose live row hasn't loaded):
  // the neutral / snapshot avatar, non-navigable.
  return <span className="mt-0.5 inline-flex">{avatar}</span>;
}

/**
 * Header author label, driven by the resolved {@link AuthorPresentation}:
 * - active agent (live row present) → `<AgentInfoPopover>` + navigable button
 * - active user (live row present) → `<HumanInfoPopover>` + navigable button
 * - deleted → the snapshot name + a quiet "deleted" marker, non-navigable
 * - unresolved / active-without-live-row → the plain label, non-navigable
 */
export function MessageAuthorLabel({
  presentation,
  conversationId,
  liveAgent,
  liveHuman,
  avatarObjectUrl,
  onNavigateAgent,
  onNavigateHuman,
}: {
  presentation: AuthorPresentation;
  conversationId: string;
  liveAgent: AgentDto | undefined;
  liveHuman: UserDto | undefined;
  avatarObjectUrl: string | null;
  onNavigateAgent: (agentId: string) => void;
  onNavigateHuman: (userId: string) => void;
}): JSX.Element {
  const label = presentation.label;
  if (presentation.status === "active" && liveAgent) {
    return (
      <AgentInfoPopover
        agent={liveAgent}
        conversationId={conversationId}
        trigger="hover"
        avatarObjectUrl={avatarObjectUrl}
      >
        <button
          type="button"
          onClick={(event) => {
            event.preventDefault();
            onNavigateAgent(liveAgent.id);
          }}
          /* `data-message-author` + `data-author-kind` are theme-neutral
           *  identity hooks. BOTH kinds take `text-text-primary` in EVERY theme:
           *  the label is a recognition surface, not a status signal, and agent
           *  and human are isomorphic actors (shiki 2026-08-02). A theme must
           *  not repaint the author name by kind. */
          data-message-author
          data-author-kind="agent"
          className="rounded font-bold text-text-primary focus-outline"
        >
          {label}
        </button>
      </AgentInfoPopover>
    );
  }
  if (presentation.status === "active" && liveHuman) {
    return (
      <HumanInfoPopover
        human={liveHuman}
        userId={liveHuman.id}
        trigger="hover"
        avatarObjectUrl={avatarObjectUrl}
      >
        <button
          type="button"
          onClick={(event) => {
            event.preventDefault();
            onNavigateHuman(liveHuman.id);
          }}
          data-message-author
          data-author-kind="user"
          className="rounded font-bold text-text-primary focus-outline"
        >
          {label}
        </button>
      </HumanInfoPopover>
    );
  }
  // Deleted / unresolved (or active-without-live-row): a non-navigable label.
  // Deleted keeps its snapshot name + the quiet marker; unresolved shows the
  // neutral "Unknown …" the seam already resolved. No popover — there is no
  // profile to open. The name + deleted marker go through the shared
  // `AuthorName` primitive so this surface can't drift from the list / export
  // surfaces on how a deleted author reads.
  return (
    <span
      data-message-author
      data-author-kind={presentation.kind}
      className="inline-flex items-baseline gap-1.5"
    >
      <AuthorName presentation={presentation} className="font-bold text-text-primary" />
    </span>
  );
}
