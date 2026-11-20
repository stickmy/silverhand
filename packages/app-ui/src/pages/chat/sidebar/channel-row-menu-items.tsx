import { ArchiveIcon, CopyIcon, StarIcon, UnarchiveIcon } from "@/icons";
import type { ChannelAttentionModeRowDto } from "@silverhand/shared";
import {
  AppContextMenuItem,
  AppContextMenuSeparator,
  AppContextMenuSub,
} from "@/components/menus/app-context-menu";
import {
  attentionIcon,
  ChannelAttentionMenuItems,
} from "../../../features/conversation-shell/components/channel-attention-surfaces";
import { copyConversationName } from "../../../components/layout/sidebar-shared";
import { t } from "@/lib/i18n";

/** Sidebar placement changes star state, never channel management capabilities. */
export function ChannelRowMenuItems({
  channelId,
  title,
  rosterPolicy,
  isMember,
  isArchived,
  starred,
  attention,
  workspaceMuted,
  onStar,
  onArchive,
  archivePending,
}: {
  channelId: string;
  title: string;
  rosterPolicy: "open" | "fixed";
  isMember: boolean;
  isArchived: boolean;
  starred: boolean;
  attention?: ChannelAttentionModeRowDto;
  workspaceMuted: boolean | undefined;
  onStar: () => void;
  onArchive: () => void;
  archivePending: boolean;
}): JSX.Element {
  const AttentionGlyph = attentionIcon(attention?.mode);

  return (
    <>
      <AppContextMenuItem
        icon={<StarIcon className={"size-3.5" + (starred ? " fill-current" : "")} />}
        onSelect={onStar}
      >
        {starred ? t("chat.unstar") : t("chat.star")}
      </AppContextMenuItem>
      {isMember && rosterPolicy === "open" ? (
        <AppContextMenuSub
          icon={<AttentionGlyph className="size-3.5" />}
          label={t("chat.notifications")}
        >
          <ChannelAttentionMenuItems
            channelId={channelId}
            mode={attention?.mode}
            workspaceMuted={workspaceMuted}
          />
        </AppContextMenuSub>
      ) : null}
      {isMember ? (
        <AppContextMenuItem
          icon={
            isArchived ? (
              <UnarchiveIcon className="size-3.5" />
            ) : (
              <ArchiveIcon className="size-3.5" />
            )
          }
          disabled={archivePending}
          onSelect={onArchive}
        >
          {isArchived ? t("chat.restore") : t("chat.archive")}
        </AppContextMenuItem>
      ) : null}
      <AppContextMenuSeparator />
      <AppContextMenuItem
        icon={<CopyIcon className="size-3.5" />}
        onSelect={() => void copyConversationName(title)}
      >
        {t("chat.copyName")}
      </AppContextMenuItem>
    </>
  );
}
