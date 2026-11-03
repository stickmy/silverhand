import type {
  ConnectionPolicy,
  ConnectionInfo,
  ProfileSaveWithTestResult,
  ClearProfileResult,
} from "../connection/types.js";
import type {
  ClientAuthState,
  ClientAuthResult,
  PasswordRecoveryCall,
  PasswordRecoveryResult,
} from "../session/types.js";
import type {
  AuthSignupInput,
  AuthLoginInput,
  ServerConnectionProfileView,
  ServerInfo,
} from "@silverhand/shared";
import type {
  BackendProxyRequest,
  BackendProxyResponse,
  BackendUploadRequest,
  EventWsDiagnostic,
  EventSocket,
} from "../transport/types.js";
import type { LocalHostCapability } from "../platform/local-host.js";

export interface ClientAdapter {
  getConnectionPolicy(): Promise<ConnectionPolicy>;
  getConnectionInfo(): Promise<ConnectionInfo | null>;
  /** Anonymous `/api/server/info` from the selected server. */
  getServerInfo(): Promise<ServerInfo>;
  getAuthState(): Promise<ClientAuthState>;
  signup(input: AuthSignupInput): Promise<ClientAuthResult>;
  login(input: AuthLoginInput): Promise<ClientAuthResult>;
  logout(): Promise<{ ok: true }>;
  passwordRecovery(input: PasswordRecoveryCall): Promise<PasswordRecoveryResult>;
  backendRequest(input: BackendProxyRequest): Promise<BackendProxyResponse>;
  backendUpload(input: BackendUploadRequest): Promise<BackendProxyResponse>;
  recordEventWsDiagnostic(input: EventWsDiagnostic): Promise<void>;
  getServerConnectionProfile(): Promise<ServerConnectionProfileView | null>;
  saveServerConnectionProfile(input: { serverUrl: string }): Promise<ProfileSaveWithTestResult>;
  clearServerConnectionProfile(): Promise<ClearProfileResult>;
  onEpochUpdateRequired(handler: () => void): () => void;
  openEventSocket(url: string): EventSocket;
}

export interface ShellCapability {
  setPinchZoom(enabled: boolean): void;
  onShutdownProgress(handler: (step: string) => void): () => void;
  onNavigateSettings(handler: () => void): () => void;
  onContextualClose(handler: (reqId: string) => void): () => void;
  respondContextualClose(reqId: string, outcome: "closed-panel" | "background-app"): void;
}

export interface NotificationCapability {
  showSystemNotification(opts: {
    title: string;
    /** macOS shows it under the title. */
    subtitle?: string;
    body?: string;
    /** Stacks alerts that share it into one group (macOS, Windows). */
    groupId?: string;
    force?: boolean;
    notificationId?: string;
  }): Promise<void>;
  onNotificationClicked(handler: (notificationId: string) => void): () => void;
  dismissSystemNotifications(notificationIds: string[]): Promise<void>;
  setBadgeCount(count: number): Promise<void>;
}

export interface FileCapability {
  openExternal(url: string): Promise<void>;
  /** Omitted when the host cannot hand attachment files to external apps. */
  openAttachmentExternal?(input: {
    attachmentId: string;
    filename: string;
    bytes: Uint8Array;
  }): Promise<{ ok: true } | { ok: false; reason: string }>;
  saveAttachment(input: {
    filename: string;
    bytes: Uint8Array;
  }): Promise<
    { ok: true; path?: string } | { ok: false; canceled: true } | { ok: false; reason: string }
  >;
  sharePngExport?(input: {
    pngBytes: Uint8Array;
  }): Promise<{ ok: true } | { ok: false; canceled: true } | { ok: false; error: string }>;
  /** Shows an image in a native sheet with a share action; resolves when the
   * sheet closes, with whether a share completed. The sheet opens at once:
   * given an attachment id, native downloads the bytes itself while it shows. */
  previewPngExport?(input: {
    image: { pngBytes: Uint8Array } | { attachmentId: string };
    title: string;
    shareLabel: string;
    closeLabel: string;
  }): Promise<{ shared: boolean }>;
  savePngExport(input: {
    pngBytes: Uint8Array;
    suggestedName: string;
  }): Promise<
    { ok: true; path?: string } | { ok: false; canceled: true } | { ok: false; error: string }
  >;
  copyPngExport(input: {
    pngBytes: Uint8Array;
  }): Promise<{ ok: true } | { ok: false; error: string }>;
}

