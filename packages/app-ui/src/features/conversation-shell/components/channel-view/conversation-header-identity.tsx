import { useMemo } from "react";
import { BotIcon, HashIcon, UserIcon } from "@/icons";
import type { PageBarState } from "@silverhand/client";
import { agentAvatarImage } from "@/components/agent-avatar";
import { iconMarkup } from "@/components/layout/native-page-bar";
import type { DirectPairCounterpart } from "@silverhand/shared";
import {
  AuthorAvatarView,
  DeletedActorMarker,
} from "@/features/messages/components/author-avatar-view";
import { cn } from "@/lib/cn";
import { useNav } from "@/router";
import { focusAgentSideSection } from "@/features/agents/components/agent-view";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { useAgent, useAgentPresence } from "@/features/agents/hooks/hooks";
import { PRESENCE_TONE_DOT, projectPresence } from "@/features/agents/model/presence-projection";
import { usePresenceById } from "@/features/users/presence";
import { useUserAvatarObjectUrls } from "@/features/users/avatar-hooks";
import {
  collectCounterpartAvatarSources,
  presentCounterpart,
} from "@/features/direct-pairs/counterpart-presentation";
import type { ConversationPaneCapabilities } from "@/features/conversations/pane-mode";
import { t } from "@/lib/i18n";

const TITLE_CLASS =
  "min-w-0 truncate font-serif text-base leading-4.5 font-semibold text-text-primary";

/** Both header arms — the channel `#` and the DM avatar — share this one box edge
 *  so switching conversation kind never resizes the identity. */
export const HEADER_IDENTITY_PX = 30;

const HEADER_IDENTITY_GLYPH_PX = 18;

/** Inline size WINS over a theme's own `#` selectors, so the box is fixed here and
 *  a theme only projects paint/effect — it must never carry the size itself. */
const IDENTITY_BOX_STYLE = {
  display: "inline-flex",
  alignItems: "center",
  justifyContent: "center",
  inlineSize: HEADER_IDENTITY_PX,
  blockSize: HEADER_IDENTITY_PX,
  boxSizing: "border-box",
  padding: 0,
  verticalAlign: "middle",
  flex: "none",
} as const;

export function ConversationHeaderIdentity({
  identity,
  title,
  counterpart,
}: {
  identity: ConversationPaneCapabilities["headerIdentity"];
  title: string;
  counterpart?: DirectPairCounterpart | null;
}): JSX.Element {
  if (identity === "counterpart" && counterpart) {
    return <CounterpartIdentity title={title} counterpart={counterpart} />;
  }
  // A flex row, not inline flow: the 30px `#` box on a text line grows the line
  // box past the box itself (middle-aligned against the serif strut), so the title
  // would stand taller than the header's identity edge and sit off its centre.
  return (
    <span data-conversation-title className={cn(TITLE_CLASS, "flex items-center")}>
      {identity === "channel_hash" ? (
        <span
          data-conversation-hash-icon
          data-sticker
          data-sticker-tone="channel"
          style={IDENTITY_BOX_STYLE}
        >
          <HashIcon size={HEADER_IDENTITY_GLYPH_PX} />
        </span>
      ) : null}
      <span className="min-w-0 truncate">{title}</span>
    </span>
  );
}

/** The same identity as the native bar's title icon: the channel `#`, or the
 * counterpart's face, falling back to its glyph while the face loads. */
export function useHeaderIdentityIcon(
  identity: ConversationPaneCapabilities["headerIdentity"],
  counterpart: DirectPairCounterpart | null,
): PageBarState["titleIcon"] {
  const { agents, users } = useMemo(
    () =>
      identity === "counterpart" && counterpart
        ? collectCounterpartAvatarSources([{ counterpart }])
        : { agents: [], users: [] },
    [identity, counterpart],
  );
  const agentAvatarUrls = useAgentAvatarObjectUrls(agents);
  const humanAvatarUrls = useUserAvatarObjectUrls(users);

  if (identity === "channel_hash") {
    return { glyph: iconMarkup(HashIcon) };
  }
  if (identity !== "counterpart" || !counterpart) {
    return undefined;
  }

  const { avatar } = presentCounterpart(counterpart);
  const image =
    avatar.variant === "agent"
      ? agentAvatarImage(avatar.runtime, agentAvatarUrls.get(avatar.id) ?? null)
      : avatar.variant === "user"
        ? (humanAvatarUrls.get(avatar.id) ?? null)
        : null;
  if (image) {
    return { image };
  }
  return { glyph: iconMarkup(counterpart.kind === "agent" ? BotIcon : UserIcon) };
}

