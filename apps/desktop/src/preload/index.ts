import { contextBridge, ipcRenderer, webFrame, type IpcRendererEvent } from "electron";
import type {
  AuthLoginInput,
  AuthSignupInput,
  ServerConnectionProfileView,
} from "@silverhand/shared";
import type {
  ClientAuthState,
  ClientAuthResult,
  PasswordRecoveryCall,
  PasswordRecoveryResult,
  BackendProxyRequest,
  BackendProxyResponse,
  BackendUploadRequest,
  LocalHostProxyConfig,
  LocalHostProxyConfigResult,
  LocalHostMutationResult,
  LocalHostSaveServerUrlResult,
  ProfileSaveWithTestResult,
  LocalHostPartialApplyState,
  LocalHostPartialApplyRetryResult,
  ClearProfileResult,
  AppUpdateInstallMode,
  AppUpdateState,
  EventWsDiagnostic,
  MenuBarConversation,
} from "@silverhand/client";
import { desktopBuildConfig, type DesktopBuildConfig } from "../build-flags.js";
import type { DesktopBackendInfo, DesktopBridge } from "./contract.js";
let previewPinchZoomEnabled = false;

interface PinchWheelEvent {
  readonly ctrlKey: boolean;
  preventDefault(): void;
}

interface PreloadEventTarget {
  addEventListener(
    type: "wheel",
    listener: (event: PinchWheelEvent) => void,
    options: { capture: true; passive: false },
  ): void;
  removeEventListener(
    type: "wheel",
    listener: (event: PinchWheelEvent) => void,
    capture: true,
  ): void;
}

const preloadEventTarget = globalThis as unknown as PreloadEventTarget;

const preventPagePinchZoom = (event: PinchWheelEvent): void => {
  if (event.ctrlKey) event.preventDefault();
};

/**
 * Expose trackpad pinch events while a previewer is open, but never let
 * Chromium apply them to the page viewport. The previewer consumes the same
 * cancelable wheel event to update its own content scale.
 */
function setPreviewPinchZoom(enabled: boolean): void {
  if (enabled === previewPinchZoomEnabled) return;
  previewPinchZoomEnabled = enabled;

  if (enabled) {
    preloadEventTarget.addEventListener("wheel", preventPagePinchZoom, {
      capture: true,
      passive: false,
    });
    webFrame.setVisualZoomLevelLimits(1, 3);
    return;
  }

  webFrame.setVisualZoomLevelLimits(1, 1);
  preloadEventTarget.removeEventListener("wheel", preventPagePinchZoom, true);
}