export type AppUpdateInstallMode = "now" | "on-quit";

/** One app-wide update state; every entry point reads it and acts through the same capability. */
export type AppUpdateState =
  /** `currentVersion` is the running build, so "up to date" can say which one. */
  | { status: "idle"; currentVersion: string; lastCheckedAt: number | null }
  | { status: "checking" }
  | { status: "available"; version: string }
  | { status: "downloading"; version: string; transferred: number; total: number | null }
  | { status: "downloaded"; version: string }
  /** Staging the downloaded bundle for install (signature check happens here). */
  | { status: "installing"; version: string }
  /** Staged; the next normal quit installs it. */
  | { status: "scheduled"; version: string }
  | { status: "error"; message: string; version: string | null };

export interface AppUpdateCapability {
  /** Null when this build has no update channel. */
  getState(): Promise<AppUpdateState | null>;
  subscribe(handler: (state: AppUpdateState) => void): () => void;
  check(): Promise<void>;
  download(): Promise<void>;
  /** `now` restarts into the update; `on-quit` installs on the next normal quit. */
  install(mode: AppUpdateInstallMode): Promise<void>;
  /** Main asks the UI to show the update dialog (e.g. the app menu's Check for Updates). */
  onShowDialog(handler: () => void): () => void;
}

/** One action in a native context menu; `icon` is SVG markup on a 24px grid. */
export interface ContextMenuItem {
  id: string;
  label: string;
  icon: string;
}
export interface ContextMenu {
  /** Where it was asked for, in viewport points; the menu opens beside it. */
  point: { x: number; y: number };
  /** A row of emoji above the actions. */
  reactions: { emoji: string; label: string }[];
  /** Groups of actions, divided from each other. */
  sections: ContextMenuItem[][];
}
export type ContextMenuChoice = { kind: "item"; id: string } | { kind: "react"; emoji: string };
/** A context menu the platform draws above everything on screen, native
 * overlays included. The UI owns the actions and every string. */
export interface ContextMenuCapability {
  /** Null when it was dismissed without a choice. */
  show(menu: ContextMenu): Promise<ContextMenuChoice | null>;
}

/** A native avatar editor over the shared part library. The UI owns storage: it
 *  passes the starting config and uploads what comes back. */
export interface AvatarMakerCapability {
  /** Null when the person closes the editor without choosing. */
  edit(input: { config: { parts: Record<string, number>; bg: number } }): Promise<{
    config: { parts: Record<string, number>; bg: number };
    pngBytes: Uint8Array<ArrayBuffer>;
  } | null>;
}

/** Installed once by an application entry point, before mounting the UI. */
export interface ClientPlatform {
  client: ClientAdapter;
  files: FileCapability;
  localHost?: LocalHostCapability;
  shell?: ShellCapability;
  notifications?: NotificationCapability;
  navigation?: NavigationCapability;
  titlebar?: TitlebarCapability;
  pageBar?: PageBarCapability;
  composer?: ComposerCapability;
  selectionBar?: SelectionBarCapability;
  banner?: BannerCapability;
  toast?: ToastCapability;
  avatarMaker?: AvatarMakerCapability;
  contextMenu?: ContextMenuCapability;
  /** Present only where the app can update itself. */
  appUpdate?: AppUpdateCapability;
  /** Native chrome (menus, tabs, sheets) follows the language the page resolved. */
  setLocale?(locale: "en" | "zh"): void;
  /** The store page this build is installed from, offered when the server requires a newer app. */
  storeUrl?: string;
  /** The running build's release version; omitted where the host has none. */
  getAppVersion?(): Promise<string>;
}

/** Optional native presentation. Navigation history, the page stack and its
 * transitions remain owned by the shared UI; native draws the tab bar and
 * reports tab selection. */
