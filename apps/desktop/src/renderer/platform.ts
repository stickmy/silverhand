import type { ClientPlatform } from "@silverhand/client";
import type { DesktopBridge } from "../preload/contract";

/** Desktop owns HTTP credentials, WS handshake policy and local-host side effects. */
export function createDesktopPlatform(
  bridge: DesktopBridge,
  restartUi: () => void,
): ClientPlatform {
  return {
    client: {
      getAuthState: () => bridge.getAuthState(),
      getServerInfo: () => bridge.getServerInfo(),
      signup: (input) => bridge.signup(input),
      login: (input) => bridge.login(input),
      logout: () => bridge.logout(),
      passwordRecovery: (input) => bridge.passwordRecovery(input),
      backendRequest: (input) => bridge.backendRequest(input),
      backendUpload: (input) => bridge.backendUpload(input),
      recordEventWsDiagnostic: (input) => bridge.recordEventWsDiagnostic(input),
      getServerConnectionProfile: () => bridge.getServerConnectionProfile(),
      saveServerConnectionProfile: async (input) => {
        const before = await bridge.getBackendInfo();
        const result = await bridge.saveServerConnectionProfile(input);
        if (result.ok) {
          const after = await bridge.getBackendInfo();
          if (before?.url !== after?.url) restartUi();
        }
        return result;
      },
      clearServerConnectionProfile: () => bridge.clearServerConnectionProfile(),
      onEpochUpdateRequired: (handler) => bridge.onEpochUpdateRequired(handler),
      getConnectionInfo: async () => {
        const info = await bridge.getBackendInfo();
        return info ? { url: info.url, mode: info.mode } : null;
      },
      getConnectionPolicy: () => bridge.getDesktopBuildConfig(),
      openEventSocket: (url) => new WebSocket(url),
    },
    files: {
      openExternal: (url) => bridge.openExternal(url),
      openAttachmentExternal: (input) => bridge.openAttachmentExternal(input),
      saveAttachment: (input) => bridge.saveAttachment(input),
      savePngExport: (input) => bridge.savePngExport(input),
      copyPngExport: (input) => bridge.copyPngExport(input),
    },
    localHost: {
      getSnapshot: async () => {
        const info = await bridge.getBackendInfo();
        if (!info) return null;
        const { url, mode, ...localHost } = info;
        return localHost;
      },
      rotateHostToken: (input) => bridge.rotateHostToken(input),
      getHostProxyConfig: (input) => bridge.getHostProxyConfig(input),
      saveHostProxyConfig: (input) => bridge.saveHostProxyConfig(input),
      saveServerUrl: (input) => bridge.saveServerUrl(input),
      getPartialApplyState: () => bridge.getPartialApplyState(),
      retryPartialApply: () => bridge.retryPartialApply(),
      pickDirectory: (input) => bridge.pickDirectory(input),
    },
    setLocale: (locale) => bridge.setLocale(locale),
    shell: {
      setPinchZoom: (enabled) => bridge.setPinchZoom(enabled),
      onShutdownProgress: (handler) => bridge.onShutdownProgress(handler),
      onNavigateSettings: (handler) => bridge.onNavigateSettings(handler),
      onContextualClose: (handler) => bridge.onContextualClose(handler),
      respondContextualClose: (id, outcome) => bridge.respondContextualClose(id, outcome),
    },
    getAppVersion: () => bridge.getAppVersion(),
    appUpdate: {
      getState: () => bridge.getAppUpdateState(),
      subscribe: (handler) => bridge.onAppUpdateState(handler),
      check: () => bridge.checkForAppUpdate(),
      download: () => bridge.downloadAppUpdate(),
      install: (mode) => bridge.installAppUpdate(mode),
      onShowDialog: (handler) => bridge.onAppUpdateShowDialog(handler),
    },
    notifications: {
      showSystemNotification: (input) => bridge.showSystemNotification(input),
      onNotificationClicked: (handler) => bridge.onNotificationClicked(handler),
      dismissSystemNotifications: (ids) => bridge.dismissSystemNotifications(ids),
      setBadgeCount: (count) => bridge.setBadgeCount(count),
    },
    menuBar: {
      setConversations: (items) => bridge.setMenuBarConversations(items),
      onOpenConversation: (handler) => bridge.onMenuBarOpenConversation(handler),
    },
  };
}
