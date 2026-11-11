import { createContext, useContext, type ReactNode } from "react";
import "./message-row-content.css";

/**
 * The shared, presentational content column for a message row — the single
 * source of truth for WHICH default-visible sections a message shows and in
 * what order / spacing (#task:1173 P1).
 *
 * Both render paths go through this:
 *   - the live `MessageItem` fills each slot with its interactive node
 *     (popover author label, interactive Markdown, clickable attachments, the
 *     interactive `MessageReactions` row);
 *   - the static `MessageScreenshotView` fills each slot with its
 *     button/hook/portal-free node (static author span, `interactive={false}`
 *     Markdown, static attachment tiles, `ReactionChipRow`).
 *
 * ── Anti-drift contract (the point of this component) ──────────────────────
 * This props type IS the registry of a message row's adornments. EVERY slot is
 * a REQUIRED `ReactNode` — there are no optional slots and no defaults. To add
 * a new row adornment you add a new required slot here, and the type then fails
 * to compile until BOTH surfaces declare it explicitly: live by supplying a
 * node, export by supplying a node OR `null`. So "does this appear in the
 * screenshot?" is forced to a compile-time decision, and "added to live,
 * forgot the export" — the exact recurrence this task fixes (reactions were
 * invisible on screenshots) — becomes a build error rather than a silent gap.
 *
 * That is why even genuinely live-only adornments (`taskTag` = task tag,
 * `trailing` = thread-replies badge, `threadPreview` = inline reply preview)
 * are required: the export passes `null`, making its deliberate omission
 * explicit instead of implicit. The single
 * residual hole — inlining an adornment straight into the live chrome wrapper
 * without going through a slot — is left to review + the screenshot parity test
 * (the stricter "content-starve MessageItem" form is a named follow-up).
 *
 * Slots are named by SEMANTICS, never by surface (`reactions`, not
 * `liveReactions` / `exportReactions`): this layer owns order + layout, the
 * surfaces only supply the node.
 */
export interface MessageRowContentProps {
  /** User-right layout (the viewer's own message, flush right). The column marks
   *  itself `data-align="end"`; every part's mirroring is stated once in
   *  `message-row-content.css`, and popovers read `useMessageAlignEnd`. */
  alignRight: boolean;
  /** Outer column width class. Differs by surface BY DESIGN — live is
   *  responsive (`max-w-[85%]`), export is the fixed capture width
   *  (`max-w-[640px] flex-1`) — so it is supplied, not baked in. */
  columnClassName: string;
  /** Author label node (interactive popover trigger live; static span export).
   *  `null` on a grouped continuation row (#task:1737) — the author is stated
   *  once by the group head. */
  author: ReactNode;
  /** Timestamp node (a `<time>`/`<span>` with its intrinsic type styles only;
   *  the positional margin is owned by this layout). `null` on a grouped
   *  continuation row, whose time lives in the row's hover rail instead. */
  timestamp: ReactNode;
  /** Live-only below-content task tag — its own row at the message's bottom
   *  edge, ahead of `reactions` / `trailing` (#task:1185). Export passes
   *  `null`. */
  taskTag: ReactNode;
  /** Rendered markdown body, or `null` on an attachment-only message (empty
   *  content is sendable when an attachment is attached). */
  body: ReactNode;
  /** Attachment list node, or `null` when the message has no attachments. */
  attachments: ReactNode;
  /** Reaction row node, or `null` when there are no reactions. Live supplies
   *  the interactive `MessageReactions`, export the static `ReactionChipRow`. */
  reactions: ReactNode;
  /** Live-only below-content adornment: the thread-replies badge WHEN it stands
   *  alone. `null` once there are reply preview lines for it to head — it then
   *  travels inside `threadPreview`, which is the block the badge counts. Export
   *  passes `null`. */
  trailing: ReactNode;
  /** Live-only inline reply block — the badge as its header line plus the
   *  cluster's most recent replies, under ONE wash, BELOW the bottom adornment
   *  row (shiki 2026-07-31). Export passes `null`. Read-inert (see
   *  ThreadReplyPreview). Mutually exclusive with `trailing`. */
  threadPreview: ReactNode;
  /** Live-only: the own message's hover time, drawn beside its bubble so it hugs
   *  the words it labels however short they are. `null` without a bubble (an
   *  attachment-only message leads its tile strip with it instead) and in the
   *  export. */
  aside: ReactNode;
}

