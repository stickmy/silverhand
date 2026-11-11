import { DEFAULT_TASK_FILTERS, type PersonFilter, type TaskFilters } from "./task-filters";

/**
 * The global task board's last filters, kept per renderer like the other
 * `silverhand:layout.*` preferences. They name projects, lines and people, so
 * they are identity-bound and logout clears them (`clearViewerLocalState`).
 * A selection whose object has since gone stays applied; the board shows it
 * as empty and Reset clears it.
 */
export const TASK_FILTERS_KEY = "silverhand:layout.task-board-filters";

const CONTAINERS = new Set(["workspace", "agent_space", "direct_messages"]);

function parsePerson(value: unknown, allowUnassigned: boolean): PersonFilter {
  if (!value || typeof value !== "object") {
    return { kind: "any" };
  }
  const v = value as { kind?: unknown; person?: { type?: unknown; id?: unknown } };
  if (v.kind === "unassigned" && allowUnassigned) {
    return { kind: "unassigned" };
  }
  if (
    v.kind === "person" &&
    (v.person?.type === "agent" || v.person?.type === "user") &&
    typeof v.person.id === "string" &&
    v.person.id.length > 0
  ) {
    return { kind: "person", person: { type: v.person.type, id: v.person.id } };
  }
  return { kind: "any" };
}

function parseId(value: unknown): string | null {
  return typeof value === "string" && value.length > 0 ? value : null;
}

/** Read stored filters back into a valid value; anything malformed is dropped
 *  field by field rather than failing the whole board. */
export function parseTaskFilters(raw: string | null): TaskFilters {
  if (!raw) {
    return DEFAULT_TASK_FILTERS;
  }

  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return DEFAULT_TASK_FILTERS;
  }
  if (!value || typeof value !== "object") {
    return DEFAULT_TASK_FILTERS;
  }

  const v = value as Record<string, unknown>;
  const container =
    typeof v.container === "string" && CONTAINERS.has(v.container)
      ? (v.container as TaskFilters["container"])
      : null;
  return {
    container,
    workspaceId: container ? parseId(v.workspaceId) : null,
    channelId: parseId(v.channelId),
    assignee: parsePerson(v.assignee, true),
    creator: parsePerson(v.creator, false),
  };
}

export function readTaskFilters(): TaskFilters {
  try {
    return parseTaskFilters(localStorage.getItem(TASK_FILTERS_KEY));
  } catch {
    return DEFAULT_TASK_FILTERS;
  }
}

export function writeTaskFilters(filters: TaskFilters): void {
  try {
    localStorage.setItem(TASK_FILTERS_KEY, JSON.stringify(filters));
  } catch {
    // Quota / disabled storage: the board still holds the filters in memory.
  }
}

export function clearTaskFilters(): void {
  try {
    localStorage.removeItem(TASK_FILTERS_KEY);
  } catch {
    // Quota / disabled storage: nothing to remove.
  }
}
