import { SidebarRowsSkeleton } from "@/components/layout/sidebar-skeleton";
import { SidebarReveal, SidebarRevealRow } from "@/components/layout/sidebar-reveal";
import { useMemo } from "react";
import type { DirectPairCounterpart } from "@silverhand/shared";
import { DirectMessageIcon, StarIcon } from "@/icons";
import { AppContextMenuItem } from "@/components/menus/app-context-menu";
import { useSidebarPairs } from "./sidebar-pairs";
import {
  SidebarConversationRow,
  useSidebarList,
  type SidebarConversation,
} from "./sidebar-conversation";
import {
  starredConversationIds,
  useFavoritesList,
  useStarDirectPair,
} from "@/features/workspaces/favorites";
import type { DirectMessageTarget } from "@/features/direct-pairs/target";
import { useDraftCounterpart } from "@/features/direct-pairs/use-draft-counterpart";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { useUserAvatarObjectUrls } from "@/features/users/avatar-hooks";
import {
  AuthorAvatarView,
  DeletedActorMarker,
} from "@/features/messages/components/author-avatar-view";
import { useDirectPairsList } from "@/features/direct-pairs/hooks";
import {
  collectCounterpartAvatarSources,
  presentCounterpart,
} from "@/features/direct-pairs/counterpart-presentation";
import { mergeDirectLineRows, type DirectLineRow } from "@/features/direct-pairs/direct-line-rows";
import { directRowBadge } from "@/features/attention/row-badges";
import { collapsedRevealRows } from "../../../features/workspaces/home-grouping";
import { SidebarSection, SidebarEmptyRow } from "../../../components/layout/sidebar-shared";
import { t } from "@/lib/i18n";

export function DirectMessagesGroup(props: {
  /** The open conversation — the ONLY thing that highlights a row here. */
  selectedChannelId: string | null;
  /** The participant whose pre-mint draft pane is open, if any. Nothing about it is
   *  stored: it comes from the route and leaves with it. */
  draftTarget?: DirectMessageTarget | null;
  /** Open a pair. Takes no workspace: there is none, and that is the point. */
  onSelectDirectPair: (conversationId: string) => void;
  /** Re-open the draft pane — the transient row's only action. */
  onSelectDirectDraft?: (target: DirectMessageTarget) => void;
}): JSX.Element | null {
  const pairsQuery = useDirectPairsList();
  const favorites = useFavoritesList();
  // A starred pair moves to STARRED, like a starred channel leaves its parent.
  const pairs = useMemo(() => {
    const starred = starredConversationIds(favorites.data);
    return (pairsQuery.data?.direct_pairs ?? []).filter((p) => !starred.has(p.conversation_id));
  }, [pairsQuery.data, favorites.data]);
  const target = props.draftTarget ?? null;
  const { counterpart } = useDraftCounterpart(target);
  const draft = useMemo(
    () => (target && counterpart ? { target, counterpart } : null),
    [target, counterpart],
  );
  // Merging, ordering and selection are ONE pure decision (`mergeDirectLineRows`),
  // so this component only paints what it is handed.
  const rows = useMemo(
    () =>
      mergeDirectLineRows({
        pairs,
        draft,
        selectedConversationId: props.selectedChannelId,
      }),
    [pairs, draft, props.selectedChannelId],
  );
  // ONE avatar batch for the section, over the counterpart set of the rows that
  // are actually rendered — the same discipline every author surface follows (a
  // leaf row never fetches its own blob). Deleted counterparts contribute nothing
  // and fetch nothing.
  const { agents, users } = useMemo(() => collectCounterpartAvatarSources(rows), [rows]);
  const agentAvatarUrls = useAgentAvatarObjectUrls(agents);
  const humanAvatarUrls = useUserAvatarObjectUrls(users);

  const pairing = useSidebarPairs();
  const urls = { agentAvatarUrls, humanAvatarUrls };
  const conversationOf = (row: DirectLineRow): SidebarConversation =>
    row.kind === "pair"
      ? directConversation(row.conversationId, row, urls, false, () =>
          props.onSelectDirectPair(row.conversationId),
        )
      : {
          // A draft is not a conversation yet; it lists itself but never
          // folds into a pair or opens beside anything.
          ...directConversation(row.key, row, urls, false, () =>
            props.onSelectDirectDraft?.(row.target),
          ),
          openable: false,
        };
  const { lines, byId } = useSidebarList(
    "direct-messages",
    rows,
    conversationOf,
    pairing?.pairs ?? [],
    (c) => c.openable !== false,
  );
  const line = ({ row, pair }: (typeof lines)[number]): JSX.Element => (
    <SidebarConversationRow
      conversation={byId.get(conversationOf(row).id)!}
      pair={row.kind === "pair" ? pair : null}
      selected={row.selected}
    />
  );
  const revealed = collapsedRevealRows(lines, {
    pokeUnread: true,
    isSelected: ({ row, pair }) =>
      row.selected || (pair !== null && pair.pair.right === props.selectedChannelId),
    isLoud: ({ row, pair }) =>
      directRowBadge(row.unread)?.tier === "loud" || pair?.right.badge?.tier === "loud",
  });

  return (
    <SidebarSection
      label={t("chat.directMessages")}
      labelClassName="text-xs"
      icon={<DirectMessageIcon className="size-3.5" />}
      // The count is the number of lines the section shows, draft included —
      // a count that disagreed with the visible rows would read as a bug.
      count={rows.length}
      collapsible
      defaultExpanded
      persistId="direct-messages"
      collapsedContent={
        <SidebarReveal>
          {revealed.map(({ row, pair }) => (
            <SidebarRevealRow key={row.key}>{line({ row, pair })}</SidebarRevealRow>
          ))}
        </SidebarReveal>
      }
    >
      {rows.length === 0 ? (
        pairsQuery.isLoading ? (
          <li>
            <SidebarRowsSkeleton label={t("chat.loadingDirectMessages")} nested />
          </li>
        ) : (
          <SidebarEmptyRow>{t("chat.noDirectMessages")}</SidebarEmptyRow>
        )
      ) : null}
      {lines.map(({ row, pair }) => (
        <li key={row.key}>{line({ row, pair })}</li>
      ))}
    </SidebarSection>
  );
}

