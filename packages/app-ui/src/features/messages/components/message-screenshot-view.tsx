import type { AgentDto, AttachmentSnapshot, MessageDto, UserDto } from "@silverhand/shared";
import { resolveAttachmentLabels } from "@silverhand/shared";
import { DocumentIcon, HashIcon } from "@/icons";
import { AVATAR_PX } from "@/components/avatar-scale";
import { getAttachmentBlobUrl } from "@/features/attachments/attachment-blob-cache";
import { AuthorAvatarView, AuthorName } from "./author-avatar-view";
import { authorStateLabel, presentAuthor } from "../model/author-presentation";
import { continuesAuthorGroup } from "../model/author-grouping";
import { Markdown } from "./markdown";
import { MessageRowContent } from "./message-row-content";
import { ReactionChipRow } from "./reaction-chip-row";
import type { ChannelReference } from "./channel-ref";
import type { WorkspaceChannelReference } from "./conversation-ref";
import type { StaticTaskRefSummary } from "./task-ref";
import { t } from "@/lib/i18n";

/**
 * Static, NON-interactive renderer for the image-export pipeline.
 *
 * Deliberately does NOT reuse `MessageItem` because that component
 * carries per-row hover state, action buttons, popovers, TaskRef
 * interaction surfaces, and a bunch of branching that's irrelevant
 * to a screenshot. Every interaction prop is omitted here so the
 * rendered DOM contains zero event handlers — html-to-image is then
 * deterministic, can't trip over a stray `<button>` portal, and the
 * captured image is identical run-to-run.
 *
 * Per Cindy's review #6: lookup maps (agentsByName, viewer,
 * channelsByName, etc.) are passed in EXPLICITLY rather than reaching
 * into hooks, so the screenshot view's dependencies are obvious and
 * an offscreen render in a different React tree still resolves
 * names/avatars consistently. Author identity (byline label + avatar
 * variant) comes from the ONE `MessageAuthorDto` seam (#task:1792) via
 * `presentAuthor` — never a live `from_*_id` directory re-join — so a
 * hard-deleted author keeps its snapshot name and neutral avatar here too.
 * The `agentsByName` / `humansByName` maps that remain feed only the
 * inline @-mention pills inside the Markdown body, a separate axis.
 *
 * The byline NAME renders through the SAME shared `AuthorName` primitive as
 * the live row (#task:1802): identical `font-bold` ink weight, a BARE name for
 * both actor kinds (no `@` — recognition surfaces dropped it in #task:1768; the
 * avatar carries agent-vs-human, and #task:1771 made the name read equal for
 * both), and a deleted author shows the quiet `DeletedActorMarker` chip (a
 * static badge, so it survives the no-hover export intact). The name is
 * sanitized before it enters the primitive — same XML-control-char guard the
 * body text gets. Nothing about the byline is hand-assembled here, so a future
 * byline change (weight, color, deleted styling) propagates to the export for
 * free rather than silently drifting. Timestamps match the live `MM/DD HH:mm`.
 *
 * What we render per row (per @shiki spec): avatar + display name +
 * time + markdown body + attachment chips + reaction chips.
 * Reactions go through the SAME `ReactionChipRow` primitive as the live
 * row (#task:1173) — a static, button/hook/portal-free variant — so a
 * reaction visible in chat is always visible in the screenshot too.
 * Consecutive same-author rows GROUP exactly as the live stream does
 * (#task:1737 / #task:1802): the run's head keeps the full header, the rest
 * drop avatar + name + time. The continuation predicate compares each row
 * against the one directly ABOVE IT IN THE EXPORT (not the live stream), so a
 * skip-selected excerpt can never hide a byline that mismatches its visible
 * predecessor — the dropped name always equals the row above it.
 * NOT rendered (interactive affordances, not default-visible content):
 * saved indicator, hover bar, checkbox, thread-replies badge, task tag,
 * system rows.
 */
