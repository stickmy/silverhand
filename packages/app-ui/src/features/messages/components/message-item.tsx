import { useEffect, useState } from "react";
import dayjs from "dayjs";
import { CheckIcon } from "@/icons";
import type {
  AgentDto,
  AttachmentSnapshot,
  MessageAuthorDto,
  ReplyBadgeSummary,
  StreamUnreadDto,
  ThreadReplyPreviewRow,
  UserDto,
} from "@silverhand/shared";
import { useNavActions } from "@/router";
import { isCurrentUserId, type UserNameResolver } from "@/features/session/current-user";
import { Markdown } from "./markdown";
import { MessageAuthorAvatar, MessageAuthorLabel } from "./message-author";
import { MessageHoverToolbar } from "./message-hover-toolbar";
import { messageThreadSummary } from "./message-thread-summary";
import { presentAuthor, classifyRowAuthor } from "../model/author-presentation";
import type { ChannelReference } from "./channel-ref";
import type { WorkspaceChannelReference } from "./conversation-ref";
import { MessageContextMenu } from "./message-context-menu";
import type { CachedMessage } from "@/features/channels/pending-message";
import { isCommittedMessage } from "@/features/channels/pending-message";
import { MessageReactions } from "./message-reactions";
import { MessageRowContent } from "./message-row-content";
import { useToggleReaction } from "@/features/channels/reaction-hooks";
import { useMessageLayoutMode } from "../hooks/use-message-layout-mode";
import { useCoarsePointer } from "@/navigation/layout";
import { MessageTaskTag } from "./message-task-tag";
import { AttachmentList } from "./message-attachments";
import { t } from "@/lib/i18n";

