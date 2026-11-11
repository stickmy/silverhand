import type { ReactNode } from "react";
import { plainTextFromMarkdown } from "@/lib/plain-text-from-markdown";
import type { MessageAuthorDto, ThreadReplyPreviewRow } from "@silverhand/shared";
import { AttachmentIcon } from "@/icons";
import { t } from "@/lib/i18n";

/**
 * The wire row is the SHARED contract (`@silverhand/shared`), not a renderer
 * restatement of it (Alice, #task:1746): a second local declaration of the same
 * shape type-checks fine while silently drifting the day the DTO changes. The
 * field-level contracts — `seq` is an ordering key and never a version key,
 * `preview` is a normalized Markdown SOURCE excerpt (so it is rendered as
 * ordinary text, never re-parsed as Markdown), `first_attachment_label` is the
 * canonical `Image #N` / `File #N` from the shared `resolveAttachmentLabels`
 * SSOT rather than a filename — are documented on that schema. What this file
 * owns is how those facts are DISPLAYED.
 */

/** Product contract (shiki): at most the 3 most recent replies. Enforced here
 *  as a tail slice so an over-long payload (or an optimistic append that has
 *  not been trimmed yet) still renders the NEWEST three rather than dropping
 *  the newest. */
const MAX_PREVIEW_LINES = 3;

/**
 * "The 3 most recent" as a deterministic function of `seq` rather than of
 * payload order — so an out-of-order WS append or a replayed frame cannot
 * promote a stale reply into the window or render one twice (Cindy,
 * #task:1746). Sort ascending, render the tail three.
 *
 * Exported because the CALLER needs the resolved line count before it renders:
 * an empty result means the reply badge stands alone in the bottom adornment
 * row, a non-empty one means the badge becomes this block's header line inside
 * the shared wash. Deriving that twice (once to decide, once to render) would
 * let the two answers drift.
 *
 * The `message_id` collapse is IDEMPOTENT CONVERGENCE, not version arbitration:
 * a given `message_id` always carries the same `seq` and the same persisted row,
 * so a duplicate is the same line arriving twice and either copy is equally
 * correct. A duplicate id bearing a DIFFERENT `seq` is malformed input, not a
 * newer edit — this block does not, and must not, rank it. Bounded input, so
 * this is a few comparisons per root.
 */
export function visibleThreadReplyRows(
  previews: ThreadReplyPreviewRow[] | undefined,
): ThreadReplyPreviewRow[] {
  if (!previews) return [];
  const byId = new Map<string, ThreadReplyPreviewRow>();
  for (const row of previews) {
    if (!byId.has(row.message_id)) byId.set(row.message_id, row);
  }
  return [...byId.values()].sort((a, b) => a.seq - b.seq).slice(-MAX_PREVIEW_LINES);
}

interface ThreadReplyPreviewProps {
  /** The lines to render, already resolved by `visibleThreadReplyRows` — the
   *  caller derives them to decide whether this block exists at all, so it
   *  passes the result rather than the raw payload. Non-empty by contract. */
  rows: ThreadReplyPreviewRow[];
  /** The reply badge, rendered as this block's HEADER LINE inside the same wash
   *  (shiki 2026-07-31: "chip 那一行跟下面的回复 rows 不要分开,整体一个背景色").
   *  It stays the caller's node — the badge owns the thread-unread modifier and
   *  the jump-to-first-unread click, neither of which this presentational block
   *  should learn. It arrives WITHOUT its own fill / corner / inset: those come
   *  from the wrapper here, because two stacked translucent fills of the same
   *  token compose darker and would draw exactly the seam this change removes. */
  badge: ReactNode;
  /** Same human-readable action label the reply badge uses; it titles the whole
   *  block, since the whole block performs that action. */
  title: string;
  /** Opens the thread. Fired by a click anywhere in the block that a nested
   *  control did not already claim — identical action to the reply badge. */
  onOpen: () => void;
  /** Author identity is resolved by the owner of the agent/human maps
   *  (MessageItem's `renderReplierAvatar`, the same call the reply chip's
   *  stacked avatars make), so this block stays purely presentational and every
   *  surface keeps ONE avatar path — including its miss behaviour. */
  renderAuthorAvatar: (author: MessageAuthorDto, size: number, key: string) => ReactNode;
  /** The author's display NAME NODE, resolved by the SAME owner as the avatar so
   *  identity is one lookup, not two. It arrives as a node (not a bare string)
   *  because a visible text author name MUST carry its deleted state through the
   *  shared `AuthorName` primitive (#task:1792 B3) — a deleted replier shows the
   *  quiet marker beside its snapshot name here exactly as in the byline, never
   *  a bare label whose only deleted hint is the avatar tooltip. `active` /
   *  `unresolved` render the plain label (no `@` for either actor kind). This
   *  block owns the width cap; the node owns weight / color / the marker. */
  renderAuthorName: (author: MessageAuthorDto) => ReactNode;
}

