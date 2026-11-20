import { SidebarRowsSkeleton } from "@/components/layout/sidebar-skeleton";
import { SidebarReveal, SidebarRevealRow } from "@/components/layout/sidebar-reveal";
import { forwardRef, useMemo, type HTMLAttributes, type MouseEvent } from "react";
import type { DirectPairCounterpart } from "@silverhand/shared";
import { DirectMessageIcon, StarIcon } from "@/icons";
import { AppContextMenu, AppContextMenuItem } from "@/components/menus/app-context-menu";
import { useConversationDragSource, useRowSplitActions, useSidebarSplit } from "./sidebar-split";
import { foldPairs, useRegisterRowFaces, useRowFaces, type RowFace } from "./sidebar-faces";
import { SidebarPairRow } from "./pair-row";
import type { SplitPair } from "@/features/conversation-shell/split-pairs";
import {
  starredConversationIds,
  useFavoritesList,
  useStarDirectPair,
} from "@/features/workspaces/favorites";
import { cn } from "@/lib/cn";
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
import {
  ConversationRowLayout,
  SidebarSection,
  SidebarEmptyRow,
  UnreadBadge,
} from "../../../components/layout/sidebar-shared";
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

  const split = useSidebarSplit();
  const idOf = (row: DirectLineRow) => (row.kind === "pair" ? row.conversationId : row.key);
  const ownFaces = new Map(
    rows.flatMap((row) =>
      row.kind === "pair"
        ? [[row.conversationId, directPairHalf(row, { agentAvatarUrls, humanAvatarUrls })] as const]
        : [],
    ),
  );
  useRegisterRowFaces("direct-messages", ownFaces);
  const lines = foldPairs(rows, idOf, split?.pairs ?? [], useRowFaces());
  const lineProps = {
    agentAvatarUrls,
    humanAvatarUrls,
    onSelectDirectPair: props.onSelectDirectPair,
    onSelectDirectDraft: props.onSelectDirectDraft,
  };
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
            <SidebarRevealRow key={row.key}>
              <DirectMessageLine row={row} pair={pair} {...lineProps} />
            </SidebarRevealRow>
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
        <li key={row.key}>
          <DirectMessageLine row={row} pair={pair} {...lineProps} />
        </li>
      ))}
    </SidebarSection>
  );
}

type LineProps = {
  agentAvatarUrls: ReadonlyMap<string, string | null>;
  humanAvatarUrls: ReadonlyMap<string, string | null>;
  onSelectDirectPair: (conversationId: string) => void;
  onSelectDirectDraft?: (target: DirectMessageTarget) => void;
};

/** A direct line, or a kept pair of two direct lines folded into one row. */
function DirectMessageLine({
  row,
  pair,
  ...props
}: {
  row: DirectLineRow;
  pair: { pair: SplitPair; right: RowFace } | null;
} & LineProps): JSX.Element {
  if (pair && row.kind === "pair") {
    return (
      <SidebarPairRow
        pair={pair.pair}
        workspaceId={null}
        left={directPairHalf(row, props)}
        right={pair.right}
        menu={<DirectPairStarMenuItem conversationId={row.conversationId} starred={false} />}
      />
    );
  }
  return <DirectMessageRow row={row} {...props} />;
}

/** One half of a pair row drawn from a direct line. */
export function directPairHalf(
  line: { counterpart: DirectPairCounterpart; unread: number },
  urls: Pick<LineProps, "agentAvatarUrls" | "humanAvatarUrls">,
): RowFace {
  const counterpart = presentCounterpart(line.counterpart);
  const avatarId = line.counterpart.status === "active" ? line.counterpart.id : "";
  return {
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
  };
}

/** One line's row. The counterpart IS the row: its avatar, its name, and — when the
 *  principal is gone — the same quiet deleted marker a byline shows, which is also
 *  the ONLY on-row explanation for the pane's read-only composer.
 *
 *  A pair and a draft differ in exactly one thing — WHAT opening it navigates to —
 *  so they share one component instead of a second row species with its own
 *  painting. */
function DirectMessageRow({
  row,
  agentAvatarUrls,
  humanAvatarUrls,
  onSelectDirectPair,
  onSelectDirectDraft,
}: { row: DirectLineRow } & LineProps): JSX.Element {
  const splitActions = useRowSplitActions(
    row.kind === "pair" ? row.conversationId : row.key,
    row.selected || row.kind === "draft",
  );
  const dragSource = useConversationDragSource(
    row.kind === "pair" ? row.conversationId : row.key,
    row.selected || row.kind === "draft",
  );
  const face = (
    <DirectLineRowView
      {...dragSource}
      counterpart={row.counterpart}
      unread={row.unread}
      selected={row.selected}
      agentAvatarUrls={agentAvatarUrls}
      humanAvatarUrls={humanAvatarUrls}
      onSelect={splitActions.select(() => {
        // A draft row stays on the draft even when a pair for the same agent
        // exists: that is what makes it one row instead of two.
        if (row.kind === "draft") {
          onSelectDirectDraft?.(row.target);
          return;
        }
        onSelectDirectPair(row.conversationId);
      })}
    />
  );
  if (row.kind === "draft") return face;
  return (
    <AppContextMenu trigger={face}>
      {splitActions.menuItem}
      <DirectPairStarMenuItem conversationId={row.conversationId} starred={false} />
    </AppContextMenu>
  );
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

/** A direct line's sidebar face, shared by DIRECT MESSAGES and STARRED. Forwards
 *  its ref and extra props so it can be a context-menu trigger. */
export const DirectLineRowView = forwardRef<
  HTMLDivElement,
  Omit<HTMLAttributes<HTMLDivElement>, "children" | "onSelect" | "title"> & {
    counterpart: DirectPairCounterpart;
    unread: number;
    selected: boolean;
    draggable?: boolean;
    agentAvatarUrls: ReadonlyMap<string, string | null>;
    humanAvatarUrls: ReadonlyMap<string, string | null>;
    onSelect: (event: MouseEvent<HTMLButtonElement>) => void;
  }
>(function DirectLineRowView(
  { counterpart: raw, unread, agentAvatarUrls, humanAvatarUrls, ...layout },
  ref,
): JSX.Element {
  // The label is also the pane's title, and both must come from the same
  // presentation rule (see `presentCounterpart`) — so the draft row's face and
  // name are already the ones the minted pair will wear.
  const counterpart = presentCounterpart(raw);
  const badge = directRowBadge(unread);

  return (
    <ConversationRowLayout ref={ref} column="nested" {...layout}>
      <div className="flex size-5.5 shrink-0 items-center justify-center">
        <AuthorAvatarView
          avatar={counterpart.avatar}
          size={16}
          label={counterpart.label}
          agentAvatarUrls={agentAvatarUrls}
          humanAvatarUrls={humanAvatarUrls}
        />
      </div>
      <span className={cn("min-w-0 truncate", layout.selected && "font-medium")}>
        {counterpart.label}
      </span>
      {counterpart.isDeleted ? (
        <span className="flex shrink-0 items-center">
          <DeletedActorMarker />
        </span>
      ) : null}
      {badge ? <UnreadBadge count={badge.count} tier={badge.tier} className="ml-auto" /> : null}
    </ConversationRowLayout>
  );
});