export function MessageScreenshotView({
  compact = false,
  messages,
  agentsByName,
  humansByName,
  channelsByName,
  conversationRefsById,
  taskRefsByNumber,
  agentAvatarUrls,
  authorAgentAvatarUrls,
  authorUserAvatarUrls,
  attachmentImageUrls,
  viewer,
  viewerAvatarObjectUrl,
  channelTitle,
  mermaidSvgByCode,
}: {
  compact?: boolean;
  messages: readonly MessageDto[];
  agentsByName: ReadonlyMap<string, AgentDto>;
  humansByName?: ReadonlyMap<string, UserDto>;
  channelsByName?: ReadonlyMap<string, ChannelReference | null>;
  conversationRefsById?: ReadonlyMap<string, WorkspaceChannelReference>;
  taskRefsByNumber?: ReadonlyMap<number, StaticTaskRefSummary>;
  /** Directory batch — feeds only the inline @-mention pills in the Markdown
   *  body, a separate axis from the row's own author. */
  agentAvatarUrls?: ReadonlyMap<string, string | null>;
  /** Byline avatar bytes, derived from each row's own author DTO
   *  (#task:1792 B2) — never a directory join. Split by kind. */
  authorAgentAvatarUrls?: ReadonlyMap<string, string | null>;
  authorUserAvatarUrls?: ReadonlyMap<string, string | null>;
  attachmentImageUrls?: ReadonlyMap<string, string>;
  viewer?: UserDto;
  viewerAvatarObjectUrl?: string | null;
  channelTitle?: string | null;
  /** Mermaid diagrams pre-rendered to SVG before capture (#task:1322), keyed
   *  by trimmed source. Threaded into each row's Markdown so the export shows
   *  rendered diagrams; a diagram absent here falls back to source. */
  mermaidSvgByCode?: ReadonlyMap<string, string>;
}): JSX.Element {
  const safeChannelTitle = channelTitle ? sanitizeScreenshotText(channelTitle) : null;
  return (
    <div
      data-screenshot-root
      data-compact-export={compact || undefined}
      className="flex flex-col gap-3 bg-bg-floating text-text-primary"
      style={{ width: compact ? 360 : 760, padding: compact ? 16 : 24, overflowWrap: "anywhere" }}
    >
      {safeChannelTitle ? (
        <div className="border-b border-border-subtle pb-2 text-sm font-semibold">
          {/* Every conversation header carries the channel `#` — channel and DM
              alike (matching the live header, #task:1179). No branch: a
              channel-like conversation always earns it (#task:1373). Painted per
              theme via `[data-conversation-hash-icon]` (poster = poster
              sticker). */}
          <span
            data-conversation-title
            className="inline-flex items-center font-serif text-base font-semibold text-text-primary"
          >
            <HashIcon
              data-conversation-hash-icon
              data-sticker
              data-sticker-tone="channel"
              className="size-4 shrink-0 align-[-0.15em]"
            />
            {safeChannelTitle}
          </span>
        </div>
      ) : null}
      {messages.map((m, i) => {
        // Group a run of one author exactly as the live stream does, but measure
        // against the PREVIOUS EXPORTED row rather than the live-stream
        // predecessor. `messages` is `selectedSorted()` (seq-ascending) and a
        // multi-select can skip rows, so the live-stream neighbour may be absent
        // here; comparing against the row that will actually sit above this one
        // keeps the collapse honest — the byline we drop always equals the
        // visible row above, never a different author (#task:1802).
        const previous = i > 0 ? messages[i - 1]! : null;
        const continuesPrevious = previous != null && continuesAuthorGroup(m, previous);
        return (
          <ScreenshotRow
            key={m.id}
            message={m}
            continuesPrevious={continuesPrevious}
            agentsByName={agentsByName}
            humansByName={humansByName}
            channelsByName={channelsByName}
            conversationRefsById={conversationRefsById}
            taskRefsByNumber={taskRefsByNumber}
            agentAvatarUrls={agentAvatarUrls}
            authorAgentAvatarUrls={authorAgentAvatarUrls}
            authorUserAvatarUrls={authorUserAvatarUrls}
            attachmentImageUrls={attachmentImageUrls}
            viewer={viewer}
            viewerAvatarObjectUrl={viewerAvatarObjectUrl}
            mermaidSvgByCode={mermaidSvgByCode}
          />
        );
      })}
    </div>
  );
}

