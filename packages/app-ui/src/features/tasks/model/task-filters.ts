import type { TaskBoardContainer as TaskBoardContainerAxis } from "@silverhand/shared";
import { isAgentSpace, type WorkspaceOwnership } from "@/features/workspaces/workspace-ownership";
import { t } from "@/lib/i18n";

export type { TaskBoardContainerAxis };

/** Which data source a channel option came from — the PRESENTATION axis the
 *  channel dropdown sections on, named once here so the three consumers cannot
 *  drift (#task:2066). Not a stored key: the filter value stays a channel id. */
export type ChannelSourceGroup = "agent_space" | "workspace";

/** A person reference keyed by `{type,id}` — id alone is unsafe because
 *  agent and user ids share no namespace guarantee (Cindy boundary). */
export type PersonRef = { type: "agent"; id: string } | { type: "user"; id: string };

/** Assignee/creator filter value. `unassigned` is meaningful for the
 *  assignee axis only (a task always has a creator). */
export type PersonFilter =
  | { kind: "any" }
  | { kind: "unassigned" }
  | { kind: "person"; person: PersonRef };

export interface TaskFilters {
  /** The board's TOP-LEVEL container axis (#task:2041): which KIND of
   *  container a task's owning conversation sits in —
   *  `workspace | agent_space | direct_messages`. null = the All tab: no
   *  container narrowing, the union of every container the viewer can see
   *  (DM tasks included). This is the coarsest axis and stacks with
   *  `workspaceId` / `channelId` as an INTERSECTION on the wire — picking
   *  `agent_space` then a specific workspace narrows to that one Agent Space.
   *  Unlike the within-board axes, it is also the SOLE input to the facet
   *  menus (the option sets describe the container you are on), so it is the
   *  one narrowing that legitimately reshapes those menus. */
  container: TaskBoardContainerAxis | null;
  /** Workspace id to narrow to — the UPSTREAM axis of the workspace→channel
   *  cascade (shiki #task:1387). null = no workspace narrowing. The channel
   *  filter's option set is scoped to this workspace, and switching workspace
   *  drops a channel selection that no longer belongs to it (see
   *  `applyWorkspaceCascade`). Matched by the workspace arm's
   *  `task.place.workspace_id` — no
   *  surface predicate needed, workspace identity isn't thread-aware. */
  workspaceId: string | null;
  /** Conversation id to narrow to — a channel id (matches that channel and
   *  all its threads) or a thread id (matches only that thread). null = no
   *  channel narrowing. Both surfaces read ONE cross-workspace board
   *  (#task:1165), and the match is now the server's `channel_id` param
   *  (`boardQuery.boardFilterParams`) rather than a per-surface predicate —
   *  a narrowing the server applies is the same for every surface by
   *  construction, and it is what lets a column stay a bounded window. */
  channelId: string | null;
  assignee: PersonFilter;
  creator: PersonFilter;
}

export const DEFAULT_TASK_FILTERS: TaskFilters = {
  container: null,
  workspaceId: null,
  channelId: null,
  assignee: { kind: "any" },
  creator: { kind: "any" },
};

/** Structural equality across all dimensions — drives the "filters differ
 *  from baseline" check (show Clear, reset target). */
export function taskFiltersEqual(a: TaskFilters, b: TaskFilters): boolean {
  return (
    a.container === b.container &&
    a.workspaceId === b.workspaceId &&
    a.channelId === b.channelId &&
    personFilterKey(a.assignee) === personFilterKey(b.assignee) &&
    personFilterKey(a.creator) === personFilterKey(b.creator)
  );
}

/**
 * Workspace→channel cascade (shiki #task:1387). When the workspace filter
 * changes, a channel selection that no longer belongs to the new workspace is
 * dropped back to "all channels" — never leave an orphan channel that would
 * yield an empty board (Cindy invariant). Selecting "All workspaces"
 * (`nextWorkspaceId === null`) keeps any channel, since the channel filter is
 * then unconstrained.
 *
 * `channelWorkspaceId` resolves a channel id to its owning workspace id (null
 * if unknown). Pure so the cascade is unit-testable without the option source.
 */
export function applyWorkspaceCascade(
  filters: TaskFilters,
  nextWorkspaceId: string | null,
  channelWorkspaceId: (channelId: string) => string | null,
): TaskFilters {
  if (nextWorkspaceId === null) {
    return { ...filters, workspaceId: null };
  }
  const keepChannel =
    filters.channelId !== null && channelWorkspaceId(filters.channelId) === nextWorkspaceId;
  return {
    ...filters,
    workspaceId: nextWorkspaceId,
    channelId: keepChannel ? filters.channelId : null,
  };
}

export function workspaceBoardContainer(
  workspace: WorkspaceOwnership,
): Extract<TaskBoardContainerAxis, "workspace" | "agent_space"> {
  return isAgentSpace(workspace) ? "agent_space" : "workspace";
}

/** The container a channel of this data-source group belongs to. */
export function channelGroupContainer(
  group: ChannelSourceGroup,
): Extract<TaskBoardContainerAxis, "workspace" | "agent_space"> {
  return group === "agent_space" ? "agent_space" : "workspace";
}

/** Whether the within-workspace channel axis applies under a container, and over
 *  which channel population. Generic over the channel option shape (the concrete
 *  type lives in the filter bar) so this model module stays dependency-free and
 *  unit-testable on plain literals. */
export interface ChannelAxisScope<C> {
  /** Show the channel picker (false under `direct_messages`). */
  show: boolean;
  /** The channels this container offers (empty under `direct_messages`). */
  channelOptions: C[];
}

