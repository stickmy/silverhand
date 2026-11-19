import type { ConversationPanesModel } from "./pane-render-models";
import { PrimaryPaneContent } from "./primary-pane-content";
import { SecondaryPaneContent } from "./secondary-pane-content";
import { ChannelView } from "@/features/conversation-shell/components/channel-view";
import { SideUnresolvedPlaceholder } from "@/features/conversation-shell/components/side-unresolved-placeholder";

export function renderConversationPanes(model: ConversationPanesModel) {
  return {
    // Reserve the primary host's tab strip during deferred mounting. Secondary
    // panes do not own this strip; unresolved primary descriptors use host intent.
    loadingTabs:
      model.primary.kind === "conversation" && model.primary.props.viewTabsEnabled === true,
    primary: <PrimaryPaneContent model={model.primary} />,
    split:
      model.split === null ? null : model.split.kind === "conversation" ? (
        <ChannelView {...model.split.props} />
      ) : (
        <SideUnresolvedPlaceholder {...model.split.props} />
      ),
    secondary:
      model.secondary.kind === "none" ? null : <SecondaryPaneContent model={model.secondary} />,
  };
}

export type ConversationContent = ReturnType<typeof renderConversationPanes>;
