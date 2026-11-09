import dayjs from "dayjs";
import relativeTime from "dayjs/plugin/relativeTime";
import type {
  MessageAuthorDto,
  ReplyBadgeSummary,
  StreamUnreadDto,
  ThreadReplyPreviewRow,
} from "@silverhand/shared";
import { ClockIcon, ThreadIcon } from "@/icons";
import type { CachedMessage } from "@/features/channels/pending-message";
import { t } from "@/lib/i18n";
import { authorStateLabel, presentAuthor } from "../model/author-presentation";
import { AuthorAvatarView, AuthorName } from "./author-avatar-view";
import { ThreadReplyPreview, visibleThreadReplyRows } from "./thread-reply-preview";

// Idempotent — the reply badge's "{time} ago" label uses dayjs.fromNow().
dayjs.extend(relativeTime);

export interface MessageThreadSummaryInput {
  message: CachedMessage;
  alignRight: boolean;
  threadReplyCount?: number;
  threadReplySummary?: ReplyBadgeSummary;
  threadUnread?: StreamUnreadDto;
  threadReplyPreviews?: ThreadReplyPreviewRow[];
  onStartThread?: (msg: CachedMessage, jumpToMessageId?: string) => void;
  authorAgentAvatarUrls?: Map<string, string | null>;
  authorUserAvatarUrls?: Map<string, string | null>;
}

/** A root message's thread summary: the `N replies` badge, standing on the
 *  row's bottom line (`trailing`) or heading the reply preview block
 *  (`threadPreview`). Exactly one of the two is non-null. */
