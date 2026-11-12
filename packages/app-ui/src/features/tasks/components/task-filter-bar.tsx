import {
  forwardRef,
  useId,
  useMemo,
  useState,
  type ButtonHTMLAttributes,
  type KeyboardEvent,
  type ReactNode,
} from "react";
import * as Popover from "@radix-ui/react-popover";
import {
  CheckIcon,
  ChevronDownIcon,
  CreatedByIcon,
  LayersIcon,
  SearchIcon,
  UndoIcon,
  UserIcon,
} from "@/icons";
import { cn } from "@/lib/cn";
import { WorkspaceIcon } from "@/components/workspace-icon";
import { ChannelIcon } from "@/components/channel-icon";
import { useCompactLayout } from "@/navigation/layout";
import { AVATAR_PX } from "@/components/avatar-scale";
import { useDirectPairs } from "@/features/direct-pairs/hooks";
import { directLinePairVisibleInSidebar } from "@/features/direct-pairs/direct-line-rows";
import { presentCounterpart } from "@/features/direct-pairs/counterpart-presentation";
import { AuthorAvatarView } from "@/features/messages/components/author-avatar-view";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import {
  PersonAvatar,
  usePersonDirectory,
  type PersonDirectory,
} from "@/features/tasks/components/person-avatar";
import { useRovingList } from "@/features/tasks/model/task-filter-keyboard";
import {
  mapBoardAuthority,
  personMenuState,
  type BoardAuthority,
  type BoardFacets,
} from "@/features/tasks/model/board-authority";
import {
  applyBoardScopeSelection,
  applyWorkspaceCascade,
  boardScopeGroups,
  channelAxisScope,
  personFilterKey,
  taskFiltersEqual,
  workspaceBoardContainer,
  type BoardScopeSelection,
  type ChannelSourceGroup,
  type PersonFilter,
  type PersonOption,
  type TaskBoardContainerAxis,
  type TaskFilters,
} from "@/features/tasks/model/task-filters";
import { t, useLocale } from "@/lib/i18n";

/** Rows for a population nobody has been told about yet — never rendered as an
 *  answer; the menu's footer says which non-answer it is. */
const NO_PEOPLE: PersonOption[] = [];

/**
 * Unified task filter bar — one row that replaces the old channel/
 * conversation chip strip. Holds the non-status narrowing (channel,
 * assignee, creator) so the kanban columns stay purely a status axis
 * (Todo / In progress / In review / Done / Closed — `closed` is a status,
 * not a separate visibility dimension; design #task:769 / #task:770).
 *
 * Theme adaptation (Cindy acceptance): triggers are control surfaces
 * (`--control-radius` + `--control-border-width`); dropdown panels carry
 * `role="listbox"` / option rows `role="option"` and route radius through
 * `--select-content-radius` / `--select-item-radius`; selected state
 * reuses `--accent`. No hardcoded radius/border literals, so poster stays
 * square + hard-edged automatically.
 */
export interface ChannelOption {
  id: string;
  /** Owning workspace id — the workspace→channel cascade scopes and clears
   *  channel options by this (shiki #task:1387). Presentation still groups by
   *  data source (`group`), not by this id. */
  workspaceId: string;
  /** Primary line — always the channel TITLE. This is a channel picker, so
   *  the row reads as the channel; the source (workspace name, or the owning
   *  agent's name for an agent space) goes in `sublabel` (shiki #task:1165). */
  label: string;
  /** Data-source group for the dropdown's section split (#task:1165):
   *  `'agent_space'` = the channel comes from an agent space, `'workspace'` = a
   *  regular workspace channel. Grouping is a PRESENTATION axis only — the
   *  filter value stays a plain channel id. */
  group: ChannelSourceGroup;
  /** Secondary line = the source: the workspace name for a regular channel,
   *  the AGENT'S NAME for an agent-space channel (#task:2044 — no `@`, that
   *  form belongs to the direct pair). This is what disambiguates the several
   *  channels inside one agent's space: distinct titles, shared subtitle. */
  sublabel?: string;
}

/** A workspace offered by the upstream workspace filter (#task:1387). Both
 *  people-owned workspaces and agent spaces are selectable (shiki review: match the
 *  message-search workspace filter, which lists them too). `ownerAgentId` is both
 *  halves of the identity: it says WHICH side of the split this row is on and, for
 *  an agent space, WHICH agent's avatar the canonical {@link WorkspaceIcon} shows —
 *  a serif "P" when it is null (#task:2078). */
export interface WorkspaceOption {
  id: string;
  label: string;
  /** The agent that owns this container, or null for a people-owned workspace.
   *  Classify through `isAgentSpace`, not by checking it inline. */
  owner_agent_id: string | null;
}

/** A conversation considered for the channel-filter dropdown, normalized
 *  from whatever read model the caller has on hand. */
export interface ChannelFilterCandidate {
  id: string;
  /** Owning workspace id — carried through to `ChannelOption.workspaceId`
   *  for the workspace→channel cascade (#task:1387). */
  workspaceId: string;
  title: string;
  archived_at: number | null;
  /** The data-source group, asked of the CONTAINER: true when this channel lives
   *  in an agent space. Required, because it is the group's only determinant —
   *  an optional bit would let a producer stay silent and be filed under
   *  Workspaces by default. Classify through `isAgentSpace` (#task:2077). */
  fromAgentSpace: boolean;
  /** Secondary line = the source (workspace name, or the agent's name for an
   *  agent space). */
  sublabel?: string;
}

/**
 * Single source of truth for *which* conversations are valid channel-filter
 * options. The eligibility rules — never offer an archived channel, dedup by
 * id, and map to the dropdown's `ChannelOption` shape — live here ONLY, so the
 * task section (TasksSection) and the per-conversation Tasks tab (ChannelTasksTab)
 * cannot drift. Callers own their own *scope* (which candidates to feed in)
 * and their own *ordering* (sort the result); those are legitimate
 * per-surface differences. Eligibility and option shape are not.
 *
 * Insertion order is preserved, so a caller that pre-sorts its candidates
 * gets that order back.
 */
export function buildChannelFilterOptions(
  candidates: Iterable<ChannelFilterCandidate>,
): ChannelOption[] {
  const byId = new Map<string, ChannelOption>();
  for (const c of candidates) {
    if (c.archived_at !== null) continue;
    if (byId.has(c.id)) continue;
    byId.set(c.id, {
      id: c.id,
      workspaceId: c.workspaceId,
      label: c.title,
      // The group is a CONTAINER question, so the container answers it. This
      // used to also read `roster_policy === 'fixed'`, which is a ROSTER
      // (capability) fact: redundant wherever the container already said yes,
      // and wrong wherever it said no — a fixed-roster channel in a people-owned
      // workspace would have been filed under Agent Spaces (#task:2077).
      group: c.fromAgentSpace ? "agent_space" : "workspace",
      ...(c.sublabel !== undefined ? { sublabel: c.sublabel } : {}),
    });
  }
  return [...byId.values()];
}