function ScreenshotRow({
  message,
  continuesPrevious,
  agentsByName,
  humansByName,
  channelsByName,
  conversationRefsById,
  taskRefsByNumber,
  agentAvatarUrls,
  authorAgentAvatarUrls,
  authorUserAvatarUrls,
  attachmentImageUrls,
  viewer,
  viewerAvatarObjectUrl,
  mermaidSvgByCode,
}: {
  message: MessageDto;
  continuesPrevious: boolean;
  agentsByName: ReadonlyMap<string, AgentDto>;
  humansByName?: ReadonlyMap<string, UserDto>;
  channelsByName?: ReadonlyMap<string, ChannelReference | null>;
  conversationRefsById?: ReadonlyMap<string, WorkspaceChannelReference>;
  taskRefsByNumber?: ReadonlyMap<number, StaticTaskRefSummary>;
  agentAvatarUrls?: ReadonlyMap<string, string | null>;
  authorAgentAvatarUrls?: ReadonlyMap<string, string | null>;
  authorUserAvatarUrls?: ReadonlyMap<string, string | null>;
  attachmentImageUrls?: ReadonlyMap<string, string>;
  viewer?: UserDto;
  viewerAvatarObjectUrl?: string | null;
  mermaidSvgByCode?: ReadonlyMap<string, string>;
}): JSX.Element {
  // Identity comes from the ONE author seam, not a `from_*_id` directory join.
  // The byline renders through the shared `AuthorName` primitive (below), so
  // this file no longer hand-assembles the label — it only sanitizes the seam's
  // snapshot name for the SVG serializer (a display name can carry the same
  // XML-invalid control chars the body guards against). `authorStateLabel`
  // gives the avatar its alt/title text; a system row (null author) has no
  // presentation and falls back to a bare "system".
  const presentation = message.author ? presentAuthor(message.author) : null;
  const safePresentation = presentation
    ? { ...presentation, label: sanitizeScreenshotText(presentation.label) }
    : null;
  const avatarLabel = safePresentation ? authorStateLabel(safePresentation) : "system";
  // Feed the viewer's own already-resolved avatar blob when they authored the
  // row (active user matching the viewer's id); every other active author pulls
  // its bytes from the author-DTO-derived batch (#task:1792 B2 — not the
  // directory), and deleted/unresolved render a neutral square.
  const viewerAvatarOverride =
    presentation?.avatar.variant === "user" &&
    viewer != null &&
    presentation.avatar.id === viewer.id
      ? (viewerAvatarObjectUrl ?? null)
      : null;
  const time = formatExportTime(message.created_at);
  const content = sanitizeScreenshotText(message.content);

  return (
    // A grouped continuation pulls up toward its head (the parent's `gap-3`
    // minus this) so a one-author run reads as one block, matching the live
    // stream's tightening (#task:1737 / #task:1802).
    <div className={"flex items-start gap-3" + (continuesPrevious ? " -mt-2" : "")}>
      {continuesPrevious ? (
        // Continuation: no avatar, but hold the avatar column's WIDTH so every
        // body in the run stays on the group head's text baseline. No hover
        // rail here — a static export has nowhere to reveal the per-row time,
        // and stamping it on every line would defeat the grouping.
        <div className="w-8 shrink-0" aria-hidden />
      ) : (
        <AuthorAvatarView
          avatar={presentation?.avatar ?? { variant: "neutral-user" }}
          size={AVATAR_PX.lg}
          label={avatarLabel}
          className="border-(length:--border-avatar-width) border-border-avatar"
          agentAvatarUrls={authorAgentAvatarUrls}
          humanAvatarUrls={authorUserAvatarUrls}
          objectUrlOverride={viewerAvatarOverride}
        />
      )}
      {/* Shared content layer (#task:1173 P1). The export fills each required
          slot with a STATIC node and passes `null` for the live-only
          adornments (header task tag, thread badge) — making its deliberate
          omission explicit. A new default-visible adornment added to the
          shared registry won't compile here until the export declares it. */}
      <MessageRowContent
        alignRight={false}
        columnClassName="min-w-0 max-w-[640px] flex-1"
        /* Byline through the shared `AuthorName` primitive — same `font-bold`
           ink weight as the live row, a bare name for both actor kinds (no
           `@`), and a deleted author's quiet chip. A continuation row states
           neither author nor time: its group head already named the author
           (#task:1802). A system row (no presentation) shows a bare "system". */
        author={
          continuesPrevious ? null : safePresentation ? (
            <AuthorName presentation={safePresentation} className="font-bold text-text-primary" />
          ) : (
            <span className="font-bold text-text-primary">{t("messages.system")}</span>
          )
        }
        timestamp={
          continuesPrevious ? null : (
            <span className="text-(length:--text-meta) text-text-muted">{time}</span>
          )
        }
        taskTag={null}
        body={
          /* Reuse Markdown renderer but omit every interaction callback.
             Inline @-mention / #-channel pills still resolve via the explicit
             lookup maps; click handlers are intentionally absent so the
             captured DOM contains no buttons. */
          <Markdown
            content={content}
            conversationId={message.conversation_id}
            agentsByName={new Map(agentsByName)}
            humansByName={humansByName ? new Map(humansByName) : undefined}
            agentAvatarUrls={agentAvatarUrls ? new Map(agentAvatarUrls) : undefined}
            channelsByName={channelsByName ? new Map(channelsByName) : undefined}
            conversationRefsById={conversationRefsById ? new Map(conversationRefsById) : undefined}
            taskRefsByNumber={taskRefsByNumber ? new Map(taskRefsByNumber) : undefined}
            interactive={false}
            mermaidSvgByCode={mermaidSvgByCode}
          />
        }
        attachments={
          message.attachments && message.attachments.length > 0 ? (
            <ScreenshotAttachmentList
              attachments={message.attachments}
              imageUrls={attachmentImageUrls}
            />
          ) : null
        }
        reactions={
          message.reactions && message.reactions.length > 0 ? (
            <ReactionChipRow reactions={message.reactions} />
          ) : null
        }
        trailing={null}
        /* The export captures the MESSAGE, not its thread: the reply badge is
           already omitted, so its preview lines are too. */
        threadPreview={null}
        aside={null}
      />
    </div>
  );
}

