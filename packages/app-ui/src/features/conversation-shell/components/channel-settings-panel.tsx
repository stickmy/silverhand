import { ChannelSettingsSkeleton } from "./channel-settings-skeleton";
import { useChannelSettingsDetail } from "../use-channel-settings-detail";
import { toast } from "sonner";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { AnimatePresence, motion, useIsPresent, useReducedMotion } from "motion/react";
import { ChevronRightIcon, HashIcon, LockIcon, PowerIcon, RefreshIcon } from "@/icons";
import type { ChannelDetailResponse } from "@silverhand/shared";
import { useNav, useRouteState } from "@/router";
import { paneChrome } from "@/components/layout/pane-chrome";
import { SidePanelHeader, SidePanelHeaderTitle } from "@/components/layout/side-panel-header";
import { Button } from "@/components/ui/button";
import {
  useArchiveChannel,
  useLeaveChannel,
  useRenameChannel,
  useSetChannelVisibility,
} from "@/features/workspaces/hooks";
import { useAgents } from "@/features/agents/hooks/hooks";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { canEditAgentProfile } from "@/features/agents/model/permissions";
import { isBackendUnreachable, isUpdateRequired, useViewerUser } from "@/features/session/hooks";
import {
  useChannelAgents,
  useChannelAgentRuntime,
  useStartAgent,
  useStopAgent,
} from "@/features/channels/hooks";
import { useConversationMembers } from "@/features/conversations/use-conversation-members";
import { ChannelSettingsDetails } from "./channel-settings-details";
import { ChannelMembersPreview } from "./channel-members-preview";
import { ChannelNotificationSettings } from "./channel-notification-settings";
import { ChannelMembersPage } from "./channel-members-page";
import { t } from "@/lib/i18n";

export function ChannelSettingsPanel({
  channelId,
  onDepartChannel,
}: {
  onDepartChannel: (channelId: string) => void;
  channelId: string;
}): JSX.Element {
  const nav = useNav();
  const membersPage = useRouteState().channelSettingsPage;
  const mounted = useRef(false);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const close = (): void => {
    if (mounted.current) {
      nav.closeChannelSettings();
    }
  };

  const onLeft = (channel: ChannelDetailResponse["channel"]): void => {
    if (!mounted.current) return;
    close();
    if (channel.roster_policy === "open" && channel.visibility === "private")
      onDepartChannel(channelId);
  };

  const onArchived = (archived: boolean): void => {
    if (!mounted.current) return;
    close();
    if (archived) onDepartChannel(channelId);
  };

  const detail = useChannelSettingsDetail(channelId);

  return (
    <aside
      data-panel="side"
      data-pane="channel-settings"
      className="flex h-full min-h-0 w-full flex-col overflow-hidden"
      style={paneChrome}
    >
      <SidePanelHeader
        onClose={nav.closeChannelSettings}
        onBack={membersPage ? nav.navigateBack : undefined}
        nativeBar={{
          title: membersPage
            ? t("conversationShell.members")
            : (detail.data?.channel.title ?? t("conversationShell.channelSettings")),
          subtitle: membersPage ? detail.data?.channel.title : undefined,
          titleAction: null,
          actions: [],
          tabs: null,
          addTabs: null,
        }}
      >
        <SidePanelHeaderTitle
          icon={
            detail.data?.channel.visibility === "private" ? (
              <LockIcon
                aria-label={t("conversationShell.privateChannel")}
                className="size-4 text-text-secondary"
              />
            ) : (
              <HashIcon
                aria-label={t("conversationShell.publicChannel")}
                className="size-4 text-text-secondary"
              />
            )
          }
          title={
            membersPage
              ? `${detail.data?.channel.title ?? t("conversationShell.channel")} - Members`
              : (detail.data?.channel.title ?? t("conversationShell.channelSettings"))
          }
        />
      </SidePanelHeader>
      {detail.isError ? (
        <div className="p-4 text-sm text-error">
          {t("conversationShell.unableToLoadChannel")}
          <Button
            variant="neutral"
            size="sm"
            className="mt-3"
            onClick={() => void detail.refetch()}
          >
            {t("common.retry")}
          </Button>
        </div>
      ) : !detail.data ? (
        <ChannelSettingsSkeleton />
      ) : !detail.data.is_member ? (
        <p className="p-4 text-sm text-text-muted">{t("conversationShell.joinThisChannelTo")}</p>
      ) : (
        <SettingsBody
          key={channelId}
          detail={detail.data}
          membersPage={membersPage !== null}
          focusMemberSearch={membersPage === "add"}
          onOpenMembers={(focusSearch) => nav.openChannelMembers(channelId, focusSearch)}
          onArchived={onArchived}
          onLeft={onLeft}
        />
      )}
    </aside>
  );
}

