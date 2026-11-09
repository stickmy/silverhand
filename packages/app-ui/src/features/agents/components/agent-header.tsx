import { ProfileHeaderContent } from "@/features/profile/profile-header-content";
import { useState } from "react";
import { DeleteIcon, MessageIcon, MoreIcon, PowerIcon } from "@/icons";
import type { AgentDto, AgentPublicDto, AgentRuntimeStateDto } from "@silverhand/shared";
import { AgentAvatar } from "@/components/agent-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { iconMarkup, useNativeBar } from "@/components/layout/native-page-bar";
import { SidePanelHeader } from "@/components/layout/side-panel-header";
import { ProfileHeaderAction as HeaderAction } from "@/features/profile/profile-header-action";
import { useAgentPresence, useStopAgent } from "../hooks/hooks";
import { projectPresence, PRESENCE_TONE_DOT } from "../model/presence-projection";
import { useAgentAvatarObjectUrl } from "../hooks/avatar-hooks";
import { DeleteAgentDialog } from "./delete-agent-dialog";
import { isPrivilegedAgent } from "../model/agent-read";
import { t } from "@/lib/i18n";

interface AgentHeaderProps {
  /** A public read carries identity only, so the header drops every action. */
  agent: AgentDto | AgentPublicDto;
  runtimeState: AgentRuntimeStateDto | undefined;
  onDeleted: () => void;
  onClose?: () => void;
  mode: "full" | "side";
  onMessageAgent?: (agentId: string) => void;
}

export function AgentHeader({
  agent,
  runtimeState,
  onDeleted,
  onMessageAgent,
  onClose,
  mode,
}: AgentHeaderProps): JSX.Element {
  const compact = mode === "side";
  const full = isPrivilegedAgent(agent) ? agent : null;

  const [deleteOpen, setDeleteOpen] = useState(false);

  const stopAgent = useStopAgent(agent.id);
  const avatarObjectUrl = useAgentAvatarObjectUrl(agent.avatar_url);
  const presence = useAgentPresence(agent.id);

  // Availability describes the agent globally; seats only decide whether Stop is actionable.
  const runningCount = (runtimeState?.seats ?? []).filter(
    (seat) => seat.status === "starting" || seat.status === "active",
  ).length;
  const canMessage = Boolean(onMessageAgent) && full?.session_scope === "agent";
  const displayName = agent.display_name || agent.name;
  const presenceDisplay = presence === null ? null : projectPresence({ scope: "global", presence });

  const statusDot = presenceDisplay ? (
    <span
      role="img"
      aria-label={presenceDisplay.a11yLabel}
      title={presenceDisplay.detail}
      className={`size-1.5 shrink-0 self-center rounded-full ${PRESENCE_TONE_DOT[presenceDisplay.tone]}`}
    />
  ) : null;

  const avatar = (
    <AgentAvatar
      runtime={agent.runtime}
      size={compact ? AVATAR_PX.md : AVATAR_PX.xl}
      avatarObjectUrl={avatarObjectUrl}
      alt={displayName}
    />
  );

  const actions = full ? (
    <>
      {canMessage && onMessageAgent ? (
        <HeaderAction
          compact={compact}
          label={t("agents.message")}
          title={t("agents.messageAgent", { name: agent.name })}
          onClick={() => onMessageAgent(agent.id)}
        >
          <MessageIcon className="size-3.5" />
        </HeaderAction>
      ) : null}
      <HeaderAction
        compact={compact}
        label={t("agents.stop")}
        title={
          runningCount === 0
            ? t("agents.nothingIsRunning")
            : t("agents.hardStop", { count: runningCount })
        }
        disabled={runningCount === 0 || stopAgent.isPending}
        onClick={() => stopAgent.mutate()}
      >
        <PowerIcon className="size-3.5" />
      </HeaderAction>
      {!compact ? (
        <HeaderAction
          compact={compact}
          label={t("agents.delete")}
          title={t("agents.deleteAgent")}
          destructive
          onClick={() => setDeleteOpen(true)}
        >
          <DeleteIcon className="size-3.5" />
        </HeaderAction>
      ) : null}
    </>
  ) : null;

  // A full page on compact iOS: the bar names the section, as the page under
  // it is the member list; Message stays out and the rest go behind More. The
  // identity is the profile's own hero.
  const fullBar = useNativeBar(
    compact
      ? null
      : {
          title: t("agents.members"),
          titleAction: null,
          actions: full
            ? [
                [
                  ...(canMessage
                    ? [
                        {
                          id: "message",
                          label: t("agents.messageAgent", { name: agent.name }),
                          icon: iconMarkup(MessageIcon),
                          primary: true,
                        },
                      ]
                    : []),
                  ...(runningCount > 0
                    ? [{ id: "stop", label: t("agents.stop"), icon: iconMarkup(PowerIcon) }]
                    : []),
                  {
                    id: "delete",
                    label: t("agents.delete"),
                    icon: iconMarkup(DeleteIcon),
                    destructive: true,
                  },
                ],
              ]
            : [],
          overflow: { label: t("agents.more"), icon: iconMarkup(MoreIcon) },
          tabs: null,
          addTabs: null,
        },
    {
      action: ({ id }) => {
        if (id === "message") {
          onMessageAgent?.(agent.id);
        } else if (id === "stop" && !stopAgent.isPending) {
          stopAgent.mutate();
        } else if (id === "delete") {
          setDeleteOpen(true);
        }
      },
    },
  );

  const content = (
    <ProfileHeaderContent
      compact={compact}
      avatar={avatar}
      displayName={displayName}
      status={statusDot}
      description={agent.description}
      actions={actions}
    />
  );

  return (
    <>
      {compact ? (
        <SidePanelHeader
          onClose={onClose}
          nativeBar={{
            title: displayName,
            subtitle: presenceDisplay?.a11yLabel,
            titleAction: null,
            actions: [
              [
                ...(canMessage
                  ? [
                      {
                        id: "message",
                        label: t("agents.messageAgent", { name: agent.name }),
                        icon: iconMarkup(MessageIcon),
                      },
                    ]
                  : []),
                ...(runningCount > 0
                  ? [
                      {
                        id: "stop",
                        label: t("agents.hardStop", { count: runningCount }),
                        icon: iconMarkup(PowerIcon),
                      },
                    ]
                  : []),
              ],
            ].filter((group) => group.length > 0),
            tabs: null,
            addTabs: null,
          }}
          nativeHandlers={{
            action: ({ id }) => {
              if (id === "message") {
                onMessageAgent?.(agent.id);
              } else if (id === "stop" && !stopAgent.isPending) {
                stopAgent.mutate();
              }
            },
          }}
        >
          {content}
        </SidePanelHeader>
      ) : fullBar.active ? (
        fullBar.clearance
      ) : (
        <header className="agent-header-full">{content}</header>
      )}
      {full ? (
        <DeleteAgentDialog
          open={deleteOpen}
          onOpenChange={setDeleteOpen}
          agent={full}
          onDeleted={onDeleted}
        />
      ) : null}
    </>
  );
}