/**
 * A root message's reply-cluster as ONE object under the root row: the reply
 * badge as its header line, then the most recent replies one line each, all
 * inside a single recessed wash (shiki 2026-07-31).
 *
 * ── Why the badge lives INSIDE this block ──────────────────────────────────
 * The badge counts the very lines below it, so a second surface tone between
 * them read as two unrelated adornments stacked by accident. One wash, one
 * hover, one target: the whole block opens the thread, so hovering anywhere —
 * badge included — lifts the whole block (shiki 2026-07-31). The cost is that
 * the badge leaves the shared `taskTag · replies · reactions` line (#task:1185)
 * whenever there are lines to head; with no lines it stays on that line
 * untouched — and the block, along with its click, does not exist at all.
 *
 * ── Read state: this block is READ-INERT ───────────────────────────────────
 * Rendering it advances NO cursor. That is not a local choice —
 * `docs/thread-unread-v1.md` §6 already states that a root message appearing in
 * the mainline viewport does not count as opening the thread, and invariant 2
 * forbids a thread cursor from falling back to the mainline one. Per-thread
 * unread stays server-scored (`GET /api/conversations/:id/unread`) and the
 * thread cursor only advances when the user actually opens the thread, up to
 * that open's `stream_head_seq`. So there is deliberately no `observe()` call,
 * no visibility gate and no per-line read state here.
 *
 * Consequently unread is expressed exactly ONCE, by the badge header
 * (`N replies · 2 new` in the unread ink). These lines are uniformly quiet:
 * they answer "what is being discussed", not "where do I resume" — and they
 * could not answer the latter anyway, since the tail three are not the unread
 * boundary when a cluster has more unread than that (the badge's click, which
 * jumps to first-unread, owns resuming).
 *
 * Rendering is unconditional (not gated on unread): gating it would collapse
 * the root row's height the moment the thread is read, and would quietly turn
 * content visibility into a second expression of read state.
 */
