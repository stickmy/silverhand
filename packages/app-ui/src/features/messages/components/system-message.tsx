import { memo } from "react";
import {
  type MessageDto,
  type SystemSubtype,
  type UserDto,
  resolveSubtype,
} from "@silverhand/shared";
import type { ChannelReference } from "./channel-ref";
import type { WorkspaceChannelReference } from "./conversation-ref";
import { type SystemMessageRenderer, GenericNarrationFallback } from "./system-message/shared";
import { ScheduleFireRow } from "./system-message/schedule-fire";
import { TaskEventRow } from "./system-message/task-event";
import {
  ChannelContextCard,
  ChildContextCard,
  ThreadAnchorCard,
} from "./system-message/context-cards";
import {
  ConversationMemberChangeRow,
  MentionCcCard,
  OnboardingRow,
  RenameRow,
  SessionClearedCard,
} from "./system-message/message-cards";

interface SystemMessageProps {
  message: MessageDto;
  conversationRefsById?: Map<string, WorkspaceChannelReference>;
  onOpenConversationRef?: (id: string) => void;
  channelsByName?: Map<string, ChannelReference | null>;
  onOpenChannelRef?: (channelId: string) => void;
  humansByName?: Map<string, UserDto>;
}

/**
 * System messages render via a per-subtype registry. The registry is typed
 * `Record<SystemSubtype, SystemMessageRenderer>` — missing entries fail
 * typecheck, so adding a new subtype to `@silverhand/shared` forces a
 * matching renderer here.
 *
 * Resolution is two-state (`resolveSubtype`):
 *  - `current`: look up in `systemMessageRenderers` and render.
 *  - `unknown`: missing/unrecognized subtype — generic narration.
 *
 * We do NOT re-parse content prefixes to fake current rendering for retired
 * rows; that path lived in the old `inferSubtype` helper and was removed in
 * PR-1.
 */
export const SystemMessage = memo(function SystemMessage(props: SystemMessageProps): JSX.Element {
  const resolution = resolveSubtype(readRawSubtype(props.message.metadata));
  const Renderer =
    resolution.kind === "current"
      ? systemMessageRenderers[resolution.subtype]
      : GenericNarrationFallback;
  // PR-8: every system message renders inside `.system-message-row` so
  // the user's theme-level prose font (`--appearance-prose-font` /
  // `--md-prose-size`) and code font (`--appearance-code-font`) reach
  // plain-text rows that don't go through `<Markdown>` (task event rows,
  // schedule banner pills, rename chips, etc.). `.markdown-body` already
  // pulls the same vars; this class is the system-row analogue.
  // PR-10: also tag with `data-system-event-row="1"` so the activity
  // cluster CSS in `chrome.css` (`.system-activity-cluster
  // [data-system-event-row="1"] > *`) can demote the inner row body's
  // chunky padding without depending on the row body's exact Tailwind
  // utility class list.
  // PR-10 follow-up #7 (msg `M1prpsoXI9rSK6HQ`): info-block subtypes
  // (banners + cards that aren't conversational replies) align to the
  // avatar column instead of the regular content column, giving them
  // the full row width minus a small symmetric gutter — matches
  // Slack/Discord block-level system events. Conversation-adjacent
  // narration — task_* activity clusters, plus standalone rename /
  // session_cleared (no longer clustered since #task:1145) — keeps the
  // content column alignment because it reads as commentary alongside
  // replies. The split is expressed as `data-system-layout="info"` on
  // the wrapper; the layout CSS (`.system-message-row[data-system-
  // layout="info"]` in `chrome.css`) does the alignment override.
  const layout =
    resolution.kind === "current" && INFO_BLOCK_SUBTYPES.has(resolution.subtype)
      ? "info"
      : undefined;
  return (
    <div className="system-message-row" data-system-event-row="1" data-system-layout={layout}>
      <Renderer {...props} />
    </div>
  );
});

function readRawSubtype(metadata: MessageDto["metadata"]): string | null | undefined {
  if (!metadata) return null;
  const v = (metadata as Record<string, unknown>)["subtype"];
  return typeof v === "string" ? v : null;
}

// ─── Registry ──────────────────────────────────────────────────────────

/**
 * Per-subtype renderer registry. TypeScript narrows this to
 * `Record<SystemSubtype, SystemMessageRenderer>` — missing keys break
 * typecheck. Adding a new subtype to `SYSTEM_SUBTYPES` in shared forces
 * a corresponding entry here before the build passes.
 *
 * PR-1 baseline: existing custom cards (mention_cc / session_cleared) and
 * the existing collapsible narration treatment (onboarding / child_context /
 * thread_anchor) are wired up. The remaining current subtypes
 * (channel_context / schedule_fire / rename / task_*) point at
 * `GenericNarrationFallback` and will be replaced one by one in subsequent
 * PRs (PR-2..PR-6).
 */
const systemMessageRenderers: Record<SystemSubtype, SystemMessageRenderer> = {
  // P0 (PR-2): task event row reads enriched metadata; legacy rows
  // (no actor / task_title / task_message_id metadata) fall back to
  // muted narration without the structured chip/pill presentation.
  task_create: TaskEventRow,
  task_claim: TaskEventRow,
  task_unclaim: TaskEventRow,
  task_status: TaskEventRow,
  // task_edit (#task:1531): a title/content definition change — same row
  // as the rest of the task_* family, rendered with an "edited" verb.
  task_edit: TaskEventRow,
  // P0 (PR-3): inline audit row + RotateCcw icon (no longer a toast).
  session_cleared: SessionClearedCard,
  schedule_fire: ScheduleFireRow,
  // P1 (PR-5): unified context-card series — shared chrome (left
  // accent line + kind tag + source chip + collapsible body), color
  // per kind. Each subtype renderer parses its enriched metadata and
  // falls back to a tagged single-line muted row when the row predates
  // PR-5's metadata enrichment.
  channel_context: ChannelContextCard,
  child_context: ChildContextCard,
  thread_anchor: ThreadAnchorCard,
  // P2 (PR-6): minor polish (badge, icon).
  // P2 (PR-6): minor polish — onboarding gets an explicit badge in
  // its collapsible header; rename reads `{old_name, new_name}` and
  // draws a compact `✎ old → new` audit row (legacy rows without
  // metadata fall through to generic italic narration).
  onboarding: OnboardingRow,
  rename: RenameRow,
  conversation_member_added: ConversationMemberChangeRow,
  conversation_member_removed: ConversationMemberChangeRow,
  mention_cc: MentionCcCard,
};

/**
 * Subtypes that render as block-level "info" — banners, cards, and
 * dividers that aren't conversational replies. These get the wider
 * avatar-column alignment in CSS (`.system-message-row[data-system-
 * layout="info"]`), giving them the full row gutter on both sides
 * for banner/card visual weight. Conversation-adjacent narration —
 * `task_*` activity clusters, plus standalone `rename` / `session_cleared`
 * (the latter two dropped from the clusterable set in #task:1145) — is
 * intentionally NOT in this set —
 * those read as conversation-adjacent commentary and stay in the
 * regular content column. Generic / unknown fallbacks also stay at
 * content column (default behavior of `.system-message-row`).
 *
 * Drift guard: every entry here must be a `SystemSubtype`; new
 * subtypes added to `@silverhand/shared` won't auto-join — author
 * gets to decide which lane the new subtype belongs in.
 */
const INFO_BLOCK_SUBTYPES: ReadonlySet<SystemSubtype> = new Set<SystemSubtype>([
  "schedule_fire",
  "channel_context",
  "child_context",
  "thread_anchor",
  "onboarding",
  "mention_cc",
]);
