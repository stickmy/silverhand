import { useCallback, useMemo, useSyncExternalStore } from "react";
import {
  addTab,
  removeTab,
  reorderTabs,
  tabKey,
  type Tab,
} from "@/features/conversation-shell/model/conversation-tabs";
import {
  getConversationTabs,
  getConversationActiveTab,
  getConversationTabsVersion,
  recordConversationTabs,
  recordConversationActiveTab,
  subscribeConversationTabs,
} from "@/features/app-shell/place-memory";

export interface ConversationTabsController {
  /** Shared functional tabs combined with this conversation's file tabs. */
  tabs: Tab[];
  /** The active tab's key, resolved to a present tab. */
  activeKey: string;
  /** Make a present tab active. A key naming no current tab is ignored by
   *  {@link resolveActiveTabKey} at read time. */
  setActive: (key: string) => void;
  /** Open a custom tab, or focus it if already open (#task:2122). Always makes
   *  the tab active — an already-open attachment re-clicked just refocuses. */
  openTab: (tab: Tab) => void;
  /** Close a panel globally or a file in this conversation. Builtins stay present. */
  closeTab: (key: string) => void;
  /** Move the tab at `fromIndex` to `toIndex` — drag-to-reorder over the WHOLE
   *  list, builtins included. */
  reorder: (fromIndex: number, toIndex: number) => void;
}

/** Open (or focus) a tab in a conversation from outside its pane. The store is
 *  module-level, so a sibling pane writes here directly instead of relaying. */
export function openConversationTab(conversationId: string, tab: Tab): void {
  recordConversationTabs(conversationId, addTab(getConversationTabs(conversationId), tab));
  recordConversationActiveTab(conversationId, tabKey(tab));
}

/** Shared preferences carry no channel data; each consumer still reads its own target. */
export function useConversationTabs(conversationId: string): ConversationTabsController {
  const version = useSyncExternalStore(subscribeConversationTabs, () =>
    getConversationTabsVersion(conversationId),
  );
  const tabs = useMemo(() => getConversationTabs(conversationId), [conversationId, version]);
  const setActive = useCallback(
    (key: string) => recordConversationActiveTab(conversationId, key),
    [conversationId],
  );
  const openTab = useCallback(
    (tab: Tab) => openConversationTab(conversationId, tab),
    [conversationId],
  );
  const closeTab = useCallback(
    (key: string) => {
      recordConversationTabs(conversationId, removeTab(getConversationTabs(conversationId), key));
    },
    [conversationId],
  );
  const reorder = useCallback(
    (fromIndex: number, toIndex: number) => {
      recordConversationTabs(
        conversationId,
        reorderTabs(getConversationTabs(conversationId), fromIndex, toIndex),
      );
    },
    [conversationId],
  );
  return {
    tabs,
    activeKey: getConversationActiveTab(conversationId),
    setActive,
    openTab,
    closeTab,
    reorder,
  };
}