export interface MessageItemProps {
  message: CachedMessage;
  /** Conversation this message belongs to. Forwarded to AgentInfoPopover so
   *  the author/avatar popover can show per-conversation presence + retry. */
  conversationId: string;
  /** Looked up by `from_agent_id` to resolve display name + runtime
   *  (avatar) and also by mention name (via `agentsByName`). */
  agentsById: Map<string, AgentDto>;
  /** Lookup by `agent.name` (the @mention handle). Threaded to the
   *  Markdown component so inline `@handle` references can resolve. */
  agentsByName: Map<string, AgentDto>;
  /** Lookup by `user.name` in the same @mention namespace as agents. */
  humansByName?: Map<string, UserDto>;
  /** Lookup by `users.id`, used to resolve user-authored message rows. */
  humansById?: Map<string, UserDto>;
  /** Lookup by `channel.title` (the #handle used in inline refs).
   *  `null` value = ambiguity sentinel (multiple channels share that
   *  title in this workspace) → chip degrades to inert text. Threaded
   *  to the Markdown component so inline `#name` references render
   *  as clickable pills when unique. */
  channelsByName: Map<string, ChannelReference | null>;
  /** Click handler for resolved channel refs. Wired by the parent
   *  to navigate the right workspace via `nav.toChannel`. */
  onOpenChannelRef?: (channelId: string) => void;
  /** Lookup by conversation id for inline `#conv:<id>` references. */
  conversationRefsById?: Map<string, WorkspaceChannelReference>;
  onOpenConversationRef?: (id: string) => void;
  /** Open a `.md` / `.html` attachment in its in-app previewer tab (#task:2122),
   *  open-or-focus on the attachment id. Wired by the parent to the
   *  conversation's tab controller. Absent (e.g. thread pane, or view tabs
   *  disabled) → md/html keep the open-with-OS behaviour of every other type. */
  onOpenAttachment?: (attachment: AttachmentSnapshot) => void;
  onStartThread?: (msg: CachedMessage, jumpToMessageId?: string) => void;
  /** When this message has an open thread anchored to it, the count of
   *  non-system replies in that thread. Drives the button label
   *  `{count} Reply` (clicking opens the existing thread instead of
   *  creating a new one — handler short-circuits the POST). Undefined
   *  = no thread, button shows "Thread". */
  threadReplyCount?: number;
  /** Enriched reply-badge data for this root (P1, #task:1073). When present
   *  the inline badge upgrades from "{N} Replies" to a Slack-style strip:
   *  stacked replier avatars + count + last-reply time. Absent → count-only
   *  fallback (driven by `threadReplyCount`). */
  threadReplySummary?: ReplyBadgeSummary;
  /** Per-viewer server-scored unread for this root's reply-cluster, from the
   *  `useConversationUnread` read-model (#task:1226). The SINGLE source of the
   *  chip's "N new" + jump-to-first-unread — unread is server-scored and no
   *  longer lives in `threadReplySummary` (which keeps only the optimistically
   *  patchable count / last-reply / repliers). Absent / sparse-missing ⇒ 0. */
  threadUnread?: StreamUnreadDto;
  /** Most recent replies of this root's cluster (at most 3, ascending time),
   *  rendered as one line each under the row (#task:1745). Mainline-only: the
   *  ThreadPanel shows the real replies, so it never passes this. Absent ⇒ no
   *  preview block, which is also the degradation when the backend sidecar
   *  (#task:1746) is not present — the chip alone stays correct.
   *
   *  READ-INERT: rendering it advances no cursor (`docs/thread-unread-v1.md`
   *  §6 / invariant 2). See `ThreadReplyPreview`. */
  threadReplyPreviews?: ThreadReplyPreviewRow[];
  onSave?: (msg: CachedMessage) => void;
  saved?: boolean;
  /** Whether the viewer may add/remove reactions here. False in a closed or
   *  archived (read-only) channel: the backend rejects the write, so the
   *  React entry, the inline add chip, and chip click-to-toggle are hidden —
   *  existing reaction chips stay visible as display-only counts
   *  (#task:1122 dogfood follow-up). */
  canReact?: boolean;
  /** Whether the viewer is a member who may perform member-only per-message
   *  actions: START a new thread (the hover "Thread" button) and EXPORT an
   *  attachment (download / open-external). Defaults to `true`; `false` for a
   *  non-member browsing a public channel. Opening an EXISTING
   *  thread (the reply badge) and inline attachment PREVIEW stay available —
   *  those are reads. Save/unsave is gated separately via `onSave` (undefined
   *  hides it). */
  canParticipate?: boolean;
  /** Whether the viewer may WRITE the linked task's status from its inline
   *  chip (the "Move to" picker). The write axis, mirroring `canReact`:
   *  `true` only for a member of an open channel. Defaults to `false`
   *  (fail-closed) — a non-member browsing a public channel, or any read-only
   *  surface (screenshot / saved / search preview), still SEES the chip with
   *  identical look + content, but the status picker is inert. Reading the
   *  task's state is public; only the mutation entry is member-gated. */
  canMutateLinkedTask?: boolean;
  /** Object URL pointing at the current viewer's avatar blob (or null
   *  if the viewer has no avatar / isn't loaded yet). Lifted into the
   *  parent pane so a single fetch drives every message render in the
   *  list. Resolved by `useViewerAvatarObjectUrl`. */
  viewerAvatarObjectUrl?: string | null;
  /** The current viewer's user row. When present, viewer-authored
   *  message rows wrap their avatar + author label in a
   *  `HumanInfoPopover` so hover gives the same identity card the
   *  member chip / `@mention` surfaces show. Undefined → registration
   *  not complete or still loading; the row falls back to the bare
   *  avatar/label. */
  viewer?: UserDto;
  /** Byline + thread-replier avatar bytes, derived by the parent pane STRICTLY
   *  from the rendered rows' own `MessageAuthorDto`s (#task:1792 B2) via
   *  `useAuthorAvatarObjectUrls` — never a live-directory join. Split by kind.
   *  Only the active author arm carries `avatar_url`, so a deleted / unresolved
   *  author draws its neutral glyph with no lookup and an active author whose
   *  account has left the directory still renders its own snapshot avatar. */
  authorAgentAvatarUrls?: Map<string, string | null>;
  authorUserAvatarUrls?: Map<string, string | null>;
  /** Directory-backed agent avatar batch (workspace `useAgents`), for the
   *  INDEPENDENT axis of inline `@handle` mention pills inside the Markdown
   *  body — a different actor reference than the row's own author. Missing
   *  entries / `null` fall back to the runtime icon. */
  agentAvatarUrls?: Map<string, string | null>;
  /** Persistently render the hover frame around this row — used by the
   *  parent pane to mark "the message a thread is anchored to" while
   *  the thread pane is open. Doesn't expose the action bar, since the
   *  highlight is informational, not interactive. */
  highlighted?: boolean;
  /** Render this row as a CONTINUATION of the message above it — the second and
   *  later rows of a consecutive-same-author run (#task:1737). The avatar and
   *  the author name drop out (the group head already said who), the row's
   *  vertical padding tightens so the run reads as one block, and this row's
   *  timestamp moves into the leading rail where hover reveals it. The decision
   *  is NOT made here: it comes from `authorGroupContinuations`, which is the
   *  single rule shared by the channel stream and the ThreadPanel. Defaults to
   *  false — a row with no known predecessor renders its full header. */
  continuesPrevious?: boolean;
  /** One-shot "jump flash": render the `.msg-jump-flash` animation class
   *  on this row. Declarative (not an imperative `classList.add`) so the
   *  flash survives a Virtuoso remount / refetch-driven window reset — the
   *  failure mode that swallowed the flash when the jump target was already
   *  on-screen (#task:1076). Whichever DOM node renders this id picks the
   *  class up from props and plays the animation from the start. */
  jumpFlash?: boolean;
  /** When the parent's MessageList is in multi-select mode this row
   *  renders a checkbox column on the leading side (mirrored when
   *  `alignRight`). Idle mode: no checkbox, no extra column. */
  selectionMode?: boolean;
  /** Whether this row is currently in the selection set. Drives the
   *  checkbox visual + the row's selected highlight. */
  selected?: boolean;
  /** Toggle this row's selection membership. Required whenever
   *  `selectionMode` is true. */
  onToggleSelected?: (msg: CachedMessage) => void;
  /** Right-click → Copy markdown menu item. Hidden when undefined. */
  onCopyMarkdown?: (msg: CachedMessage) => void;
  /** Right-click → Share menu item. Hidden when undefined; the
   *  parent only passes this in once the export pipeline is live so
   *  the user never sees a half-baked Share entry (Cindy review). */
  onShare?: (msg: CachedMessage) => void;
  /** Right-click → "Unfollow thread" menu item. Passed in only by the
   *  ThreadPanel and only while the human viewer is following this reply
   *  cluster, so the entry never appears on main-line rows or unfollowed
   *  threads (item 5). */
  onUnfollowThread?: (msg: CachedMessage) => void;
  /** Resolve a `user_id` → display name for surfaces that don't already
   *  carry the user dto (task assignees, foreign-author fallback). Built
   *  by the parent from `useUsersById()` (multi-human audit 2026-06-03
   *  #task:857 P0.10). Optional — when absent, formatUserDisplayName
   *  falls back to the raw id, matching v1 behaviour. */
  userResolver?: UserNameResolver;
  /** In-conversation header find (#task:1335). When set, every occurrence of
   *  this term in the row's markdown body is highlighted (`<mark>`). Threaded
   *  straight to `<Markdown>`. Empty / absent → no highlight. */
  searchQuery?: string;
  /** This row is the CURRENT prev/next find match: it takes the framed look
   *  (same `border-accent bg-bg-floating` as `highlighted`) and its keyword
   *  marks read as "current" via the `data-search-active` hook. */
  searchActive?: boolean;
}

