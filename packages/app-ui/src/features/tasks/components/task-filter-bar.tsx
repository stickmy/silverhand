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
import { AVATAR_PX } from "@/components/avatar-scale";
import { useDirectPairs } from "@/features/direct-pairs/hooks";
import { directLinePairVisibleInSidebar } from "@/features/direct-pairs/direct-line-rows";
import { presentCounterpart } from "@/features/direct-pairs/counterpart-presentation";
import { AuthorAvatarView } from "@/features/messages/components/author-avatar-view";
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
  boardScopeGroups,
  channelAxisScope,
  personFilterKey,
  taskFiltersEqual,
  workspaceBoardContainer,
  type BoardScopeSelection,
  type ChannelSourceGroup,
  type PersonFilter,
  type PersonOption,
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

  const selectScope = (selection: BoardScopeSelection): void => {
    onChange(applyBoardScopeSelection(filters, selection, channelWorkspaceId));
  };

  // One row of uniform dropdowns, coarsest axis first: where a task lives
  // (project), then its channel, then the people on it.
  return (
    <div
      data-task-filter-bar
      className={cn("flex min-w-0 flex-wrap items-center gap-1.5", className)}
    >
      {scopeLocked ? null : (
        <BoardScopeFilter
          filters={filters}
          workspaceOptions={workspaceOptions}
          onSelect={selectScope}
          {...personDirectory}
        />
      )}
      {!scopeLocked && scope.show && visibleChannelOptions.length > 0 ? (
        <ChannelFilter
          options={visibleChannelOptions}
          allChannelsLabel={allChannelsLabel}
          value={filters.channelId}
          onChange={(channelId) => onChange({ ...filters, channelId })}
        />
      ) : null}
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
      <PersonFilterControl
        icon={<CreatedByIcon className="size-3.5" />}
        label={t("tasks.from")}
        anyLabel={t("tasks.all")}
        population={mapBoardAuthority(people, (f) => f.creators)}
        value={filters.creator}
        onChange={(creator) => onChange({ ...filters, creator })}
        {...personDirectory}
      />
      {dirty ? (
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
      ) : null}
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
  }
