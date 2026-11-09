import { AgentActivitySkeleton, SchedulesSkeleton, FileTreeSkeleton } from "./agent-loading";
import { ProfileSkeleton } from "@/features/profile/profile-skeleton";
import { sideAgentWorkspaceSource } from "../model/workspace-source";
import "./agent-page.css";
import { AgentPageLayout } from "./agent-page-layout";
import { visibleAgentSideSections, type AgentSideSection } from "../model/side-section-access";
import { useEffect, useState } from "react";
import { LockIcon } from "@/icons";
import type { AgentDto, AgentPublicDto, AgentRuntimeStateDto } from "@silverhand/shared";
import { cn } from "@/lib/cn";
import { SidePanelHeader } from "@/components/layout/side-panel-header";
import { AgentHeader } from "./agent-header";
import { AgentProfileCard } from "./agent-profile-card";
import { AgentSchedulesCard } from "./agent-schedules-card";
import { AgentWorkspaceContainer, AgentWorkspacePane } from "./agent-workspace-pane";
import { AgentMcpPane } from "./agent-mcp-pane";
import { AgentActivityPane } from "./agent-activity-pane";
import { AgentActivityTimeline, AgentStreamActivityTimeline } from "./agent-activity-timeline";
import { AgentChatsPane } from "./agent-chats-pane";
import { AGENT_PANE_INSET } from "../model/agent-pane-layout";
import { agentSections, AgentSectionTabs, type AgentSection } from "./agent-section-tabs";
import { useAgent, useAgentRuntimeState } from "../hooks/hooks";
import { isPrivilegedAgent } from "../model/agent-read";
import {
  canAuditAgentChats,
  canManageAgentSchedules,
  canViewAgentFiles,
} from "../model/permissions";
import { isBackendUnreachable, isUpdateRequired, useViewerUser } from "@/features/session/hooks";
import { t } from "@/lib/i18n";
import { isNotFound } from "@/lib/by-id-selection";
import { Button } from "@/components/ui/button";

const SECTION_STORAGE_KEYS = {
  full: "silverhand:layout.agent-section",
  side: "silverhand:layout.agent-side-section",
};
const SIDE_SECTION_STORAGE_KEY = SECTION_STORAGE_KEYS.side;
export type AgentViewMode = "full" | "side";

function readSection(mode: AgentViewMode): AgentSection {
  try {
    const raw = localStorage.getItem(SECTION_STORAGE_KEYS[mode]);
    if (agentSections().some((section) => section.id === raw)) {
      return raw as AgentSection;
    }
  } catch {
    // Storage can be unavailable in private mode.
  }
  return mode === "side" ? "activity" : "profile";
}

interface AgentViewProps {
  agentId: string;
  mode?: AgentViewMode;
  onDeleted: () => void;
  onOpenChat?: (target: { conversationId: string; workspaceId: string | null }) => void;
  onMessageAgent?: (agentId: string) => void;
  /** Docked activity follows this conversation; full mode provides its own picker. */
  contextConversationId?: string | null;
  /** Docked workspace memory follows this workspace, except for cross-conversation agents. */
  contextWorkspaceId?: string | null;
  onClose?: () => void;
}

