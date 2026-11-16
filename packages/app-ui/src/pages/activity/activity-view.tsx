import { useListReadingPosition } from "@/navigation/view-memory";
import { plainTextFromMarkdown } from "@/lib/plain-text-from-markdown";
import { useMemo, useState } from "react";
import { ActivityIcon, CheckAllIcon, CheckIcon, ThreadIcon, WarningIcon } from "@/icons";
import type { ActivityItemDto, MessageAuthorDto, AgentDto } from "@silverhand/shared";
import { WorkspaceIcon } from "@/components/workspace-icon";
import { CollectionItemCard } from "@/components/layout/collection-item-card";
import { ChannelIcon } from "@/components/channel-icon";
import {
  AuthorAvatarView,
  AuthorName,
  DeletedActorMarker,
} from "@/features/messages/components/author-avatar-view";
import { presentAuthor } from "@/features/messages/model/author-presentation";
import type { AuthorAvatar } from "@/features/messages/model/author-presentation";
import {
  collectCounterpartAvatarSources,
  presentCounterpart,
} from "@/features/direct-pairs/counterpart-presentation";
import type { CounterpartPresentation } from "@/features/direct-pairs/counterpart-presentation";
import { AVATAR_PX } from "@/components/avatar-scale";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import type { AgentAvatarSource } from "@/features/agents/hooks/avatar-hooks";
import {
  useActivity,
  useClearActivityItem,
  useMarkActivityAllRead,
} from "@/features/attention/hooks";
import { activityItemSource } from "@/features/attention/observe";
import { ActivityListSkeleton } from "./activity-list-skeleton";
import type { MessageJumpTarget } from "@/features/channels/use-message-jump-controller";
import { PaneHeader, PaneShell } from "../../components/layout/pane-shell";
import { iconMarkup } from "@/components/layout/native-page-bar";
import { useCompactLayout } from "@/navigation/layout";
import { t } from "@/lib/i18n";

/**
 * Activity — the human's "queue waiting on me" (#task:1227). Read-side
 * projection of `listActivity`: the server returns the list already filtered
 * (§6), per-(viewer,message) de-duped (§7) and salience-sorted (§13.2). The
 * renderer renders it AS-IS and never re-scores; the three filter chips slice it
 * purely client-side.
 *
 * v1 UI scope = GLOBAL: one list spanning every workspace. Each row is a LIST
 * CARD in the Saved-item visual language — NOT an alert feed: no left icon box,
 * no left salience rail, items spaced apart, themed border, poster hover
 * via the shared `data-collection-card` hook. Salience is carried by the two sections
 * (Requires attention / Subscribed), not by a per-row border. Location reads
 * through real identity icons on the where-line.
 *
 * Row structure is SOURCE-FIRST (server-resolved fields only; never a
 * client-stitched message cache), now ICON-LED so location reads at a glance:
 *   - row 1 = WHERE, dispatched on `location.kind` (#task:2115) because Activity
 *     spans BOTH placements: a `workspace_channel` draws workspace icon + name,
 *     then channel icon + conversation `title`; a `direct_pair` draws the
 *     COUNTERPART's face + name and NO workspace segment (a workspace-less pair
 *     has no such coordinate to draw, and one is never fabricated). The icons are
 *     the canonical identity primitives (`WorkspaceIcon`; `HashIcon` with the
 *     `data-channel-icon` theme hook; `AuthorAvatarView` for a counterpart), NOT
 *     raw glyphs — so poster paints them as its `W`/`#` posters instead of
 *     generic icon shapes. The old "Channel/mention/Thread" text label is
 *     dropped: real icons + the thread glyph (row 2) + the sender (row 3) carry
 *     that signal now.
 *   - row 2 = SUBJECT: thread → a thread glyph + `thread_root_preview` (the root
 *     summary it belongs under); else → `preview` (the message excerpt).
 *   - row 3 = THREAD reply: `preview` (the triggering reply), shown only when
 *     row 2 already carries the thread root summary. The sender (avatar +
 *     display_name) lands here once the backend resolves `preview_sender` on the
 *     anchor message (additive DTO field).
 *
 * Clearing (§5.3 source_ref-out) — by EXPLICIT fields, never by parsing `id`,
 * and routed through one chokepoint (`activityItemSource`):
 *   - agent_failed → mark the `notification_id` read.
 *   - unread (a stream, with or without mentions) → advance the stream cursor via
 *     an EXPLICIT stream-head clear, which also reads its mentions (#task:1231
 *     phase B; knowingly tail-swallows, §2).
 *   Each row exposes a hover-✓ that clears WITHOUT navigating. The header's
 *   mark-all clears the whole list regardless of the active filter, plus every
 *   unread directed notification the list cannot show. Navigating to a source
 *   still clears it too (directed on click; ambient via open-time mark-read).
 *
 * `kind=task` is intentionally NOT in Activity v1:
 * an `in_review && member` projection is a standing mirror of the task panel's
 * In-review column, not a transient attention event — it has no other home only
 * mentions/unread do, so tasks stay in the task panel. Task re-enters Activity in
 * v2 ONLY via a real event source (notification-backed `task_needs_review`,
 * read-cleared), never a status mirror. See `docs/activity-lifecycle-v1.md`.
 */

