import { useProfileHeroSize } from "@/features/profile/use-profile-hero-size";
import { ProfileMessageActivity } from "@/features/profile/profile-message-activity";
import { useState, type JSX, type ReactNode } from "react";
import { ClockIcon, ComputerIcon, LinkIcon, ProfileCardIcon, UnlinkIcon } from "@/icons";
import "./agent-profile-hero.css";
import dayjs from "dayjs";
import type { AgentDto } from "@silverhand/shared";
import { AgentAvatar } from "@/components/agent-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { cn } from "@/lib/cn";
import {
  ProfileFact,
  ProfileFactBand,
  ProfileSection,
} from "@/features/profile/profile-summary-sections";
import { useUpdateAgent } from "../hooks/hooks";
import { useAgentAvatarObjectUrl } from "../hooks/avatar-hooks";
import { canEditAgentProfile } from "../model/permissions";
import { EditableAgentAvatar } from "./editable-agent-avatar";
import { EditPencilButton } from "@/components/common/edit-pencil-button";
import { InlineText } from "@/components/common/inline-text";
import { rejectDisplayName } from "@/features/identity/display-name";
import { InlineTextarea } from "./inline-textarea";
import { RuntimeConfigEditor } from "./runtime-config-editor";
import { isBackendUnreachable, isUpdateRequired, useViewerUser } from "@/features/session/hooks";
import { unknownComputerLabel } from "@/features/computers/model/host-display";
import { t } from "@/lib/i18n";
import { useNav } from "@/router";

interface AgentProfileCardProps {
  agent: AgentDto;
}

