import { MobileHeaderProvider, MobileHeaderScope } from "@/components/layout/mobile-header-slot";
import { useAppOptions } from "./app-options";
import { NavigationLaunch } from "@/navigation/launch";
import { NavigationProvider } from "@/navigation/provider";
import { PageBarHost } from "@/navigation/page-bar";
import { PageStack } from "@/navigation/page-stack";
import { useCompactLayout, useHoverCapable } from "@/navigation/layout";
import { getPlatform } from "@/platform";
import { MobileHeader, MobileTabBar } from "@/components/layout/mobile-shell";
import { useEffect, useState } from "react";
import { ShutdownOverlay } from "@/features/app-shell/shutdown-overlay";
import { ChatShell, isChatSidebarPage } from "@/pages/chat/chat-shell";
import { ActivityShell } from "@/pages/activity/activity-shell";
import { TaskShell } from "@/pages/task/task-shell";
import { useConversationShell } from "@/features/conversation-shell/use-conversation-shell";
import { renderConversationPanes } from "@/features/conversation-shell/conversation-content";
import { useApplicationSignals } from "@/features/app-shell/application-signals";
import { AgentShell } from "@/pages/agent/agent-shell";
import { ComputerShell } from "@/pages/computer/computer-shell";
import { Toaster } from "sonner";
import { NavRail } from "@/components/layout/nav-rail";
import { SectionLayer } from "@/components/layout/section-layer";
import { hasCollapsibleSidebar, SidebarLayoutProvider } from "@/components/layout/sidebar-slot";
import { SidebarPeekLayer, useSidebarPeek } from "@/components/layout/sidebar-peek";
import { ChatPeekSidebar } from "@/pages/chat/chat-peek-sidebar";
import { MembersPeekSidebar } from "@/pages/agent/agent-shell";
import { ComputersPeekSidebar } from "@/pages/computer/computer-shell";
import { SettingsPeekSidebar } from "@/pages/settings/settings-shell";
import { useResizableWidth } from "@/hooks/use-resizable-width";
import { useSidebarStyle } from "@/features/conversation-shell/use-sidebar-style";
import { Titlebar } from "@/components/layout/titlebar";
import { TitlebarSearch, type TitlebarSearchNav } from "@/features/search/titlebar-search";
import { NativeTitlebar } from "@/features/app-shell/native-titlebar";
import { useAppearance } from "@/features/appearance/use-appearance";
import {
  isBackendUnreachable,
  isRegistrationRequired,
  isUpdateRequired,
  useRuntimeEpochPromotion,
  useViewerAvatarObjectUrl,
  useViewerUser,
} from "@/features/session/hooks";
import { useServerEnvironment } from "@/features/session/server-environment";
import { EmbeddedProfileSetupGate } from "@/features/app-shell/embedded-profile-setup-gate";
import { RegistrationGate } from "@/features/app-shell/registration-gate";
import { UpdateRequiredGate } from "@/features/app-shell/update-required-gate";
import { AppStatusBanner } from "@/features/app-shell/app-status-banner";
import { MenuBarSync } from "@/features/app-shell/menu-bar-sync";
import { SettingsShell } from "@/pages/settings/settings-shell";
import { MeShell } from "@/pages/me/me-shell";
import { shouldSecondaryFullscreen } from "@/features/conversation-shell/model/secondary-pane";
import { LAST_CHANNEL_KEY, clearPlaceMemory } from "@/features/app-shell/place-memory";
import { toasterTypedIcons } from "@/lib/toast-icons";
import { AppUpdateDialog } from "@/features/app-update/app-update-dialog";
import {
  hasActionableUpdate,
  openAppUpdateDialog,
  useAppUpdateState,
} from "@/features/app-update/app-update-store";
import { t } from "@/lib/i18n";

export { LAST_CHANNEL_KEY, clearPlaceMemory };

/** The update dialog sits outside every gate: updating must not need a session or a compatible server. */
export function AppRoot(props: { initialNavigationUrl?: string }): JSX.Element {
  return (
    <>
      <App {...props} />
      <AppUpdateDialog />
    </>
  );
}