export function AgentView({
  agentId,
  onDeleted,
  onOpenChat,
  onMessageAgent,
  mode = "full",
  contextConversationId = null,
  contextWorkspaceId = null,
  onClose,
}: AgentViewProps): JSX.Element {
  const agentQuery = useAgent(agentId);
  const publicView = agentQuery.data?.view === "public";
  const runtimeQuery = useAgentRuntimeState(agentId, {
    enabled: agentQuery.data?.view === "privileged",
  });
  const viewerQuery = useViewerUser();

  const [selection, setSelection] = useState(() => ({
    full: readSection("full"),
    side: readSection("side"),
  }));

  const section = selection[mode];
  const setSection = (next: AgentSection) => {
    setSelection((current) => ({ ...current, [mode]: next }));
  };

  const agent = agentQuery.data?.agent;
  const fullAgent = agentQuery.data?.view === "privileged" ? agentQuery.data.agent : undefined;
  const viewer =
    viewerQuery.data &&
    !isBackendUnreachable(viewerQuery.data) &&
    !isUpdateRequired(viewerQuery.data)
      ? viewerQuery.data
      : undefined;
  const accessKnown = Boolean(agent && viewer);
  const canManageSchedules = Boolean(
    fullAgent && viewer && canManageAgentSchedules(viewer, fullAgent),
  );
  const canViewFiles = Boolean(fullAgent && viewer && canViewAgentFiles(viewer, fullAgent));
  const canAuditChats = Boolean(fullAgent && viewer && canAuditAgentChats(viewer, fullAgent));
  // A public read is identity only: the profile is the one section it can fill.
  const visibleSections: AgentSection[] = publicView
    ? ["profile"]
    : mode === "side"
      ? visibleAgentSideSections(canViewFiles, canManageSchedules)
      : agentSections()
          .filter((item) => !item.ownerOnly || canAuditChats)
          .map((item) => item.id);
  const sections = visibleSections.map((id) => agentSections().find((item) => item.id === id)!);
  const effectiveSection = visibleSections.some((id) => id === section)
    ? section
    : publicView
      ? "profile"
      : mode === "side"
        ? "activity"
        : "profile";

  useEffect(() => {
    try {
      localStorage.setItem(SECTION_STORAGE_KEYS[mode], section);
    } catch {
      // Selection remains usable when storage is unavailable.
    }
  }, [mode, section]);

  useEffect(() => {
    if (mode !== "side") {
      return;
    }

    const onFocus = (event: Event) => {
      const detail = (event as CustomEvent<AgentSideSectionFocusDetail>).detail;
      if (detail?.agentId === agentId) {
        setSelection((current) => ({ ...current, side: detail.section }));
      }
    };
    window.addEventListener(AGENT_SIDE_SECTION_FOCUS_EVENT, onFocus);
    return () => window.removeEventListener(AGENT_SIDE_SECTION_FOCUS_EVENT, onFocus);
  }, [agentId, mode]);

  const ready = !agentQuery.isLoading && !agentQuery.isError && agent;

  return (
    <AgentPageLayout
      host={mode === "side" ? "side" : "agent"}
      header={
        ready ? (
          <AgentHeader
            mode={mode}
            agent={agent}
            runtimeState={runtimeQuery.data}
            onDeleted={onDeleted}
            onMessageAgent={onMessageAgent}
            onClose={onClose}
          />
        ) : mode === "side" ? (
          <SidePanelHeader
            onClose={onClose}
            nativeBar={{
              title: "",
              subtitle: undefined,
              titleAction: null,
              actions: [],
              tabs: null,
              addTabs: null,
            }}
          >
            {null}
          </SidePanelHeader>
        ) : null
      }
      navigation={
        ready && !publicView ? (
          <AgentSectionTabs sections={sections} active={effectiveSection} onChange={setSection} />
        ) : null
      }
    >
      {ready ? (
        <AgentSectionContent
          mode={mode}
          section={effectiveSection}
          agent={agent}
          runtimeState={runtimeQuery.data}
          accessKnown={accessKnown}
          canViewFiles={canViewFiles}
          canManageSchedules={canManageSchedules}
          contextConversationId={contextConversationId}
          contextWorkspaceId={contextWorkspaceId}
          onOpenChat={onOpenChat}
        />
      ) : agentQuery.isLoading ? (
        <ProfileSkeleton agent />
      ) : agentQuery.isError && !isNotFound(agentQuery.error) ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-3 text-sm text-text-muted">
          <span>{t("agents.couldNotLoadAgent")}</span>
          <Button variant="neutral" size="sm" onClick={() => void agentQuery.refetch()}>
            {t("common.retry")}
          </Button>
        </div>
      ) : (
        <div className="flex flex-1 items-center justify-center text-sm text-text-muted">
          {t("agents.agentNotFound")}
        </div>
      )}
    </AgentPageLayout>
  );
}

function AgentSectionContent({
  mode,
  section,
  agent,
  runtimeState,
  accessKnown,
  canViewFiles,
  canManageSchedules,
  contextConversationId,
  contextWorkspaceId,
  onOpenChat,
}: {
  mode: AgentViewMode;
  section: AgentSection;
  agent: AgentDto | AgentPublicDto;
  runtimeState: AgentRuntimeStateDto | undefined;
  accessKnown: boolean;
  canViewFiles: boolean;
  canManageSchedules: boolean;
  contextConversationId: string | null;
  contextWorkspaceId: string | null;
  onOpenChat: AgentViewProps["onOpenChat"];
}): JSX.Element {
  const inset = mode === "side" ? "p-4" : AGENT_PANE_INSET;

  if (!isPrivilegedAgent(agent)) {
    return (
      <div className="min-w-0 flex-1 overflow-y-auto [scrollbar-gutter:stable]">
        <div className={inset}>
          <AgentProfileCard agent={agent} />
        </div>
      </div>
    );
  }

  switch (section) {
    case "activity":
      return mode === "full" ? (
        <AgentActivityPane
          sessionScope={agent.session_scope}
          agentId={agent.id}
          state={runtimeState}
        />
      ) : (
        <AgentContextActivity
          agent={agent}
          runtimeState={runtimeState}
          conversationId={contextConversationId}
        />
      );
    case "workspace": {
      const source = sideAgentWorkspaceSource(agent.session_scope, contextWorkspaceId);
      const sourceKey = source.kind === "home" ? "home" : source.workspaceId;

      return (
        <div className={cn("flex min-h-0 flex-1 flex-col", mode === "full" && inset)}>
          {!canViewFiles ? (
            <AgentWorkspaceFilesPermissionState loading={!accessKnown} />
          ) : mode === "full" ? (
            <AgentWorkspacePane agentId={agent.id} />
          ) : (
            <AgentWorkspaceContainer
              key={sourceKey}
              agentId={agent.id}
              source={source}
              layout="compact"
            />
          )}
        </div>
      );
    }
    case "chats":
      return <AgentChatsPane agentId={agent.id} onOpenChat={onOpenChat} />;
    case "profile":
    case "mcp":
    case "schedules":
      return (
        <div
          className={cn(
            "min-w-0 flex-1 overflow-y-auto",
            section === "profile" && "[scrollbar-gutter:stable]",
          )}
        >
          <div className={inset}>
            {section === "profile" ? (
              <AgentProfileCard agent={agent} />
            ) : section === "mcp" ? (
              <AgentMcpPane agentId={agent.id} runtime={agent.runtime} />
            ) : canManageSchedules ? (
              <AgentSchedulesCard agentId={agent.id} />
            ) : (
              <AgentSchedulesPermissionState loading={!accessKnown} />
            )}
          </div>
        </div>
      );
  }
}

