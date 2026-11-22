/**
 * The panes beside the route's conversation. The route's conversation owns the
 * stage and always stands first; every other pane is attached: it opens,
 * closes and moves on its own, and several stand side by side. A pane's list
 * and detail pages (Threads list and one thread, Files and one preview,
 * Settings and Members) are pages inside the one pane.
 *
 * The URL holds the panes in display order as repeated `pane` params, so the
 * layout survives reload and back/forward without memory of its own.
 */

/** The conversation a conversation-scoped pane belongs to. */
type Of = string;

export type SettingsPage = "members" | "add";

export type Pane =
  | { kind: "threads"; of: Of; thread: string | null }
  | { kind: "files"; of: Of; attachment: string | null }
  | { kind: "settings"; of: Of; page: SettingsPage | null }
  | { kind: "task"; id: string }
  | { kind: "agent"; id: string }
  | { kind: "human"; id: string };

export type PaneKind = Pane["kind"];

export type Panes = readonly Pane[];

/** Which pane a pane is: opening one with the same key replaces it in place.
 *  Each conversation has its own Threads, Files and Settings; a task, agent or
 *  person pane is one per stage. */
export function paneKey(pane: Pane): string {
  switch (pane.kind) {
    case "threads":
    case "files":
    case "settings":
      return `${pane.kind}:${pane.of}`;
    case "task":
    case "agent":
    case "human":
      return pane.kind;
  }
}

/** The conversation a pane belongs to; a task, agent or person pane belongs
 *  to none. */
export function paneOwner(pane: Pane): Of | undefined {
  switch (pane.kind) {
    case "threads":
    case "files":
    case "settings":
      return pane.of;
    default:
      return undefined;
  }
}

/** Open `pane`: replaces the pane with its key where it stands, otherwise
 *  joins at the end. */
export function openPane(panes: Panes, pane: Pane): Panes {
  const key = paneKey(pane);
  const at = panes.findIndex((p) => paneKey(p) === key);
  if (at === -1) return [...panes, pane];
  return panes.map((p, i) => (i === at ? pane : p));
}

/** Close the pane with `key`. */
export function closePane(panes: Panes, key: string): Panes {
  const closing = panes.find((p) => paneKey(p) === key);
  if (!closing) return panes;
  return panes.filter((p) => p !== closing);
}

/** Open `pane`, or close it when a pane with its key is already open. */
export function togglePane(panes: Panes, pane: Pane): Panes {
  const key = paneKey(pane);
  return panes.some((p) => paneKey(p) === key) ? closePane(panes, key) : openPane(panes, pane);
}

/** Move the pane with `key` to stand at `index` among the panes. */
export function movePane(panes: Panes, key: string, index: number): Panes {
  const from = panes.findIndex((p) => paneKey(p) === key);
  const moving = panes[from];
  if (!moving) return panes;
  const rest = panes.filter((_, i) => i !== from);
  const to = Math.max(0, Math.min(index, rest.length));
  return [...rest.slice(0, to), moving, ...rest.slice(to)];
}

type OwnedKind = "threads" | "files" | "settings";

/** The `kind` pane of conversation `of`, if open. */
export function ownedPane<K extends OwnedKind>(
  panes: Panes,
  kind: K,
  of: Of,
): Extract<Pane, { kind: K }> | null {
  const hit = panes.find((p) => p.kind === kind && paneOwner(p) === of);
  return (hit as Extract<Pane, { kind: K }> | undefined) ?? null;
}

/** The task, agent or person pane, if open. */
export function singlePane<K extends "task" | "agent" | "human">(
  panes: Panes,
  kind: K,
): Extract<Pane, { kind: K }> | null {
  const hit = panes.find((p) => p.kind === kind);
  return (hit as Extract<Pane, { kind: K }> | undefined) ?? null;
}

/** The last open pane of `kind`. */
export function lastPane<K extends PaneKind>(
  panes: Panes,
  kind: K,
): Extract<Pane, { kind: K }> | null {
  const hit = panes.findLast((p) => p.kind === kind);
  return (hit as Extract<Pane, { kind: K }> | undefined) ?? null;
}

/** `pane` one page up: a thread, preview or members page returns to its
 *  list; a pane on its first page has none (null). */
export function paneParent(pane: Pane): Pane | null {
  switch (pane.kind) {
    case "threads":
      return pane.thread ? { ...pane, thread: null } : null;
    case "files":
      return pane.attachment ? { ...pane, attachment: null } : null;
    case "settings":
      return pane.page ? { ...pane, page: null } : null;
    default:
      return null;
  }
}

/**
 * Keep the list whole beside the route's conversation `routeId`: no pane
 * owned by another conversation, one pane per key (the later wins, in the
 * earlier's place).
 */
export function settlePanes(panes: Panes, routeId: string | null): Panes {
  return panes
    .filter((p) => {
      const owner = paneOwner(p);
      return owner === undefined || owner === routeId;
    })
    .reduce<Panes>(openPane, []);
}