const MessageAlignEnd = createContext(false);

/** Whether the message being rendered sits on the right edge, for the parts
 *  whose popovers must grow inward from it. */
export function useMessageAlignEnd(): boolean {
  return useContext(MessageAlignEnd);
}

export function MessageRowContent({
  alignRight,
  columnClassName,
  author,
  timestamp,
  taskTag,
  body,
  attachments,
  reactions,
  trailing,
  threadPreview,
  aside,
}: MessageRowContentProps): JSX.Element {
  return (
    <MessageAlignEnd.Provider value={alignRight}>
      <div
        data-message-column
        data-align={alignRight ? "end" : undefined}
        className={columnClassName}
      >
        {/* Header renders only when it has something to say. A grouped
          continuation row (#task:1737) passes BOTH slots as null — its author
          is already stated by the group head and its timestamp moved to the
          row's hover rail — so the whole element (and its `mb-1`) drops out
          rather than leaving an empty band above the body. Same
          render-only-when-present shape as the bottom adornment row below. */}
        {author || timestamp ? (
          <header className="mb-1 text-xs">
            {author}
            <span className="ml-2">{timestamp}</span>
          </header>
        ) : null}

        {/* An own message's words keep a straight LEFT edge inside a box that
          hugs them at the right edge: `text-align: right` would rag every line
          after the first and flip lists, tables and code with it (shiki
          2026-08-06). That box is the bubble (#task:1838); the fill belongs to
          the words, so attachments, reactions and the reply block stay outside
          it. `max-w-full` is load-bearing: `w-fit` never goes below its
          content's min-content, and a code fence's `pre` has a min-content of
          its longest line, so without the cap a wide fence would push the
          bubble out of the column instead of scrolling inside it (#task:1852).
          The column itself fills its cap on both sides, which is what gives
          that percentage a definite width to resolve against. */}
        <div data-message-body className="text-sm text-text-primary">
          {/* `body` is null on an attachment-only message (Composer allows
            sending one) — without that the bubble would render as an empty
            padded blob above the attachments. */}
          {body ? (
            alignRight ? (
              <div className="relative ml-auto w-fit max-w-full">
                {aside ? <div className="absolute top-0 right-full mr-3">{aside}</div> : null}
                <div
                  data-own-bubble
                  className="rounded-(--message-bubble-radius) bg-(--bg-message-bubble) px-3 py-2"
                >
                  {body}
                </div>
              </div>
            ) : (
              body
            )
          ) : null}
          {attachments}
        </div>

        {/* One shared bottom row — task tag · replies · reactions on a single
          line, no wrapping (shiki #task:1185). Each child is a bare inline
          element; this row owns the top spacing + alignment + gap, so they
          never stack. Renders only when at least one is present. Reversed for
          own messages (in the stylesheet) so the task tag hugs the reading
          edge. The replies badge is absent from this row when it heads the reply block
          below — grouping it with the lines it counts outranks keeping it here
          (shiki 2026-07-31). */}
        {taskTag || trailing || reactions ? (
          <div data-adornment-row className="mt-1.5 flex flex-nowrap items-center gap-2">
            {taskTag}
            {trailing}
            {reactions}
          </div>
        ) : null}

        {/* The reply block — badge header + preview lines under one wash. Its own
          block below the adornment row: the lines are full-width, so they never
          share a line with anything else. */}
        {threadPreview}
      </div>
    </MessageAlignEnd.Provider>
  );
}