type FilterKey = "all" | "mention" | "thread";

const filterTabs = (): { key: FilterKey; label: string }[] => [
  { key: "all", label: t("activity.all") },
  { key: "mention", label: t("activity.mentions") },
  { key: "thread", label: t("activity.threads") },
];

/** Namespaced key into Activity's ONE avatar object-url batch. A face's identity
 *  is `{kind, id}` — users and agents are separate tables, so a bare `id` could
 *  collide and cross-wire two avatars. */
function principalBatchKey(kind: "agent" | "user", id: string): string {
  return `${kind}:${id}`;
}

/** The batch key for an already-resolved face, or `null` when it carries no bytes
 *  at all (a neutral glyph: deleted / unresolved) and is therefore never batched.
 *
 *  Read off the resolved {@link AuthorAvatar} rather than off a DTO's own `kind`
 *  field, because that variant is the ONE vocabulary the two kinds of face in this
 *  list already share: a message sender resolves through `presentAuthor`, a pair
 *  counterpart through `presentCounterpart` — which is also where the endpoint's
 *  `human` → avatar `user` spelling change lives, so it does not happen again
 *  here. */
function avatarBatchKey(avatar: AuthorAvatar): string | null {
  return avatar.variant === "agent" || avatar.variant === "user"
    ? principalBatchKey(avatar.variant, avatar.id)
    : null;
}

function matchesFilter(item: ActivityItemDto, key: FilterKey): boolean {
  switch (key) {
    case "all":
      return true;
    case "mention":
      return (item.mention_count ?? 0) > 0;
    case "thread":
      // A "Thread" item is anything located in a reply cluster, regardless of
      // kind — overlapping facet, not a partition (a thread @me matches both
      // Mentions and Threads). Single-select, so the user views one lens.
      return item.thread_root_id != null;
  }
}

/** Row-1 location glyph — the canonical themed primitive for WHERE the item
 *  lives. Its dispatch order is fixed:
 *
 *    1. `agent_failed` → the warning glyph (error-tinted), FIRST and identical in
 *       both placements. A start failure has no normal conversation context to
 *       draw, and the warning must survive the dropped text label; a start failure
 *       inside a DM is still a start failure, so placement never displaces it.
 *    2. otherwise → the placement's own identity (#task:2115):
 *       - `direct_pair` → the COUNTERPART's face, through the pair SSOT
 *         (`presentCounterpart` → {@link AuthorAvatarView}), in the same
 *         `AVATAR_PX.xs` box the `#` sticker and `WorkspaceIcon` occupy — so the
 *         where-line's height does not move between placements.
 *       - `workspace_channel` → the `#` channel sticker.
 *
 *  The `location.kind` test itself happens ONCE per row, in
 *  {@link resolveRowLocation} — this component consumes its result (`counterpart`
 *  non-null ⟺ the DM arm) precisely so a second copy of the discriminant cannot
 *  drift out of agreement with the first, which also draws the row's NAME and
 *  builds its jump target.
 *
 *  Keyed on PLACEMENT, never on the roster (#task:2076 / #task:2077): a workspace
 *  conversation IS a channel whatever its roster policy is (#task:1373). An earlier
 *  `roster_policy === 'fixed'` arm here drew a message-circle and was wrong on
 *  every state it could reach — within a workspace the only rows it matched were an
 *  agent space's fixed-roster DM/Onboarding CHANNELS. Roster answers a capability
 *  question (can members change); identity is placement's to answer. That judgment
 *  is why the DTO's discriminant is the placement and why it still carries no
 *  roster field at all (see `ActivityLocationSchema`).
 *
 *  An if-chain (not an exhaustive switch on `kind`) so it stays decoupled from
 *  whether the shared `kind` union still carries the now-removed `task`. */
