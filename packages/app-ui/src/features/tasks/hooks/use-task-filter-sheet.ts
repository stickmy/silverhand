import { useEffect, useMemo, useState } from "react";
import type {
  FilterSheetEvent,
  FilterSheetField,
  FilterSheetOption,
  FilterSheetState,
} from "@silverhand/client";
import type { DirectPairListItem } from "@silverhand/shared";
import { getPlatform } from "@/platform";
import { useEventCallback } from "@/hooks/use-event-callback";
import { useCompactLayout } from "@/navigation/layout";
import { t, useLocale } from "@/lib/i18n";
import { useDirectPairsList } from "@/features/direct-pairs/hooks";
import { directLinePairVisibleInSidebar } from "@/features/direct-pairs/direct-line-rows";
import { presentCounterpart } from "@/features/direct-pairs/counterpart-presentation";
import { usePersonDirectory, type PersonDirectory } from "../components/person-avatar";
import type { TaskFilterBarProps } from "../components/task-filter-bar";
import { mapBoardAuthority, personMenuState, type BoardAuthority } from "../model/board-authority";
import {
  applyBoardScopeSelection,
  channelAxisScope,
  personFilterKey,
  taskFiltersEqual,
  workspaceBoardContainer,
  type PersonFilter,
  type PersonOption,
  type TaskFilters,
} from "../model/task-filters";

type FilterSheetInput = Omit<TaskFilterBarProps, "className">;

export interface TaskFilterSheet {
  /** Whether this platform draws the filters in a native sheet. */
  available: boolean;
  /** The filters differ from the board's baseline. */
  filtered: boolean;
  open: () => void;
}

const ALL = "all";
const WORKSPACE = "ws:";
const DIRECT = "dm:";
const NO_PAIRS: readonly DirectPairListItem[] = [];

/**
 * The board's filters in the platform's native sheet (compact iOS). The sheet
 * only draws and reports picks; each pick runs through the same model the
 * filter bar uses, and the sheet is handed the new state, so the cascade
 * between fields stays here.
 */
export function useTaskFilterSheet(input: FilterSheetInput): TaskFilterSheet {
  const capability = getPlatform().filterSheet;
  const available = useCompactLayout() && capability !== undefined;
  const [shown, setShown] = useState(false);
  const directory = usePersonDirectory();
  const pairs = useDirectPairsList().data?.direct_pairs;
  const locale = useLocale();
  const { filters, onChange, baselineFilters, channelOptions } = input;
  const filtered = !taskFiltersEqual(filters, baselineFilters);

  const channelWorkspaceId = useMemo(() => {
    const byId = new Map(channelOptions.map((o) => [o.id, o.workspaceId]));
    return (channelId: string): string | null => byId.get(channelId) ?? null;
  }, [channelOptions]);

  const { agentsById, agentAvatarUrls, userAvatarUrls } = directory;
  const state = useMemo(
    () =>
      shown
        ? sheetState(input, { agentsById, agentAvatarUrls, userAvatarUrls }, pairs ?? NO_PAIRS)
        : null,
    // `input` is spread from props; its parts are the dependencies.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [
      shown,
      locale,
      filters,
      baselineFilters,
      input.workspaceOptions,
      channelOptions,
      input.allChannelsLabel,
      input.people,
      agentsById,
      agentAvatarUrls,
      userAvatarUrls,
      pairs,
    ],
  );

  useEffect(() => {
    if (state && capability) {
      void capability.show(state);
    }
  }, [state, capability]);

  useEffect(() => {
    if (!shown || !capability) {
      return;
    }

    return () => {
      void capability.hide();
    };
  }, [shown, capability]);

  const handle = useEventCallback((event: FilterSheetEvent) => {
    if (event.kind === "dismiss") {
      setShown(false);
    } else if (event.kind === "reset") {
      onChange(baselineFilters);
    } else {
      const next = applyPick(filters, event.field, event.option, channelWorkspaceId);
      if (next) {
        onChange(next);
      }
    }
  });

  useEffect(() => {
    if (!shown || !capability) {
      return;
    }

    let stop: (() => void) | null = null;
    let cancelled = false;
    void capability.subscribe(handle).then((remove) => {
      if (cancelled) {
        remove();
      } else {
        stop = remove;
      }
    });
    return () => {
      cancelled = true;
      stop?.();
    };
  }, [shown, capability, handle]);

  const open = useEventCallback(() => setShown(true));
  return { available, filtered, open };
}

function applyPick(
  filters: TaskFilters,
  field: string,
  option: string,
  channelWorkspaceId: (channelId: string) => string | null,
): TaskFilters | null {
  switch (field) {
    case "scope":
      return applyBoardScopeSelection(filters, scopeSelection(option), channelWorkspaceId);
    case "channel":
      return { ...filters, channelId: option === ALL ? null : option };
    case "assignee":
      return { ...filters, assignee: personFilter(option) };
    case "creator":
      return { ...filters, creator: personFilter(option) };
    default:
      return null;
  }
}

function scopeSelection(option: string): Parameters<typeof applyBoardScopeSelection>[1] {
  if (option.startsWith(DIRECT)) {
    return { kind: "direct_messages", conversationId: option.slice(DIRECT.length) };
  }
  if (option.startsWith(WORKSPACE)) {
    const [container, workspaceId = ""] = option.slice(WORKSPACE.length).split(":");
    return {
      kind: "workspace",
      workspaceId,
      container: container === "agent_space" ? "agent_space" : "workspace",
    };
  }
  return { kind: "all" };
}