/**
 * The pair's face + name. The avatar is a SIBLING of the identity block, not a
 * child: the theme hooks are attribute-scoped (`[data-conversation-title]`) and
 * the name truncates, so an avatar must not live inside it.
 *
 * Only an ACTIVE AGENT counterpart's face is a button that opens its Activity,
 * keyed on the counterpart's OWN resolved id — a human or deleted endpoint has no
 * Agent Activity and stays a plain face.
 *
 * An active agent also gets its description under the name; the counterpart union
 * carries no description, so it is read from the same agent authority by that id,
 * and an empty one renders nothing rather than an empty line.
 */
function CounterpartIdentity({
  title,
  counterpart,
}: {
  title: string;
  counterpart: DirectPairCounterpart;
}): JSX.Element {
  const presentation = presentCounterpart(counterpart);
  // One batch for one face: the header holds a single counterpart, so this is the
  // same hook every other surface uses, over a one-element source set.
  const { agents, users } = useMemo(
    () => collectCounterpartAvatarSources([{ counterpart }]),
    [counterpart],
  );
  const agentAvatarUrls = useAgentAvatarObjectUrls(agents);
  const humanAvatarUrls = useUserAvatarObjectUrls(users);
  const { openAgentPanel, openHumanPanel } = useNav();
  const agentId =
    counterpart.status === "active" && counterpart.kind === "agent" ? counterpart.id : null;
  const description = useAgent(agentId).data?.agent.description?.trim() || null;
  const face = (
    <AuthorAvatarView
      avatar={presentation.avatar}
      size={HEADER_IDENTITY_PX}
      label={presentation.label}
      className="shrink-0"
      agentAvatarUrls={agentAvatarUrls}
      humanAvatarUrls={humanAvatarUrls}
    />
  );
  return (
    <>
      {agentId !== null ? (
        <button
          type="button"
          onClick={() => {
            focusAgentSideSection(agentId, "activity");
            openAgentPanel(agentId);
          }}
          aria-label={t("conversationShell.openActivity", { name: presentation.label })}
          title={t("conversationShell.openActivity", { name: presentation.label })}
          className="focus-outline inline-flex shrink-0 rounded-(--avatar-radius) transition-[filter] duration-100 hover:brightness-90"
        >
          {face}
        </button>
      ) : counterpart.status === "active" && counterpart.kind === "human" ? (
        <button
          type="button"
          onClick={() => openHumanPanel(counterpart.id)}
          aria-label={t("conversationShell.openProfile", { name: presentation.label })}
          title={t("conversationShell.openProfile", { name: presentation.label })}
          className="focus-outline inline-flex shrink-0 rounded-(--avatar-radius) transition-[filter] duration-100 hover:brightness-90"
        >
          {face}
        </button>
      ) : (
        face
      )}
      <span className="flex min-w-0 flex-col justify-center">
        <span className="flex min-w-0 items-center gap-1.5">
          <span data-conversation-title className={TITLE_CLASS}>
            {title}
          </span>
          {presentation.isDeleted ? (
            <span className="flex shrink-0 items-center">
              <DeletedActorMarker />
            </span>
          ) : (
            <CounterpartPresenceDot counterpart={counterpart} />
          )}
        </span>
        {description ? (
          <span
            className="min-w-0 truncate text-(length:--text-meta) text-text-muted leading-3"
            title={description}
          >
            {description}
          </span>
        ) : null}
      </span>
    </>
  );
}

function CounterpartPresenceDot({
  counterpart,
}: {
  counterpart: DirectPairCounterpart;
}): JSX.Element | null {
  if (counterpart.status !== "active") return null;
  return counterpart.kind === "agent" ? (
    <AgentPresenceDot agentId={counterpart.id} />
  ) : (
    <HumanPresenceDot userId={counterpart.id} />
  );
}

const PRESENCE_DOT_CLASS = "size-2 shrink-0 rounded-[var(--pill-radius)]";

function AgentPresenceDot({ agentId }: { agentId: string }): JSX.Element | null {
  const presence = useAgentPresence(agentId);
  if (presence === null) return null;
  const display = projectPresence({ scope: "global", presence });
  return (
    <span
      data-status-pip
      title={display.detail}
      aria-label={display.a11yLabel}
      className={`${PRESENCE_DOT_CLASS} ${PRESENCE_TONE_DOT[display.tone]}`}
    />
  );
}

function HumanPresenceDot({ userId }: { userId: string }): JSX.Element {
  const presence = usePresenceById().get(userId)?.presence;
  const online = presence === "online";
  return (
    <span
      data-status-pip
      title={online ? t("conversationShell.online") : t("conversationShell.offline")}
      className={`${PRESENCE_DOT_CLASS} ${online ? "bg-success" : "bg-text-muted/40"}`}
    />
  );
}