/** The `#` glyph for channel-filter dropdown rows — a sticker-variant
 *  {@link ChannelIcon} so it picks up the theme's `#` sticker recipe, the same
 *  identity `#` as the sidebar's channel rows. The compact filter TRIGGER uses the
 *  utility variant instead (no sticker hook → can't inflate; #task:1437). The
 *  source (workspace, or the owning agent for an agent space) is conveyed by the
 *  subtitle, not the icon (shiki #task:1165). */
function ChannelGlyph(): JSX.Element {
  return <ChannelIcon variant="sticker" />;
}

export interface TaskFilterBarProps {
  filters: TaskFilters;
  onChange: (next: TaskFilters) => void;
  /** Reset target. Clear shows only when `filters` differ from this and
   *  resets every dimension (workspace / channel / assignee / creator) back
   *  to it. Global passes DEFAULT_TASK_FILTERS; the per-conversation Tasks tab passes its
   *  current-channel default, so Clear restores that tab's entry
   *  context rather than jumping to All (Cindy review). */
  baselineFilters: TaskFilters;
  /** Upstream cascade axis (#task:1387). Empty → the workspace filter hides,
   *  so a single-workspace deployment doesn't grow a pointless control. */
  workspaceOptions: WorkspaceOption[];
  channelOptions: ChannelOption[];
  /** Label for the "no channel filter" row, e.g. "All channels". */
  allChannelsLabel: string;
  /**
   * The person menus' population as the facets face answered it — the AUTHORITY,
   * not a plain option set.
   *
   * An empty set here is a real claim about this viewer ("nobody to filter by"),
   * so a cold or failed read must not be able to arrive as one: it would present
   * a transport failure as an empty workspace (#task:1885, Cindy MUST FIX). The
   * `hasUnassigned` bit rides along for the same reason — it gates the
   * "No assignee" row, which would otherwise read as a stray pseudo-person when
   * every task is claimed (shiki feedback).
   */
  people: BoardAuthority<BoardFacets>;
  className?: string;
}

export function TaskFilterBar({
  filters,
  onChange,
  baselineFilters,
  workspaceOptions,
  channelOptions,
  allChannelsLabel,
  people,
  className,
}: TaskFilterBarProps): JSX.Element {
  const dirty = !taskFiltersEqual(filters, baselineFilters);

  // Consumer mode fork (contract §5). A baseline that names a conversation is
  // only ever a conversation's own Tasks tab (a workspace channel or a direct
  // line); the global board's baseline names none. That tab means "this
  // conversation's tasks", so the board scope and channel axes are hidden
  // rather than offered as a way out; only the Assignee / From narrowing shows.
  // Looking across conversations is the global board's job.
  const scopeLocked = baselineFilters.channelId !== null;

  // Person avatars for the Assignee / From dropdowns (shiki #task:1441).
  // Resolved ONCE here (agents + users directories + their avatar object URLs)
  // and shared with both PersonFilterControls, so the two dropdowns don't each
  // re-resolve the directory.
  const personDirectory = usePersonDirectory();

  // The channel axis under the current container, resolved through the pure
  // model (#task:2041): `channelAxisScope` decides whether the channel picker
  // shows (hidden under DM, where the pair conversation is the coordinate) and
  // over which channel population. The board scope itself (container + coordinate
  // together) is applied by `applyBoardScopeSelection` at BoardScopeFilter's
  // onSelect; the workspace / agent-space objects are listed by that dropdown
  // directly, not by this model.
  const scope = useMemo(
    () => channelAxisScope(filters.container, channelOptions),
    [filters.container, channelOptions],
  );

  // Cascade (#task:1387): the channel filter shows only channels of the
  // selected workspace; at "All workspaces" it shows the container-scoped set.
  // Selecting a workspace also drops a channel that no longer belongs to it —
  // never an orphan channel that yields an empty board.
  const visibleChannelOptions = useMemo(
    () =>
      filters.workspaceId
        ? scope.channelOptions.filter((o) => o.workspaceId === filters.workspaceId)
        : scope.channelOptions,
    [scope.channelOptions, filters.workspaceId],
  );
  const channelWorkspaceId = useMemo(() => {
    const byId = new Map(channelOptions.map((o) => [o.id, o.workspaceId]));
    return (channelId: string): string | null => byId.get(channelId) ?? null;
  }, [channelOptions]);

  // On a wide pane the two axes people switch most, project and assignee, lie
  // flat so one click applies them; channel and creator stay behind dropdowns.
  const compact = useCompactLayout();
  const selectScope = (selection: BoardScopeSelection): void => {
    onChange(applyBoardScopeSelection(filters, selection, channelWorkspaceId));
  };

  const channelControl =
    !scopeLocked && scope.show && visibleChannelOptions.length > 0 ? (
      <ChannelFilter
        options={visibleChannelOptions}
        allChannelsLabel={allChannelsLabel}
        value={filters.channelId}
        onChange={(channelId) => onChange({ ...filters, channelId })}
        ghost={!compact}
      />
    ) : null;
  const creatorControl = (
    <PersonFilterControl
      icon={<CreatedByIcon className="size-3.5" />}
      label={t("tasks.from")}
      anyLabel={t("tasks.all")}
      population={mapBoardAuthority(people, (f) => f.creators)}
      value={filters.creator}
      onChange={(creator) => onChange({ ...filters, creator })}
      ghost={!compact}
      {...personDirectory}
    />
  );
  const resetControl = dirty ? (
    <button
      type="button"
      onClick={() => onChange(baselineFilters)}
      aria-label={t("tasks.resetFilters")}
      title={t("tasks.resetFilters")}
      className={cn(
        "inline-flex shrink-0 items-center justify-center rounded-(--control-radius) p-1.5",
        "text-text-muted transition-colors hover:bg-bg-hover hover:text-text-primary",
      )}
    >
      <UndoIcon className="size-3.5" />
    </button>
  ) : null;

  if (!compact) {
    return (
      <div
        data-task-filter-bar
        // The bar sizes to its content so New task follows the filters instead of
        // jumping to the far end; it shrinks (project chips scroll) when narrow.
        className={cn("flex min-w-0 items-center gap-1.5", className)}
      >
        {scopeLocked ? null : (
          <ScopeStrip
            workspaceOptions={workspaceOptions}
            filters={filters}
            onSelect={selectScope}
            {...personDirectory}
          />
        )}
        <AssigneeAvatars
          population={mapBoardAuthority(people, (f) => f.assignees)}
          allowUnassigned={people.known && people.value.hasUnassigned}
          value={filters.assignee}
          onChange={(assignee) => onChange({ ...filters, assignee })}
          {...personDirectory}
        />
        <span aria-hidden className="mx-1 h-4 w-px shrink-0 bg-border-subtle" />
        {channelControl}
        {creatorControl}
        {resetControl}
      </div>
    );
  }

  return (
    <div data-task-filter-bar className={cn("flex flex-wrap items-center gap-1.5", className)}>
      {/* Coarsest axis first: WHICH container a task lives in. One grouped
          dropdown (#task:2041, shiki 2026-08-17) — an "All" row over two sections
          (Workspaces / Agent Spaces), each listing the concrete objects — so a
          pick names the container KIND and the object together instead of the old
          pick-kind-then-object two-step. DM is never a selectable scope here
          (contract §5): its tasks reach the board through "All" or their own
          locked Tasks tab. It leads the row because it reshapes what the downstream
          channel menu even describes, and reads left-to-right as narrowing. Hidden
          entirely in a locked direct_pair context (§5), where there is no scope to
          switch to. */}
      {scopeLocked ? null : (
        <BoardScopeFilter
          workspaceOptions={workspaceOptions}
          containerValue={filters.container}
          workspaceValue={filters.workspaceId}
          onSelect={selectScope}
          {...personDirectory}
        />
      )}
      {channelControl}
      <PersonFilterControl
        icon={<UserIcon className="size-3.5" />}
        label={t("tasks.assignee")}
        anyLabel={t("tasks.all")}
        population={mapBoardAuthority(people, (f) => f.assignees)}
        allowUnassigned={people.known && people.value.hasUnassigned}
        value={filters.assignee}
        onChange={(assignee) => onChange({ ...filters, assignee })}
        {...personDirectory}
      />
      {creatorControl}
      {resetControl}
    </div>
  );
}

