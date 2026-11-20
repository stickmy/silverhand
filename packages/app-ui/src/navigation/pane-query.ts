import { SECONDARY_PANES, SECONDARY_PANE_ORDER, type SecondaryPane } from "./secondary-panes";

/** A conversation column of the chat stage: the route's own conversation, or
 *  the second column of a split view. */
export type PaneColumn = "primary" | "split";

export type SettingsPage = "members" | "add";

/**
 * Everything the query string says about the stage around the route's
 * conversation: the second column, the right-pane stack, and who owns it.
 * The ONE typed form of those params — parsed once, edited as a value, and
 * written back once, so no writer edits `URLSearchParams` by hand.
 *
 * Several panes may be set at once: an overlay keeps the panes flagged
 * `survivesOverlay` beneath it as the restore stack. Which one shows is the
 * registry's stacking order ({@link SECONDARY_PANES}).
 */
export interface PaneQuery {
  /** `?split=` — the conversation in the second column. */
  split: string | null;
  /** `?ctx=split` — the column the {@link COLUMN_PANES} resolve against. */
  owner: PaneColumn;
  side: string | null;
  thread: string | null;
  threads: boolean;
  files: boolean;
  attachment: string | null;
  agent: string | null;
  human: string | null;
  task: string | null;
  settings: string | null;
  settingsPage: SettingsPage | null;
  fullscreen: boolean;
}

export const EMPTY_PANES: PaneQuery = {
  split: null,
  owner: "primary",
  side: null,
  thread: null,
  threads: false,
  files: false,
  attachment: null,
  agent: null,
  human: null,
  task: null,
  settings: null,
  settingsPage: null,
  fullscreen: false,
};

/** Right panes resolved against one column's conversation. One column owns
 *  them at a time, so opening one from the other column replaces them. */
export const COLUMN_PANES = [
  "thread",
  "threads",
  "files",
  "attachment",
] as const satisfies readonly SecondaryPane[];

const isColumnPane = (pane: SecondaryPane): boolean =>
  (COLUMN_PANES as readonly SecondaryPane[]).includes(pane);

const nonEmpty = (value: string | null): string | null =>
  value !== null && value.length > 0 ? value : null;

/** `channelId` is the route's conversation: a `?split` naming it is no split. */
export function parsePaneQuery(params: URLSearchParams, channelId: string | null): PaneQuery {
  const rawSplit = nonEmpty(params.get("split"));
  const split = rawSplit !== channelId ? rawSplit : null;
  const settings = nonEmpty(params.get("settings"));
  const page = params.get("settingsPage");
  return {
    split,
    owner: split !== null && params.get("ctx") === "split" ? "split" : "primary",
    side: nonEmpty(params.get("side")),
    thread: nonEmpty(params.get("thread")),
    threads: params.get("threads") === "1",
    files: params.get("files") === "1",
    attachment: nonEmpty(params.get("attachment")),
    agent: nonEmpty(params.get("agent")),
    human: nonEmpty(params.get("human")),
    task: nonEmpty(params.get("task")),
    settings,
    settingsPage: settings !== null && (page === "members" || page === "add") ? page : null,
    fullscreen: params.get("fullscreen") === "1",
  };
}

/** The query value each field writes, `null` = absent. Order is the order new
 *  params are appended in; params already present keep their position. */
function serializedFields(q: PaneQuery): Array<[string, string | null]> {
  return [
    ["split", q.split],
    ["ctx", q.split !== null && q.owner === "split" ? "split" : null],
    ["side", q.side],
    ["thread", q.thread],
    ["threads", q.threads ? "1" : null],
    ["files", q.files ? "1" : null],
    ["attachment", q.attachment],
    ["agent", q.agent],
    ["human", q.human],
    ["task", q.task],
    ["settings", q.settings],
    ["settingsPage", q.settings !== null ? q.settingsPage : null],
    ["fullscreen", q.fullscreen ? "1" : null],
  ];
}

/** Write `q` onto `params` in place; params outside the pane family are kept. */
export function writePaneQuery(params: URLSearchParams, q: PaneQuery): void {
  for (const [key, value] of serializedFields(q)) {
    if (value === null) {
      params.delete(key);
    } else if (params.get(key) !== value) {
      params.set(key, value);
    }
  }
}

type PaneValue<P extends SecondaryPane> = P extends "threads" | "files" ? true : string;