function personFilter(option: string): PersonFilter {
  if (option === "any" || option === "unassigned") {
    return { kind: option };
  }
  const [type, id = ""] = option.split(":");
  return { kind: "person", person: { type: type === "agent" ? "agent" : "user", id } };
}

function sheetState(
  input: FilterSheetInput,
  directory: PersonDirectory,
  pairs: readonly DirectPairListItem[],
): FilterSheetState {
  const { filters, baselineFilters, workspaceOptions, channelOptions, people } = input;
  // A conversation's own Tasks tab narrows by people only, as its filter bar does.
  const scopeLocked = baselineFilters.channelId !== null;
  const fields: FilterSheetField[] = [];

  if (!scopeLocked) {
    const agentAvatar = (agentId: string | null): FilterSheetOption["avatar"] =>
      agentId ? { image: directory.agentAvatarUrls.get(agentId) ?? null } : undefined;
    const scopeOptions: FilterSheetOption[] = [{ id: ALL, label: t("tasks.all") }];
    for (const ws of workspaceOptions) {
      const container = workspaceBoardContainer(ws);
      scopeOptions.push({
        id: `${WORKSPACE}${container}:${ws.id}`,
        label: ws.label,
        group: container === "agent_space" ? t("tasks.agentSpaces") : t("tasks.projects"),
        avatar: agentAvatar(ws.owner_agent_id),
      });
    }
    scopeOptions.sort((a, b) => groupRank(a) - groupRank(b));
    for (const pair of pairs.filter(directLinePairVisibleInSidebar)) {
      const { label, avatar } = presentCounterpart(pair.counterpart);
      scopeOptions.push({
        id: `${DIRECT}${pair.conversation_id}`,
        label,
        group: t("tasks.directMessages"),
        avatar: {
          image:
            avatar.variant === "agent"
              ? (directory.agentAvatarUrls.get(avatar.id) ?? null)
              : avatar.variant === "user"
                ? (directory.userAvatarUrls.get(avatar.id) ?? null)
                : null,
        },
      });
    }
    fields.push({
      id: "scope",
      label: t("tasks.project"),
      options: scopeOptions,
      searchPlaceholder: t("common.search"),
      selected:
        filters.container === null
          ? ALL
          : filters.container === "direct_messages"
            ? `${DIRECT}${filters.channelId ?? ""}`
            : `${WORKSPACE}${filters.container}:${filters.workspaceId ?? ""}`,
    });

    const scope = channelAxisScope(filters.container, channelOptions);
    const channels = filters.workspaceId
      ? scope.channelOptions.filter((o) => o.workspaceId === filters.workspaceId)
      : scope.channelOptions;
    if (scope.show && channels.length > 0) {
      fields.push({
        id: "channel",
        label: t("tasks.channel"),
        options: [
          { id: ALL, label: input.allChannelsLabel },
          ...channels
            .map((o) => ({
              id: o.id,
              label: `#${o.label}`,
              detail: o.sublabel,
              group: o.group === "agent_space" ? t("tasks.agentSpaces") : t("tasks.projects"),
            }))
            .sort((a, b) => groupRank(a) - groupRank(b)),
        ],
        selected: filters.channelId ?? ALL,
        searchPlaceholder: t("tasks.searchChannels"),
      });
    }
  }

  fields.push(
    personField({
      id: "assignee",
      label: t("tasks.assignee"),
      population: mapBoardAuthority(people, (f) => f.assignees),
      allowUnassigned: people.known && people.value.hasUnassigned,
      value: filters.assignee,
      directory,
    }),
    personField({
      id: "creator",
      label: t("tasks.from"),
      population: mapBoardAuthority(people, (f) => f.creators),
      allowUnassigned: false,
      value: filters.creator,
      directory,
    }),
  );

  return {
    title: t("tasks.filterTasks"),
    doneLabel: t("tasks.filterDone"),
    fields,
    resetLabel: taskFiltersEqual(filters, baselineFilters) ? null : t("tasks.resetFilters"),
  };
}

/** Projects first, then agent spaces; the ungrouped "All" row leads. */
function groupRank(option: FilterSheetOption): number {
  if (option.group === undefined) return 0;
  return option.group === t("tasks.projects") ? 1 : 2;
}

function personField({
  id,
  label,
  population,
  allowUnassigned,
  value,
  directory,
}: {
  id: string;
  label: string;
  population: BoardAuthority<PersonOption[]>;
  allowUnassigned: boolean;
  value: PersonFilter;
  directory: PersonDirectory;
}): FilterSheetField {
  const state = personMenuState(population);
  const options: FilterSheetOption[] = [{ id: "any", label: t("tasks.all") }];
  if (allowUnassigned) {
    options.push({ id: "unassigned", label: t("tasks.noAssignee") });
  }
  for (const option of population.known ? population.value : []) {
    const urls =
      option.person.type === "agent" ? directory.agentAvatarUrls : directory.userAvatarUrls;
    options.push({
      id: personFilterKey({ kind: "person", person: option.person }),
      label: option.name,
      avatar: { image: urls.get(option.person.id) ?? null },
    });
  }
  return {
    id,
    label,
    options,
    selected: personFilterKey(value),
    searchPlaceholder: t("common.search"),
    note:
      state === "ready"
        ? undefined
        : state === "loading"
          ? t("tasks.loading")
          : state === "failed"
            ? t("tasks.couldntLoadPeople")
            : t("tasks.noneYet"),
  };
}