// ── Shared trigger + panel shells ──────────────────────────────────

/** Icon edge (px) for the WORKSPACE filter trigger. The plain glyphs
 *  (`UserIcon`, `CreatedByIcon`) render at `size-3.5` = 14px and the channel `#` uses
 *  `ChannelIcon` at `md` (also 14px); `WorkspaceIcon` takes a numeric `size`,
 *  so it consumes this value to sit at the same height. Kept == 14 (the
 *  control density) so no trigger glyph drifts taller than the row
 *  (#task:1437 / #task:1438). */
const TRIGGER_GLYPH_PX = 14;

/**
 * Box recipe for a control that must line up in height with the filter chips
 * (`FilterTrigger`) on a task toolbar row — the New task CTA that sits right
 * after the filters (shiki: 高度和前面 filter button 保持一致). Mirrors the
 * trigger's box EXACTLY — `px-2 py-1 text-[length:var(--text-meta)]` plus a border-box border of
 * `--control-border-width` — instead of a fixed height, because the filter's
 * own height is content-driven and grows with the per-theme border (1px default
 * / 2px poster). Spread this over the CTA's own fill/identity: only the OUTER
 * box is shared.
 *
 * The border is `transparent` here on purpose — the CTA has no consumer-edge
 * frame (its identity is the fill; poster repaints this same border into the
 * poster ink frame via `[data-primary-action]`). But it still RESERVES the
 * border width, so a borderless fill can't come out 1–2px shorter than the
 * bordered filters beside it in any theme.
 */
export const FILTER_ROW_TRIGGER_BOX =
  "h-auto px-2 py-1 text-[length:var(--text-meta)] leading-[14px] border-[length:var(--control-border-width)] border-transparent";

const FilterTrigger = forwardRef<
  HTMLButtonElement,
  ButtonHTMLAttributes<HTMLButtonElement> & {
    icon: ReactNode;
    active: boolean;
    /** Borderless, for the wide toolbar where it sits beside the flat scope chips. */
    ghost?: boolean;
  }
>(function FilterTrigger({ icon, active, ghost = false, children, className, ...props }, ref) {
  return (
    <button
      ref={ref}
      type="button"
      data-filter-trigger
      data-active={active || undefined}
      data-ink-selected={(active && !ghost) || undefined}
      className={cn(
        "flex max-w-50 items-center gap-1.5 rounded-(--control-radius)",
        "border-(length:--control-border-width) px-2 py-1 text-(length:--text-meta) transition-colors",
        // Hover always paints the theme's control hover border
        // (`--border-control-hover`: poster near-black ink / warm
        // accent / slack-dark `--border` 10% white). Note this is a
        // CONTROL-surface hover token, not the row-state token —
        // slack-dark intentionally sets `--border-row-hover:
        // transparent` (sidebar bg veil expresses hover there), and
        // we don't want that to bleed into control triggers where the
        // border is the only hover signal (#task:912 review round 3).
        ghost
          ? cn("border-transparent", flatChipTone(active))
          : "hover:border-(--border-control-hover)",
        ghost
          ? null
          : active
            ? // Non-default value → theme selected-row fill (poster festive
              // lime) + on-selected ink + the selected-row border colour
              // (`--border-row-selected`, same near-black as hover) so the
              // chip reads as a framed poster sticker, not a borderless fill
              // (shiki #task:790).
              "border-(--border-row-selected) bg-(--bg-row-selected) text-text-on-selected"
            : "border-border-subtle bg-bg-primary text-text-secondary hover:text-text-primary",
        className,
      )}
      {...props}
    >
      <span
        className={cn(
          "flex shrink-0 items-center",
          ghost ? "opacity-70" : active ? "text-text-on-selected" : "text-text-muted",
        )}
      >
        {icon}
      </span>
      <span className="truncate leading-[14px]">{children}</span>
      <ChevronDownIcon
        className={cn(
          "size-3 shrink-0",
          ghost ? "opacity-70" : active ? "text-text-on-selected" : "text-text-muted",
        )}
      />
    </button>
  );
});

function FilterPanel({
  id,
  activeDescendant,
  onKeyDown,
  children,
}: {
  id?: string;
  activeDescendant?: string;
  onKeyDown?: (e: KeyboardEvent) => void;
  children: ReactNode;
}): JSX.Element {
  return (
    <Popover.Portal>
      <Popover.Content
        align="start"
        sideOffset={4}
        role="listbox"
        id={id}
        aria-activedescendant={activeDescendant}
        onKeyDown={onKeyDown}
        data-task-filter-panel
        data-dropdown-frame
        className="scrollbar-hide z-(--z-toolbar) max-h-[50vh] min-w-45 overflow-y-auto rounded-(--select-content-radius) bg-bg-floating p-1 shadow-popover outline-none"
      >
        {children}
      </Popover.Content>
    </Popover.Portal>
  );
}