function LocationGlyph({
  item,
  counterpart,
  objectUrl,
}: {
  item: ActivityItemDto;
  counterpart: CounterpartPresentation | null;
  objectUrl: string | null;
}): JSX.Element {
  if (item.kind === "agent_failed") {
    return <WarningIcon className="size-3.5 shrink-0 text-error" />;
  }
  if (counterpart) {
    return (
      <AuthorAvatarView
        avatar={counterpart.avatar}
        size={AVATAR_PX.xs}
        label={counterpart.label}
        objectUrlOverride={objectUrl}
      />
    );
  }
  return <ChannelIcon variant="sticker" density="sm" className="text-text-muted" />;
}

/** The sender's avatar for a message line, drawn through the ONE shared author
 *  seam (#task:1792): the server-authoritative {@link MessageAuthorDto} →
 *  `presentAuthor` → {@link AuthorAvatarView}. An active agent carries its own
 *  runtime in the DTO (no roster lookup), a deleted / unresolved sender gets the
 *  kind-generic neutral square. The object URL comes from Activity's own
 *  namespaced batch, so it is fed as an override rather than through the seam's
 *  plain-id maps. */
function SenderAvatar({
  sender,
  objectUrl,
}: {
  sender: MessageAuthorDto;
  objectUrl: string | null;
}): JSX.Element {
  const p = presentAuthor(sender);
  return (
    <AuthorAvatarView
      avatar={p.avatar}
      size={AVATAR_PX.xs}
      label={p.label}
      objectUrlOverride={objectUrl}
    />
  );
}

/** A source message line: `<avatar> <display_name>: <content>`. The sender
 *  prefix is shown only when `sender` is present (NULL for `agent_failed` /
 *  system) — then the line is just the content. The name is body-scale + muted
 *  (a subordinate label, never a heading) so it doesn't steal the content's role
 *  as subject. A deleted sender keeps its snapshot name; an unresolved one reads
 *  "Unknown …" — both via `presentAuthor`, the same label the byline shows. */
function SenderMessageLine({
  sender,
  objectUrl,
  content,
  className,
}: {
  sender: MessageAuthorDto | null;
  objectUrl: string | null;
  content: string | null;
  className?: string;
}): JSX.Element {
  const presentation = sender ? presentAuthor(sender) : null;
  return (
    <div className={"mt-1 flex items-center gap-2 " + (className ?? "")}>
      <div className="flex min-w-0 flex-1 items-center gap-1.5">
        {sender && presentation ? (
          <>
            <SenderAvatar sender={sender} objectUrl={objectUrl} />
            {/* The name carries its deleted state through the ONE shared
                primitive (#task:1792 B3) — a deleted sender reads with the quiet
                marker here exactly as in the byline. */}
            <span className="flex min-w-0 max-w-[45%] shrink-0 items-baseline gap-1.5">
              <AuthorName
                presentation={presentation}
                className="text-sm font-normal text-text-secondary"
              />
              <span className="text-sm font-normal text-text-secondary">:</span>
            </span>
          </>
        ) : null}
        <span className="min-w-0 flex-1 truncate text-sm text-text-primary">{content}</span>
      </div>
    </div>
  );
}

function relTime(ts: number): string {
  const diff = Date.now() - ts;
  const m = Math.floor(diff / 60_000);
  if (m < 1) return "now";
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  const d = Math.floor(h / 24);
  if (d < 7) return `${d}d`;
  return `${Math.floor(d / 7)}w`;
}

/** Per-workspace identity for a row's workspace icon + disambiguating name.
 *  `ownerAgentId` feeds the canonical {@link WorkspaceIcon} (null → themed `W`;
 *  set → that agent's avatar). Activity never CLASSIFIES a workspace — it only
 *  resolves an identity — so this shape carries the id alone and no predicate
 *  (#task:2078).
 *
 *  Only the `workspace_channel` arm of a row's location has anything to look up
 *  here (#task:2115): a canonical `direct_pair` is workspace-LESS, so its row draws
 *  no workspace segment rather than an invented one. */
export interface ActivityWorkspaceRef {
  name: string;
  ownerAgentId: string | null;
}

/** Everything row 1 draws and everything `jump()` needs, resolved from the item's
 *  placement in ONE dispatch on `location.kind` (#task:2115) — so the glyph, the
 *  name, the workspace segment and the jump target cannot disagree about which
 *  placement this row is.
 *
 *  The two arms are asymmetric by design, not by omission:
 *    - `workspace_channel` → a workspace coordinate (icon + a name that
 *      disambiguates same-titled channels across workspaces) plus the
 *      conversation's server-side `title`;
 *    - `direct_pair` → NO workspace axis at all. `workspaceId` is `undefined` and
 *      nothing is fabricated to fill the segment: a faked coordinate would build a
 *      channel URL under a workspace the conversation is not in. The name is the
 *      COUNTERPART's, resolved by the pair SSOT (`presentCounterpart`), because a
 *      pair holds no title and the DTO deliberately carries a counterpart instead
 *      of a backend-synthesized display string — UI copy is never computed
 *      server-side (#task:2035). */
