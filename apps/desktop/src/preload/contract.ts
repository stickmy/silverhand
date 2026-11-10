import type {
  AuthLoginInput,
  AuthSignupInput,
  ServerConnectionProfileView,
  ServerInfo,
} from "@silverhand/shared";
import type {
  ConnectionInfo,
  LocalHostInfo,
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
  EventWsDiagnostic,
  AppUpdateInstallMode,
  AppUpdateState,
  MenuBarConversation,
} from "@silverhand/client";
import type { DesktopBuildConfig } from "../build-flags.js";

export interface DesktopBackendInfo extends ConnectionInfo, LocalHostInfo {}

export interface DesktopBridge {
  getDesktopBuildConfig(): Promise<DesktopBuildConfig>;
  getBackendInfo(): Promise<DesktopBackendInfo | null>;
  getAuthState(): Promise<ClientAuthState>;
  getServerInfo(): Promise<ServerInfo>;
  signup(input: AuthSignupInput): Promise<ClientAuthResult>;
  login(input: AuthLoginInput): Promise<ClientAuthResult>;
  logout(): Promise<{ ok: true }>;
  passwordRecovery(input: PasswordRecoveryCall): Promise<PasswordRecoveryResult>;
  backendRequest(input: BackendProxyRequest): Promise<BackendProxyResponse>;
  backendUpload(input: BackendUploadRequest): Promise<BackendProxyResponse>;
  recordEventWsDiagnostic(input: EventWsDiagnostic): Promise<void>;

  setPinchZoom(enabled: boolean): void;
  rotateHostToken(input: { hostId: string }): Promise<LocalHostMutationResult>;
  getHostProxyConfig(input: { hostId: string }): Promise<LocalHostProxyConfigResult>;
  saveHostProxyConfig(input: {
    hostId: string;
    config: LocalHostProxyConfig;
  }): Promise<LocalHostProxyConfigResult>;

  saveServerUrl(input: { serverUrl: string }): Promise<LocalHostSaveServerUrlResult>;

  getServerConnectionProfile(): Promise<ServerConnectionProfileView | null>;

  saveServerConnectionProfile(input: { serverUrl: string }): Promise<ProfileSaveWithTestResult>;

  clearServerConnectionProfile(): Promise<ClearProfileResult>;

  getPartialApplyState(): Promise<LocalHostPartialApplyState>;

  retryPartialApply(): Promise<LocalHostPartialApplyRetryResult>;
  pickDirectory(options?: { defaultPath?: string }): Promise<string | null>;
  getAppVersion(): Promise<string>;
  openExternal(url: string): Promise<void>;
  openAttachmentExternal(input: {
    attachmentId: string;
    filename: string;
    bytes: Uint8Array;
  }): Promise<{ ok: true } | { ok: false; reason: string }>;
  saveAttachment(input: {
    filename: string;
    bytes: Uint8Array;
  }): Promise<
    { ok: true; path: string } | { ok: false; canceled: true } | { ok: false; reason: string }
  >;
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
  setMenuBarConversations(items: MenuBarConversation[]): void;
  onMenuBarOpenConversation(handler: (conversationId: string) => void): () => void;
  onShutdownProgress(handler: (step: string) => void): () => void;

  onEpochUpdateRequired(handler: () => void): () => void;
  getAppUpdateState(): Promise<AppUpdateState | null>;
  onAppUpdateState(handler: (state: AppUpdateState) => void): () => void;
  onAppUpdateShowDialog(handler: () => void): () => void;
  checkForAppUpdate(): Promise<void>;
  downloadAppUpdate(): Promise<void>;
  installAppUpdate(mode: AppUpdateInstallMode): Promise<void>;
  onNavigateSettings(handler: () => void): () => void;
  onFullScreenChange(handler: (fullScreen: boolean) => void): () => void;

  onContextualClose(handler: (reqId: string) => void): () => void;

  respondContextualClose(reqId: string, outcome: "closed-panel" | "background-app"): void;
  setLocale(locale: "en" | "zh"): void;
  savePngExport(input: {
    pngBytes: Uint8Array;
    suggestedName: string;
  }): Promise<
    { ok: true; path: string } | { ok: false; canceled: true } | { ok: false; error: string }
  >;
  copyPngExport(input: {
    pngBytes: Uint8Array;
  }): Promise<{ ok: true } | { ok: false; error: string }>;
}