export function OptionRow({
  selected,
  neutral = false,
  active = false,
  id,
  optionRef,
  onHover,
  onSelect,
  children,
}: {
  selected: boolean;
  /** This row is the "no filter" default (ALL / All channels). When it's
   *  the current value it gets a muted check but NOT the lime active fill
   *  — lime in this surface means "a filter is applied", which would
   *  contradict the plain (inactive) trigger for the ALL state (shiki +
   *  Cindy #task:790). */
  neutral?: boolean;
  /** Keyboard roving highlight (#task:1257). Utility signal only, so it
   *  reuses the neutral `--bg-hover` fill (same as mouse hover), never
   *  the brand lime — that stays reserved for the applied-filter selection. */
  active?: boolean;
  id?: string;
  optionRef?: (el: HTMLButtonElement | null) => void;
  onHover?: () => void;
  onSelect: () => void;
  children: ReactNode;
}): JSX.Element {
  const activeSelected = selected && !neutral;
  return (
    <button
      ref={optionRef}
      id={id}
      type="button"
      role="option"
      aria-selected={selected}
      data-active-option={active || undefined}
      data-option-row=""
      data-ink-selected={activeSelected || undefined}
      onMouseMove={onHover}
      onClick={onSelect}
      className={cn(
        // Hover = neutral preview FILL only, no border, via the shared
        // `--bg-hover` token — never the brand lime, which stays reserved
        // for the applied-filter selection. The transparent border is kept
        // only to reserve layout space (no hover colour) so rows don't shift
        // (shiki #task:790).
        "flex w-full items-center gap-2 rounded-(--select-item-radius) px-2 py-1.5 text-left text-xs transition-colors",
        "border border-transparent",
        !activeSelected && "hover:bg-(--bg-hover) hover:text-text-primary",
        // A real (non-default) current value = the app-wide selected-row
        // vocabulary (festive lime fill + on-selected ink), NOT the magenta
        // `accent-soft` tint. The ALL/All-channels default row stays plain
        // text even when current (shiki #task:790: ALL was reading pink).
        // Keyboard-highlighted (but not the current value) → neutral hover
        // fill, so mouse and keyboard share one visual language.
        activeSelected
          ? "bg-(--bg-row-selected) text-text-on-selected"
          : active
            ? "bg-(--bg-hover) text-text-primary"
            : "text-text-secondary",
      )}
    >
      <span className="min-w-0 flex-1 truncate">{children}</span>
      {selected ? (
        <CheckIcon
          className={cn(
            "size-3.5 shrink-0",
            // Real selection → on-selected ink on the lime fill; the
            // neutral default current row → a muted check (no pink, no
            // lime) so it reads as "current" without the active look.
            activeSelected ? "text-text-on-selected" : "text-text-muted",
          )}
        />
      ) : null}
    </button>
  );
}

// ── Channel (searchable) ───────────────────────────────────────────

function ChannelFilter({
  options,
  allChannelsLabel,
  value,
  onChange,
  ghost,
}: {
  options: ChannelOption[];
  allChannelsLabel: string;
  value: string | null;
  onChange: (id: string | null) => void;
  ghost?: boolean;
}): JSX.Element {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const selected = value ? (options.find((o) => o.id === value) ?? null) : null;
  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return options;
    return options.filter(
      (o) => o.label.toLowerCase().includes(q) || (o.sublabel?.toLowerCase().includes(q) ?? false),
    );
  }, [options, query]);
  // Order the options — regular-workspace channels first, then agent-space
  // channels — so the flat list reads in a stable grouping. No section HEADERS:
  // each row's sublabel already names its source (workspace / agent), so a
  // "Workspaces" header over channel rows was both redundant and semantically
  // wrong (shiki #task:1387 review — the label named workspaces but the rows are
  // channels). The ALL row stays at the top.
  const locale = useLocale();
  const sections = useMemo(
    () =>
      [
        { key: "workspace" as const, label: t("tasks.projects") },
        { key: "agent_space" as const, label: t("tasks.agentSpaces") },
      ]
        .map((s) => ({ ...s, items: filtered.filter((o) => o.group === s.key) }))
        .filter((s) => s.items.length > 0),
    [locale, filtered],
  );

  // Flat, in-render-order option list for roving keyboard nav (#task:1257):
  // the ALL row at index 0, then every section's items (Workspaces then Agent
  // Spaces), matching the visual order below so arrow keys walk the list as it
  // reads.
  const flatOptions = useMemo(() => sections.flatMap((s) => s.items), [sections]);
  const optionIndex = useMemo(() => {
    const m = new Map<string, number>();
    flatOptions.forEach((o, i) => m.set(o.id, i + 1)); // +1: ALL occupies 0
    return m;
  }, [flatOptions]);
  const listId = useId();
  const optionId = (index: number): string => `${listId}-opt-${index}`;
  const roving = useRovingList({
    count: flatOptions.length + 1,
    open,
    resetKey: query,
    onSelect: (index) => {
      if (index === 0) onChange(null);
      else {
        const o = flatOptions[index - 1];
        if (o) onChange(o.id);
      }
      setOpen(false);
    },
  });

  const renderOption = (o: ChannelOption): JSX.Element => {
    const index = optionIndex.get(o.id) ?? -1;
    return (
      <OptionRow
        key={o.id}
        selected={value === o.id}
        active={roving.activeIndex === index}
        id={optionId(index)}
        optionRef={(el) => roving.registerItem(index, el)}
        onHover={() => roving.setActiveIndex(index)}
        onSelect={() => {
          onChange(o.id);
          setOpen(false);
        }}
      >
        <span className="flex min-w-0 items-center gap-1.5">
          <span className="shrink-0 text-text-muted">
            <ChannelGlyph />
          </span>
          <span className="flex min-w-0 flex-col">
            <span className="truncate">{o.label}</span>
            {o.sublabel ? (
              <span className="truncate text-(length:--text-badge) text-text-muted">
                {o.sublabel}
              </span>
            ) : null}
          </span>
        </span>
      </OptionRow>
    );
  };

  return (
    <Popover.Root
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) setQuery("");
      }}
    >
      <Popover.Trigger asChild>
        <FilterTrigger
          // Sticker-variant `#` (shiki #task:1441): the selected-label `#`
          // wears the same yellow poster sticker as the dropdown rows + the rest
          // of the app, so the channel's identity glyph reads consistently. The
          // `task-filter-channel-hash` class lets poster TIGHTEN this one sticker
          // to the shared 14px trigger box (12px glyph + 1px ink frame, no
          // padding) so it sits height-uniform with its sibling triggers instead
          // of inflating to ~18px — identity AND alignment (shiki #task:1441
          // review). warm/slack carry no box chrome, so `md` (14px) is untouched
          // there.
          icon={<ChannelIcon variant="sticker" density="md" className="task-filter-channel-hash" />}
          active={value !== null}
          ghost={ghost}
          aria-label={t("tasks.channel")}
        >
          {t("tasks.channelFilter", { value: selected?.label ?? allChannelsLabel })}
        </FilterTrigger>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          align="start"
          sideOffset={4}
          data-task-filter-panel
          data-dropdown-frame
          className="z-(--z-toolbar) flex max-h-[50vh] w-60 flex-col rounded-(--select-content-radius) bg-bg-floating p-1 shadow-popover outline-none"
        >
          <div
            data-input=""
            className="flex items-center gap-1.5 rounded-(--control-radius) border-(length:--control-border-width) border-border bg-bg-base px-2 py-1.5 transition-colors focus-within:border-border-focus"
          >
            <SearchIcon className="size-3.5 shrink-0 text-text-muted" />
            <input
              autoFocus
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={roving.onKeyDown}
              role="combobox"
              aria-expanded={open}
              aria-controls={listId}
              aria-activedescendant={optionId(roving.activeIndex)}
              placeholder={t("tasks.searchChannels")}
              className="w-full bg-transparent text-xs text-text-primary outline-none placeholder:text-text-muted"
            />
          </div>
          <div
            role="listbox"
            id={listId}
            className="scrollbar-hide mt-1 min-h-0 flex-1 overflow-y-auto"
          >
            <OptionRow
              selected={value === null}
              neutral
              active={roving.activeIndex === 0}
              id={optionId(0)}
              optionRef={(el) => roving.registerItem(0, el)}
              onHover={() => roving.setActiveIndex(0)}
              onSelect={() => {
                onChange(null);
                setOpen(false);
              }}
            >
              {allChannelsLabel}
            </OptionRow>
            {flatOptions.map(renderOption)}
            {filtered.length === 0 ? (
              <p className="px-2 py-3 text-center text-(length:--text-meta) italic text-text-muted">
                {t("tasks.noMatches")}
              </p>
            ) : null}
          </div>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