export function App({
  initialNavigationUrl = "/chat",
}: { initialNavigationUrl?: string } = {}): JSX.Element {
  const { appearance } = useAppearance();
  const { loginFirst, showServerAddress = false } = useAppOptions();

  useRuntimeEpochPromotion();

  const viewer = useViewerUser();
  const environment = useServerEnvironment();
  const features = environment.data?.features ?? null;
  const signupEnabled = features?.signup_enabled ?? false;
  // The embedded local server has no account recovery, so the entry is absent there.
  const passwordRecovery =
    environment.data?.mode === "remote"
      ? features?.password_recovery_enabled
        ? ("enabled" as const)
        : ("disabled" as const)
      : undefined;
  const retry = (): void => {
    void viewer.refetch();
    void environment.refetch();
  };
  const [navigationLaunch] = useState(() => new NavigationLaunch(initialNavigationUrl));
  const [hasSignedIn, setHasSignedIn] = useState(false);
  const authenticated = Boolean(viewer.data && "id" in viewer.data);
  useEffect(() => {
    if (authenticated) setHasSignedIn(true);
  }, [authenticated]);

  // Three-way render: loading → registration gate → registered shell.
  // Critical: each branch returns BEFORE any further hooks, and the
  // shell lives in a separate component (`RegisteredAppShell`) so the
  // hook order is stable inside `App` across re-renders. If we kept
  // the shell hooks inline here, the post-registration render would
  // call dozens more hooks than the pre-registration render and React
  // would crash with "Rendered more hooks than during the previous
  // render".
  // A relaunch (the OS reclaims the WebView in the background) starts with no
  // session answer. Until it arrives, paint the bare canvas the native launch
  // screen already shows: a signed-in user must not see the login form flash.
  if (loginFirst && !viewer.data) {
    return <div className="h-screen w-screen bg-bg-base" />;
  }
  if (!viewer.data) {
    return (
      <div className="grid h-screen w-screen place-items-center bg-bg-base text-xs text-text-muted">
        {t("common.loading")}
      </div>
    );
  }

  // Reached only before anyone signed in this session: a signed-in viewer keeps
  // the app and the status banner reports the connection instead.
  if (isBackendUnreachable(viewer.data)) {
    return (
      <RegistrationGate
        signupEnabled={false}
        preferLogin
        autoFocus={!loginFirst}
        connectionStatus={viewer.data.message}
        connectionChecking={viewer.isFetching}
        connectionReady={false}
        showServerAddress
        onRetry={retry}
      />
    );
  }

  if (isUpdateRequired(viewer.data)) {
    return (
      <UpdateRequiredGate
        direction={viewer.data.direction}
        serverApiEpoch={viewer.data.server_api_epoch}
        onRetry={() => {
          void viewer.refetch();
        }}
      />
    );
  }
  if (isRegistrationRequired(viewer.data)) {
    // Signup or login is chosen once on mount, so wait for the server's answer.
    if (environment.isPending) {
      return <div className="h-screen w-screen bg-bg-base" />;
    }
    return (
      <RegistrationGate
        signupEnabled={signupEnabled}
        passwordRecovery={passwordRecovery}
        preferLogin={loginFirst || hasSignedIn}
        autoFocus={!loginFirst}
        connectionChecking={viewer.isFetching}
        showServerAddress={showServerAddress}
        onRetry={retry}
      />
    );
  }
  // slack-dark is our only dark theme; studio-light / poster are light. sonner
  // then picks the matching default scale for the parts our token overrides in
  // chrome.css don't cover (e.g. any action/close button greys).
  return (
    <EmbeddedProfileSetupGate key={viewer.data.id} viewerId={viewer.data.id}>
      <NavigationProvider launch={navigationLaunch} dark={appearance.theme === "slack-dark"}>
        <PageBarHost>
          <RegisteredAppShell
            toasterTheme={appearance.theme === "slack-dark" ? "dark" : "light"}
            darkFrame={appearance.theme === "poster"}
          />
        </PageBarHost>
      </NavigationProvider>
    </EmbeddedProfileSetupGate>
  );
}

/**
 * Toast viewport insets (#task:1824 — notifications moved from the bottom-right
 * to the top-right).
 *
 * Top clears `--titlebar-height` instead of sitting on it: the titlebar is a
 * window-DRAG region, and a float parked over it both reads as "on top of the
 * chrome" and hands its own clicks to the window drag unless it opts out
 * (`AppStatusBanner` has to). Starting below the bar avoids that whole class,
 * and top-right of the CONTENT area is what "in-app" means anyway. Right inset
 * matches the titlebar's trailing spacer so the toast's edge lines up with the
 * chrome above it.
 *
 * `mobileOffset` is spelled out because the main window has no `minWidth`:
 * under 600px sonner swaps to its mobile insets (default 16px all round) and
 * would land back on the titlebar. All four keys are set in both objects —
 * sonner only writes the keys it is given, and its mobile rules compute
 * `width: calc(100% - var(--mobile-offset-left) * 2)`, which breaks on an
 * unset var.
 */