// ─── URL ────────────────────────────────────────────────────────────────────

const PARAM = "pane";

/** Every query param the pane list owns. */
export const PANE_PARAMS: readonly string[] = [PARAM];

const ID = /^[A-Za-z0-9_-]+$/;
const idOrNull = (value: string | undefined): string | null =>
  value !== undefined && ID.test(value) ? value : null;

/** One pane as a param value: `kind[.value][~of]`, the owner left out when it
 *  is the route's. Ids are base64url, so `.` and `~` never occur inside one. */
function encode(pane: Pane, routeId: string | null): string {
  const of = (o: Of) => (o === routeId ? "" : `~${o}`);
  switch (pane.kind) {
    case "task":
    case "agent":
    case "human":
      return `${pane.kind}.${pane.id}`;
    case "threads":
      return `threads${pane.thread ? `.${pane.thread}` : ""}${of(pane.of)}`;
    case "files":
      return `files${pane.attachment ? `.${pane.attachment}` : ""}${of(pane.of)}`;
    case "settings":
      return `settings${pane.page ? `.${pane.page}` : ""}${of(pane.of)}`;
  }
}

function decode(raw: string, routeId: string | null): Pane | null {
  const [head = "", ownerPart] = raw.split("~");
  const [kind, value] = head.split(".");
  switch (kind) {
    case "task":
    case "agent":
    case "human": {
      const id = idOrNull(value);
      return id ? { kind, id } : null;
    }
  }
  // An owned pane without an owner belongs to the route's conversation.
  const of = ownerPart === undefined ? routeId : idOrNull(ownerPart);
  if (of === null) return null;
  switch (kind) {
    case "threads":
      return { kind, of, thread: idOrNull(value) };
    case "files":
      return { kind, of, attachment: idOrNull(value) };
    case "settings":
      return { kind, of, page: value === "members" || value === "add" ? value : null };
    default:
      return null;
  }
}

/** The panes `params` name, beside the route's conversation `routeId`. Bad
 *  entries drop, and the list is settled ({@link settlePanes}). */
export function parsePanes(params: URLSearchParams, routeId: string | null): Panes {
  const parsed = params
    .getAll(PARAM)
    .map((value) => decode(value, routeId))
    .filter((p): p is Pane => p !== null);
  return settlePanes(parsed, routeId);
}

/** Write `panes` onto `params` in place; params outside the pane list stay. */
export function writePanes(params: URLSearchParams, panes: Panes, routeId: string | null): void {
  params.delete(PARAM);
  for (const pane of panes) params.append(PARAM, encode(pane, routeId));
}

/** `pathname` (whose conversation is `routeId`) with exactly `panes`. */
export function panesUrl(pathname: string, routeId: string | null, panes: Panes): string {
  const params = new URLSearchParams();
  writePanes(params, settlePanes(panes, routeId), routeId);
  const query = params.toString();
  return query ? `${pathname}?${query}` : pathname;
}

/** Rewrite the pane list of a URL through `edit`. */
export function editPanesUrl(
  pathname: string,
  search: string,
  routeId: string | null,
  edit: (panes: Panes) => Panes,
): string {
  const params = new URLSearchParams(search);
  writePanes(params, settlePanes(edit(parsePanes(params, routeId)), routeId), routeId);
  const query = params.toString();
  return query ? `${pathname}?${query}` : pathname;
}

/** Peel the last pane of the URL one page: back to its list, or closed. */
export function peelPaneParam(params: URLSearchParams): boolean {
  const raw = params.getAll(PARAM);
  const last = raw.at(-1);
  if (last === undefined) return false;
  // The owner stays as written: an owner left out stays left out.
  const pane = decode(last, "");
  const parent = pane ? paneParent(pane) : null;
  params.delete(PARAM);
  for (const value of raw.slice(0, -1)) params.append(PARAM, value);
  if (parent) params.append(PARAM, encode(parent, ""));
  return true;
}

// ─── What is on screen ──────────────────────────────────────────────────────

export interface OnScreen {
  /** Conversations whose main stream shows: the route's, when visible. */
  conversations: string[];
  /** Threads open, each with the conversation it belongs to. */
  threads: Array<{ conversationId: string; rootId: string }>;
}

/**
 * What the stage shows. A pane in the URL is on screen, so this is the open
 * set, nothing hidden beneath. Compact shows one page: the last pane, or the
 * route's conversation when none is open.
 */
export function onScreen(routeId: string | null, panes: Panes, compact: boolean): OnScreen {
  const shown = compact ? panes.slice(-1) : panes;
  const conversations: string[] = [];
  if (routeId && (!compact || shown.length === 0)) conversations.push(routeId);
  const threads: OnScreen["threads"] = [];
  for (const pane of shown) {
    if (pane.kind === "threads" && pane.thread) {
      threads.push({ conversationId: pane.of, rootId: pane.thread });
    }
  }
  return { conversations, threads };
}