/** Keyed by channel. Pending forms and confirmations never migrate to another roster. */
function SettingsBody({
  detail,
  membersPage,
  focusMemberSearch,
  onOpenMembers,
  onArchived,
  onLeft,
}: {
  detail: ChannelDetailResponse;
  membersPage: boolean;
  focusMemberSearch: boolean;
  onOpenMembers: (focusSearch: boolean) => void;
  onArchived: (archived: boolean) => void;
  onLeft: (channel: ChannelDetailResponse["channel"]) => void;
}): JSX.Element {
  const reducedMotion = useReducedMotion();
  const { channel } = detail;
  const channelId = channel.id;
  const mounted = useRef(false);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const [bulkBusy, setBulkBusy] = useState(false);
  const rename = useRenameChannel(channel.workspace_id);
  const archive = useArchiveChannel(channel.workspace_id);
  const visibility = useSetChannelVisibility(channel.workspace_id);
  const leave = useLeaveChannel(channel.workspace_id);
  const { data: agents = [] } = useAgents();
  const avatarUrls = useAgentAvatarObjectUrls(agents);
  const { data: channelAgents = [] } = useChannelAgents(channelId);
  const { data: runtime } = useChannelAgentRuntime(channelId);
  const roster = useConversationMembers(channelId);
  const { data: viewer } = useViewerUser();
  const start = useStartAgent(channelId);
  const stop = useStopAgent(channelId);
  const closed = channel.closed_at !== null;
  const openRoster = channel.roster_policy === "open";

  const canManageAgents =
    !closed &&
    channelAgents.length > 0 &&
    channelAgents.every(({ agent_id }) => {
      const agent = agents.find((a) => a.id === agent_id);
      const user =
        viewer && !isBackendUnreachable(viewer) && !isUpdateRequired(viewer) ? viewer : undefined;
      return agent !== undefined && canEditAgentProfile(user, agent);
    });

  const bulkPending = bulkBusy || start.isPending || stop.isPending;

  const bulkAction = async (restart: boolean): Promise<void> => {
    if (!canManageAgents || bulkPending) return;
    setBulkBusy(true);
    try {
      for (const { agent_id } of channelAgents) {
        if (!mounted.current) return;
        await stop.mutateAsync({ agentId: agent_id });
      }
      if (restart)
        for (const { agent_id } of channelAgents) {
          if (!mounted.current) return;
          await start.mutateAsync(agent_id);
        }
    } catch {
      /* Mutation hooks own the error toast. */
    } finally {
      if (mounted.current) setBulkBusy(false);
    }
  };

  const leaveChannel = async (): Promise<void> => {
    try {
      const response = await leave.mutateAsync(channelId);
      onLeft(response.channel);
    } catch (error) {
      toast.error(t("conversationShell.leaveFailed"));
      throw error;
    }
  };
  const membersLabel =
    roster.status === "resolved"
      ? t("conversationShell.membersCount", { count: roster.members.length })
      : t("conversationShell.members");

  return (
    <AnimatePresence initial={false} mode="wait" custom={membersPage ? 1 : -1}>
      <SettingsPageTransition
        key={membersPage ? "members" : "overview"}
        direction={membersPage ? 1 : -1}
        reducedMotion={!!reducedMotion}
      >
        <div className="flex flex-col gap-8 p-4">
          {membersPage ? (
            <ChannelMembersPage
              autoFocus={focusMemberSearch}
              channelId={channelId}
              workspaceId={openRoster && !closed ? channel.workspace_id : null}
              agentRuntime={runtime}
              agentAvatarUrls={avatarUrls}
              onLeave={openRoster ? leaveChannel : undefined}
              canLeave={detail.can_leave}
              leaveBlockedReason={detail.leave_blocked_reason}
              visibility={openRoster ? channel.visibility : undefined}
              actions={
                canManageAgents ? (
                  <div className="flex flex-col gap-2 pt-2">
                    <Button
                      variant="neutral"
                      size="md"
                      disabled={bulkPending}
                      onClick={() => void bulkAction(true)}
                    >
                      <RefreshIcon className="size-3.5" /> {t("conversationShell.restart")}
                    </Button>
                    <Button
                      variant="neutral"
                      size="md"
                      disabled={bulkPending}
                      onClick={() => void bulkAction(false)}
                    >
                      <PowerIcon className="size-3.5" /> {t("conversationShell.stop")}
                    </Button>
                  </div>
                ) : null
              }
            />
          ) : (
            <>
              <section className="flex flex-col gap-3">
                <button
                  type="button"
                  onClick={() => onOpenMembers(false)}
                  className="flex w-full items-center justify-between text-xs font-semibold uppercase tracking-wide text-text-secondary hover:text-text-primary focus-outline"
                >
                  <span>{membersLabel}</span>
                  <ChevronRightIcon className="size-4" />
                </button>
                <ChannelMembersPreview
                  members={roster.status === "resolved" ? roster.members : []}
                  avatarUrls={avatarUrls}
                  canAdd={openRoster && !closed && !!channel.workspace_id}
                  onOpen={() => onOpenMembers(false)}
                  onAdd={() => onOpenMembers(true)}
                />
              </section>
              <ChannelSettingsDetails
                notifications={
                  openRoster && channel.workspace_id ? (
                    <ChannelNotificationSettings
                      channelId={channelId}
                      workspaceId={channel.workspace_id}
                    />
                  ) : null
                }
                currentTitle={channel.title}
                onRename={
                  !closed
                    ? async (title) => {
                        await rename.mutateAsync({ channelId, title });
                      }
                    : undefined
                }
                visibility={openRoster ? channel.visibility : undefined}
                canUpdateVisibility={detail.can_update_visibility}
                onSetVisibility={
                  openRoster
                    ? async (next) => {
                        await visibility.mutateAsync({ channelId, visibility: next });
                      }
                    : undefined
                }
                isArchived={channel.archived_at !== null}
                onToggleArchive={async (archived) => {
                  await archive.mutateAsync({ channelId, archived });
                  onArchived(archived);
                }}
                archivePending={archive.isPending}
                onLeave={openRoster ? leaveChannel : undefined}
                canLeave={detail.can_leave}
                leaveBlockedReason={detail.leave_blocked_reason}
              />
            </>
          )}
        </div>
      </SettingsPageTransition>
    </AnimatePresence>
  );
}

/** An exiting page stays visible briefly, but must no longer accept input. */
function SettingsPageTransition({
  children,
  direction,
  reducedMotion,
}: {
  children: ReactNode;
  direction: number;
  reducedMotion: boolean;
}): JSX.Element {
  const isPresent = useIsPresent();

  return (
    <motion.div
      ref={(node) => node?.toggleAttribute("inert", !isPresent)}
      aria-hidden={!isPresent || undefined}
      className="min-h-0 flex-1 overflow-y-auto"
      style={{ pointerEvents: isPresent ? undefined : "none" }}
      custom={direction}
      variants={{
        enter: (nextDirection: number) => ({
          opacity: 0,
          x: reducedMotion ? 0 : nextDirection * 8,
        }),
        visible: {
          opacity: 1,
          x: 0,
          transition: { duration: reducedMotion ? 0 : 0.12, ease: "easeOut" },
        },
        exit: (nextDirection: number) => ({
          opacity: 0,
          x: reducedMotion ? 0 : nextDirection * -8,
          transition: { duration: reducedMotion ? 0 : 0.06, ease: "easeIn" },
        }),
      }}
      initial="enter"
      animate="visible"
      exit="exit"
    >
      {children}
    </motion.div>
  );
}