const TOAST_TOP = "calc(var(--titlebar-height) + env(safe-area-inset-top, 0px) + 8px)";
const TOAST_OFFSET = {
  top: TOAST_TOP,
  right: "12px",
  bottom: "24px",
  left: "24px",
} as const;
const TOAST_MOBILE_OFFSET = {
  top: TOAST_TOP,
  right: "16px",
  bottom: "16px",
  left: "16px",
} as const;

function RegisteredAppShell({
  toasterTheme,
  darkFrame,
}: {
  toasterTheme: "light" | "dark";
  /** The theme's window frame is dark (poster's wall). */
  darkFrame: boolean;
}): JSX.Element {
  const compact = useCompactLayout();
  const hoverCapable = useHoverCapable();
  const signals = useApplicationSignals();
  const { navRailDots } = signals;
  const appUpdateState = useAppUpdateState();
  const conversation = useConversationShell(signals);
  const viewerAvatarObjectUrl = useViewerAvatarObjectUrl();
  const {
    toggleWorkspaceSidebar,
    nav,
    appSection,
    workspaceSidebarVisible,
    selectedWorkspace,
    openAgentConversation,
    deferredSection,
    handleJumpToMessage,
    handleMessageAgent,
    resolveNavWorkspaceId,
  } = conversation.app;
  const content = renderConversationPanes(conversation.panes);
  const nativeTitlebar = getPlatform().titlebar;
  const chromeMode = appSection === "activity" ? "activity" : "chat";
  const sidebarToggle = hasCollapsibleSidebar(appSection);

  // Recent rows carry a preview line, so that style keeps its own, wider width.
  const sidebarStyle = useSidebarStyle();
  const sidebarWidth = useResizableWidth({
    storageKey:
      sidebarStyle === "recent"
        ? "silverhand.layout.sidebar-width.recent"
        : "silverhand.layout.sidebar-width",
    defaultWidth: sidebarStyle === "recent" ? 320 : 240,
    min: 230,
    max: 400,
    cssVar: "--sidebar-width",
  });
  const sidebarPeek = useSidebarPeek(
    sidebarToggle && !workspaceSidebarVisible && !compact && hoverCapable,
  );
  const searchNav: TitlebarSearchNav = {
    onJumpMessage: handleJumpToMessage,
    onOpenConversation: (workspaceId, channelId) => {
      const navWorkspaceId = resolveNavWorkspaceId(workspaceId, channelId);
      if (navWorkspaceId) nav.toChannel(navWorkspaceId, channelId);
    },
    onOpenWorkspace: nav.toWorkspace,
    onOpenDirectPair: nav.toDirect,
    onOpenDirectDraft: nav.toDirectDraft,
    onOpenAdvanced: (query) => nav.toChatSearch(query ? { q: query } : undefined),
  };
  const fullscreen = shouldSecondaryFullscreen(
    conversation.app.secondaryDescriptor,
    conversation.app.routeFullscreen,
  );

  return (
    <SidebarLayoutProvider
      value={{ pane: conversation.chat.workspaceSidebarPane, width: sidebarWidth }}
    >
      <MobileHeaderProvider section={appSection}>
        <div
          className="app-shell flex h-screen w-screen flex-col overflow-hidden"
          style={{
            backgroundImage: "var(--app-paper, none)",
            backgroundSize: "100vw 100vh",
            backgroundRepeat: "no-repeat",
          }}
        >
          {compact ? (
            <MobileHeader
              hideFallback={appSection === "chat" && isChatSidebarPage(conversation.chat, content)}
            />
          ) : nativeTitlebar ? (
            <NativeTitlebar
              titlebar={nativeTitlebar}
              sidebarShown={sidebarToggle ? workspaceSidebarVisible : null}
              archived={selectedWorkspace?.archived_at != null}
              onToggleSidebar={toggleWorkspaceSidebar}
              searchNav={searchNav}
              darkFrame={darkFrame}
            />
          ) : (
            <Titlebar
              workspaceArchivedAt={selectedWorkspace?.archived_at ?? null}
              workspaceSidebarVisible={workspaceSidebarVisible}
              onToggleWorkspaceSidebar={toggleWorkspaceSidebar}
              chromeMode={chromeMode}
              sidebarToggle={sidebarToggle}
              search={<TitlebarSearch nav={searchNav} />}
            />
          )}
          <div
            data-app-frame
            className="relative flex min-w-0 flex-1 overflow-hidden"
            style={{
              padding: "0 var(--frame-padding, 0) var(--frame-padding, 0)",
              gap: "var(--frame-padding, 0)",
            }}
          >
            {!compact && (
              <NavRail
                current={appSection}
                dots={navRailDots}
                update={
                  hasActionableUpdate(appUpdateState)
                    ? { label: t("ui.updateSilverhand"), onClick: openAppUpdateDialog }
                    : null
                }
                onHoverSection={sidebarPeek.hover}
                onHoverEnd={sidebarPeek.leave}
                peeking={sidebarPeek.section !== null}
                onChange={(section) => {
                  if (section === "chat") {
                    nav.toChat();
                  } else if (section === "activity") {
                    nav.toActivity();
                  } else if (section === "tasks") {
                    nav.toTasks();
                  } else if (
                    section === "members" ||
                    section === "settings" ||
                    section === "computers"
                  ) {
                    nav.toUtilitySection(section);
                  }
                }}
              />
            )}
            <div
              data-section-sheet=""
              className="flex min-w-0 flex-1"
              style={{ gap: "var(--pane-gap)" }}
            >
              <PageStack>
                <>
                  {deferredSection === "me" && (
                    <SectionLayer visible={appSection !== "chat"}>
                      <MobileHeaderScope section="me">
                        <MeShell />
                      </MobileHeaderScope>
                    </SectionLayer>
                  )}
                  {deferredSection === "settings" && (
                    <SectionLayer visible={appSection !== "chat"}>
                      <MobileHeaderScope section="settings">
                        <SettingsShell />
                      </MobileHeaderScope>
                    </SectionLayer>
                  )}
                  <MobileHeaderScope section="tasks">
                    <TaskShell
                      mounted={deferredSection === "tasks"}
                      visible={appSection !== "chat"}
                      onJump={handleJumpToMessage}
                    />
                  </MobileHeaderScope>
                  <MobileHeaderScope section="members">
                    <AgentShell
                      mounted={deferredSection === "members"}
                      visible={appSection !== "chat"}
                      onMessageAgent={handleMessageAgent}
                      onOpenConversation={openAgentConversation}
                    />
                  </MobileHeaderScope>
                  <MobileHeaderScope section="computers">
                    <ComputerShell
                      mounted={deferredSection === "computers"}
                      visible={appSection !== "chat"}
                    />
                  </MobileHeaderScope>
                  <MobileHeaderScope section="activity">
                    <ActivityShell
                      model={conversation.activity}
                      content={content}
                      fullscreen={fullscreen}
                    />
                  </MobileHeaderScope>
                  <MobileHeaderScope section="chat">
                    <ChatShell
                      model={conversation.chat}
                      content={content}
                      fullscreen={fullscreen}
                    />
                  </MobileHeaderScope>
                </>
              </PageStack>
            </div>
            <SidebarPeekLayer peek={sidebarPeek}>
              {sidebarPeek.mountedSection === "chat" ? (
                <ChatPeekSidebar model={conversation.chat} />
              ) : sidebarPeek.mountedSection === "members" ? (
                <MembersPeekSidebar />
              ) : sidebarPeek.mountedSection === "computers" ? (
                <ComputersPeekSidebar />
              ) : sidebarPeek.mountedSection === "settings" ? (
                <SettingsPeekSidebar />
              ) : null}
            </SidebarPeekLayer>
          </div>

          {/* With a native tab bar this renders nothing and only syncs badges,
           * which the bar needs in every layout. */}
          {(compact || getPlatform().navigation) && (
            <MobileTabBar dots={navRailDots} avatarObjectUrl={viewerAvatarObjectUrl} />
          )}

          {/* Toast surface follows the active theme: `theme` picks sonner's
           * light/dark default scale, and chrome.css maps its fill / text /
           * border / radius onto our tokens (poster gets the ink frame). No
           * `richColors` — that paints the whole toast in sonner's own hue
           * palette; instead the surface stays neutral and the type icon carries
           * the semantic, matching AppStatusBanner. Icons come from the shared
           * lib/toast-icons authority; only sonner's typed levels have map slots
           * here — neutral `toast()` calls pass their own icon from the same
           * table. */}
          <Toaster
            position="top-right"
            theme={toasterTheme}
            icons={toasterTypedIcons}
            offset={TOAST_OFFSET}
            mobileOffset={TOAST_MOBILE_OFFSET}
          />

          <AppStatusBanner />

          <ShutdownOverlay />

          {getPlatform().menuBar ? <MenuBarSync /> : null}
        </div>
      </MobileHeaderProvider>
    </SidebarLayoutProvider>
  );
}