interface RowLocation {
  /** Non-null ⟺ the `direct_pair` arm; the row's whole identity. */
  counterpart: CounterpartPresentation | null;
  /** The workspace segment to draw, or null (a DM row, or an id the map has not
   *  resolved yet). */
  workspace: ActivityWorkspaceRef | null;
  /** The jump target's workspace coordinate — `undefined` for a DM, which is what
   *  routes it through the viewer's pair authority. From Activity that resolves to
   *  the workspace-less Activity route `/activity/d/:id` (stays in the shell,
   *  #task:2115); the section, not this field, decides the shell. */
  workspaceId: string | undefined;
  /** Row 1's name, and the subject fallback when the item carries no preview. */
  label: string;
}

function resolveRowLocation(
  location: ActivityItemDto["location"],
  workspaceRefById: ReadonlyMap<string, ActivityWorkspaceRef>,
): RowLocation {
  if (location.kind === "direct_pair") {
    const counterpart = presentCounterpart(location.counterpart);
    return {
      counterpart,
      workspace: null,
      workspaceId: undefined,
      label: counterpart.label,
    };
  }
  return {
    counterpart: null,
    workspace: workspaceRefById.get(location.workspace_id) ?? null,
    workspaceId: location.workspace_id,
    label: location.title,
  };
}

interface ActivityViewProps {
  /** Fixed list width when a conversation is open beside it; fills the section
   *  when nothing is selected. */
  width?: string;
  /** workspace_id → identity (name + icon inputs). A global list spans every
   *  workspace, so each row shows its workspace icon and a name that
   *  disambiguates same-named channels across workspaces. */
  workspaceRefById: ReadonlyMap<string, ActivityWorkspaceRef>;
  /** agent_id → agent, for an agent space's icon (the owning agent's avatar). */
  agentsById: ReadonlyMap<string, AgentDto>;
  /** Jump to a message source — the parent owns section-aware routing. The
   *  section decides the shell (an Activity-list jump STAYS in `/activity/...`;
   *  #task:2115) and the placement decides the coordinate: a workspace channel
   *  carries its own `workspaceId`, a DM has none (its route is workspace-less).
   *  Cross-workspace stable on the channel arm via that carried id. */
  onJump: (target: MessageJumpTarget) => void;
}