// ── Board scope (container × concrete object) ─────────────────────

/** One selectable row in the board-scope dropdown — a workspace whose pick stamps
 *  the container matching its OWNERSHIP (people-owned → `workspace`, agent space →
 *  `agent_space`). A DM is never a row here (contract §5). */
interface ScopeRow {
  id: string;
  label: string;
  ws: WorkspaceOption;
}

/**
 * The board-scope filter — the coarsest axis, as ONE grouped dropdown
 * (#task:2041, shiki 2026-08-17). An "All" row (the union of every container the
 * viewer can see, DM tasks included) sits above two sections that list the
 * CONCRETE objects the viewer can narrow to: Workspaces (people-owned) and Agent
 * Spaces. Picking a leaf names the board container and the object together —
 * `applyBoardScopeSelection` stamps both — so the old pick-container-then-object
 * two-step is gone, and with it the bare "all agent spaces" pick (a container with
 * no object): the "All" row covers the union, and every listed row is a
 * concrete scope. A workspace row wears its own icon (an agent space = its agent's
 * avatar), resolved from the directory the person menus already
 * loaded (no extra fetch). Direct Messages are not a section: a DM is never a
 * selectable global scope (contract §5) — its tasks reach the board through the
 * "All" union or the direct line's own locked Tasks tab, which hides this
 * trigger entirely.
 */
function BoardScopeFilter({
  workspaceOptions,
  containerValue,
  workspaceValue,
  onSelect,
  agentsById,
  agentAvatarUrls,
}: {
  workspaceOptions: WorkspaceOption[];
  containerValue: TaskBoardContainerAxis | null;
  workspaceValue: string | null;
  onSelect: (selection: BoardScopeSelection) => void;
} & PersonDirectory): JSX.Element {
  const [open, setOpen] = useState(false);

  // The two sections come from the pure `boardScopeGroups` seam (ownership →
  // section and the show-iff-rows rule live there, tested without a DOM); this memo
  // only projects each section's workspaces into the ScopeRow shape the list renders
  // and roves over.
  const groups = useMemo(
    () =>
      boardScopeGroups<WorkspaceOption>({ workspaces: workspaceOptions }).map((g) => ({
        key: g.key,
        label: g.label,
        rows: g.workspaces.map((ws): ScopeRow => ({ id: ws.id, label: ws.label, ws })),
      })),
    [workspaceOptions],
  );

  // Flatten for roving focus: index 0 is the All row, then every selectable row
  // in section order.
  const flatRows = useMemo(() => groups.flatMap((g) => g.rows), [groups]);
  const optionIndex = useMemo(() => {
    const m = new Map<string, number>();
    flatRows.forEach((r, i) => m.set(r.id, i + 1)); // +1: All occupies 0
    return m;
  }, [flatRows]);

  const selectionFor = (row: ScopeRow): BoardScopeSelection => ({
    kind: "workspace",
    workspaceId: row.ws.id,
    container: workspaceBoardContainer(row.ws),
  });

  const listId = useId();
  const optionId = (index: number): string => `${listId}-opt-${index}`;
  const roving = useRovingList({
    count: flatRows.length + 1,
    open,
    onSelect: (index) => {
      if (index === 0) onSelect({ kind: "all" });
      else {
        const row = flatRows[index - 1];
        if (row) onSelect(selectionFor(row));
      }
      setOpen(false);
    },
  });

  // Trigger identity mirrors the selection: a workspace's own icon, or the generic
  // Layers for All — sized to the shared 14px control-glyph box so the trigger
  // never drifts taller than the row (#task:1437).
  const selectedWs = flatRows.find((r) => r.ws.id === workspaceValue);
  const triggerIcon = selectedWs ? (
    <WorkspaceIcon
      ownerAgentId={selectedWs.ws.owner_agent_id}
      agentsById={agentsById}
      ownerAgentAvatarUrls={agentAvatarUrls}
      size={TRIGGER_GLYPH_PX}
    />
  ) : (
    <LayersIcon className="size-3.5" />
  );

  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <FilterTrigger
          icon={triggerIcon}
          // Active = a concrete scope is chosen (container ≠ null): the lime chip
          // signals the board is narrowed, same vocabulary as the sibling
          // triggers. All is the neutral default.
          active={containerValue !== null}
          aria-label={t("tasks.boardScope")}
        >
          {t("tasks.projectFilter", {
            value:
              selectedWs?.label ??
              (containerValue === "direct_messages" ? t("tasks.directMessages") : t("tasks.all")),
          })}
        </FilterTrigger>
      </Popover.Trigger>
      <FilterPanel
        id={listId}
        activeDescendant={optionId(roving.activeIndex)}
        onKeyDown={roving.onKeyDown}
      >
        <OptionRow
          selected={containerValue === null}
          neutral
          active={roving.activeIndex === 0}
          id={optionId(0)}
          optionRef={(el) => roving.registerItem(0, el)}
          onHover={() => roving.setActiveIndex(0)}
          onSelect={() => {
            onSelect({ kind: "all" });
            setOpen(false);
          }}
        >
          {t("tasks.all")}
        </OptionRow>
        {groups.map((group) => (
          <div key={group.key}>
            <div className="px-2 pb-1 pt-2 text-(length:--text-badge) font-medium uppercase tracking-wider text-text-muted">
              {group.label}
            </div>
            {group.rows.map((row) => {
              const index = optionIndex.get(row.id) ?? -1;
              return (
                <OptionRow
                  key={row.id}
                  selected={row.ws.id === workspaceValue}
                  active={roving.activeIndex === index}
                  id={optionId(index)}
                  optionRef={(el) => roving.registerItem(index, el)}
                  onHover={() => roving.setActiveIndex(index)}
                  onSelect={() => {
                    onSelect(selectionFor(row));
                    setOpen(false);
                  }}
                >
                  <span className="flex min-w-0 items-center gap-1.5">
                    <span className="shrink-0">
                      <WorkspaceIcon
                        ownerAgentId={row.ws.owner_agent_id}
                        agentsById={agentsById}
                        ownerAgentAvatarUrls={agentAvatarUrls}
                      />
                    </span>
                    <span className="truncate">{row.label}</span>
                  </span>
                </OptionRow>
              );
            })}
          </div>
        ))}
      </FilterPanel>
    </Popover.Root>
  );
}