export interface NativeNavigationState {
  blocked: boolean;
  modalVersion: number;
  session: string;
  key: string;
  tab: "chat" | "activity" | "tasks" | "me";
  visible: boolean;
  /** The theme is dark, so native chrome and the status bar take the dark appearance. */
  dark: boolean;
}
export interface NativeNavigationIntent {
  modalVersion: number;
  session: string;
  kind: "tab";
  target: string;
}
/**
 * Native bars render what the shared UI declares and report taps back as
 * events; every state, rule and string comes from the declaration.
 */
/** The window titlebar's controls; the shared UI owns their state. */
export interface TitlebarState {
  /** The browse sidebar toggle; null hides it. */
  sidebar: { shown: boolean; label: string } | null;
  /** A status label beside the toggle, such as an archived workspace's. */
  status: string | null;
  searchPlaceholder: string;
  /** The search shortcut's name in the keyboard shortcut list. */
  searchShortcut: string;
  /** The window frame behind the panes is dark, so the bars and status bar
   *  over it take the dark appearance. */
  dark: boolean;
  /** The page's own header buttons, where the bar carries them in place of
   *  the page (beside a vertical tab bar). */
  actions: PageBarItem[];
}
export type TitlebarSearchKey = "up" | "down" | "tab" | "backtab" | "enter" | "escape";
export type TitlebarEvent =
  | { kind: "toggleSidebar" }
  | { kind: "searchFocus" }
  /** Search was asked for where no native field shows; the page opens its own. */
  | { kind: "searchOpen" }
  | { kind: "searchQuery"; query: string }
  | { kind: "searchKey"; key: TitlebarSearchKey }
  | { kind: "action"; id: string };
/** A native window titlebar with a search field. Results stay in the shared UI. */
export interface TitlebarCapability {
  /** Shows the titlebar with this state; null removes it. */
  sync(state: TitlebarState | null): Promise<void>;
  /** Leaves the search field, clearing it when a search has completed. */
  endSearch(input: { clear: boolean }): Promise<void>;
  subscribe(handler: (event: TitlebarEvent) => void): Promise<() => void>;
}
/** A control the native page bar draws. `icon` is SVG markup on a 24px grid, drawn as a template. */
export interface PageBarItem {
  id: string;
  label: string;
  /** Spoken in place of `label`, when they differ. */
  accessibilityLabel?: string;
  icon: string;
  selected?: boolean;
  /** Shows the unread dot. */
  dot?: boolean;
  /** The action destroys something; a menu marks it so. */
  destructive?: boolean;
  /** The page's main action, which a bar keeps out of its overflow menu. */
  primary?: boolean;
}
export interface PageBarTab extends PageBarItem {
  /** The menu item that closes the tab; null when it cannot close. */
  closeLabel: string | null;
}
/** The compact layout's page header, as the shared UI declares it. Fields
 * are semantic; the native bar decides how they look. iOS decodes each field
 * by name (`SilverhandClientPlugin.swift`), so one it does not read is
 * dropped without error. */
export interface PageBarState {
  title: string;
  subtitle?: string;
  /** What tapping the title does, as its accessibility hint; null when it is not a button. */
  titleAction: string | null;
  /** The identity a tappable title wears: a glyph (SVG markup, drawn as a
   * template) or a picture (an image URL, drawn as it is). */
  titleIcon?: { glyph: string } | { image: string };
  /** The page paints its own backdrop under the bar, which shows through
   * in place of the system's edge fade. */
  backdrop?: boolean;
  /** A Tab's root page shows its title large, beside the bar's buttons at
   * the bar's own height. */
  largeTitle?: boolean;
  /** Back's label; null hides it. Back runs the shared navigation. */
  back: string | null;
  /** One glass capsule per group. */
  actions: PageBarItem[][];
  /** The button a horizontal bar gathers the actions behind, all but the
   * primary ones, opening them as a menu; a vertical bar, with room for them,
   * lays them out. */
  overflow?: { label: string; icon: string };
  /** View tabs under the bar; null shows none. */
  tabs: PageBarTab[] | null;
  /** The tab strip's add menu; null shows none. */
  addTabs: { label: string; items: PageBarItem[] } | null;
}
export type PageBarEvent =
  | { kind: "back" }
  | { kind: "title" }
  | { kind: "action"; id: string }
  | { kind: "tab"; id: string }
  | { kind: "closeTab"; id: string }
  | { kind: "moveTab"; id: string; to: number }
  | { kind: "addTab"; id: string };