type AvatarUrls = {
  agentAvatarUrls: ReadonlyMap<string, string | null>;
  humanAvatarUrls: ReadonlyMap<string, string | null>;
};

/** A direct line as a sidebar conversation, shared by DIRECT MESSAGES and
 *  STARRED. The counterpart IS the row: its avatar, its name, and — when the
 *  principal is gone — the same quiet deleted marker a byline shows, which is
 *  also the ONLY on-row explanation for the pane's read-only composer. */
export function directConversation(
  id: string,
  line: { counterpart: DirectPairCounterpart; unread: number },
  urls: AvatarUrls,
  starred: boolean,
  open: () => void,
): SidebarConversation {
  // The label is also the pane's title, and both come from the same
  // presentation rule (see `presentCounterpart`) — so the draft row's face and
  // name are already the ones the minted pair will wear.
  const counterpart = presentCounterpart(line.counterpart);
  const avatarId = line.counterpart.status === "active" ? line.counterpart.id : "";
  return {
    id,
    workspaceId: null,
    face: {
      glyphKey: `${line.counterpart.status}:${
        urls.agentAvatarUrls.get(avatarId) ?? urls.humanAvatarUrls.get(avatarId) ?? ""
      }`,
      glyph: (
        <AuthorAvatarView
          avatar={counterpart.avatar}
          size={16}
          label={counterpart.label}
          agentAvatarUrls={urls.agentAvatarUrls}
          humanAvatarUrls={urls.humanAvatarUrls}
        />
      ),
      title: counterpart.label,
      badge: directRowBadge(line.unread),
    },
    muted: false,
    marker: counterpart.isDeleted ? <DeletedActorMarker /> : undefined,
    open,
    menu: <DirectPairStarMenuItem conversationId={id} starred={starred} />,
  };
}

export function DirectPairStarMenuItem(props: {
  conversationId: string;
  starred: boolean;
}): JSX.Element {
  const star = useStarDirectPair();
  return (
    <AppContextMenuItem
      icon={<StarIcon className={"size-3.5" + (props.starred ? " fill-current" : "")} />}
      onSelect={() =>
        star.mutate({ conversationId: props.conversationId, starred: !props.starred })
      }
    >
      {props.starred ? t("chat.unstar") : t("chat.star")}
    </AppContextMenuItem>
  );
}