/**
 * The channel axis under a container — one of the two pure faces of the container
 * model (see the block above). After the board-scope collapse (#task:2041) the
 * workspace + agent-space OBJECTS are listed directly by the grouped dropdown from
 * the full option sets, so the container model's only remaining projection is the
 * channel picker:
 *
 *  - `direct_messages` — a DM's coordinate is the pair conversation (→ the
 *    `channel_id` param), not a channel, so the channel picker is HIDDEN, not fed
 *    an empty set: an empty option list reads as "no channels", a different
 *    (false) claim than "this axis does not apply here". (In practice a
 *    direct_messages board is only ever the locked DM Tasks tab, which hides the
 *    whole scope + channel row anyway; this keeps the pure model honest for the
 *    one value that reaches it.)
 *  - `workspace` / `agent_space` — narrow the channel population to that KIND (a
 *    regular workspace's channels vs. an agent home's);
 *  - `null` (All) — every channel the viewer can see.
 */
export function channelAxisScope<C extends { group: ChannelSourceGroup }>(
  container: TaskBoardContainerAxis | null,
  channelOptions: C[],
): ChannelAxisScope<C> {
  if (container === "direct_messages") {
    return { show: false, channelOptions: [] };
  }
  return {
    show: true,
    channelOptions:
      container === null
        ? channelOptions
        : channelOptions.filter((o) => channelGroupContainer(o.group) === container),
  };
}

export type BoardScopeSelection =
  | { kind: "all" }
  | {
      kind: "workspace";
      workspaceId: string;
      container: Extract<TaskBoardContainerAxis, "workspace" | "agent_space">;
    }
  | { kind: "direct_messages"; conversationId: string };

/**
 * Apply a board-scope pick — the second pure face of the model — setting the
 * container axis AND its concrete coordinate together. Every pick lands on a
 * fully-resolved scope, so there is no orphan-coordinate transition to repair
 * afterward; the only cleanup is the workspace cascade dropping a coordinate the
 * pick strands:
 *   - `all` — clear the container and every coordinate (the union tab, DM tasks
 *     included);
 *   - `workspace` — run the workspace→channel cascade (so a channel that no
 *     longer belongs to the picked workspace is dropped) and stamp the
 *     workspace's own container;
 *   - `direct_messages` — one direct line: its conversation is the coordinate
 *     (`channelId`), and a pair has no workspace.
 *
 * `channelWorkspaceId` resolves a channel id to its owning workspace (null if
 * unknown) — the same injected resolver `applyWorkspaceCascade` takes, so this
 * stays pure and unit-testable without the option source.
 */
export function applyBoardScopeSelection(
  filters: TaskFilters,
  selection: BoardScopeSelection,
  channelWorkspaceId: (channelId: string) => string | null,
): TaskFilters {
  switch (selection.kind) {
    case "all":
      return { ...filters, container: null, workspaceId: null, channelId: null };
    case "workspace":
      return {
        ...applyWorkspaceCascade(filters, selection.workspaceId, channelWorkspaceId),
        container: selection.container,
      };
    case "direct_messages":
      return {
        ...filters,
        container: "direct_messages",
        workspaceId: null,
        channelId: selection.conversationId,
      };
  }
}

/** One section of the board-scope dropdown: the workspaces it lists (regular
 *  workspaces under "Workspaces", agent homes under "Agent Spaces"). Generic over
 *  the concrete option shape — that lives in the filter bar — so the
 *  classification stays in this model module. Direct Messages are not a section:
 *  a DM is never a selectable global scope (contract §5). */
export interface BoardScopeGroup<W> {
  key: "workspaces" | "agent-spaces";
  label: string;
  workspaces: readonly W[];
}

/**
 * Classify the board-scope dropdown's sections (#task:2041) — the third pure face
 * of the container model, so "which workspace goes in which section, and when does
 * a section show" is falsifiable without a DOM (the whole classification used to be
 * inlined in the filter bar's `useMemo`, where deleting a section or mis-routing a
 * workspace stayed green — @Cindy).
 *
 * The people-owned vs agent-space split reuses {@link workspaceBoardContainer}, the
 * SAME taxonomy the wire axis uses, so a workspace can never land in a section that
 * disagrees with the container its pick stamps. A section shows iff it has
 * objects: an answered-empty section is simply absent. Direct Messages are not
 * offered here (contract §5) — the coarsest axis's only selectable objects are
 * workspaces and agent spaces; DM tasks reach the board through the "All" union or
 * their own locked Tasks tab.
 */
export function boardScopeGroups<W extends WorkspaceOwnership>(input: {
  workspaces: readonly W[];
}): Array<BoardScopeGroup<W>> {
  const inContainer = (
    container: Extract<TaskBoardContainerAxis, "workspace" | "agent_space">,
  ): readonly W[] => input.workspaces.filter((w) => workspaceBoardContainer(w) === container);
  const defs: Array<BoardScopeGroup<W>> = [
    { key: "workspaces", label: t("tasks.projects"), workspaces: inContainer("workspace") },
    { key: "agent-spaces", label: t("tasks.agentSpaces"), workspaces: inContainer("agent_space") },
  ];
  return defs.filter((g) => g.workspaces.length > 0);
}

/** One selectable person in the assignee / creator menus. The `name` is the
 *  denormalized snapshot the server already put on the row (no profile lookup —
 *  Cindy boundary); the option SETS themselves are a server facet read
 *  (`/api/tasks/board/facets`), so they never shrink to the rows a surface
 *  happens to hold. */
export interface PersonOption {
  person: PersonRef;
  name: string;
}

export function personFilterKey(f: PersonFilter): string {
  if (f.kind === "person") return `${f.person.type}:${f.person.id}`;
  return f.kind;
}