// ── Person (assignee / creator) ────────────────────────────────────

function PersonFilterControl({
  icon,
  label,
  anyLabel,
  population,
  allowUnassigned = false,
  value,
  onChange,
  ghost,
  agentsById,
  agentAvatarUrls,
  userAvatarUrls,
}: {
  ghost?: boolean;
  icon: ReactNode;
  label: string;
  anyLabel: string;
  /** This menu's population, or the reason there is none to show yet. */
  population: BoardAuthority<PersonOption[]>;
  allowUnassigned?: boolean;
  value: PersonFilter;
  onChange: (next: PersonFilter) => void;
} & PersonDirectory): JSX.Element {
  const [open, setOpen] = useState(false);
  // Rows come only from an answered read. The footer below says which of the
  // three answer-less states this is, so the menu never presents "we have not
  // been told" as "there is nobody".
  const state = personMenuState(population);
  const options = population.known ? population.value : NO_PEOPLE;
  const selectedName = options.find(
    (o) => personFilterKey({ kind: "person", person: o.person }) === personFilterKey(value),
  )?.name;
  // Value half of the trigger label. The trigger renders `${label}:${valuePart}`
  // (e.g. "Assignee:All" / "From:@cindy") so the dimension stays legible even at
  // the default "All" state (shiki #task:790).
  const valuePart =
    value.kind === "any"
      ? anyLabel
      : value.kind === "unassigned"
        ? t("tasks.noAssignee")
        : // The filter shows the assignee's/creator's display name (the
          // denormalized `name` on the dto — NOT a handle), so it renders
          // plain, no `@`, matching every other assignee/creator DISPLAY
          // (#task:1768, shiki 2026-08-01). `@` is reserved for surfaces that
          // show a real handle (mentions, composer, the assignee picker).
          selectedName
          ? selectedName
          : t("tasks.selected");

  const choose = (next: PersonFilter): void => {
    onChange(next);
    setOpen(false);
  };

  // Roving keyboard nav (#task:1257). The option order below is fixed —
  // ALL, then optional No-assignee, then each person — so the row index is
  // positional; `actions` mirrors that order 1:1 for Enter-to-commit.
  const listId = useId();
  const optionId = (index: number): string => `${listId}-opt-${index}`;
  const anyIndex = 0;
  const unassignedIndex = allowUnassigned ? 1 : -1;
  const optionsBase = allowUnassigned ? 2 : 1;
  const actions: (() => void)[] = [
    () => choose({ kind: "any" }),
    ...(allowUnassigned ? [() => choose({ kind: "unassigned" })] : []),
    ...options.map((o) => () => choose({ kind: "person", person: o.person })),
  ];
  const roving = useRovingList({
    count: actions.length,
    open,
    onSelect: (index) => actions[index]?.(),
  });

  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <FilterTrigger icon={icon} active={value.kind !== "any"} ghost={ghost} aria-label={label}>
          {`${label}:${valuePart}`}
        </FilterTrigger>
      </Popover.Trigger>
      <FilterPanel
        id={listId}
        activeDescendant={optionId(roving.activeIndex)}
        onKeyDown={roving.onKeyDown}
      >
        <OptionRow
          selected={value.kind === "any"}
          neutral
          active={roving.activeIndex === anyIndex}
          id={optionId(anyIndex)}
          optionRef={(el) => roving.registerItem(anyIndex, el)}
          onHover={() => roving.setActiveIndex(anyIndex)}
          onSelect={() => choose({ kind: "any" })}
        >
          {anyLabel}
        </OptionRow>
        {allowUnassigned ? (
          <OptionRow
            selected={value.kind === "unassigned"}
            active={roving.activeIndex === unassignedIndex}
            id={optionId(unassignedIndex)}
            optionRef={(el) => roving.registerItem(unassignedIndex, el)}
            onHover={() => roving.setActiveIndex(unassignedIndex)}
            onSelect={() => choose({ kind: "unassigned" })}
          >
            {t("tasks.noAssignee")}
          </OptionRow>
        ) : null}
        {options.map((o, i) => {
          const key = personFilterKey({ kind: "person", person: o.person });
          const index = optionsBase + i;
          return (
            <OptionRow
              key={key}
              selected={value.kind === "person" && personFilterKey(value) === key}
              active={roving.activeIndex === index}
              id={optionId(index)}
              optionRef={(el) => roving.registerItem(index, el)}
              onHover={() => roving.setActiveIndex(index)}
              onSelect={() => choose({ kind: "person", person: o.person })}
            >
              <span className="flex min-w-0 items-center gap-1.5">
                <span className="shrink-0">
                  <PersonAvatar
                    person={o.person}
                    name={o.name}
                    agentsById={agentsById}
                    agentAvatarUrls={agentAvatarUrls}
                    userAvatarUrls={userAvatarUrls}
                  />
                </span>
                <span className="truncate">{o.name}</span>
              </span>
            </OptionRow>
          );
        })}
        {state === "ready" ? null : (
          <p className="px-2 py-3 text-center text-(length:--text-meta) italic text-text-muted">
            {state === "loading"
              ? t("tasks.loading")
              : state === "failed"
                ? t("tasks.couldntLoadPeople")
                : t("tasks.noneYet")}
          </p>
        )}
      </FilterPanel>
    </Popover.Root>
  );
}