/**
 * Renders a non-system message (user or agent) as a hoverable row with
 * markdown body, role-name + absolute time header, and a Slack-style
 * pair of action affordances:
 *
 * - **Frame-corner action controls** (Save + Thread) — absolutely
 *   pinned to the top-right of the OUTER hover frame (mirrored to
 *   top-left in user-right mode), each rendered as an icon-only
 *   button using `ActionButton` (the label survives as `title` +
 *   `aria-label` for tooltip / screen readers). They sit on the
 *   framed card, NOT over the markdown body. Default hidden via
 *   `pointer-events-none opacity-0`; revealed on row hover OR
 *   `focus-within`, so a keyboard user can tab to them. They do not
 *   reserve vertical space, so two consecutive messages now sit
 *   tight against each other instead of being padded apart by an
 *   always-on action row.
 * - **Inline "{N} Reply" badge** — rendered below the body only when
 *   the message has an open thread. Carries real information, so it
 *   earns persistent space.
 *
 * Layout: respects `useMessageLayoutMode`. In `'user-right'` mode the
 * current user's messages flush right with no avatar or byline (the bubble
 * already says who wrote them; their time is hover-revealed); in `'all-left'`
 * everything is left-aligned.
 */
/**
 * The message row's HOOKS-FREE classifier boundary (#task:1792, Cindy). It runs
 * NO hooks and switches on the actor-row classification, so the actor arm mounts
 * a distinct component (`ActorMessageItem`) that always runs a fixed hook set. A
 * carrier that hydrates `null → actor` for the same message key is then a clean
 * unmount→mount across component TYPES — never a changing hook count on one
 * instance (which React rejects with "Rendered more hooks than during the
 * previous render"). A conditional early-return inside the hook-bearing body
 * could not give that guarantee once `missing` (carrier absent) is reachable.
 *
 *  - `actor`   → the full row.
 *  - `system`  → nothing. The list/thread parents already route `from_kind ===
 *                'system'` to SystemMessage, so this arm is unreachable here; it
 *                stays inert rather than inventing a system byline.
 *  - `missing` → a fail-closed diagnostic (carrier absent, or a raw-vs-DTO kind
 *                mismatch). Never fabricates an identity, and is not the actor
 *                hook lifecycle.
 */