>(function FilterTrigger({ icon, active, children, className, ...props }, ref) {
  return (
    <button
      ref={ref}
      type="button"
      data-filter-trigger
      data-active={active || undefined}
      className={cn(
        "flex max-w-50 shrink-0 items-center gap-1.5 rounded-(--control-radius)",
        "border-(length:--control-border-width) border-border px-2 py-1 text-(length:--text-meta) transition-colors focus-outline",
        active
          ? "bg-(--flat-chip-selected-bg,var(--bg-inset)) text-(--flat-chip-selected-fg,var(--text-primary))"
          : "text-text-secondary hover:bg-bg-hover hover:text-text-primary",
        className,
      )}
      {...props}
    >
      <span className="flex shrink-0 items-center opacity-70">{icon}</span>
      <span className="truncate leading-[14px]">{children}</span>
      <ChevronDownIcon className="size-3 shrink-0 opacity-70" />
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
}: {
  options: ChannelOption[];
  allChannelsLabel: string;
  value: string | null;
  onChange: (id: string | null) => void;
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
          icon={<ChannelIcon variant="utility" density="md" />}
          active={value !== null}
          aria-label={t("tasks.channel")}
        >
          {selected?.label ?? t("tasks.channel")}
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

// ── Project (board scope) ──────────────────────────────────────────

interface ScopeRow {
  id: string;
  label: string;
  icon: (size?: number) => JSX.Element;
  selection: BoardScopeSelection;
}

/**
 * Where a task lives: an All row over Projects, Agent Spaces and Direct
 * Messages, each listing concrete objects, so one pick names the container and
 * the object together (`applyBoardScopeSelection` stamps both). Sections with
 * no rows are left out.
 */
function BoardScopeFilter({
  filters,
  workspaceOptions,
  onSelect,
  agentsById,
  agentAvatarUrls,
  userAvatarUrls,
}: {
  filters: TaskFilters;
  workspaceOptions: WorkspaceOption[];
  onSelect: (selection: BoardScopeSelection) => void;
} & PersonDirectory): JSX.Element {
  const [open, setOpen] = useState(false);
  const pairs = useDirectPairs();

  // A handful of rows, rebuilt per render so section labels follow the locale.
  const workspaceGroups = boardScopeGroups<WorkspaceOption>({
    workspaces: workspaceOptions,
  }).map((g) => ({
    key: g.key,
    label: g.label,
    rows: g.workspaces.map((ws): ScopeRow => ({
      id: ws.id,
      label: ws.label,
      icon: (size) => (
        <WorkspaceIcon
          ownerAgentId={ws.owner_agent_id}
          agentsById={agentsById}
          ownerAgentAvatarUrls={agentAvatarUrls}
          size={size}
        />
      ),
      selection: {
        kind: "workspace",
        workspaceId: ws.id,
        container: workspaceBoardContainer(ws),
      },
    })),
  }));
  const directRows = pairs.filter(directLinePairVisibleInSidebar).map((pair): ScopeRow => {
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
      selection: { kind: "direct_messages", conversationId: pair.conversation_id },
    };
  });
  const groups =
    directRows.length > 0
      ? [
          ...workspaceGroups,
          { key: "direct-messages", label: t("tasks.directMessages"), rows: directRows },
        ]
      : workspaceGroups;

  // Roving order: All at 0, then every row in section order.
  const flatRows = groups.flatMap((g) => g.rows);
  const selectedId =
    filters.container === "direct_messages" ? filters.channelId : filters.workspaceId;
  const selected =
    filters.container === null ? null : (flatRows.find((r) => r.id === selectedId) ?? null);

  const listId = useId();
  const optionId = (index: number): string => `${listId}-opt-${index}`;
  const pick = (index: number): void => {
    if (index === 0) {
      onSelect({ kind: "all" });
    } else {
      const row = flatRows[index - 1];
      if (row) {
        onSelect(row.selection);
      }
    }
    setOpen(false);
  };
  const roving = useRovingList({ count: flatRows.length + 1, open, onSelect: pick });

  let index = 0;
  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <FilterTrigger
          icon={selected ? selected.icon(TRIGGER_GLYPH_PX) : <LayersIcon className="size-3.5" />}
          active={selected !== null}
          aria-label={t("tasks.project")}
        >
          {selected?.label ?? t("tasks.project")}
        </FilterTrigger>
      </Popover.Trigger>
      <FilterPanel
        id={listId}
        activeDescendant={optionId(roving.activeIndex)}
        onKeyDown={roving.onKeyDown}
      >
        <OptionRow
          selected={selected === null}
          neutral
          active={roving.activeIndex === 0}
          id={optionId(0)}
          optionRef={(el) => roving.registerItem(0, el)}
          onHover={() => roving.setActiveIndex(0)}
          onSelect={() => pick(0)}
        >
          {t("tasks.all")}
        </OptionRow>
        {groups.map((group) => (
          <div key={group.key}>
            <div className="px-2 pb-1 pt-2 text-(length:--text-badge) font-medium uppercase tracking-wider text-text-muted">
              {group.label}
            </div>
            {group.rows.map((row) => {
              index += 1;
              const at = index;
              return (
                <OptionRow
                  key={row.id}
                  selected={row === selected}
                  active={roving.activeIndex === at}
                  id={optionId(at)}
                  optionRef={(el) => roving.registerItem(at, el)}
                  onHover={() => roving.setActiveIndex(at)}
                  onSelect={() => pick(at)}
                >
                  <span className="flex min-w-0 items-center gap-1.5">
                    <span className="shrink-0">{row.icon()}</span>
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
  agentsById,
  agentAvatarUrls,
  userAvatarUrls,
}: {
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
        <FilterTrigger icon={icon} active={value.kind !== "any"} aria-label={label}>
          {value.kind === "any" ? label : valuePart}
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