// ── Scope strip (wide layout) ──────────────────────────────────────

const FLAT_CHIP =
  "flex h-6 shrink-0 items-center gap-1.5 rounded-(--control-radius) px-2 text-(length:--text-meta) transition-colors focus-outline";

/** Selection here is a utility state, so it takes a neutral ink fill rather than
 *  the theme's selected-row colour, which spends the accent. A theme whose
 *  selection is ink rather than accent (poster) repaints it through the tokens. */
function flatChipTone(selected: boolean): string {
  return selected
    ? "bg-(--flat-chip-selected-bg,var(--bg-inset)) text-(--flat-chip-selected-fg,var(--text-primary))"
    : "text-text-secondary hover:bg-bg-hover hover:text-text-primary";
}

function FlatChip({
  selected,
  onSelect,
  children,
}: {
  selected: boolean;
  onSelect: () => void;
  children: ReactNode;
}): JSX.Element {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      onClick={onSelect}
      className={cn(FLAT_CHIP, "max-w-40", flatChipTone(selected))}
    >
      <span className="truncate">{children}</span>
    </button>
  );
}

/**
 * The board scope laid flat: All, each project, and Direct messages are one
 * click; agent spaces read like people's names beside projects, so they sit
 * behind one picker that shows the chosen space in place.
 */
function ScopeStrip({
  workspaceOptions,
  filters,
  onSelect,
  agentsById,
  agentAvatarUrls,
  userAvatarUrls,
}: {
  workspaceOptions: WorkspaceOption[];
  filters: TaskFilters;
  onSelect: (selection: BoardScopeSelection) => void;
} & PersonDirectory): JSX.Element {
  const projects = workspaceOptions.filter((w) => workspaceBoardContainer(w) === "workspace");
  const workspaceIcon = (ws: WorkspaceOption, size?: number): JSX.Element => (
    <WorkspaceIcon
      ownerAgentId={ws.owner_agent_id}
      agentsById={agentsById}
      ownerAgentAvatarUrls={agentAvatarUrls}
      size={size}
    />
  );
  const agentSpaces = workspaceOptions
    .filter((w) => workspaceBoardContainer(w) === "agent_space")
    .map((ws): ScopePickerOption => ({
      id: ws.id,
      label: ws.label,
      icon: (size) => workspaceIcon(ws, size),
    }));

  const pairs = useDirectPairs();
  const directLines = pairs
    .filter(directLinePairVisibleInSidebar)
    .map((pair): ScopePickerOption => {
      const { label, avatar } = presentCounterpart(pair.counterpart);
      return {
        id: pair.conversation_id,
        label,
        icon: (size = AVATAR_PX.xs) => (
          <AuthorAvatarView
            avatar={avatar}
            size={size}
            label={label}
            agentAvatarUrls={agentAvatarUrls}
            humanAvatarUrls={userAvatarUrls}
          />
        ),
      };
    });

  return (
    <div
      role="radiogroup"
      aria-label={t("tasks.boardScope")}
      className="flex min-w-0 items-center gap-0.5"
    >
      <FlatChip selected={filters.container === null} onSelect={() => onSelect({ kind: "all" })}>
        {t("tasks.all")}
      </FlatChip>
      <div className="scrollbar-hide flex min-w-0 items-center gap-0.5 overflow-x-auto">
        {projects.map((ws) => (
          <FlatChip
            key={ws.id}
            selected={filters.container === "workspace" && filters.workspaceId === ws.id}
            onSelect={() =>
              onSelect({ kind: "workspace", workspaceId: ws.id, container: "workspace" })
            }
          >
            {ws.label}
          </FlatChip>
        ))}
      </div>
      {agentSpaces.length > 0 ? (
        <ScopePicker
          label={t("tasks.agentSpaces")}
          options={agentSpaces}
          value={filters.container === "agent_space" ? filters.workspaceId : null}
          onSelect={(id) =>
            onSelect({ kind: "workspace", workspaceId: id, container: "agent_space" })
          }
        />
      ) : null}
      {directLines.length > 0 ? (
        <ScopePicker
          label={t("tasks.directMessages")}
          options={directLines}
          value={filters.container === "direct_messages" ? filters.channelId : null}
          onSelect={(id) => onSelect({ kind: "direct_messages", conversationId: id })}
        />
      ) : null}
    </div>
  );
}

interface ScopePickerOption {
  id: string;
  label: string;
  icon: (size?: number) => JSX.Element;
}

/** A scope family with many members (agent spaces, direct lines) behind one chip
 *  that shows the chosen member in place. */
function ScopePicker({
  label,
  options,
  value,
  onSelect,
}: {
  label: string;
  options: ScopePickerOption[];
  value: string | null;
  onSelect: (id: string) => void;
}): JSX.Element {
  const [open, setOpen] = useState(false);
  const selected = options.find((o) => o.id === value) ?? null;
  const listId = useId();
  const optionId = (index: number): string => `${listId}-opt-${index}`;
  const pick = (index: number): void => {
    const o = options[index];
    if (o) {
      onSelect(o.id);
    }
    setOpen(false);
  };
  const roving = useRovingList({ count: options.length, open, onSelect: pick });

  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <button
          type="button"
          aria-label={label}
          className={cn(FLAT_CHIP, "max-w-40", flatChipTone(selected !== null))}
        >
          {selected ? selected.icon(TRIGGER_GLYPH_PX) : null}
          <span className="truncate">{selected?.label ?? label}</span>
          <ChevronDownIcon className="size-3 shrink-0 opacity-70" />
        </button>
      </Popover.Trigger>
      <FilterPanel
        id={listId}
        activeDescendant={optionId(roving.activeIndex)}
        onKeyDown={roving.onKeyDown}
      >
        {options.map((o, index) => (
          <OptionRow
            key={o.id}
            selected={o.id === value}
            active={roving.activeIndex === index}
            id={optionId(index)}
            optionRef={(el) => roving.registerItem(index, el)}
            onHover={() => roving.setActiveIndex(index)}
            onSelect={() => pick(index)}
          >
            <span className="flex min-w-0 items-center gap-1.5">
              <span className="shrink-0">{o.icon()}</span>
              <span className="truncate">{o.label}</span>
            </span>
          </OptionRow>
        ))}
      </FilterPanel>
    </Popover.Root>
  );
}

