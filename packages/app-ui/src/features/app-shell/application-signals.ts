import { deriveNavRailDots } from "@/components/layout/nav-rail";
import { useBackendInfoSnapshot } from "@/features/computers/hooks/hooks";
import { useTaskBoardRealtime } from "@/features/tasks/hooks/use-board";
import {
  useAppShellGlobalSync,
  useComposerDraftCleanup,
  useDockBadge,
  useSettingsMenuNav,
} from "@/features/app-shell/effects";
import { useNavSectionShortcuts } from "@/features/app-shell/keyboard-shortcuts";
import { useUnreadCounts } from "@/features/attention/notifications";
import { useMessageUnreadSummary } from "@/features/channels/hooks";
import { useActivity, useAttentionBadges, useAttentionRealtime } from "@/features/attention/hooks";
import { useNav } from "@/router";

export function useApplicationSignals() {
  const nav = useNav();
  useAppShellGlobalSync();

  useComposerDraftCleanup();

  useSettingsMenuNav(nav.toSettings);

  useNavSectionShortcuts(nav);

  const { data: unreadCounts } = useUnreadCounts();

  useMessageUnreadSummary();

  useAttentionRealtime();

  const { data: attentionBadges } = useAttentionBadges();

  useTaskBoardRealtime();

  const { data: activityData } = useActivity();

  const navRailDots = deriveNavRailDots({
    workspaceUnread: attentionBadges?.by_workspace,
    activityItemCount: activityData?.items.length,
  });

  useDockBadge(attentionBadges?.total);

  useBackendInfoSnapshot({ pollProtocolCompatibility: true });

  return { unreadCounts, attentionBadges, navRailDots };
}
