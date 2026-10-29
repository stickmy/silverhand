import type { BannerNotice } from "@silverhand/client";
import { parseLocator, parseReference, type MessageAuthorDto } from "@silverhand/shared";
import { BotIcon, UserIcon } from "@/icons";
import { agentAvatarImage } from "@/components/agent-avatar";
import { iconMarkup } from "@/components/layout/native-page-bar";
import { getPlatform } from "@/platform";
import { loadAvatarBlobUrl } from "@/features/identity/avatar-blob-cache";
import { presentAuthor } from "@/features/messages/model/author-presentation";

/** A mention, read off a notification's payload and the sender it resolves to. */
export interface MentionBannerContent {
  /** Absent from servers that predate the field; the name then comes from `fallbackName`. */
  sender: MessageAuthorDto | null;
  fallbackName: string;
  conversationTitle: string | null;
  excerpt: string | null;
}

/** Shows who mentioned the viewer, where, and what they said, in the
 *  platform's banner (iOS); taps come back by `id`. */
export function showMentionBanner(content: MentionBannerContent, id: string): void {
  const banner = getPlatform().banner;
  if (!banner) {
    return;
  }
  void mentionNotice(content, id).then((notice) => banner.show(notice));
}

async function mentionNotice(content: MentionBannerContent, id: string): Promise<BannerNotice> {
  const { sender } = content;
  const presentation = sender ? presentAuthor(sender) : null;
  return {
    id,
    title: presentation?.label ?? content.fallbackName,
    subtitle: content.conversationTitle ? `in #${content.conversationTitle}` : undefined,
    body: content.excerpt ? readableExcerpt(content.excerpt) : undefined,
    icon: sender ? await senderIcon(sender) : undefined,
  };
}

async function senderIcon(sender: MessageAuthorDto): Promise<BannerNotice["icon"]> {
  const picture =
    sender.status === "active" && sender.avatar_url
      ? await loadAvatarBlobUrl(sender.avatar_url)
      : null;
  if (sender.kind === "agent") {
    const image = sender.status === "active" ? agentAvatarImage(sender.runtime, picture) : null;
    return image ? { image } : { glyph: iconMarkup(BotIcon), ground: true };
  }
  return picture ? { image: picture } : { glyph: iconMarkup(UserIcon), ground: true };
}

const AUTOLINK = /<(silverhand:\/\/[^\s<>]+)>/g;

/** The excerpt is the source body's raw bytes; references and locators read as
 *  the names a chip would show, everything else stays as written. */
export function readableExcerpt(excerpt: string): string {
  return excerpt.replace(AUTOLINK, (token, href: string) => {
    const reference = parseReference(href);
    if (reference) {
      return `@${reference.handle}`;
    }
    const locator = parseLocator(href);
    if (locator?.kind === "channel") {
      return `#${locator.channel.title}`;
    }
    if (locator?.kind === "workspace") {
      return locator.workspace.name;
    }
    return token;
  });
}