export function MessageItem(props: MessageItemProps): JSX.Element | null {
  const rowAuthor = classifyRowAuthor(props.message.from_kind, props.message.author);
  if (rowAuthor.kind === "actor") {
    return <ActorMessageItem {...props} author={rowAuthor.author} />;
  }
  if (rowAuthor.kind === "missing") {
    return <MissingAuthorRow message={props.message} reason={rowAuthor.reason} />;
  }
  return null; // 'system' — parents route these to SystemMessage.
}

function MissingAuthorRow({
  message,
  reason,
}: {
  message: CachedMessage;
  reason: "absent" | "kind-mismatch";
}): null {
  useEffect(() => {
    console.error(
      `[MessageItem] actor row ${message.id} has no presentable author ` +
        `(${reason}); rendered inert. from_kind=${message.from_kind}`,
    );
  }, [message.id, message.from_kind, reason]);
  return null;
}

/** The actor-row author + all shared props. `author` is the NON-NULL resolved
 *  DTO the wrapper narrowed to (active / deleted / unresolved); this component
 *  never sees `null`, so it runs a FIXED hook set unconditionally. */
type ActorMessageItemProps = MessageItemProps & { author: MessageAuthorDto };

function ActorMessageItem({
  message,
  author,
  conversationId,
  agentsById,
  agentsByName,
  humansByName,
  humansById,
  channelsByName,
  onOpenChannelRef,
  conversationRefsById,
  onOpenConversationRef,
  onOpenAttachment,
  onStartThread,
  threadReplyCount,
  threadReplySummary,
  threadUnread,
  threadReplyPreviews,
  onSave,
  saved = false,
  canReact = true,
  canParticipate = true,
  canMutateLinkedTask = false,
  highlighted = false,
  continuesPrevious = false,
  jumpFlash = false,
  viewerAvatarObjectUrl = null,
  viewer,
  authorAgentAvatarUrls,
  authorUserAvatarUrls,
  agentAvatarUrls,
  selectionMode = false,
  selected = false,
  onToggleSelected,
  onCopyMarkdown,
  onShare,
  onUnfollowThread,
  userResolver,
  searchQuery,
  searchActive = false,
}: ActorMessageItemProps): JSX.Element {
  const isUser = message.from_kind === "user";
  // A pending optimistic row has no server id/seq: every action that needs a
  // real server row (reaction, thread, save, share, copy, select) is not just
  // no-op'd in the handler — its affordance is not rendered at all, so the row
  // reads as "not operable yet" instead of showing a dead button (#task:1267
  // slice B, Cindy review). The action-surface handlers keep their committed
  // guard as defense-in-depth, but rendering is gated here.
  const committed = isCommittedMessage(message);
  const isCurrentUser = isUser && isCurrentUserId(message.from_user_id);
  const mode = useMessageLayoutMode();
  const alignRight = mode === "user-right" && isCurrentUser;
  // Identity / label / avatar all read the server-authoritative author DTO
  // (#task:1792) — NEVER a live-directory re-join by `from_*_id`, which loses a
  // hard-deleted actor's name and would guess "deleted" from a roster miss. The
  // DTO carries a send-time snapshot, so a deleted author stays named + marked;
  // an `unresolved` author (no FK, no snapshot) is a neutral "Unknown …". Raw
  // provenance (`from_user_id`) still drives viewer→"you" alignment above — that
  // is about the CURRENT viewer, not the author's display identity.
  const presentation = presentAuthor(author);
  // The live actor row is looked up ONLY for an active author's hover popover
  // (presence + retry need the full dto). A deleted / unresolved author has no
  // live row and no profile to open, so neither is fetched. The viewer's own
  // row wins for a self-send so the popover matches the member chip.
  const liveAgent =
    presentation.status === "active" && presentation.kind === "agent"
      ? agentsById.get(presentation.navigableId ?? "")
      : undefined;
  const liveHuman =
    presentation.status === "active" && presentation.kind === "user"
      ? isCurrentUser
        ? (viewer ?? humansById?.get(presentation.navigableId ?? ""))
        : humansById?.get(presentation.navigableId ?? "")
      : undefined;

  const viewerAvatarOverride =
    isCurrentUser && viewerAvatarObjectUrl ? viewerAvatarObjectUrl : undefined;
  const bylineAvatarUrl =
    presentation.avatar.variant === "agent"
      ? (authorAgentAvatarUrls?.get(presentation.avatar.id) ?? null)
      : presentation.avatar.variant === "user"
        ? (viewerAvatarOverride ?? authorUserAvatarUrls?.get(presentation.avatar.id) ?? null)
        : null;

  const createdAt = dayjs(message.created_at);
  const timeLabel = createdAt.format("MM/DD HH:mm");
  const timeTooltip = createdAt.format("YYYY-MM-DD HH:mm:ss");

  const continuationTimeLabel = createdAt.format("HH:mm");
  const { openAgentPanel: navigateToAgent, openHumanPanel: navigateToHuman } = useNavActions();
  const taskTag =
    message.linked_task !== null ? (
      <MessageTaskTag
        message={message}
        agentsById={agentsById}
        userResolver={userResolver}
        canMutate={canMutateLinkedTask}
      />
    ) : null;
  const [pointerHover, setHover] = useState(false);
  // WebKit on touch screens follows a scroll or tap with compatibility
  // events typed "mouse" at the finger's last spot, so the event type cannot
  // tell a finger from a mouse; a touch-first device gets no hover at all and
  // reaches the same actions through the long-press menu.
  const coarsePointer = useCoarsePointer();
  const hover = pointerHover && !coarsePointer;
  // Pins the row "framed" + keeps the hover-revealed corner controls visible
  // while a reaction picker (corner or inline) is open — the picker portals
  // out, so the row would otherwise collapse from under it.
  const [reactionPickerOpen, setReactionPickerOpen] = useState(false);
  const toggleReaction = useToggleReaction();
  // The reaction delta's routing identity, read off this row (#task:1753) — the
  // ACK's authoritative recovery needs the channel + thread context, not just the
  // message id, to reach every projection that displays the row.
  const reactionTarget = {
    channelId: message.conversation_id,
    messageId: message.id,
    threadRootId: message.thread_root_id,
  };
  // Visual-only: highlight forces the framed look; only real hover
  // reveals the action bar. Selection-mode also keeps the frame on
  // selected rows so the user can see at a glance which messages are
  // in the export queue.
  const framed =
    hover || highlighted || reactionPickerOpen || searchActive || (selectionMode && selected);
  // Accent border is reserved for the emphasized subset (jumped-to / current
  // search match / export-selected). Plain hover and the reaction picker are
  // utility affordances and take the neutral row-hover border instead.
  const emphasized = highlighted || searchActive || (selectionMode && selected);

  const { trailing: trailingNode, threadPreview: threadPreviewNode } = messageThreadSummary({
    message,
    threadReplyCount,
    threadReplySummary,
    threadUnread,
    threadReplyPreviews,
    onStartThread,
    authorAgentAvatarUrls,
    authorUserAvatarUrls,
  });

  // ── MessageRowContent slot nodes ────────────────────────────────────────
  // The shared content layer owns section order + layout; MessageItem fills
  // each (required) slot with its INTERACTIVE node. Anything default-visible
  // must flow through a slot here — that is the anti-drift contract with the
  // static export path (see MessageRowContent). `taskTag`/`trailing` are
  // live-only adornments; the export passes `null` for them explicitly.
  const authorNode = (
    <MessageAuthorLabel
      presentation={presentation}
      conversationId={conversationId}
      liveAgent={liveAgent}
      liveHuman={liveHuman}
      avatarObjectUrl={bylineAvatarUrl}
      onNavigateAgent={navigateToAgent}
      onNavigateHuman={navigateToHuman}
    />
  );
  const timestampNode = (
    <time
      dateTime={new Date(message.created_at).toISOString()}
      title={timeTooltip}
      className="text-(length:--text-meta) text-text-muted"
    >
      {timeLabel}
    </time>
  );
  // `null` when the message is attachment-only (the composer allows an empty
  // draft as long as an upload finished): rendering an empty Markdown tree is
  // invisible, but the own-message bubble it now sits in is not — it would be
  // a padded blob above the attachments (#task:1838).
  const bodyNode = message.content.trim() ? (
    <Markdown
      content={message.content}
      conversationId={conversationId}
      agentsByName={agentsByName}
      humansByName={humansByName}
      agentAvatarUrls={agentAvatarUrls}
      channelsByName={channelsByName}
      onOpenChannelRef={onOpenChannelRef}
      conversationRefsById={conversationRefsById}
      onOpenConversationRef={onOpenConversationRef}
      searchQuery={searchQuery}
    />
  ) : null;
  /* Continuation rail — occupies the avatar column's exact WIDTH (`w-8`)
     so the body lands on the same text baseline as the group head's, but
     only ONE body line of HEIGHT: a taller rail would pad the row back out
     and undo the tightening that makes a run read as one block. The line box
     is taken off the same Appearance token `.markdown-body` uses (see
     `[data-message-continuation-rail]` in chrome.css), so it still matches the
     body's first line at any font-size slider value.

     It carries THIS row's own clock time, revealed on row hover. An own
     right-aligned message shows it too, beside its bubble or ahead of its
     first tile: alignment and the bubble already say who wrote it, so it has
     no avatar or byline. Driven by
     the row's `hover` state rather than a `group-hover:` variant — Tailwind's
     variant ties with the base opacity class and loses in the compiled sheet.
     Only opacity animates, so the time stays in the a11y tree throughout. */
  const hoverTime = (
    <time
      data-message-continuation-rail
      dateTime={new Date(message.created_at).toISOString()}
      title={timeTooltip}
      className={
        // Either side, the time hugs the text it labels; beside a bubble it
        // drops by the bubble's padding to sit on its first line.
        "w-8 shrink-0 select-none text-right text-(length:--text-badge) text-text-muted transition-opacity " +
        (alignRight && bodyNode ? "pt-2 " : "") +
        (hover ? "opacity-100" : "opacity-0")
      }
    >
      {continuationTimeLabel}
    </time>
  );

  const attachmentsNode =
    message.attachments && message.attachments.length > 0 ? (
      <AttachmentList
        attachments={message.attachments}
        lead={alignRight && !bodyNode ? hoverTime : null}
        canExport={canParticipate}
        onOpenAttachment={onOpenAttachment}
      />
    ) : null;
  // Null when the message has no reactions — keeps the shared bottom row
  // (task tag · replies · reactions) from rendering an empty slot.
  const reactionsNode =
    isCommittedMessage(message) && message.reactions && message.reactions.length > 0 ? (
      <MessageReactions
        message={message}
        canReact={canReact}
        onReact={(emoji, remove) =>
          toggleReaction.mutate({ target: reactionTarget, emoji, remove })
        }
        onPickerOpenChange={setReactionPickerOpen}
      />
    ) : null;

  const row = (
    <div
      id={`msg-${message.id}`}
      data-message-row
      data-search-active={searchActive || undefined}
      onPointerEnter={(event) => {
        if (event.pointerType === "mouse") {
          setHover(true);
        }
      }}
      onPointerLeave={(event) => {
        if (event.pointerType === "mouse") {
          setHover(false);
        }
      }}
      onClickCapture={
        // shiki `0eolIYXZmVVYAjrM`: when MessageList is in selection
        // mode, clicking anywhere on the row toggles selection — not
        // just the checkbox. Capture phase is intentional: it fires
        // BEFORE any nested onClick (TaskRef navigation, attachment
        // open, link follow), so `preventDefault + stopPropagation`
        // suppresses every child interaction in selection mode. The
        // checkbox's own onClick is also intercepted; the row's toggle
        // handler covers it. Outside selection mode this branch is a
        // no-op so links / attachments / popovers still work.
        selectionMode && committed
          ? (event) => {
              event.preventDefault();
              event.stopPropagation();
              onToggleSelected?.(message);
            }
          : undefined
      }
      className={
        // Row radius routed via `--row-radius` token (#task:912 review
        // round 2). The framed (saved/replying) outer ring picks up
        // 6px in slack-dark and stays 0 in warm/poster — the soft hover
        // box is invisible at 0 radius anyway, so consistency here is
        // about *framed* presentations.
        //
        // A grouped continuation row (#task:1737) tightens its vertical
        // padding so the run reads as ONE block instead of N evenly-spaced
        // rows; the horizontal padding and the leading-column width are
        // untouched, so every body in the group stays on the same text
        // baseline as its head.
        "relative flex items-start gap-3 rounded-(--row-radius) border-2 px-2 transition-colors " +
        (continuesPrevious ? "py-0.5 " : "py-1.5 ") +
        (jumpFlash ? "msg-jump-flash " : "") +
        (selectionMode && committed ? "cursor-pointer " : "") +
        (alignRight ? "flex-row-reverse" : "flex-row") +
        " " +
        (framed
          ? emphasized
            ? "border-accent bg-bg-floating"
            : "border-(--border-row-hover) bg-(--row-frame-hover-bg)"
          : "border-transparent")
      }
    >
      {selectionMode && committed ? (
        <SelectionCheckbox checked={selected} onChange={() => onToggleSelected?.(message)} />
      ) : null}
      {alignRight ? null : continuesPrevious ? (
        hoverTime
      ) : (
        <MessageAuthorAvatar
          presentation={presentation}
          avatarObjectUrl={bylineAvatarUrl}
          liveAgent={liveAgent}
          liveHuman={liveHuman}
          conversationId={conversationId}
          onNavigateAgent={navigateToAgent}
          onNavigateHuman={navigateToHuman}
        />
      )}
      <MessageRowContent
        alignRight={alignRight}
        // The column FILLS its 85% cap on both sides. A fixed column is what
        // gives every percentage inside it (bubble, tiles, fences, tables, the
        // reply wash) a definite width; the own side's end alignment is the
        // column's business (`data-align`), not each part's. The own side also
        // stops short of the other side's text edge: a row spends avatar + gap
        // (2.75rem) on its leading side, so leaving that twice keeps an own
        // bubble from starting left of where others' text starts on a phone.
        columnClassName={
          "w-full min-w-0 " + (alignRight ? "max-w-[min(85%,calc(100%-5.5rem))]" : "max-w-[85%]")
        }
        /* A continuation row states neither author nor time in the content
           column: the group head already named the author, and this row's own
           time sits in the hover rail beside it. Both null ⇒ MessageRowContent
           drops the whole header element (#task:1737). */
        author={continuesPrevious || alignRight ? null : authorNode}
        timestamp={continuesPrevious || alignRight ? null : timestampNode}
        taskTag={taskTag}
        body={bodyNode}
        attachments={attachmentsNode}
        reactions={reactionsNode}
        trailing={trailingNode}
        threadPreview={threadPreviewNode}
        aside={alignRight && bodyNode ? hoverTime : null}
      />

      {/* Hover actions (react, thread, save) — one framed toolbar on the row's
          top-right corner for every message, own right-aligned ones included,
          straddling the top edge so it covers neither this body nor the row's
          height. The full-height track keeps it sticky inside the row: on a
          message taller than the viewport it stays pinned under the pane's
          top while the row is on screen. It is a descendant of the row, so
          moving onto the part outside the row keeps `hover`.

          Visibility is React state (hover / focus-within / open picker):
          Tailwind's `focus-within:` ties with the base `opacity-0` and loses.

          Thread is omitted when the message already has replies — the inline
          "{N} Reply" badge is that entry. */}
      {committed ? (
        <MessageHoverToolbar
          shown={hover || reactionPickerOpen}
          onPickerOpenChange={setReactionPickerOpen}
          onReact={
            canReact
              ? (emoji) => toggleReaction.mutate({ target: reactionTarget, emoji, remove: false })
              : undefined
          }
          onStartThread={
            onStartThread && threadReplyCount === undefined && canParticipate
              ? () => onStartThread(message)
              : undefined
          }
          saved={saved}
          onSave={onSave ? () => onSave(message) : undefined}
        />
      ) : null}
    </div>
  );

  return (
    <MessageContextMenu
      trigger={row}
      text={committed ? message.content : undefined}
      saved={saved}
      onCopyMarkdown={committed && onCopyMarkdown ? () => onCopyMarkdown(message) : undefined}
      onToggleSave={committed && onSave ? () => onSave(message) : undefined}
      onShare={committed && onShare ? () => onShare(message) : undefined}
      onUnfollowThread={committed && onUnfollowThread ? () => onUnfollowThread(message) : undefined}
      onReact={
        committed && canReact
          ? (emoji) => toggleReaction.mutate({ target: reactionTarget, emoji, remove: false })
          : undefined
      }
      onReplyInThread={
        committed && onStartThread && canParticipate ? () => onStartThread(message) : undefined
      }
    />
  );
}

/**
 * Visual checkbox column shown when MessageList is in selection mode.
 * Uses a real `<button>` for keyboard access; `aria-pressed` reflects
 * the selection state. We deliberately don't use a native checkbox
 * because the row's flex layout + theme tokens are easier to control
 * with a styled button.
 */
function SelectionCheckbox({
  checked,
  onChange,
}: {
  checked: boolean;
  onChange: () => void;
}): JSX.Element {
  return (
    <button
      type="button"
      role="checkbox"
      aria-checked={checked}
      aria-label={checked ? t("messages.deselectMessage") : t("messages.selectMessage")}
      onClick={onChange}
      className={
        "mt-1.5 inline-flex size-4 shrink-0 items-center justify-center rounded-(--control-radius) border transition-colors focus-outline " +
        (checked
          ? "border-accent bg-accent text-bg-floating"
          : "border-border bg-bg-floating text-transparent hover:border-accent")
      }
    >
      <CheckIcon className="size-3" strokeWidth={3} />
    </button>
  );
}