/** A native navigation bar for the compact page header. The page owns state and behaviour. */
export interface PageBarCapability {
  /** Shows the bar with this state; null removes it. */
  sync(state: PageBarState | null): Promise<void>;
  /** How far an edge swipe has carried the page off screen, from 0 to 1; the
   * bar fades with it. Null ends the swipe, after any bar synced before it. */
  backProgress(progress: number | null): Promise<void>;
  subscribe(handler: (event: PageBarEvent) => void): Promise<() => void>;
}
/** An item of the composer's add menu. With `pick`, native runs that system
 * picker and its files come back as a `files` event; without it the tap comes
 * back as `add`. `icon` is SVG markup on a 24px grid. */
export interface ComposerMenuItem {
  id: string;
  label: string;
  icon: string;
  /** `camera` is left out where the device has none. */
  pick?: "photos" | "camera" | "files";
}
/** The composer, drawn natively over the page. The shared
 * UI keeps the draft, the send and every string; native edits the text and
 * reports it. `key` names the draft: native starts a new one empty until
 * `setText`. iOS decodes each field by name (`SilverhandClientPlugin.swift`). */
export interface ComposerState {
  key: string;
  placeholder: string;
  enabled: boolean;
  canSend: boolean;
  sendLabel: string;
  addLabel: string;
  add: ComposerMenuItem[];
  /** The height of the controls the shared UI floats above the composer and
   * its picker (the activity line, the attachments); a tap on them keeps the
   * keyboard. */
  accessoryHeight: number;
  /** The `@` / `#` candidates native lists above the composer; a tap comes
   * back as `pick`. Absent while no trigger is open. */
  picker?: ComposerPicker;
  /** The agents at work here, on a line above the composer; a tap comes back
   * as `activity`. Absent while none runs. */
  activity?: ComposerActivity;
  /** The pane it belongs to, in viewport points, where panes stand side by
   * side; each pane's composer shows at once. Absent spans the screen and
   * replaces any other. `bottom` is the pane's end, up from the viewport's. */
  frame?: { left: number; width: number; bottom: number };
}
/** One line for the agents running: up to three faces, the first agent's
 * name and state, and how long it has run, which native counts from `since`
 * (epoch ms). */
export interface ComposerActivity {
  /** The agent a tap opens. */
  id: string;
  faces: ComposerPickerIcon[];
  title: string;
  detail: string;
  since: number;
  label: string;
}
/** A candidate's face: a picture drawn as it is, or a glyph (SVG markup on a
 * 24px grid) native tints, on a square ground when `ground`. */
export type ComposerPickerIcon = { image: string } | { glyph: string; ground?: boolean };
export interface ComposerPickerRow {
  id: string;
  title: string;
  /** A handle, set in monospace. */
  handle?: string;
  detail?: string;
  icon: ComposerPickerIcon;
  /** A row outside the conversation, drawn quieter. */
  dimmed?: boolean;
  /** Starts a section with this header. */
  section?: string;
}
export interface ComposerPicker {
  rows: ComposerPickerRow[];
  /** A note under the rows (more matches, loading). */
  footer?: string;
}
export type ComposerEvent =
  | { kind: "change"; key: string; text: string; caret: number; composing: boolean }
  /** Carries the text sent, which a `change` may not have delivered yet. */
  | { kind: "send"; key: string; text: string; caret: number }
  | { kind: "add"; key: string; id: string }
  | { kind: "pick"; key: string; id: string }
  | { kind: "activity"; key: string; id: string }
  /** Files the user chose, announced before their bytes are read so their
   * cards show at once; each later arrives in `files` under its `id`, or in
   * `filesFailed`. */
  | { kind: "filesPending"; key: string; files: { id: string; name: string; type: string }[] }
  | {
      kind: "files";
      key: string;
      files: { id?: string; name: string; type: string; base64: string }[];
    }
  | { kind: "filesFailed"; key: string; ids: string[] }
  /** Distances up from the bottom of the window to the composer's top: at
   * rest, at the height its text gives it (the keyboard included), and as
   * drawn; `picker` is the room the candidate list takes over it. */
  | {
      kind: "metrics";
      key: string;
      rest: number;
      reserve: number;
      overlay: number;
      picker: number;
    };
