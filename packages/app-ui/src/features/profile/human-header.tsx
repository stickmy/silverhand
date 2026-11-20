import { MessageIcon } from "@/icons";
import { ProfileHeaderAction } from "./profile-header-action";
import type { UserDto } from "@silverhand/shared";
import { UserAvatar } from "@/components/user-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { SidePanelHeader } from "@/components/layout/side-panel-header";
import { iconMarkup, nativeActionBar, useNativeBar } from "@/components/layout/native-page-bar";
import { ProfileHeaderContent } from "@/features/profile/profile-header-content";
import { usePresenceOf, presenceDotStatus, humanPresenceText } from "@/features/users/presence";
import { useHumanAvatar } from "./use-human-avatar";
import { t } from "@/lib/i18n";

export function HumanHeader({
  user,
  compact = false,
  onClose,
  onMessage,
}: {
  user: UserDto;
  compact?: boolean;
  onClose?: () => void;
  onMessage?: () => void;
}): JSX.Element | null {
  const avatarObjectUrl = useHumanAvatar(user.id);
  const presence = usePresenceOf(user.id);
  const online = presenceDotStatus(presence?.presence) === "active";
  // A full page on compact iOS: the bar names the section and carries Message;
  // the identity is the profile's own hero.
  const [memberBar, memberHandlers] = nativeActionBar(
    { title: t("profile.members"), titleAction: null, tabs: null, addTabs: null },
    [
      onMessage
        ? {
            id: "message",
            label: t("agents.messageAgent", { name: user.name }),
            icon: iconMarkup(MessageIcon),
            run: onMessage,
          }
        : null,
    ],
  );
  const fullBar = useNativeBar(compact ? null : memberBar, memberHandlers);
  const content = (
    <ProfileHeaderContent
      compact={compact}
      avatar={
        <UserAvatar
          size={compact ? AVATAR_PX.md : AVATAR_PX.xl}
          avatarObjectUrl={avatarObjectUrl}
          alt={user.display_name || user.name}
        />
      }
      displayName={user.display_name || user.name}
      actions={
        onMessage ? (
          <ProfileHeaderAction
            compact={compact}
            label={t("profile.message")}
            title={t("agents.messageAgent", { name: user.name })}
            onClick={onMessage}
          >
            <MessageIcon className="size-3.5" />
          </ProfileHeaderAction>
        ) : undefined
      }
      status={
        <span
          data-status-pip
          className={`size-1.5 shrink-0 self-center rounded-full ${online ? "bg-success" : "bg-text-muted/40"}`}
          title={humanPresenceText(presence?.presence, null, "")}
        />
      }
    />
  );

  return compact ? (
    <SidePanelHeader
      onClose={onClose}
      nativeBar={{
        title: user.display_name || user.name,
        subtitle: humanPresenceText(presence?.presence, null, "") || undefined,
        titleAction: null,
        actions: onMessage
          ? [
              [
                {
                  id: "message",
                  label: t("agents.messageAgent", { name: user.name }),
                  icon: iconMarkup(MessageIcon),
                },
              ],
            ]
          : [],
        tabs: null,
        addTabs: null,
      }}
      nativeHandlers={{
        action: ({ id }) => {
          if (id === "message") {
            onMessage?.();
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
  );
}