export function messageThreadSummary({
  message,
  alignRight,
  threadReplyCount,
  threadReplySummary,
  threadUnread,
  threadReplyPreviews,
  onStartThread,
  authorAgentAvatarUrls,
  authorUserAvatarUrls,
}: MessageThreadSummaryInput): {
  trailing: JSX.Element | null;
  threadPreview: JSX.Element | null;
  badgeHeadsPreview: boolean;
} {
  // "{N} Reply/Replies" — pluralized to match the ThreadPanel header
  // (which says "N replies"); the badge previously hard-coded "Reply".
  const replyBadgeLabel =
    threadReplyCount === undefined
      ? ""
      : `${threadReplyCount} ${threadReplyCount === 1 ? t("messages.reply") : t("messages.replies")}`;

  // P1 (#task:1073): when the enriched summary is present, the badge upgrades
  // to avatars + count + last-reply time. `repliers.length > 0` guards the
  // avatar strip; otherwise we keep the count-only label.
  const enrichedReply =
    threadReplySummary && threadReplySummary.repliers.length > 0 ? threadReplySummary : null;
  const replyRelativeTime = enrichedReply ? dayjs(enrichedReply.last_reply_at).fromNow() : "";
  // Surface 3 (#task:1226): per-viewer unread for this reply-cluster, read from
  // the `useConversationUnread` read-model (threaded in as `threadUnread`) — the
  // SINGLE source of truth, NOT the message-window summary (which can't score
  // per-viewer unread). When > 0 the chip's trailing segment swaps the
  // last-reply time for a text-form "N new" modifier in the unread hue (ink,
  // not a filled sticker — see the trailing-node comment for the volume-budget
  // rationale) and the chip click jumps to the first unread reply instead of
  // the thread tail. Follow-agnostic — unread is independent of whether the
  // viewer follows.
  const threadUnreadCount = threadUnread?.unread_count ?? 0;
  const threadFirstUnreadId = threadUnread?.first_unread_message_id ?? null;
  // Unread is the FIRST-priority title, independent of `enrichedReply` — the
  // "N new" visual now renders in both the enriched and fallback branches, so
  // the tooltip/aria-label must announce it in both too (else a fallback chip
  // reads "2 Replies" to a screen reader while the eye sees "2 Replies · 1 new"
  // — #task:1519 P2). Only when there's no unread does the label diverge: the
  // enriched chip appends the last-reply time, the fallback stays bare.
  const replyBadgeTitle =
    threadUnreadCount > 0
      ? t("messages.replyBadgeUnread", { label: replyBadgeLabel, count: threadUnreadCount })
      : enrichedReply
        ? t("messages.replyBadgeLast", { label: replyBadgeLabel, time: replyRelativeTime })
        : replyBadgeLabel;
  // Thread repliers now arrive as the SAME server-authoritative
  // {@link MessageAuthorDto} the byline uses (#task:1791/#task:1792), so their
  // face + name flow through the ONE seam — a deleted replier keeps its snapshot
  // name + neutral avatar, an unresolved one degrades to "Unknown …", and no
  // surface re-joins the directory by a raw id.
  const renderReplierAvatar = (
    author: MessageAuthorDto,
    size: number,
    key: string,
  ): JSX.Element => {
    const p = presentAuthor(author);
    return (
      <AuthorAvatarView
        key={key}
        avatar={p.avatar}
        size={size}
        label={authorStateLabel(p)}
        agentAvatarUrls={authorAgentAvatarUrls}
        humanAvatarUrls={authorUserAvatarUrls}
      />
    );
  };

  // The reply-preview replier NAME carries its deleted state through the ONE
  // shared primitive (#task:1792 B3) — same marker the byline shows — so a
  // visible text name in the preview never drops the state to the avatar
  // tooltip alone. The preview block owns the width cap; this node owns weight /
  // color / marker.
  const renderReplierName = (author: MessageAuthorDto): JSX.Element => (
    <AuthorName presentation={presentAuthor(author)} className="font-medium text-text-secondary" />
  );

  // Inline thread-replies badge — only when the message has an open thread.
  // Carries real information (N replies, click to open), so it earns persistent
  // space below the bubble. Live-only: the export omits it (passes `null`). A
  // bare inline element either way: whichever container it lands in — the shared
  // bottom row or the preview block's header — owns its spacing and alignment.
  //
  // ONE shape, not an enriched/fallback pair: `ThreadIcon` + `N replies` +
  // trailing modifier. The stacked replier avatars were removed once the reply
  // preview began rendering the same identities one line each directly below
  // (shiki 2026-07-31): the same faces twice in ~20px of vertical space read as
  // repetition and crowding, and identity belongs on the line that says what
  // the person actually wrote. The avatars were only ever standing in for
  // content that now exists. `enrichedReply` therefore no longer switches the
  // chip's composition — it only decides whether the last-reply time can be
  // stated (it is the sole carrier of that fact), so the chip no longer changes
  // silhouette when the summary loads.
  //
  // Unread signal as a TEXT modifier, not a filled sticker. The chip is a
  // LOCAL affordance (already a full click target); the loud filled square
  // belongs to the GLOBAL pressure signals (sidebar badge / Threads list
  // followed-row), so borrowing it here overspends the volume budget and
  // crowds the `N replies` subject (shiki dogfood `dSjdBvz4`). Keep the single
  // unread hue as INK (`text-accent` deep magenta, AA on light) + medium
  // weight. Derived SOLELY from `threadUnreadCount` (the authoritative
  // per-stream read-model), so it renders whether or not the enriched summary
  // has loaded — the enriched read-model is a freshness surface, NOT the fact
  // (§12.1#5). Gating it on `enrichedReply` previously made the "N new" chip
  // vanish until the summary refetched (ghost-unread, #task:1519). Click still
  // jumps to first unread.
  const threadUnreadNode =
    threadUnreadCount > 0 ? (
      <span className="shrink-0 whitespace-nowrap font-medium text-(--unread)">
        {t("messages.newCount", { count: threadUnreadCount > 99 ? "99+" : threadUnreadCount })}
      </span>
    ) : null;
  // The badge and the preview lines are ONE object when there are lines to head
  // (shiki 2026-07-31) — see ThreadReplyPreview for why. Resolved once, here,
  // because it decides BOTH where the badge renders and whether the block
  // exists: deriving it separately in each place would let the two answers
  // drift. Same `onStartThread` gate as the badge — a surface that cannot open a
  // thread must not render a target that claims it can.
  const threadPreviewRows = onStartThread ? visibleThreadReplyRows(threadReplyPreviews) : [];
  // The block exists only when it can have its HEADER. Two independent cache
  // maps feed this row — the count (`replyCountsByRoot`) and the tail
  // (`replyPreviewsByRoot`) — so a tail can be present while the count is still
  // absent, either in a transitional apply or from a malformed payload. Deriving
  // the block from the tail ALONE would then render a wash with reply lines and
  // no `N replies` header: the header is the block's click entry and its only
  // expression of unread, so a headerless block is a target that states nothing
  // (Cindy, #task:1751). Requiring both makes that state unrepresentable rather
  // than asserted, and the degradation is the plain root row — the same thing
  // the reader saw one frame earlier.
  const badgeHeadsPreview = threadPreviewRows.length > 0 && threadReplyCount !== undefined;
  const replyBadgeNode =
    threadReplyCount !== undefined && onStartThread ? (
      <button
        type="button"
        data-panel-pill="thread"
        onClick={() => onStartThread(message, threadFirstUnreadId ?? undefined)}
        title={replyBadgeTitle}
        aria-label={replyBadgeTitle}
        className={
          // ONE metric for every box on this row: `leading-none` makes the copy's
          // line box exactly its 12px font size, and both glyphs are `size-3`
          // (12px) to match it, so icon and text share a height instead of the
          // glyph standing 2px taller than the words next to it (shiki
          // 2026-07-31). The row's height is therefore its text metric, not a
          // number chosen for it.
          //
          // `min-w-0` + the children's own no-wrap/truncate rules are what keep
          // that true: this row USED to pin `h-[18px]` while its children were
          // free to wrap, so in a narrow bubble the time segment relaid out to
          // three 12px lines (36px) inside an 18px box and spilled — which is
          // exactly the "height is 30" misalignment. A row that states one fact
          // must stay one line high; when it cannot fit, the time segment gives
          // way (see below) rather than the row growing.
          "inline-flex min-w-0 max-w-full items-center gap-1.5 text-xs leading-none text-text-muted transition-colors hover:text-text-primary focus-outline " +
          // Mirrored ONLY as the preview block's header, and only for an
          // own-message row (#task:1855). The badge's glyph is this block's
          // COLUMN MARKER — it is supposed to sit in the same column as the
          // lines' avatars, which is what makes the badge read as their header
          // — and those lines are themselves `flex-row-reverse` under
          // `alignRight`, so their avatars are in the RIGHT column. Left
          // unmirrored, the header's marker sat a whole line-width away from
          // the faces it heads.
          //
          // Deliberately NOT applied to the standalone pill: there the glyph
          // has no column to line up with, and `icon → N replies → freshness`
          // is one PHRASE (type marker, subject, modifier), which reads
          // left-to-right on either side of the room. user-right mirrors the
          // ARRANGEMENT of siblings (the bottom adornment row reverses so the
          // task tag hugs the reading edge; the reaction row justifies to the
          // end) and never the word order inside a single label — the same
          // reason the prose keeps `text-align: left` in a right-hugging
          // bubble (shiki 2026-08-06). Reaction chips are the other precedent:
          // `emoji → count` does not flip either.
          (badgeHeadsPreview && alignRight ? "flex-row-reverse " : "") +
          // Its own fill / corner / inset ONLY when it stands alone. Inside the
          // preview block the wrapper owns all three: two stacked translucent
          // fills of the same token compose darker and would redraw the seam
          // that block exists to remove. The glyph then sits in the same
          // column as the lines' avatars — left, or right on an own-message
          // row (see the mirror above) — so the badge reads as their header.
          //
          // The fixed height belongs to the same branch, because it is PILL
          // geometry (fill + corner + inset need breathing room around the
          // 12px content), not a property of the row: as the preview block's
          // header there is no fill, so 18px would only add 6px of dead space
          // above lines that are themselves content-height.
          (badgeHeadsPreview
            ? ""
            : "h-4.5 rounded-(--chip-radius) bg-(--chip-bg) px-2 hover:bg-(--chip-bg-hover)")
        }
      >
        <ThreadIcon className="size-3 shrink-0" />
        {/* The subject of the row: it never wraps and never shrinks, so the
              count stays readable at any row width. */}
        <span className="shrink-0 whitespace-nowrap font-medium">{replyBadgeLabel}</span>
        {threadUnreadNode ??
          (enrichedReply ? (
            // Freshness is the row's DECORATION, so it is the segment that
            // gives way when the row is too narrow: it shrinks and truncates
            // (`truncate` also forbids the wrap that used to make this span
            // three lines tall inside a one-line row). The clock keeps its
            // 12px box so the glyph matches the words beside it.
            <span className="inline-flex min-w-0 shrink items-center gap-0.5 text-text-muted">
              <ClockIcon className="size-3 shrink-0" />
              <span className="truncate">{replyRelativeTime}</span>
            </span>
          ) : null)}
      </button>
    ) : null;
  // Exactly one of these is non-null: the badge either stands on the shared
  // bottom line (#task:1185) or heads the preview block. `badgeHeadsPreview`
  // now implies the badge EXISTS, so the block can never be handed a null
  // header (#task:1751).
  const trailingNode = badgeHeadsPreview ? null : replyBadgeNode;
  // The cluster's most recent replies, one line each (#task:1745). Read-inert by
  // construction: no cursor is touched here (see ThreadReplyPreview).
  const threadPreviewNode = badgeHeadsPreview ? (
    <ThreadReplyPreview
      rows={threadPreviewRows}
      badge={replyBadgeNode}
      alignRight={alignRight}
      title={replyBadgeTitle}
      onOpen={() => onStartThread?.(message, threadFirstUnreadId ?? undefined)}
      renderAuthorAvatar={renderReplierAvatar}
      renderAuthorName={renderReplierName}
    />
  ) : null;

  return { trailing: trailingNode, threadPreview: threadPreviewNode, badgeHeadsPreview };
}