function AgentContextActivity({
  agent,
  runtimeState,
  conversationId,
}: {
  agent: AgentDto;
  runtimeState: AgentRuntimeStateDto | undefined;
  conversationId: string | null;
}): JSX.Element {
  if (agent.session_scope === "agent") {
    return <AgentStreamActivityTimeline key={agent.id} agentId={agent.id} />;
  }
  if (!conversationId) {
    return (
      <div className="flex flex-1 items-center justify-center px-6 text-center text-xs text-text-muted">
        {t("agents.openAChannelTo")}
      </div>
    );
  }
  if (!runtimeState) {
    return <AgentActivitySkeleton />;
  }
  // The activity endpoint requires participation; absence is not a network error.
  if (!runtimeState.participations.some((item) => item.conversation_id === conversationId)) {
    return (
      <div className="flex flex-1 items-center justify-center px-6 text-center text-xs text-text-muted">
        {t("agents.notInChannel", { name: agent.display_name || agent.name })}
      </div>
    );
  }
  return (
    <AgentActivityTimeline
      key={conversationId}
      conversationId={conversationId}
      agentId={agent.id}
    />
  );
}

/** Event that redirects an ALREADY-mounted docked agent panel to a section.
 *  The fresh-open / different-agent cases remount and read the section from
 *  localStorage on mount; this event only covers "the panel already sits on
 *  this agent but another tab". Carries `agentId` so a panel showing a
 *  different agent ignores it. */
const AGENT_SIDE_SECTION_FOCUS_EVENT = "silverhand:agent-side-section-focus";

interface AgentSideSectionFocusDetail {
  agentId: string;
  section: AgentSideSection;
}

/**
 * Open-intent for the docked agent panel's section. Persists the choice as the
 * SoT — so the next reopen honors it too, keeping the sticky section in sync
 * with an explicit jump (cross-panel contract, #task:1539) — AND signals any
 * already-mounted panel for `agentId` to switch reactively. Call it right
 * before `openAgentPanel(agentId)` when a caller must land on a specific tab
 * (e.g. the Activity bubble → Activity), regardless of the last-used tab.
 */
export function focusAgentSideSection(agentId: string, section: AgentSideSection): void {
  try {
    localStorage.setItem(SIDE_SECTION_STORAGE_KEY, section);
  } catch {
    /* private mode / quota — the reactive event still fires */
  }
  window.dispatchEvent(
    new CustomEvent<AgentSideSectionFocusDetail>(AGENT_SIDE_SECTION_FOCUS_EVENT, {
      detail: { agentId, section },
    }),
  );
}

function AgentSchedulesPermissionState({ loading }: { loading: boolean }): JSX.Element {
  if (loading) {
    return <SchedulesSkeleton />;
  }

  return (
    <div
      data-panel-card="placeholder"
      className="flex flex-col items-center gap-2 border border-border-subtle bg-bg-floating/50 px-4 py-10 text-center"
    >
      <LockIcon className="size-6 text-text-muted" />
      <p className="max-w-md text-sm text-text-primary">{t("agents.schedulesArePrivateTo")}</p>
    </div>
  );
}

function AgentWorkspaceFilesPermissionState({ loading }: { loading: boolean }): JSX.Element {
  if (loading) {
    return <FileTreeSkeleton />;
  }

  return (
    <div
      data-panel-card="placeholder"
      className="flex flex-col items-center gap-2 border border-border-subtle bg-bg-floating/50 px-4 py-10 text-center"
    >
      <LockIcon className="size-6 text-text-muted" />
      <p className="max-w-md text-sm text-text-primary">{t("agents.agentWorkspaceFilesAre")}</p>
    </div>
  );
}