// ── Assignee (flat avatars) ────────────────────────────────────────

/** Avatars shown flat before the rest fold into a "+N" menu. */
const FLAT_ASSIGNEE_LIMIT = 6;

/**
 * The assignee axis laid flat: one avatar per person, click to narrow, click the
 * selected one again to clear. Order follows the facet read, never the selection,
 * so an avatar does not move under the pointer. Until the population is known the
 * row holds nothing; a failed or empty read falls back to the dropdown, whose
 * footer says which of those it is.
 */
function AssigneeAvatars({
  population,
  allowUnassigned,
  value,
  onChange,
  ...directory
}: {
  population: BoardAuthority<PersonOption[]>;
  allowUnassigned: boolean;
  value: PersonFilter;
  onChange: (next: PersonFilter) => void;
} & PersonDirectory): JSX.Element | null {
  const state = personMenuState(population);
  if (state === "loading") {
    return null;
  }
  if (state !== "ready" && !allowUnassigned) {
    return (
      <PersonFilterControl
        icon={<UserIcon className="size-3.5" />}
        label={t("tasks.assignee")}
        anyLabel={t("tasks.all")}
        population={population}
        value={value}
        onChange={onChange}
        {...directory}
      />
    );
  }

  const options = population.known ? population.value : NO_PEOPLE;
  const flat = options.slice(0, FLAT_ASSIGNEE_LIMIT);
  const folded = options.slice(FLAT_ASSIGNEE_LIMIT);
  const selectedKey = personFilterKey(value);
  const toggle = (next: PersonFilter): void => {
    onChange(personFilterKey(next) === selectedKey ? { kind: "any" } : next);
  };

  return (
    <TooltipProvider>
      <div
        role="radiogroup"
        aria-label={t("tasks.assignee")}
        data-assignee-avatars
        className="flex shrink-0 items-center gap-0.5"
      >
        {flat.map((o) => {
          const next: PersonFilter = { kind: "person", person: o.person };
          return (
            <AvatarToggle
              key={personFilterKey(next)}
              label={o.name}
              selected={personFilterKey(next) === selectedKey}
              onToggle={() => toggle(next)}
            >
              <PersonAvatar person={o.person} name={o.name} size={AVATAR_PX.sm} {...directory} />
            </AvatarToggle>
          );
        })}
        {folded.length > 0 ? (
          <FoldedAssignees
            options={folded}
            value={value}
            onSelect={(next) => toggle(next)}
            {...directory}
          />
        ) : null}
        {allowUnassigned ? (
          <AvatarToggle
            label={t("tasks.noAssignee")}
            selected={value.kind === "unassigned"}
            onToggle={() => toggle({ kind: "unassigned" })}
            nameWhenSelected={false}
          >
            <span
              className={cn(
                "flex size-5 shrink-0 items-center justify-center rounded-(--avatar-radius) border border-dashed",
                // Selected inherits the chip's foreground: Poster paints the
                // selected chip ink, where the muted grey disappears.
                value.kind === "unassigned"
                  ? "border-current"
                  : "border-text-muted text-text-muted",
              )}
            >
              <UserIcon className="size-3" />
            </span>
          </AvatarToggle>
        ) : null}
      </div>
    </TooltipProvider>
  );
}

const AVATAR_TOGGLE_BOX =
  "flex h-7 shrink-0 items-center justify-center rounded-(--control-radius) transition-colors focus-outline";

/** One assignee avatar; a selected person also spells their name, so the narrowed
 *  state reads without hovering. */
function AvatarToggle({
  label,
  selected,
  onToggle,
  nameWhenSelected = true,
  children,
}: {
  label: string;
  selected: boolean;
  onToggle: () => void;
  /** Off for No assignee: its dashed glyph already says it, and the name adds width. */
  nameWhenSelected?: boolean;
  children: ReactNode;
}): JSX.Element {
  const named = selected && nameWhenSelected;
  const button = (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      aria-label={label}
      onClick={onToggle}
      className={cn(
        AVATAR_TOGGLE_BOX,
        named ? "max-w-40 gap-1.5 pl-1 pr-2" : "w-7",
        flatChipTone(selected),
      )}
    >
      {children}
      {named ? <span className="truncate text-(length:--text-meta)">{label}</span> : null}
    </button>
  );
  if (named) {
    return button;
  }

  return (
    <Tooltip>
      <TooltipTrigger asChild>{button}</TooltipTrigger>
      <TooltipContent side="bottom">{label}</TooltipContent>
    </Tooltip>
  );
}

function FoldedAssignees({
  options,
  value,
  onSelect,
  ...directory
}: {
  options: PersonOption[];
  value: PersonFilter;
  onSelect: (next: PersonFilter) => void;
} & PersonDirectory): JSX.Element {
  const [open, setOpen] = useState(false);
  const selectedKey = personFilterKey(value);
  const holdsSelection = options.some(
    (o) => personFilterKey({ kind: "person", person: o.person }) === selectedKey,
  );
  const listId = useId();
  const optionId = (index: number): string => `${listId}-opt-${index}`;
  const pick = (index: number): void => {
    const o = options[index];
    if (o) {
      onSelect({ kind: "person", person: o.person });
    }
    setOpen(false);
  };
  const roving = useRovingList({ count: options.length, open, onSelect: pick });

  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <button
          type="button"
          aria-label={t("tasks.assignee")}
          className={cn(
            AVATAR_TOGGLE_BOX,
            "min-w-7 px-1 text-(length:--text-meta)",
            flatChipTone(holdsSelection),
          )}
        >
          {`+${options.length}`}
        </button>
      </Popover.Trigger>
      <FilterPanel
        id={listId}
        activeDescendant={optionId(roving.activeIndex)}
        onKeyDown={roving.onKeyDown}
      >
        {options.map((o, index) => {
          const key = personFilterKey({ kind: "person", person: o.person });
          return (
            <OptionRow
              key={key}
              selected={key === selectedKey}
              active={roving.activeIndex === index}
              id={optionId(index)}
              optionRef={(el) => roving.registerItem(index, el)}
              onHover={() => roving.setActiveIndex(index)}
              onSelect={() => pick(index)}
            >
              <span className="flex min-w-0 items-center gap-1.5">
                <span className="shrink-0">
                  <PersonAvatar person={o.person} name={o.name} {...directory} />
                </span>
                <span className="truncate">{o.name}</span>
              </span>
            </OptionRow>
          );
        })}
      </FilterPanel>
    </Popover.Root>
  );
}