export interface ComposerCapability {
  /** Shows the composer for draft `state.key`. */
  sync(state: ComposerState): Promise<void>;
  /** Removes the composer, if it still shows draft `key`. */
  remove(key: string): Promise<void>;
  /** Replaces the text of draft `key`; `caret` counts UTF-16 units. */
  setText(input: { key: string; text: string; caret: number; focus: boolean }): Promise<void>;
  subscribe(handler: (event: ComposerEvent) => void): Promise<() => void>;
}
/** The compact layout's message-selection actions, drawn natively in the
 * composer's place while messages are selected; the composer steps aside
 * until the bar is removed. The page keeps the selection and every string.
 * `key` names the pane the selection belongs to. */
export interface SelectionBarState {
  key: string;
  /** How many are selected, as the page words it. */
  count: string;
  cancelLabel: string;
  copyLabel: string;
  saveLabel: string;
  /** False while nothing is selected: only Cancel acts. */
  enabled: boolean;
  /** The image is rendering: Save shows it is at work. */
  busy: boolean;
}
export type SelectionBarEvent = { kind: "cancel" | "copy" | "save"; key: string };
export interface SelectionBarCapability {
  /** Shows the bar for pane `state.key`. */
  sync(state: SelectionBarState): Promise<void>;
  /** Removes the bar, if it still shows pane `key`. */
  remove(key: string): Promise<void>;
  subscribe(handler: (event: SelectionBarEvent) => void): Promise<() => void>;
}
/** A notice native draws over its bars; a newer one replaces the one showing. */
export interface BannerNotice {
  id: string;
  title: string;
  /** Set after the title, quieter. */
  subtitle?: string;
  body?: string;
  /** `image` is an image URL; `glyph` is SVG markup on a 24px grid. */
  icon?: ComposerPickerIcon;
}
export type BannerEvent = { kind: "open"; id: string };
export interface BannerCapability {
  show(notice: BannerNotice): Promise<void>;
  subscribe(handler: (event: BannerEvent) => void): Promise<() => void>;
}
/** A short outcome line native draws above its bars; a newer one replaces the
 *  one showing, and one with the same id updates it in place. */
export interface ToastNotice {
  id: string;
  tone: "success" | "error" | "warning" | "neutral";
  /** Replaces the tone's glyph; drawn in a neutral tint. */
  glyph?: "offline";
  title: string;
  description?: string;
  /** Stays until dismissed or its action is used. */
  sticky?: boolean;
  action?: { label: string };
}
export type ToastEvent = { kind: "action"; id: string };
export interface ToastCapability {
  show(notice: ToastNotice): Promise<void>;
  /** Without an id, dismisses whatever is showing. */
  dismiss(id?: string): Promise<void>;
  subscribe(handler: (event: ToastEvent) => void): Promise<() => void>;
}
export interface NavigationCapability {
  badges(input: {
    session: string;
    chat?: boolean;
    activity?: boolean;
    tasks?: boolean;
  }): Promise<void>;
  /** Draws the tab bar for the entry on screen; resolves with the bar's
   * height in the compact layout, which pages showing it scroll clear of. */
  sync(state: NativeNavigationState): Promise<{ tabBarHeight: number }>;
  /** The viewer's avatar for the Me tab, as base64 image bytes; absent clears it. */
  avatar(base64?: string): Promise<void>;
  reset(session: string): Promise<void>;
  subscribe(handler: (intent: NativeNavigationIntent) => void): Promise<() => void>;
}
