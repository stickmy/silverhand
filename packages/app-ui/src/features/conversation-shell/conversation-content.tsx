import type { Panes } from "@/navigation/panes";
import type { PaneContext } from "./build-conversation-panes";
import { buildPrimaryPane } from "./build-conversation-panes";
import type { PrimaryPaneModel } from "./pane-render-models";

/** What the stage renders: the route's conversation and the panes beside it,
 *  each resolved where it mounts. */
export interface ConversationContent {
  primary: PrimaryPaneModel;
  panes: Panes;
  ctx: PaneContext;
  /** Reserve the route view's tab strip while the section mounts. */
  loadingTabs: boolean;
}

export function conversationContent(ctx: PaneContext): ConversationContent {
  const primary = buildPrimaryPane(ctx);
  return {
    primary,
    panes: ctx.route.panes,
    ctx,
    loadingTabs: primary.kind === "conversation" && primary.props.viewTabsEnabled === true,
  };
}
