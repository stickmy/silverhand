import { createElement, useMemo } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { PageBarItem } from "@silverhand/client";
import type { IconComponent } from "@/icons";
import { useCompactLayout } from "@/navigation/layout";
import { usePageLive } from "@/navigation/page-stack";
import {
  usePageBarClaim,
  type PageBarClaim,
  type PageBarDeclaration,
  type PageBarHandlers,
} from "@/navigation/page-bar";
import { getPlatform } from "@/platform";
import { useHeaderSectionVisible } from "./mobile-header-slot";

export type { PageBarClaim, PageBarDeclaration, PageBarHandlers };

/** A bar button and what it does. */
export type NativeBarAction = PageBarItem & { run: () => void };

/**
 * Hands the page header to the native bar (compact iOS). The page keeps state
 * and behaviour: taps come back to `handlers`, and Back runs the shared
 * navigation. While `active`, the page renders `clearance` in place of its own
 * header; its extent follows from the declaration. `"none"` shows no bar.
 *
 * A page that declares nothing gets the shell's bar and clears it in CSS. A
 * page kept for Back keeps its layout; only the page on screen claims the bar.
 */
export function useNativeBar(
  bar: PageBarClaim | null,
  handlers: PageBarHandlers = {},
): { active: boolean; clearance: JSX.Element | null } {
  const platform = useNativeBarPlatform();
  const visible = useHeaderSectionVisible();
  const live = usePageLive();
  const declared = platform && visible ? bar : null;
  usePageBarClaim(live ? declared : null, handlers);

  const extent =
    declared === null ? null : declared === "none" ? "none" : declared.tabs ? "tabs" : "";
  const clearance = useMemo(
    () =>
      extent === null
        ? null
        : createElement("div", { "aria-hidden": true, "data-native-bar-clearance": extent }),
    [extent],
  );
  return { active: declared !== null, clearance };
}

/** Whether compact pages draw their headers in a native bar. */
export function useNativeBarPlatform(): boolean {
  return useCompactLayout() && getPlatform().pageBar !== undefined;
}

/** A bar of `actions`, each running its own handler. */
export function nativeActionBar(
  bar: Omit<PageBarDeclaration, "actions">,
  actions: readonly (NativeBarAction | null)[],
): [PageBarDeclaration, PageBarHandlers] {
  return nativeActionGroups(bar, [actions]);
}

/** A bar of action groups, each drawn as its own capsule. */
export function nativeActionGroups(
  bar: Omit<PageBarDeclaration, "actions">,
  groups: readonly (readonly (NativeBarAction | null)[])[],
): [PageBarDeclaration, PageBarHandlers] {
  const shown = groups
    .map((group) => group.filter((action) => action !== null))
    .filter((group) => group.length > 0);
  const all = shown.flat();
  return [
    { ...bar, actions: shown.map((group) => group.map(({ run: _, ...item }) => item)) },
    { action: ({ id }) => all.find((action) => action.id === id)?.run() },
  ];
}

const glyphs = new Map<IconComponent, string>();

/** An icon as SVG markup, for a native bar to draw. */
export function iconMarkup(icon: IconComponent): string {
  let markup = glyphs.get(icon);
  if (markup === undefined) {
    markup = renderToStaticMarkup(createElement(icon, { size: 24 }));
    glyphs.set(icon, markup);
  }
  return markup;
}