export function ActivityView({
  width,
  workspaceRefById,
  agentsById,
  onJump,
}: ActivityViewProps): JSX.Element {
  const readingPosition = useListReadingPosition("activity-list");
  const { data, isLoading, isError } = useActivity();
  const markAllRead = useMarkActivityAllRead();
  const compact = useCompactLayout();
  const [filter, setFilter] = useState<FilterKey>("all");

  const items = data?.items ?? [];

  // Resolve every FACE this list draws in ONE batch (leaf avatar components never
  // fetch — PR-2), keyed off the FULL list so a filter switch doesn't tear down +
  // refetch. Two producers feed it, and they share one batch and one key namespace
  // rather than growing a second parallel hook — so a principal that is both a
  // counterpart and a sender is fetched once:
  //   - each row's message sender (`preview_sender`);
  //   - each `direct_pair` row's COUNTERPART (#task:2115), which is that row's row-1
  //     identity.
  // display_name is server-guaranteed non-empty, so the fallback glyph path only
  // ever lacks an IMAGE, never a label.
  const avatarSources = useMemo<AgentAvatarSource[]>(() => {
    const byKey = new Map<string, string | null>();
    for (const it of items) {
      const sender = it.preview_sender;
      if (!sender) continue;
      // Keyed by the RESOLVED face, so a sender carrying no bytes (`deleted` /
      // `unresolved` → a neutral glyph) drops out of the batch instead of
      // occupying a slot that can never resolve to an image.
      const key = avatarBatchKey(presentAuthor(sender).avatar);
      if (key) byKey.set(key, sender.avatar_url);
    }
    // Counterparts go through the pair SSOT collector, so the active-only rule and
    // the dedup-by-id live in ONE place; Activity only adds the kind namespace its
    // shared batch needs (the collector has already split by exactly that axis).
    const dm = collectCounterpartAvatarSources(
      items.flatMap((it) => (it.location.kind === "direct_pair" ? [it.location] : [])),
    );
    for (const a of dm.agents) byKey.set(principalBatchKey("agent", a.id), a.avatar_url);
    for (const u of dm.users) byKey.set(principalBatchKey("user", u.id), u.avatar_url);
    return Array.from(byKey, ([id, avatar_url]) => ({ id, avatar_url }));
  }, [items]);
  // The hook keys its result map by the `id` we pass, so the namespaced key is
  // both what goes in and what rows look up by.
  const avatarUrls = useAgentAvatarObjectUrls(avatarSources);

  // Agent-space icons resolve through the SAME canonical `WorkspaceIcon` as the
  // rail / search / archived surfaces — so their uploaded agent avatars must be
  // resolved too, else the owned branch silently degrades to the runtime brand and
  // Activity becomes the only half-canonical surface. Batched over the stable
  // owning-agent set, mirroring `Sidebar`'s `ownerAvatarSources`.
  const ownerAvatarSources = useMemo<AgentAvatarSource[]>(() => {
    const out: AgentAvatarSource[] = [];
    for (const ref of workspaceRefById.values()) {
      // The id itself, not `isAgentSpace`: this loop needs the agent to build an
      // avatar source, so the null check is the identity read (#task:2078). The
      // old `kind !== 'agent_space' ||` conjunct in front of it could not exclude
      // anything the id alone did not already exclude.
      if (!ref.ownerAgentId) continue;
      const agent = agentsById.get(ref.ownerAgentId);
      if (!agent) continue;
      out.push({ id: agent.id, avatar_url: agent.avatar_url });
    }
    return out;
  }, [workspaceRefById, agentsById]);
  const ownerAgentAvatarUrls = useAgentAvatarObjectUrls(ownerAvatarSources);

  // Per-chip counts off the full (unfiltered) list, so each chip answers "how
  // many of this kind wait on me".
  const counts = useMemo(() => {
    const c: Record<FilterKey, number> = { all: 0, mention: 0, thread: 0 };
    for (const it of items) {
      c.all += 1;
      if ((it.mention_count ?? 0) > 0) c.mention += 1;
      if (it.thread_root_id != null) c.thread += 1;
    }
    return c;
  }, [items]);

  const filtered = useMemo(() => items.filter((it) => matchesFilter(it, filter)), [items, filter]);
  const directed = filtered.filter((it) => it.salience === "directed");
  const ambient = filtered.filter((it) => it.salience === "ambient");

  const markAll = () => markAllRead.mutate(items);
  const canMarkAll = items.length > 0 && !markAllRead.isPending;

  return (
    <PaneShell width={width}>
      <PaneHeader
        icon={<ActivityIcon className="size-3.5" />}
        title={t("activity.activity")}
        trailing={
          canMarkAll ? (
            <button
              type="button"
              title={t("activity.markAllRead")}
              aria-label={t("activity.markAllRead")}
              onClick={markAll}
              className={
                compact
                  ? "ml-auto glass-surface glass-round focus-outline"
                  : "ml-auto grid size-7 place-items-center rounded-(--control-radius) text-text-muted transition-colors hover:bg-(--bg-option-hover) hover:text-text-primary focus-outline"
              }
            >
              <CheckAllIcon size={compact ? 20 : 16} />
            </button>
          ) : undefined
        }
        nativeActions={[
          canMarkAll
            ? {
                id: "mark-all-read",
                label: t("activity.markAllRead"),
                icon: iconMarkup(CheckAllIcon),
                run: markAll,
              }
            : null,
        ]}
      />

      {/* Filter chips. Selected chips use the theme selection token, never a
          semantic hue (they must not steal the items' volume budget). */}
      <div className="flex shrink-0 items-center gap-1.5 px-4 py-2">
        {filterTabs().map((f) => {
          const on = f.key === filter;
          return (
            <button
              key={f.key}
              type="button"
              onClick={() => setFilter(f.key)}
              data-filter-chip
              className={
                "inline-flex items-center gap-1.5 border-(length:--control-border-width) rounded-(--chip-radius) px-2.5 py-1 text-xs font-medium transition-colors " +
                (on
                  ? "border-(--border-control-hover) bg-(--bg-row-selected) text-text-on-selected"
                  : "border-border bg-bg-floating text-text-muted hover:border-(--border-control-hover) hover:text-text-primary")
              }
            >
              {f.label}
              {counts[f.key] > 0 ? (
                <span className={on ? "opacity-100" : "opacity-60"}>{counts[f.key]}</span>
              ) : null}
            </button>
          );
        })}
      </div>

      <div {...readingPosition} data-scroll-root className="flex-1 overflow-y-auto px-4 py-3">
        {isLoading ? (
          <ActivityListSkeleton />
        ) : isError ? (
          // Distinct from the empty state — a failed load must not read as an
          // empty queue.
          <div className="flex flex-col items-center gap-1.5 py-16 text-center">
            <span className="font-serif text-base font-semibold text-text-secondary">
              {t("activity.couldntLoadActivity")}
            </span>
            <span className="text-sm text-text-muted">{t("activity.checkYourConnectionAnd")}</span>
          </div>
        ) : filtered.length === 0 ? (
          items.length === 0 ? (
            // The whole queue is empty — genuinely caught up.
            <div className="flex flex-col items-center gap-1.5 py-16 text-center">
              <span className="font-serif text-base font-semibold text-text-secondary">
                {t("activity.allCaughtUp")}
              </span>
            </div>
          ) : (
            // Items exist but the active filter excludes them; don't imply the
            // whole queue is empty.
            <div className="flex flex-col items-center gap-1.5 py-16 text-center">
              <span className="font-serif text-base font-semibold text-text-secondary">
                {t(`activity.emptyFilter.${filter}`)}
              </span>
            </div>
          )
        ) : (
          <div className="flex flex-col gap-5">
            {directed.length > 0 ? (
              <ActivitySection
                label={t("activity.requiresAttention")}
                items={directed}
                workspaceRefById={workspaceRefById}
                agentsById={agentsById}
                ownerAgentAvatarUrls={ownerAgentAvatarUrls}
                avatarUrls={avatarUrls}
                onJump={onJump}
              />
            ) : null}
            {ambient.length > 0 ? (
              <ActivitySection
                label={t("activity.subscribed")}
                items={ambient}
                workspaceRefById={workspaceRefById}
                agentsById={agentsById}
                ownerAgentAvatarUrls={ownerAgentAvatarUrls}
                avatarUrls={avatarUrls}
                onJump={onJump}
              />
            ) : null}
          </div>
        )}
      </div>
    </PaneShell>
  );
}