function ScreenshotAttachmentList({
  attachments,
  imageUrls,
}: {
  attachments: AttachmentSnapshot[];
  imageUrls?: ReadonlyMap<string, string>;
}): JSX.Element {
  // Same shared resolver as the live tile + agent suffix, so an exported image
  // shows the identical `Image #N` references (#task:1324).
  const labels = resolveAttachmentLabels(attachments);
  return (
    <div className="mt-2 flex flex-wrap gap-2">
      {attachments.map((attachment, i) => (
        <ScreenshotAttachmentTile
          key={attachment.id}
          attachment={attachment}
          label={labels[i]}
          imageUrl={
            attachment.content_type.startsWith("image/")
              ? (imageUrls?.get(attachment.id) ?? getAttachmentBlobUrl(attachment.id))
              : undefined
          }
        />
      ))}
    </div>
  );
}

function ScreenshotAttachmentTile({
  attachment,
  label,
  imageUrl,
}: {
  attachment: AttachmentSnapshot;
  label?: string;
  imageUrl?: string;
}): JSX.Element {
  const isImage = attachment.content_type.startsWith("image/");
  const filename = sanitizeScreenshotText(attachment.filename);
  const labelBadge = label ? (
    <span className="block w-full text-center font-mono text-(length:--text-badge) text-text-muted">
      {label}
    </span>
  ) : null;
  if (isImage && imageUrl) {
    return (
      <figure className="flex flex-col gap-1">
        <span className="block overflow-hidden rounded-md border border-border-subtle bg-bg-floating">
          <img
            src={imageUrl}
            alt={filename}
            className="block max-h-64 object-contain"
            style={{ maxWidth: "min(320px, 100%)" }}
            draggable={false}
          />
        </span>
        {labelBadge}
      </figure>
    );
  }
  return (
    <div className="flex flex-col items-start gap-1">
      <div className="flex max-w-70 items-center gap-2 rounded-md border border-border-subtle bg-bg-floating px-2 py-1.5 text-xs text-text-secondary">
        <DocumentIcon className="size-4 shrink-0 text-text-muted" />
        <span className="min-w-0 truncate">{filename}</span>
        <span className="shrink-0 text-(length:--text-badge) text-text-muted">
          {formatBytes(attachment.size)}
        </span>
      </div>
      {labelBadge}
    </div>
  );
}

// Mirrors the live row's `MM/DD HH:mm` (MessageItem's `timeLabel`) so the
// exported byline reads identically. Hand-rolled from local `Date` fields
// rather than a dayjs pass so an offscreen render carries no formatter
// dependency; the output string is the one to keep in step with live.
function formatExportTime(epochMs: number): string {
  const d = new Date(epochMs);
  const mm = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  const hh = String(d.getHours()).padStart(2, "0");
  const mi = String(d.getMinutes()).padStart(2, "0");
  return `${mm}/${dd} ${hh}:${mi}`;
}

// html-to-image serializes the offscreen DOM through an SVG foreignObject.
// XML 1.0 forbids most C0/C1 control characters; a single pasted backspace
// (`\b`) in message text can make the generated `data:image/svg+xml` fail to
// load. Keep the live message untouched and only strip those invisible controls
// in the static export tree. Preserve whitespace controls that XML allows.
const XML_INVALID_CONTROL_RE = /[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F-\u009F]/g;

function sanitizeScreenshotText(value: string): string {
  return value.replace(XML_INVALID_CONTROL_RE, "");
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(1)} GB`;
}