export function AgentProfileCard({ agent }: AgentProfileCardProps): JSX.Element {
  const { data: viewer } = useViewerUser();
  // D4.a Commit 5d-fixup (Cindy P1-2): the BackendUnreachable arm never
  // reaches the registered shell — but if it ever does, map to `undefined`
  // (deny edit, mirror loading).
  const viewerForPermissions =
    viewer && (isBackendUnreachable(viewer) || isUpdateRequired(viewer)) ? undefined : viewer;
  const canEdit = canEditAgentProfile(viewerForPermissions, agent);
  const update = useUpdateAgent(agent.id);
  const avatarObjectUrl = useAgentAvatarObjectUrl(agent.avatar_url);
  // Server-resolved label, not a local owned-hosts lookup: `GET /api/hosts` is
  // owner-scoped, so a non-owner viewer's lookup would miss and collapse to
  // "Unknown computer". `host_label` is null only when the host is truly gone.
  const computerName = agent.host_label ?? unknownComputerLabel();
  const nav = useNav();
  const { profileRef, compactHero } = useProfileHeroSize();
  const avatarSize = compactHero ? AVATAR_PX.profileCompact : AVATAR_PX.profile;

  const [nameEditing, setNameEditing] = useState(false);
  const [descriptionEditing, setDescriptionEditing] = useState(false);
  const [runtimeEditing, setRuntimeEditing] = useState(false);

  return (
    <div ref={profileRef} data-profile-layout className="mx-auto flex flex-col gap-6">
      {!canEdit ? <ReadOnlyNotice /> : null}

      <section
        aria-label={t("agents.agentProfile")}
        data-compact={compactHero}
        className="agent-profile-hero"
      >
        {canEdit ? (
          <EditableAgentAvatar
            agentId={agent.id}
            size={avatarSize}
            runtime={agent.runtime}
            alt={agent.display_name || agent.name}
            avatarObjectUrl={avatarObjectUrl}
            hasAvatar={agent.avatar_url != null}
          />
        ) : (
          <AgentAvatar
            runtime={agent.runtime}
            size={avatarSize}
            avatarObjectUrl={avatarObjectUrl}
            alt={agent.display_name || agent.name}
          />
        )}
        <div className="agent-profile-hero-fields min-w-0 flex-1">
          <div>
            <div className="flex items-start gap-1">
              <div className={cn("min-w-0", nameEditing && "flex-1")}>
                {canEdit ? (
                  <InlineText
                    value={agent.display_name}
                    ariaLabel={t("agents.displayName")}
                    editing={nameEditing}
                    onExitEdit={() => setNameEditing(false)}
                    validate={rejectDisplayName}
                    className="agent-profile-hero-name font-serif font-semibold"
                    onCommit={async (next) => {
                      await update.mutateAsync({ display_name: next });
                    }}
                  />
                ) : (
                  <div className="wrap-break-word agent-profile-hero-name font-serif font-semibold text-text-primary">
                    {agent.display_name || agent.name}
                  </div>
                )}
              </div>
              {canEdit ? (
                <EditPencilButton
                  onClick={() => setNameEditing(true)}
                  ariaLabel={t("agents.editDisplayName")}
                  hidden={nameEditing}
                />
              ) : null}
            </div>
            <div
              className="mt-1 truncate font-mono text-xs text-text-muted"
              title={`@${agent.name}`}
            >
              @{agent.name}
            </div>
          </div>
          <div className={cn("flex gap-1", descriptionEditing ? "items-start" : "items-center")}>
            <div
              className={cn(
                "agent-profile-hero-description min-w-0 wrap-break-word",
                descriptionEditing && "flex-1",
              )}
            >
              {canEdit ? (
                <InlineTextarea
                  value={agent.description}
                  placeholder={t("agents.addDescription")}
                  ariaLabel={t("agents.description")}
                  editing={descriptionEditing}
                  onExitEdit={() => setDescriptionEditing(false)}
                  onCommit={async (next) => {
                    await update.mutateAsync({ description: next });
                  }}
                />
              ) : (
                <ReadOnlyValue
                  value={agent.description}
                  placeholder={t("agents.noDescription")}
                  multiline
                />
              )}
            </div>
            {canEdit ? (
              <EditPencilButton
                onClick={() => setDescriptionEditing(true)}
                ariaLabel={t("agents.editDescription")}
                hidden={descriptionEditing}
              />
            ) : null}
          </div>
        </div>
      </section>

      <ProfileFactBand>
        <ProfileFact icon={<ComputerIcon className="size-3.5" />} label={t("agents.computer")}>
          {agent.host_label ? (
            <button
              type="button"
              onClick={() => nav.toComputer(agent.host_id)}
              className="font-medium text-text-primary underline-offset-2 hover:underline focus-visible:underline focus-visible:outline-none"
            >
              {computerName}
            </button>
          ) : (
            <span className="font-medium text-text-primary">{computerName}</span>
          )}
        </ProfileFact>
        <ProfileFact
          icon={
            agent.session_scope === "agent" ? (
              <LinkIcon className="size-3.5" />
            ) : (
              <UnlinkIcon className="size-3.5" />
            )
          }
          label={t("agents.session")}
        >
          <span className="font-medium text-text-primary">
            {agent.session_scope === "agent"
              ? t("agents.acrossConversations")
              : t("agents.perConversation")}
          </span>
        </ProfileFact>
        <ProfileFact icon={<ClockIcon className="size-3.5" />} label={t("agents.created")}>
          <span
            className="font-medium text-text-primary"
            title={dayjs(agent.created_at).format("YYYY-MM-DD HH:mm:ss")}
          >
            {dayjs(agent.created_at).format("YYYY-MM-DD")}
          </span>
        </ProfileFact>
        <ProfileFact icon={<ProfileCardIcon className="size-3.5" />} label="ID">
          <span className="font-mono">{agent.id}</span>
        </ProfileFact>
      </ProfileFactBand>

      <ProfileMessageActivity subject={{ kind: "agent", id: agent.id }} />

      <ProfileSection
        title={t("agents.runtimeConfiguration")}
        titleAdornment={
          canEdit ? (
            <EditPencilButton
              onClick={() => setRuntimeEditing(true)}
              ariaLabel={t("agents.editRuntimeConfiguration")}
              hidden={runtimeEditing}
            />
          ) : null
        }
      >
        <RuntimeConfigEditor
          agent={agent}
          editing={runtimeEditing}
          onExitEdit={() => setRuntimeEditing(false)}
        />
      </ProfileSection>

      {update.isError && canEdit ? (
        <div className="text-xs text-error">{String(update.error)}</div>
      ) : null}
    </div>
  );
}

function ReadOnlyNotice(): JSX.Element {
  return (
    <p role="note" className="text-xs text-text-muted" data-testid="agent-profile-readonly-notice">
      {t("agents.onlyTheCreatorCan")}
    </p>
  );
}

function ReadOnlyValue({
  value,
  placeholder,
  multiline = false,
}: {
  value: string | null | undefined;
  placeholder?: string;
  multiline?: boolean;
}): JSX.Element {
  const display = value && value.length > 0 ? value : null;
  return (
    <div
      className={cn(
        "text-sm",
        display ? "text-text-primary" : "text-text-muted",
        multiline ? "whitespace-pre-wrap leading-relaxed" : "",
      )}
    >
      {display ?? placeholder ?? "—"}
    </div>
  );
}
