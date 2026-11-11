import { getPlatform } from "@/platform";
import { useEffect, useRef, type RefObject } from "react";
import { usePageLive } from "@/navigation/page-stack";
import type { AppSection } from "@/components/layout/nav-rail";
import type { RouteState } from "@/router";

/**
 * App-wide keyboard shortcuts. Deliberately a SMALL, fixed set —
 * not a remappable registry or command palette (out of scope per shiki):
 *
 *   • ⌘1–⌘5  — jump to a nav-rail section. Renderer-owned (a plain `window`
 *     keydown), NOT a native-menu accelerator: ⌘+digit is never a typing key,
 *     so it stays global even with an input focused, and no default macOS menu
 *     item claims ⌘1–5, so the renderer reliably receives it.
 *   • ⌘W     — contextual close: peel the top-most in-app panel, and only fall
 *     through to backgrounding the app (macOS Hide) when there is nothing left
 *     to peel. ⌘W is a native-menu accelerator (main is the single interception
 *     owner — a menu accelerator fires before any renderer keydown), so the
 *     renderer never listens for ⌘W directly. Instead main sends a per-keypress
 *     request and the renderer answers with the authoritative outcome below;
 *     main hides the app ONLY on `background-app`, fail-closed otherwise.
 *     See the responder hook + the main-side menu wiring.
 *   • Tab / ⇧Tab, ⌃Tab / ⌃⇧Tab, ⌘⇧] / ⌘⇧[ — next / previous conversation tab,
 *     while the strip is on screen. Bare Tab yields to text fields and open
 *     dialogs / menus, where it still moves focus.
 */

// ── ⌘1–5 nav-rail sections ──────────────────────────────────────────────────

/** The five primary nav-rail sections, in rail order. `settings` is a bottom
 *  utility button and `me` is a compact-only tab, so neither binds a digit. */
export type NavShortcutSection = Exclude<AppSection, "settings" | "me">;

/** The nav-section subset of `useNav()` this feature drives. Structural so the
 *  hook doesn't couple to the full (large) nav API shape. */
export interface NavSectionApi {
  toChat: () => void;
  toActivity: () => void;
  toTasks: () => void;
  toMembers: (agentId?: string) => void;
  toComputers: () => void;
}

/** ⌘<digit> → section, keyed on `event.code` (physical digit row) so the
 *  binding is layout-stable. Order matches the nav rail top-to-bottom. */
export const NAV_SECTION_SHORTCUTS: ReadonlyArray<{
  digit: number;
  code: string;
  section: NavShortcutSection;
}> = [
  { digit: 1, code: "Digit1", section: "chat" },
  { digit: 2, code: "Digit2", section: "activity" },
  { digit: 3, code: "Digit3", section: "tasks" },
  { digit: 4, code: "Digit4", section: "members" },
  { digit: 5, code: "Digit5", section: "computers" },
];

/** Route a section to its nav call. Pure (no event / DOM) so the mapping is
 *  unit-testable without a keyboard harness. */
export function dispatchNavSection(nav: NavSectionApi, section: NavShortcutSection): void {
  switch (section) {
    case "chat":
      nav.toChat();
      return;
    case "activity":
      nav.toActivity();
      return;
    case "tasks":
      nav.toTasks();
      return;
    case "members":
      nav.toMembers();
      return;
    case "computers":
      nav.toComputers();
      return;
  }
}

/**
 * Global ⌘1–5 → nav-rail section switch. Ignores any combo mixing in
 * ctrl/alt/shift so it never hijacks another chord (mirrors the Cmd+\ handler).
 * `nav` is read through a ref so the always-current callbacks fire without the
 * listener re-subscribing every render.
 */
export function useNavSectionShortcuts(nav: NavSectionApi): void {
  const navRef = useRef(nav);
  navRef.current = nav;
  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent): void => {
      if (!event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) {
        return;
      }
      const match = NAV_SECTION_SHORTCUTS.find((s) => s.code === event.code);
      if (!match) return;
      event.preventDefault();
      dispatchNavSection(navRef.current, match.section);
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, []);
}

// ── Conversation tab cycling ─────────────────────────────────────────────────

/**
 * Step through the conversation tab strip: bare Tab / ⇧Tab, plus ⌃Tab / ⌃⇧Tab
 * (browsers, editors) and ⌘⇧] / ⌘⇧[ (macOS tab convention). Returns the step,
 * or 0 when the key is not a tab-cycling chord. Keyed on `code` for the
 * brackets so the binding survives keyboard layouts. Pure for tests.
 */
export function tabCycleStep(event: {
  key: string;
  code: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
}): -1 | 0 | 1 {
  if (event.altKey) return 0;
  if (event.key === "Tab" && !event.metaKey) {
    return event.shiftKey ? -1 : 1;
  }
  if (event.metaKey && event.shiftKey && !event.ctrlKey) {
    if (event.code === "BracketRight") return 1;
    if (event.code === "BracketLeft") return -1;
  }
  return 0;
}