function setPane(q: PaneQuery, pane: SecondaryPane, value: string | boolean | null): PaneQuery {
  switch (pane) {
    case "threads":
    case "files":
      return { ...q, [pane]: value !== null && value !== false };
    case "settings":
      return {
        ...q,
        settings: typeof value === "string" ? value : null,
        settingsPage: null,
      };
    default:
      return { ...q, [pane]: typeof value === "string" ? value : null };
  }
}

function isOpen(q: PaneQuery, pane: SecondaryPane): boolean {
  const value = q[pane];
  return value !== null && value !== false;
}

/** Ownership left with no column pane open falls back to the first column. */
function settleOwner(q: PaneQuery): PaneQuery {
  return q.owner === "split" && !COLUMN_PANES.some((p) => isOpen(q, p))
    ? { ...q, owner: "primary" }
    : q;
}

function withoutColumnPanes(q: PaneQuery): PaneQuery {
  return { ...q, thread: null, threads: false, files: false, attachment: null };
}

/**
 * Open `pane`. Its `mode` comes from the registry, never the caller: a
 * `replace` pane clears every other pane; an `overlay` keeps the
 * `survivesOverlay` panes beneath it. Fullscreen always drops — a fresh pane
 * shows at its normal width.
 *
 * A column pane belongs to `column` (default: the column already owning the
 * panes); opening it for the other column first drops that column's panes, so
 * a thread never resolves against the wrong conversation.
 *
 * `inFiles` (attachment only): the preview opens inside the Attachments panel.
 */
export function openPane<P extends SecondaryPane>(
  q: PaneQuery,
  pane: P,
  value: PaneValue<P>,
  options: { column?: PaneColumn; inFiles?: boolean } = {},
): PaneQuery {
  let next = q;
  if (isColumnPane(pane)) {
    const column = q.split === null ? "primary" : (options.column ?? q.owner);
    if (column !== q.owner) next = withoutColumnPanes(next);
    next = { ...next, owner: column };
  }
  const overlay = SECONDARY_PANES[pane].mode === "overlay";
  for (const other of SECONDARY_PANE_ORDER) {
    if (other === pane) continue;
    if (overlay && SECONDARY_PANES[other].survivesOverlay) continue;
    next = setPane(next, other, null);
  }
  next = setPane(next, pane, value);
  // A preview opened from the Attachments list keeps the list beneath it, so
  // closing the preview returns to the list.
  if (pane === "attachment" && options.inFiles) next = { ...next, files: true };
  return settleOwner({ ...next, fullscreen: false });
}

/** Close `pane`; whatever the stack still holds beneath it shows next. A
 *  preview inside the Attachments panel leaves with the panel. */
export function closePane(q: PaneQuery, pane: SecondaryPane): PaneQuery {
  let next = setPane(q, pane, null);
  if (pane === "files") next = { ...next, attachment: null };
  return settleOwner({ ...next, fullscreen: false });
}

/** Whether `pane` is open and owned by `column`. */
export function paneOpenIn(q: PaneQuery, pane: SecondaryPane, column: PaneColumn): boolean {
  return isOpen(q, pane) && (!isColumnPane(pane) || q.owner === column);
}

/** Write or clear the second column. The panes of the column being replaced
 *  or closed go with it; the first column's stay. */
export function setSplit(q: PaneQuery, split: string | null): PaneQuery {
  const next = q.owner === "split" ? withoutColumnPanes(q) : q;
  return { ...next, split, owner: "primary" };
}

/** Close the first column: the second column's conversation becomes the
 *  route's, keeping the panes it owned; the first column's panes go. */
export function promoteSplit(q: PaneQuery): PaneQuery {
  const next = q.owner === "split" ? q : withoutColumnPanes(q);
  return { ...next, split: null, owner: "primary" };
}

/** Rewrite the pane query of a URL through `edit`. */
export function editPaneUrl(
  pathname: string,
  search: string,
  channelId: string | null,
  edit: (q: PaneQuery) => PaneQuery,
): string {
  const params = new URLSearchParams(search);
  writePaneQuery(params, edit(parsePaneQuery(params, channelId)));
  const query = params.toString();
  return query ? `${pathname}?${query}` : pathname;
}

/** A fresh URL: `pathname` with exactly the panes in `panes`. */
export function paneUrl(pathname: string, panes: Partial<PaneQuery>): string {
  const params = new URLSearchParams();
  writePaneQuery(params, { ...EMPTY_PANES, ...panes });
  const query = params.toString();
  return query ? `${pathname}?${query}` : pathname;
}