function ActivitySection({
  label,
  items,
  workspaceRefById,
  agentsById,
  ownerAgentAvatarUrls,
  avatarUrls,
  onJump,
}: {
  label: string;
  items: ActivityItemDto[];
  workspaceRefById: ReadonlyMap<string, ActivityWorkspaceRef>;
  agentsById: ReadonlyMap<string, AgentDto>;
  ownerAgentAvatarUrls: ReadonlyMap<string, string | null>;
  avatarUrls: ReadonlyMap<string, string | null>;
  onJump: (target: MessageJumpTarget) => void;
}): JSX.Element {
  return (
    <section>
      <div className="px-1 pb-1.5 text-(length:--text-badge) font-medium uppercase tracking-wider text-text-muted">
        {label}
      </div>
      <ul className="space-y-2">
        {items.map((item) => (
          <ActivityRow
            key={item.id}
            item={item}
            workspaceRefById={workspaceRefById}
            agentsById={agentsById}
            ownerAgentAvatarUrls={ownerAgentAvatarUrls}
            avatarUrls={avatarUrls}
            onJump={onJump}
          />
        ))}
      </ul>
    </section>
  );
}

function ActivityRow({
  item,
  workspaceRefById,
  agentsById,
  ownerAgentAvatarUrls,
  avatarUrls,
  onJump,
}: {
  item: ActivityItemDto;
  /** The whole map, not a pre-resolved ref: WHETHER this row has a workspace
   *  segment is part of the placement dispatch, and that dispatch happens exactly
   *  once, here (`resolveRowLocation`). */
  workspaceRefById: ReadonlyMap<string, ActivityWorkspaceRef>;
  agentsById: ReadonlyMap<string, AgentDto>;
  ownerAgentAvatarUrls: ReadonlyMap<string, string | null>;
  /** The list's ONE face batch, keyed by {@link principalBatchKey} — senders and
   *  DM counterparts alike. */
  avatarUrls: ReadonlyMap<string, string | null>;
  onJump: (target: MessageJumpTarget) => void;
}): JSX.Element {
  const clearItem = useClearActivityItem();

  // The row's ONE placement dispatch: row 1's glyph, name and workspace segment
  // plus the jump target all read from it (#task:2115).
  const location = resolveRowLocation(item.location, workspaceRefById);
  const isDirectPair = location.counterpart !== null;
  const counterpartKey = location.counterpart ? avatarBatchKey(location.counterpart.avatar) : null;
  const counterpartObjectUrl = counterpartKey ? (avatarUrls.get(counterpartKey) ?? null) : null;

  // Navigation needs a message anchor; agent_failed has none (pure read_at).
  const canJump = item.message_id != null;
  const jump = () => {
    if (item.message_id == null) return;
    onJump({
      // Undefined on the DM arm (see `RowLocation`): a workspace-less target is
      // what routes the jump through the viewer's pair authority, and a
      // fabricated coordinate would instead build a channel URL under a
      // workspace this conversation is not in. `handleJumpToMessage` keeps the
      // jump in-section, so from here it lands on `/activity/d/:id` (#task:2115).
      workspaceId: location.workspaceId,
      channelId: item.conversation_id,
      threadRootId: item.thread_root_id,
      messageId: item.message_id,
    });
  };

  const isThread = item.thread_root_id != null;
  // The triggering message (`preview`) and its `preview_sender` always pair:
  //   - thread WITH a root summary → row 2 = thread glyph + `thread_root_preview`
  //     (no sender — v1 shows no root author); row 3 = the reply (`preview`) with
  //     its sender.
  //   - otherwise (mainline / thread missing a root preview) → the
  //     `preview` line IS the last row and carries the sender; no separate row 3.
  // Server-resolved fields only, never a client-stitched message cache.
  const hasRootLine = isThread && item.thread_root_preview != null;
  // A DM row draws NO sender prefix, for two independent reasons:
  //   (a) row 1 already IS that face. In a pair the counterpart is the only other
  //       endpoint, so a prefix would draw the same entity twice on one card.
  //   (b) the two faces are not even the same FACT: row 1's counterpart is a LIVE
  //       join on the endpoint's principal, while a byline's deleted arm reads a
  //       SEND-TIME snapshot. Drawing both invites a card that disagrees with
  //       itself about who this is.
  const sender = isDirectPair ? null : item.preview_sender;
  const senderKey = sender ? avatarBatchKey(presentAuthor(sender).avatar) : null;
  const senderObjectUrl = senderKey ? (avatarUrls.get(senderKey) ?? null) : null;
  // The §5.3 source_ref-out for this item, resolved through the single routing
  // chokepoint (`activityItemSource`, keyed on KIND not salience):
  //   - agent_failed → by `notification_id`
  //   - unread (incl DM and mention streams, salience:'directed') → stream-head
  //     clear (may carry a thread root)
  // null only for a malformed item — then the row offers no ✓.
  const clearSource = activityItemSource(item);

  // Dense row-1 count metadata. Sits RIGHT AFTER the channel title (an
  // attention count belongs with the stream it counts, not floated to the far
  // edge) and carries a soft-accent fill so the unread signal reads at a glance
  // — louder than the old borderless neutral, but a step below the sidebar's
  // full-accent pill so the dense header line stays scannable (#task:1269
  // item 2). Mentions get their own `@N`; "N new" shows only the rest's existence.
  const pill =
    "shrink-0 rounded-(--control-radius) bg-accent-soft px-1.5 py-px text-xs font-medium leading-tight tabular-nums text-accent";
  const mentions = item.mention_count ?? 0;
  const countEl = (
    <>
      {mentions > 0 ? <span className={pill}>@{mentions > 99 ? "99+" : mentions}</span> : null}
      {item.unread_count != null && item.unread_count > mentions ? (
        <span className={pill}>
          {t("activity.newCount", { n: item.unread_count > 99 ? "99+" : item.unread_count })}
        </span>
      ) : null}
    </>
  );

  const handleRowClick = () => {
    if (!canJump) return;
    // Only a notification-backed item is cleared on open: opening the source
    // doesn't auto-read its notification, so clear it explicitly. A stream-backed
    // item (any `unread`, INCLUDING DM positional) is cleared by navigation's
    // open-time mark-read, so clearing again here would be a redundant second
    // path. Deriving this from `clearSource.kind` (not the item's kind/salience)
    // keeps the decision in lockstep with the routing chokepoint.
    if (clearSource?.kind === "notification") {
      clearItem.mutate(clearSource);
    }
    jump();
  };

  return (
    <li>
      {/* List card in the Saved-item language: themed border + bg-floating
          ground + `--row-radius`; poster repaints it as a 0-radius poster with
          ink-border + drop-shadow + lift hover via the shared `data-collection-card`
          hook (no per-row left rail). A row hover is a utility signal → ink
          border, never the brand hue. */}
      <CollectionItemCard
        role={canJump ? "button" : undefined}
        tabIndex={canJump ? 0 : undefined}
        onClick={handleRowClick}
        onKeyDown={(e) => {
          if (canJump && (e.key === "Enter" || e.key === " ")) {
            e.preventDefault();
            handleRowClick();
          }
        }}
        className={`group flex items-start gap-3 px-4 py-3 ${canJump ? "cursor-pointer" : ""}`}
      >
        <div className="min-w-0 flex-1">
          {/* row 1 — WHERE, per placement (#task:2115): a channel draws workspace
              icon + name then `#` + title; a pair draws the counterpart's face +
              name and no workspace segment. Canonical identity primitives (themed
              `WorkspaceIcon`, `data-channel-icon` `#`, `AuthorAvatarView`) replace
              the old type text label; poster repaints them as its posters. Segments
              self-delimit by identity glyph + `gap` — no `·` middot. */}
          <div className="flex min-w-0 items-center gap-1.5 text-xs text-text-muted">
            {location.workspace ? (
              <>
                <WorkspaceIcon
                  ownerAgentId={location.workspace.ownerAgentId}
                  agentsById={agentsById}
                  ownerAgentAvatarUrls={ownerAgentAvatarUrls}
                />
                <span className="min-w-0 shrink truncate text-text-secondary">
                  {location.workspace.name}
                </span>
              </>
            ) : null}
            <LocationGlyph
              item={item}
              counterpart={location.counterpart}
              objectUrl={counterpartObjectUrl}
            />
            <span className="min-w-0 shrink truncate text-text-secondary">{location.label}</span>
            {/* A deleted endpoint keeps its row — the conversation still exists and
                its unread still waits on the viewer — and says so through the SAME
                quiet marker a byline uses, next to the kind-generic label
                `presentCounterpart` owns. */}
            {location.counterpart?.isDeleted ? <DeletedActorMarker /> : null}
            {/* "N new" rides right after the title — an aggregate about THIS
                stream, kept next to its subject rather than floated to the row
                edge (the timestamp owns the far-right). */}
            {countEl}
          </div>

          {/* row 2 — thread root summary (thread glyph marks it as the root
              message), OR the triggering message line with its sender when
              there is no separate root. The root is CONTEXT, so it's recessed
              to muted ink (cohesive with its muted glyph) — the reply below is
              the focus and keeps primary ink (#task:1269 item 3). */}
          {hasRootLine ? (
            <div className="mt-1 flex items-center gap-2">
              <ThreadIcon aria-hidden className="size-3.5 shrink-0 text-text-muted" />
              <span className="min-w-0 flex-1 truncate text-sm text-text-muted">
                {plainTextFromMarkdown(item.thread_root_preview ?? "")}
              </span>
            </div>
          ) : (
            <SenderMessageLine
              sender={sender}
              objectUrl={senderObjectUrl}
              content={item.preview ? plainTextFromMarkdown(item.preview) : location.label}
            />
          )}

          {/* row 3 — the thread reply (`preview`) with its sender, shown only
              when row 2 already carries the thread root summary. */}
          {hasRootLine ? (
            <SenderMessageLine
              sender={sender}
              objectUrl={senderObjectUrl}
              content={item.preview ? plainTextFromMarkdown(item.preview) : null}
              className="mt-0.5"
            />
          ) : null}
        </div>

        {/* Timestamp pairs with the where-line, so it stays TOP-right (the row is
            items-start). */}
        <span className="shrink-0 text-(length:--text-meta) text-text-muted">
          {relTime(item.timestamp)}
        </span>
        {clearSource ? (
          // Always-visible source-routed clear (via `clearSource`: directed → by
          // notification_id; ambient → explicit stream-head clear, §9.10) — clears
          // WITHOUT navigating. Constantly present (not hover-revealed) because
          // users missed the hover-only affordance (shiki #task:1539); kept calm
          // at rest (secondary ink + subtle border) and only emphasized on hover,
          // so persistence doesn't add row volume. `self-center` floats it to the
          // whole item's vertical center regardless of text-row count.
          <button
            type="button"
            title={t("activity.markAsRead")}
            aria-label={t("activity.markAsRead")}
            onClick={(e) => {
              e.stopPropagation();
              clearItem.mutate(clearSource);
            }}
            className="grid size-7 shrink-0 self-center place-items-center rounded-(--control-radius) border-(length:--control-border-width) border-border bg-bg-floating text-text-secondary transition-colors hover:border-(--border-control-hover) hover:text-text-primary"
          >
            <CheckIcon className="size-4" />
          </button>
        ) : null}
      </CollectionItemCard>
    </li>
  );
}