/** Where bare Tab must keep moving focus: form fields and any open dialog, menu
 *  or picker, whose own focus order is the control. The message composer is not
 *  one: focus rests there in a conversation, and its mention pick consumes Tab
 *  first (`defaultPrevented`). */
const BARE_TAB_EXEMPT =
  'input, textarea:not([data-composer-input]), select, [contenteditable=""], [contenteditable="true"], [role="dialog"], [role="alertdialog"], [role="menu"], [role="listbox"]';

function bareTabExempt(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest(BARE_TAB_EXEMPT) !== null;
}

/** `rootRef` is the pane hosting the strip: a section kept mounted behind another
 *  (display: none) has no boxes and must not take the key. */
export function useTabCycleShortcut(
  rootRef: RefObject<HTMLElement | null>,
  enabled: boolean,
  onStep: (step: -1 | 1) => void,
): void {
  const live = usePageLive();
  const stepRef = useRef(onStep);
  stepRef.current = onStep;

  useEffect(() => {
    if (!enabled || !live) return;
    const handleKeyDown = (event: KeyboardEvent): void => {
      // A handler that already used the key (the composer's mention pick) wins.
      if (event.isComposing || event.defaultPrevented) return;
      const step = tabCycleStep(event);
      if (step === 0) return;
      if (event.key === "Tab" && !event.ctrlKey && bareTabExempt(event.target)) return;
      if (!rootRef.current || rootRef.current.getClientRects().length === 0) return;
      event.preventDefault();
      stepRef.current(step);
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [rootRef, enabled, live]);
}

// ── ⌘W contextual close ──────────────────────────────────────────────────────

/**
 * The renderer's authoritative answer to a single ⌘W keypress. Wire contract
 * with the main process (mirror this union in `preload/api.d.ts`):
 *   • `closed-panel`   — the renderer peeled one in-app panel. Main does
 *                        nothing (the close already happened here).
 *   • `background-app` — nothing left to peel; main hides the app (macOS Hide)
 *                        it captured when the keypress fired. The window is
 *                        never destroyed — the session survives and ⌘Tab / the
 *                        dock bring it straight back.
 */
export type ContextualCloseOutcome = "closed-panel" | "background-app";

/** Which surface to peel this press, or none. */
type CloseTarget =
  | "human"
  | "agent"
  | "settings"
  | "files"
  | "task"
  | "side"
  | "thread"
  | "threads"
  | "activity-channel"
  | null;

/** The authoritative slice of state the ⌘W ladder reads at keypress time. ⌘W
 *  acts purely on this explicit panel allowlist — a surface is closeable only
 *  if it appears here. */
export interface ContextualCloseState {
  /** Agent side-panel (`?agent`). The topmost overlay — the render layer draws
   *  it above every other pane (`agent > files > task > side > thread > threads`), so
   *  ⌘W peels it first; closing it reveals whatever pane it covered. */
  agentPanelId: string | null;
  humanPanelId?: string | null;
  filesListOpen: boolean;
  channelSettingsId?: string | null;
  /** Task detail is showing as the OVERLAY SIDE PANE (`?task`, #task:1721), not
   *  the app-level modal. Only the pane is a ⌘W peel target — the modal (board /
   *  Tasks-tab entry) is a Radix Dialog that owns its own dismissal. Sits just below
   *  files in the ladder, mirroring the render order `agent > files > task > side
   *  > thread > threads`. */
  taskPaneOpen: boolean;
  sideConversationId: string | null;
  threadRootId: string | null;
  threadsListOpen: boolean;
  /** A channel drilled into from the Activity list (`/activity/.../c/:id`) — a
   *  closeable detail that ⌘W (once every overlay above is peeled) pops back to
   *  the full-screen Activity list. The Chat section's main channel is NOT a
   *  peel target, so callers set this only for the Activity section. */
  activityChannelOpen: boolean;
}

export interface ContextualCloseResolution {
  outcome: ContextualCloseOutcome;
  target: CloseTarget;
}

/**
 * The ⌘W ladder — one layer peeled per press, top surface first. Pure so the
 * layer order is unit-testable without IPC/DOM.
 *
 * Layer 1  ?agent (Agent panel)    → close the agent side-panel (topmost overlay)
 * Layer 2  ?files (Attachments)    → close files
 * Layer 3  ?task pane              → close the task detail side pane (#task:1721;
 *          the modal entry is not a target — Radix owns its dismissal)
 * Layer 4  secondary pane          → close side / thread / threads-list, in the
 *          SAME priority the render layer resolves (`side > thread > threads`,
 *          see `layoutMemoryFromRoute`), so we peel whichever pane is actually
 *          visible when a malformed URL sets more than one.
 * Layer 5  Activity channel detail → pop back to the full-screen Activity list
 * Layer 6  nothing left            → background-app (main hides the app)
 *
 * The order mirrors the render-layer priority `agent > files > task > side >
 * thread > threads`, then the Activity channel underneath, so ⌘W always peels
 * the surface actually on top. The Chat section's main ChannelView is not a
 * target — when only it remains, ⌘W backgrounds the app.
 */
export function resolveContextualClose(state: ContextualCloseState): ContextualCloseResolution {
  if (state.agentPanelId != null) {
    return { outcome: "closed-panel", target: "agent" };
  }
  if (state.humanPanelId != null) {
    return { outcome: "closed-panel", target: "human" };
  }
  if (state.channelSettingsId) return { outcome: "closed-panel", target: "settings" };
  if (state.filesListOpen) return { outcome: "closed-panel", target: "files" };
  if (state.taskPaneOpen) return { outcome: "closed-panel", target: "task" };
  if (state.sideConversationId != null) {
    return { outcome: "closed-panel", target: "side" };
  }
  if (state.threadRootId != null) {
    return { outcome: "closed-panel", target: "thread" };
  }
  if (state.threadsListOpen) {
    return { outcome: "closed-panel", target: "threads" };
  }
  if (state.activityChannelOpen) {
    return { outcome: "closed-panel", target: "activity-channel" };
  }
  return { outcome: "background-app", target: null };
}

/** The close half of `useNav()` this feature drives. Structural, as above. */
export interface ContextualCloseNav {
  closeAgentPanel: () => void;
  closeHumanPanel: () => void;
  closeFilesList: () => void;
  closeChannelSettings: () => void;
  /** Clear `?task` — closes the task detail side pane (#task:1721). */
  closeTask: () => void;
  closeSide: () => void;
  closeThread: () => void;
  closeThreadsList: () => void;
  /** Pop an Activity channel detail back to the full-screen Activity list. */
  toActivity: () => void;
}

type ContextualCloseRoute = Pick<
  RouteState,
  | "section"
  | "channelId"
  | "agentPanelId"
  | "humanPanelId"
  | "filesListOpen"
  | "channelSettingsId"
  | "sideConversationId"
  | "threadRootId"
  | "threadsListOpen"
>;

/**
 * Answer main's per-keypress ⌘W request with the authoritative outcome, peeling
 * the top-most panel in-place when the ladder says so. Route + nav are read
 * through a ref so every request resolves against the LATEST state (never a
 * stale closure) — this is the "authoritative per keypress" half of the
 * single-owner contract. No-ops safely until the main-side bridge exists
 * (`onContextualClose` absent), so the renderer can land ahead of main wiring.
 */
export function useContextualCloseResponder(
  route: ContextualCloseRoute,
  nav: ContextualCloseNav,
  /** Whether `?task` is currently showing as the side PANE (not the modal). The
   *  pane-vs-modal split reads task-pane state that isn't in `route`, so the caller
   *  resolves it and passes the result — only the pane is a ⌘W peel target. */
  taskPaneOpen: boolean,
): void {
  const stateRef = useRef({ route, nav, taskPaneOpen });
  stateRef.current = { route, nav, taskPaneOpen };
  useEffect(() => {
    const api = getPlatform().shell;
    if (!api?.onContextualClose || !api.respondContextualClose) return;
    return api.onContextualClose((reqId) => {
      const { route, nav, taskPaneOpen } = stateRef.current;
      const { outcome, target } = resolveContextualClose({
        agentPanelId: route.agentPanelId,
        humanPanelId: route.humanPanelId,
        filesListOpen: route.filesListOpen,
        channelSettingsId: route.channelSettingsId,
        taskPaneOpen,
        sideConversationId: route.sideConversationId,
        threadRootId: route.threadRootId,
        threadsListOpen: route.threadsListOpen,
        // Only the Activity section's drilled-in channel is a peel target; the
        // Chat section's main channel is not (⌘W backgrounds the app there).
        activityChannelOpen: route.section === "activity" && route.channelId != null,
      });
      switch (target) {
        case "agent":
          nav.closeAgentPanel();
          break;
        case "human":
          nav.closeHumanPanel();
          break;
        case "settings":
          nav.closeChannelSettings();
          break;
        case "files":
          nav.closeFilesList();
          break;
        case "task":
          nav.closeTask();
          break;
        case "side":
          nav.closeSide();
          break;
        case "thread":
          nav.closeThread();
          break;
        case "threads":
          nav.closeThreadsList();
          break;
        case "activity-channel":
          // Base-view drill-in: navigate back to the list, NOT an overlay-param
          // clear — this is the one rung that changes the path.
          nav.toActivity();
          break;
        case null:
          break;
      }
      api.respondContextualClose(reqId, outcome);
    });
  }, []);
}
