import { modalScope } from "@/lib/modal-scope";
import { useCallback, useEffect } from "react";
import { getPlatform } from "@/platform";
import { useCompactLayout } from "./layout";
import { showsTabBar } from "./presentation";
import type { NavigationStore } from "./store";
import { MAIN_TABS, type MainTab } from "./history";

/**
 * Native presentation follows history; it never holds navigation state of its
 * own. Each change sends the entry on screen, and native draws the tab bar for
 * it. The only intent native sends is a tab selection, which runs here like
 * any other navigation.
 */
export function useNativeNavigation(store: NavigationStore, allowed: () => boolean, dark: boolean) {
  const native = getPlatform().navigation;
  const compact = useCompactLayout();
  const run = useCallback(
    (commit: () => void, nativeIntent = false) => {
      if (nativeIntent && modalScope.blocked) return false;
      if (!store.active || !allowed()) return false;
      const before = store.getSnapshot();
      commit();
      return store.getSnapshot() !== before;
    },
    [store, allowed],
  );

  useEffect(() => {
    if (!native) return;
    let alive = true;
    const sync = async () => {
      const { location, tab } = store.getSnapshot();
      const result = await native
        .sync({
          session: store.session,
          key: location.key,
          tab,
          blocked: modalScope.blocked,
          modalVersion: modalScope.version,
          visible: compact && showsTabBar(location.pathname, location.search),
          dark,
        })
        .catch(() => null);
      if (alive && result)
        document.documentElement.style.setProperty(
          "--native-tab-bar-height",
          `${result.tabBarHeight}px`,
        );
    };
    void sync();
    const offModal = modalScope.subscribe(() => {
      void sync();
    });
    const off = store.subscribe(() => {
      void sync();
    });
    const listening = native.subscribe((intent) => {
      if (!alive || intent.session !== store.session) return;
      const accepted =
        intent.modalVersion === modalScope.version &&
        intent.kind === "tab" &&
        MAIN_TABS.includes(intent.target as MainTab) &&
        run(() => store.selectTab(intent.target as MainTab), true);
      // Native has already moved its selection; a refused intent changes no
      // history, so nothing else would send the tab back.
      if (!accepted) {
        void sync();
      }
    });
    return () => {
      alive = false;
      off();
      offModal();
      void listening.then((remove) => remove());
      void native.reset(store.session);
      document.documentElement.style.removeProperty("--native-tab-bar-height");
    };
  }, [store, native, run, compact, dark]);
  return run;
}
