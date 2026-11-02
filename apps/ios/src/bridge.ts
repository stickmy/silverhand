import type { ServerInfo } from "@silverhand/shared";
import { registerPlugin, type PluginListenerHandle } from "@capacitor/core";
import type {
  BannerEvent,
  BannerNotice,
  ToastEvent,
  ToastNotice,
  ComposerEvent,
  ComposerState,
  ContextMenu,
  ContextMenuChoice,
  TitlebarEvent,
  TitlebarState,
  PageBarEvent,
  PageBarState,
  SelectionBarEvent,
  SelectionBarState,
  BackendProxyRequest,
  BackendProxyResponse,
  ClientAuthResult,
  PasswordRecoveryCall,
  PasswordRecoveryResult,
  NativeNavigationState,
  NativeNavigationIntent,
} from "@silverhand/client";

export interface NativeSocketEvent {
  id: string;
  type: "open" | "message" | "close" | "error";
  data?: string;
  code?: number;
}
export interface NativeBridge {
  navigationBadges(input: {
    session: string;
    chat?: boolean;
    activity?: boolean;
    tasks?: boolean;
  }): Promise<void>;
  navigationSync(state: NativeNavigationState): Promise<{ tabBarHeight: number }>;
  navigationReset(input: { session: string }): Promise<void>;
  navigationAvatar(input: { base64?: string }): Promise<void>;
  /** The language the page resolved; native tabs and sheets follow it. */
  setLocale(input: { locale: "en" | "zh" }): Promise<void>;
  /** Groups of page controls the iPhone Duo vertical bar draws. */
  /** The wide layout's native titlebar; a null state removes it. */
  titlebar(input: { state: TitlebarState | null }): Promise<void>;
  titlebarEndSearch(input: { clear: boolean }): Promise<void>;
  /** A compact page's header as the native navigation bar; icons are base64
   * PNG. A null state removes it. */
  pageBar(input: { state: PageBarState | null }): Promise<void>;
  pageBarBackProgress(input: { progress: number | null }): Promise<void>;
  /** The compact composer; menu and picker icons are base64 PNG. */
  composerSync(input: { state: ComposerState }): Promise<void>;
  /** Removes the composer if it shows draft `key`; without a key, whatever it shows. */
  composerRemove(input: { key?: string }): Promise<void>;
  /** The compact message-selection bar, in the composer's place. */
  selectionBarSync(input: { state: SelectionBarState }): Promise<void>;
  /** Removes the selection bar if it shows pane `key`. */
  selectionBarRemove(input: { key: string }): Promise<void>;
  /** A notice over the bars; the icon is base64 PNG. */
  bannerShow(input: BannerNotice): Promise<void>;
  toastShow(input: ToastNotice): Promise<void>;
  toastDismiss(input: { id?: string }): Promise<void>;
  composerSetText(input: {
    key: string;
    text: string;
    caret: number;
    focus: boolean;
  }): Promise<void>;
  addListener(
    event: "navigation",
    handler: (intent: NativeNavigationIntent) => void,
  ): Promise<PluginListenerHandle>;
  getProfile(): Promise<{ server_url: string; updated_at: number } | { server_url: null }>;
  saveProfile(input: {
    serverUrl: string;
  }): Promise<{ server_url: string; updated_at: number; info: ServerInfo }>;
  clearProfile(): Promise<{ cleared: boolean }>;
  authState(): Promise<{
    serverUrl: string;
    info: BackendProxyResponse;
    me?: BackendProxyResponse;
  }>;
  serverInfo(): Promise<BackendProxyResponse>;
  authenticate(input: { action: "login" | "signup"; body: string }): Promise<ClientAuthResult>;
  passwordRecovery(input: PasswordRecoveryCall): Promise<PasswordRecoveryResult>;
  logout(): Promise<void>;
  request(input: BackendProxyRequest): Promise<BackendProxyResponse & { bodyBase64?: string }>;
  upload(input: {
    path: string;
    fields: Array<{ name: string; value: string }>;
    files: Array<{ fieldName: string; filename: string; contentType: string; base64: string }>;
  }): Promise<BackendProxyResponse>;
  openSocket(input: { id: string; url: string }): Promise<void>;
  sendSocket(input: { id: string; data: string }): Promise<void>;
  closeSocket(input: { id: string }): Promise<void>;
  saveAttachment(input: { filename: string; base64: string }): Promise<{ completed: boolean }>;
  sharePng(input: { base64: string }): Promise<{ completed: boolean }>;
  /** An image in a native sheet with a share action: inline, or an attachment
   * native downloads while the sheet shows. */
  previewPng(input: {
    base64?: string;
    attachmentId?: string;
    title: string;
    shareLabel: string;
    closeLabel: string;
  }): Promise<{ shared: boolean }>;
  /** Whether this iOS draws `contextMenuShow`. */
  contextMenuAvailable(): Promise<{ available: boolean }>;
  /** The system context menu; `choice` is absent when it was dismissed. */
  contextMenuShow(input: { menu: ContextMenu }): Promise<{ choice?: ContextMenuChoice }>;
  /** The native avatar editor; both fields are absent when it was dismissed. */
  avatarMakerEdit(input: {
    config: { parts: Record<string, number>; bg: number };
  }): Promise<{ config?: { parts: Record<string, number>; bg: number }; base64?: string }>;
  openExternal(input: { url: string }): Promise<void>;
  appVersion(): Promise<{ version: string }>;
  addListener(
    event: "socket",
    handler: (event: NativeSocketEvent) => void,
  ): Promise<PluginListenerHandle>;
  addListener(event: "sessionInvalidated", handler: () => void): Promise<PluginListenerHandle>;
  addListener(
    event: "keyboardChanged",
    handler: (state: { docked: boolean }) => void,
  ): Promise<PluginListenerHandle>;
  addListener(event: "foreground", handler: () => void): Promise<PluginListenerHandle>;
  addListener(
    event: "titlebar",
    handler: (event: TitlebarEvent) => void,
  ): Promise<PluginListenerHandle>;
  addListener(
    event: "pageBar",
    handler: (event: PageBarEvent) => void,
  ): Promise<PluginListenerHandle>;
  addListener(
    event: "composer",
    handler: (event: ComposerEvent) => void,
  ): Promise<PluginListenerHandle>;
  addListener(
    event: "selectionBar",
    handler: (event: SelectionBarEvent) => void,
  ): Promise<PluginListenerHandle>;
  addListener(
    event: "banner",
    handler: (event: BannerEvent) => void,
  ): Promise<PluginListenerHandle>;
  addListener(event: "toast", handler: (event: ToastEvent) => void): Promise<PluginListenerHandle>;
  addListener(
    event: "fold",
    handler: (
      state: { axis: "vertical" | "horizontal"; start: number; end: number } | { axis?: undefined },
    ) => void,
  ): Promise<PluginListenerHandle>;
  addListener(
    event: "barMetrics",
    handler: (metrics: { bottom: number; tabs: number; vertical: boolean }) => void,
  ): Promise<PluginListenerHandle>;
}
export const nativeBridge = registerPlugin<NativeBridge>("SilverhandClient");