const api: DesktopBridge = {
  getDesktopBuildConfig: async (): Promise<DesktopBuildConfig> => desktopBuildConfig(),
  getBackendInfo: (): Promise<DesktopBackendInfo | null> => ipcRenderer.invoke("backend:info"),
  getAuthState: (): Promise<ClientAuthState> => ipcRenderer.invoke("auth:getState"),
  getServerInfo: () => ipcRenderer.invoke("server:info"),
  signup: (input: AuthSignupInput): Promise<ClientAuthResult> =>
    ipcRenderer.invoke("auth:signup", input),
  login: (input: AuthLoginInput): Promise<ClientAuthResult> =>
    ipcRenderer.invoke("auth:login", input),
  logout: (): Promise<{ ok: true }> => ipcRenderer.invoke("auth:logout"),
  passwordRecovery: (input: PasswordRecoveryCall): Promise<PasswordRecoveryResult> =>
    ipcRenderer.invoke("auth:passwordRecovery", input),
  backendRequest: (input: BackendProxyRequest): Promise<BackendProxyResponse> =>
    ipcRenderer.invoke("backend:request", input),
  backendUpload: (input: BackendUploadRequest): Promise<BackendProxyResponse> =>
    ipcRenderer.invoke("backend:upload", input),
  recordEventWsDiagnostic: (input: EventWsDiagnostic): Promise<void> =>
    ipcRenderer.invoke("eventWs:diagnostic", input),
  rotateHostToken: (input: { hostId: string }): Promise<LocalHostMutationResult> =>
    ipcRenderer.invoke("hosts:rotate", input),
  getHostProxyConfig: (input: { hostId: string }): Promise<LocalHostProxyConfigResult> =>
    ipcRenderer.invoke("hosts:proxyConfig:get", input),
  saveHostProxyConfig: (input: {
    hostId: string;
    config: LocalHostProxyConfig;
  }): Promise<LocalHostProxyConfigResult> => ipcRenderer.invoke("hosts:proxyConfig:set", input),
  saveServerUrl: (input: { serverUrl: string }): Promise<LocalHostSaveServerUrlResult> =>
    ipcRenderer.invoke("settings:saveServerUrl", input),
  getServerConnectionProfile: (): Promise<ServerConnectionProfileView | null> =>
    ipcRenderer.invoke("profile:get"),
  saveServerConnectionProfile: (input: { serverUrl: string }): Promise<ProfileSaveWithTestResult> =>
    ipcRenderer.invoke("profile:saveWithTest", input),
  clearServerConnectionProfile: (): Promise<ClearProfileResult> =>
    ipcRenderer.invoke("profile:clear"),
  getPartialApplyState: (): Promise<LocalHostPartialApplyState> =>
    ipcRenderer.invoke("partialApply:get"),
  retryPartialApply: (): Promise<LocalHostPartialApplyRetryResult> =>
    ipcRenderer.invoke("partialApply:retry"),
  pickDirectory: (options?: { defaultPath?: string }): Promise<string | null> =>
    ipcRenderer.invoke("dialog:pickDirectory", options),
  getAppVersion: (): Promise<string> => ipcRenderer.invoke("app:getVersion"),
  openExternal: (url: string): Promise<void> => ipcRenderer.invoke("shell:openExternal", url),
  setPinchZoom: setPreviewPinchZoom,
  /** Materialize attachment bytes to ONE stable per-id scratch file and
   *  hand it off to the OS default handler (e.g. open `.html` in the
   *  user's browser). The renderer fetches current bytes through the
   *  main-owned backend proxy on every open, then hands them here with the
   *  attachment id; main overwrites the same path so reopening shows the
   *  latest bytes instead of spawning a new temp file each time. */
  openAttachmentExternal: (input: {
    attachmentId: string;
    filename: string;
    bytes: Uint8Array;
  }): Promise<{ ok: true } | { ok: false; reason: string }> =>
    ipcRenderer.invoke("attachment:openExternal", input),
  /** Save attachment bytes to a user-chosen path via the OS save-as
   *  dialog. The renderer stays out of `fs` and `dialog`; main owns
   *  the write. `canceled: true` is the "user dismissed the picker"
   *  case (not an error) so the caller can stay silent. */
  saveAttachment: (input: {
    filename: string;
    bytes: Uint8Array;
  }): Promise<
    { ok: true; path: string } | { ok: false; canceled: true } | { ok: false; reason: string }
  > => ipcRenderer.invoke("attachment:save", input),
  /** Subscribe to shutdown progress steps emitted by the main process
   *  just before and during graceful shutdown. Drives the in-app overlay
   *  so the user sees what's happening instead of a frozen window. */
  onShutdownProgress: (handler: (step: string) => void): (() => void) => {
    const listener = (_event: IpcRendererEvent, step: string) => handler(step);
    ipcRenderer.on("shutdown:progress", listener);
    return () => ipcRenderer.off("shutdown:progress", listener);
  },
  onEpochUpdateRequired: (handler: () => void): (() => void) => {
    const listener = (_event: IpcRendererEvent) => handler();
    ipcRenderer.on("epoch:update-required", listener);
    return () => ipcRenderer.off("epoch:update-required", listener);
  },
  getAppUpdateState: (): Promise<AppUpdateState | null> => ipcRenderer.invoke("appUpdate:getState"),
  onAppUpdateState: (handler: (state: AppUpdateState) => void): (() => void) => {
    const listener = (_event: IpcRendererEvent, state: AppUpdateState) => handler(state);
    ipcRenderer.on("appUpdate:state", listener);
    return () => ipcRenderer.off("appUpdate:state", listener);
  },
  onAppUpdateShowDialog: (handler: () => void): (() => void) => {
    const listener = (_event: IpcRendererEvent) => handler();
    ipcRenderer.on("appUpdate:showDialog", listener);
    return () => ipcRenderer.off("appUpdate:showDialog", listener);
  },
  checkForAppUpdate: (): Promise<void> => ipcRenderer.invoke("appUpdate:check"),
  downloadAppUpdate: (): Promise<void> => ipcRenderer.invoke("appUpdate:download"),
  installAppUpdate: (mode: AppUpdateInstallMode): Promise<void> =>
    ipcRenderer.invoke("appUpdate:install", mode),
  /** macOS full-screen state; the traffic lights hide while it is true. */
  onFullScreenChange: (handler: (fullScreen: boolean) => void): (() => void) => {
    const listener = (_event: IpcRendererEvent, fullScreen: boolean) => handler(fullScreen);
    ipcRenderer.on("window:full-screen", listener);
    return () => ipcRenderer.off("window:full-screen", listener);
  },
  /** Fired by the App menu's "Settings" item (Cmd+,) so the renderer
   *  can navigate to the Settings page. The main process also focuses
   *  the window before emitting, so the handler only needs to navigate. */
  onNavigateSettings: (handler: () => void): (() => void) => {
    const listener = (_event: IpcRendererEvent) => handler();
    ipcRenderer.on("navigate:settings", listener);
    return () => ipcRenderer.off("navigate:settings", listener);
  },
  /** ⌘W contextual close. Main is the single interception owner
   *  (a menu accelerator fires before any renderer keydown), so it sends a
   *  per-keypress request; the renderer resolves its close ladder and answers
   *  via `respondContextualClose` with the SAME `reqId`. Positional args match
   *  the house style (`onNotificationClicked` etc.) and the main handler in
   *  `main/index.ts`. Named "contextual close", not "close window", because the
   *  command does NOT always background the app (`closed-panel` peels a panel
   *  instead). */
  onContextualClose: (handler: (reqId: string) => void): (() => void) => {
    const listener = (_event: IpcRendererEvent, reqId: string) => handler(reqId);
    ipcRenderer.on("shortcut:contextual-close", listener);
    return () => ipcRenderer.off("shortcut:contextual-close", listener);
  },
  respondContextualClose: (reqId: string, outcome: "closed-panel" | "background-app"): void => {
    ipcRenderer.send("shortcut:contextual-close-result", reqId, outcome);
  },
  /** The renderer's resolved language, so native menus read the same as the page. */
  setLocale: (locale: "en" | "zh"): void => {
    ipcRenderer.send("app:set-locale", locale);
  },
  showSystemNotification: (opts: {
    title: string;
    subtitle?: string;
    body?: string;
    groupId?: string;
    force?: boolean;
    /** Opaque id echoed back via `onNotificationClicked` so the renderer
     *  can route the click to the right navigation target. Fire-and-
     *  forget if omitted (the notification is just informational). */
    notificationId?: string;
  }): Promise<void> => ipcRenderer.invoke("notification:show", opts),
  /** Subscribe to native-notification click events. Main fires this
   *  when the user clicks an OS-level Silverhand notification, after
   *  focusing the window. The id matches what the renderer originally
   *  passed to `showSystemNotification`. */
  onNotificationClicked: (handler: (notificationId: string) => void): (() => void) => {
    const listener = (_event: IpcRendererEvent, notificationId: string) => {
      handler(notificationId);
    };
    ipcRenderer.on("notification:clicked", listener);
    return () => ipcRenderer.off("notification:clicked", listener);
  },
  /** Dismiss OS-level notifications by the ids originally passed to
   *  `showSystemNotification`. Fired when the backend reports those
   *  notifications were read (viewer-scoped `unread_updated`), so the
   *  system notification center clears in step with in-app read state.
   *  Unknown / already-closed ids are ignored by main. */
  dismissSystemNotifications: (notificationIds: string[]): Promise<void> =>
    ipcRenderer.invoke("notification:dismiss", notificationIds),
  setBadgeCount: (count: number): Promise<void> => ipcRenderer.invoke("badge:setCount", count),
  setMenuBarConversations: (items: MenuBarConversation[]): void => {
    ipcRenderer.send("menu-bar:set-conversations", items);
  },
  /** Fired after main has brought the window forward. */
  onMenuBarOpenConversation: (handler: (conversationId: string) => void): (() => void) => {
    const listener = (_event: IpcRendererEvent, conversationId: string) => handler(conversationId);
    ipcRenderer.on("menu-bar:open-conversation", listener);
    return () => ipcRenderer.off("menu-bar:open-conversation", listener);
  },
  /** Save a PNG snapshot to disk via the native save dialog. The
   *  renderer hands over raw bytes; main owns the dialog + writeFile.
   *  See `export:savePng` in main. */
  savePngExport: (input: {
    pngBytes: Uint8Array;
    suggestedName: string;
  }): Promise<
    { ok: true; path: string } | { ok: false; canceled: true } | { ok: false; error: string }
  > => ipcRenderer.invoke("export:savePng", input),
  /** Copy PNG snapshot bytes to the OS clipboard. Main constructs a
   *  NativeImage so Electron handles pasteboard format negotiation —
   *  renderer-side `ClipboardItem` for images is brittle on macOS. */
  copyPngExport: (input: {
    pngBytes: Uint8Array;
  }): Promise<{ ok: true } | { ok: false; error: string }> =>
    ipcRenderer.invoke("export:copyPng", input),
};

contextBridge.exposeInMainWorld("silverhand", api);
