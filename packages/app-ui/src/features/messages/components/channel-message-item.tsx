import { memo } from "react";
import { MessageItem, type MessageItemProps } from "./message-item";
import { useThreadFollowers, useUnfollowThread } from "@/features/channels/hooks";

// Memoized: the channel list renders every loaded row, so a list-level state
// change must not re-render rows whose props did not change.
export const ChannelMessageItem = memo(function ChannelMessageItem(
  props: MessageItemProps,
): JSX.Element {
  const canParticipate = props.canParticipate ?? true;
  const isThreadRoot = (props.threadReplyCount ?? 0) > 0;
  const rootId = isThreadRoot && canParticipate ? props.message.id : null;
  const followers = useThreadFollowers(props.conversationId, rootId);
  const unfollow = useUnfollowThread(props.conversationId, rootId);
  const onUnfollowThread =
    isThreadRoot && followers.data?.viewer_following && !unfollow.isPending
      ? () => unfollow.mutate()
      : undefined;
  return <MessageItem {...props} onUnfollowThread={onUnfollowThread} />;
});