export function ThreadReplyPreview({
  rows,
  badge,
  title,
  onOpen,
  renderAuthorAvatar,
  renderAuthorName,
}: ThreadReplyPreviewProps): JSX.Element {
  return (
    // Not a button: it holds the badge button, and nesting buttons is invalid.
    // The wash, the corner, the hover AND the click all live here, so the whole
    // block — padding, gaps and every line — is one target (shiki 2026-07-31).
    // It stays a plain div rather than `role="button"`: a button role must not
    // contain interactive descendants, and the badge inside is already the
    // tabbable stop for this same action, so this is a MOUSE-ONLY redundant
    // affordance — a second tab stop to one destination would only lengthen the
    // keyboard path. `group` makes a hover anywhere lift the line ink too, so
    // hovering the badge and hovering a line give the identical response.
    <div
      data-thread-reply-group
      onClick={(event) => {
        // The badge is a nested control that already fires this SAME action
        // (both are `onStartThread(message, threadFirstUnreadId)` — the block
        // jumps to first-unread exactly like the badge, shiki 2026-07-31), so
        // this guard is about not firing it TWICE on one press, not about two
        // different behaviours. A bare click's target is a text node or a plain
        // span, which never matches.
        if ((event.target as HTMLElement).closest("button, a")) return;
        onOpen();
      }}
      title={title}
      className={
        "group mt-1.5 flex w-full cursor-pointer flex-col gap-1 rounded-(--chip-radius) px-2 py-1.5 transition-colors " +
        // A recessed WASH separates the replies from the message body above
        // (shiki 2026-07-31) — one layer only: the fill REPLACED the hairline
        // rule that used to do this job rather than stacking on top of it, and
        // there is no border / shadow / accent here. `--thread-preview-bg` /
        // `-hover` are the block's own pair (poster sets them lighter than the
        // chip: this wash covers a full-width multi-line area, where chip
        // strength reads as a second card; the other two themes point them
        // BACK at the chip fill, which is what keeps badge and lines reading as
        // one shape there). All three themes declare the pair — `poster.css`
        // owns `:root`, so an omitted token inherits poster's value instead of
        // reaching the fallback below.
        "items-start bg-(--thread-preview-bg,var(--chip-bg)) hover:bg-(--thread-preview-bg-hover,var(--chip-bg-hover))"
      }
    >
      {badge}
      <div
        // The lines carry no click of their own: the WRAPPER owns the block's
        // click, so a line is not a separate target that happens to do the same
        // thing. They are plain text — no `<button>`, no tab stop, no
        // aria-label restating the badge's.
        data-thread-reply-preview
        className={
          // Same 0.25rem rhythm as the wrapper's badge→lines gap (shiki
          // 2026-07-31), so every vertical gap inside the block is equal — the
          // block reads as one list, not a header plus a tighter sub-list.
          //
          // The reply text is the FAINTEST ink on the row (shiki 2026-07-31):
          // it is an excerpt of something that lives elsewhere, so it must not
          // compete with the message body it hangs under. `--text-muted` is the
          // last ink token in the system, so the block gets its own narrower
          // token — a literal neutral faded toward each theme's ground, chosen
          // to stay at WCAG AA for normal text (this is 12px prose a reader
          // actually reads, not decoration). Every theme DECLARES it —
          // `poster.css` owns `:root`, so the `var(…, var(--text-muted))`
          // fallback here only covers a theme that does not exist yet. The ink
          // lifts on a hover ANYWHERE in the block (`group-hover`), not only
          // over the lines: one target, one response.
          "flex w-full flex-col items-start gap-1 text-left text-xs leading-tight text-(--thread-preview-text,var(--text-muted)) transition-colors group-hover:text-text-secondary"
        }
      >
        {rows.map((row) => (
          <span key={row.message_id} className="flex w-full min-w-0 items-center gap-1.5">
            {renderAuthorAvatar(row.author, 14, row.message_id)}
            {/* WHO said it: the mainline's own name rule, with no `@` for either
              actor kind (shiki 2026-07-31) — the avatar directly to its left
              already identifies the actor, and an `@` would read as a mention.
              The name node carries its deleted state (the shared `AuthorName`
              marker, #task:1792 B3) so a deleted replier reads the same here as
              in the byline, not only in the avatar tooltip. It never grows past
              40% and truncates rather than shrinking to an ellipsis stub — the
              reply text keeps shrink priority and the name simply caps, while
              the deleted marker (shrink-0) stays visible past the truncation. */}
            <span className="flex max-w-[40%] shrink-0 items-baseline">
              {renderAuthorName(row.author)}
            </span>
            {/* Shrinks and truncates but never GROWS: a short reply must not
              stretch its line and push the attachment label out to the far
              edge, where the label reads as unrelated to the text it belongs
              to. The label follows the text immediately. */}
            {row.preview ? (
              <span className="min-w-0 shrink truncate">{plainTextFromMarkdown(row.preview)}</span>
            ) : null}
            {row.attachment_count > 0 ? (
              // Label only, capped so it cannot squeeze the reply text to nothing
              // (canonical labels are short, but the cap also holds if a theme
              // widens the type scale).
              <span className="flex min-w-0 max-w-[40%] items-center gap-1">
                <AttachmentIcon className="size-3 shrink-0" />
                <span className="truncate">
                  {row.first_attachment_label ?? t("messages.attachment")}
                </span>
                {row.attachment_count > 1 ? (
                  <span className="shrink-0">+{row.attachment_count - 1}</span>
                ) : null}
              </span>
            ) : null}
          </span>
        ))}
      </div>
    </div>
  );
}
