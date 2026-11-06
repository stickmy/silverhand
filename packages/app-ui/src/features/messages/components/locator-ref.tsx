import type { ReactNode } from "react";
import { HashIcon, MentionIcon, ThreadIcon } from "@/icons";
import type { Locator } from "@silverhand/shared";
import { useDirectPairs } from "@/features/direct-pairs/hooks";
import {
  locatorChannelListWorkspaceId,
  resolveLocatorTarget,
  resolveLocatorWorkspaceId,
  type LocatorTarget,
} from "@/features/conversations/locator-target";
import {
  isBackendUnreachable,
  isRegistrationRequired,
  isUpdateRequired,
  useViewerUser,
} from "@/features/session/hooks";
import {
  useArchivedChannelsForWorkspace,
  useChannelsForWorkspace,
  useWorkspaces,
} from "@/features/workspaces/hooks";
import { buildLocatorTargetUrl, useNavActions } from "@/router";
import { faceLabel } from "./conversation-ref";
import { INLINE_REF_HOVER_CLASS } from "../model/inline-ref-chip";
import { t } from "@/lib/i18n";

export function LocatorRef({
  locator,
  children,
}: {
  locator: Locator;
  children: ReactNode;
}): JSX.Element {
  const nav = useNavActions();
  const workspaces = useWorkspaces().data ?? [];
  const workspaceId = resolveLocatorWorkspaceId(locator, workspaces);
  const channelListId = locatorChannelListWorkspaceId(locator, workspaceId);
  // `workspaceResolved` is TRUE BY CONSTRUCTION here and never a guess: the only
  // non-null value this can carry came from a row in the viewer's workspace list.
  const live = useChannelsForWorkspace(channelListId, channelListId !== null);
  const archived = useArchivedChannelsForWorkspace(channelListId, channelListId !== null);
  const viewer = useViewerUser();
  const pairs = useDirectPairs();
  // The viewer's HANDLE, and only when the viewer record is a real one: the three
  // non-user arms of `ViewerState` describe the app's own state, not a person, so
  // none of them can be an endpoint of a DM. Cold or degraded reads as "not an
  // endpoint" → inert sigil, which is the fail-closed direction.
  const viewerHandle =
    viewer.data &&
    !isRegistrationRequired(viewer.data) &&
    !isBackendUnreachable(viewer.data) &&
    !isUpdateRequired(viewer.data)
      ? viewer.data.name
      : null;

  const target = resolveLocatorTarget(locator, {
    workspaceId,
    // Both tables, because an archived channel is still the channel that name
    // points at. Concatenated rather than merged: the two lists are disjoint by
    // construction (a channel sits in exactly one of them), and a duplicate would
    // read as ambiguity and go inert.
    channels: [...(live.data ?? []), ...(archived.data ?? [])],
    viewerHandle,
    pairs,
  });

  return (
    <LocatorChip
      locator={locator}
      raw={children}
      target={target}
      onOpen={(t) => nav.go(buildLocatorTargetUrl(t))}
    />
  );
}

export function LocatorRefStaticChip({ children }: { children: ReactNode }): JSX.Element {
  return <>{children}</>;
}

function LocatorChip({
  locator,
  raw,
  target,
  onOpen,
}: {
  locator: Locator;
  raw: ReactNode;
  target: LocatorTarget | null;
  onOpen: ((target: LocatorTarget) => void) | undefined;
}): JSX.Element {
  const tooltip = locatorTooltip(locator);
  const body = <LocatorSegments locator={locator} />;

  const shared =
    "inline-flex max-w-[280px] items-center gap-1 rounded-[var(--chip-radius)] px-1.5 -mx-0.5 align-baseline font-medium";

  if (!target || !onOpen) {
    return <>{raw}</>;
  }
  const dim = target.kind === "channel" && target.closed;
  return (
    <span
      role="button"
      tabIndex={0}
      className={
        `${shared} cursor-pointer bg-accent-soft text-accent focus-outline ` +
        INLINE_REF_HOVER_CLASS +
        (dim ? " opacity-60" : "")
      }
      title={tooltip}
      data-locator-ref={locator.kind}
      data-locator-navigable="true"
      onClick={(event) => {
        // The default action would follow the anchor's SANITIZED href, which is a
        // second navigation authority and the wrong one — see
        // `buildLocatorTargetUrl`.
        event.preventDefault();
        event.stopPropagation();
        onOpen(target);
      }}
      onKeyDown={(event) => {
        if (event.key !== "Enter" && event.key !== " ") return;
        event.preventDefault();
        event.stopPropagation();
        onOpen(target);
      }}
    >
      {body}
    </span>
  );
}

/** The sigil + label pairs, in the order the locator spells them. */
function LocatorSegments({ locator }: { locator: Locator }): JSX.Element {
  if (locator.kind === "workspace") {
    return <WorkspaceSegment name={locator.workspace.name} />;
  }
  if (locator.kind === "channel") {
    return (
      <>
        <WorkspaceSegment name={locator.workspace.name} />
        <Segment
          sigil={<HashIcon className="size-3 shrink-0 self-center" />}
          label={locator.channel.title}
        />
        {locator.threadRootId !== null && <ThreadSegment />}
      </>
    );
  }
  return (
    <>
      <Segment
        sigil={<MentionIcon className="size-3 shrink-0 self-center" />}
        // `&` joins the pair rather than a slash: the two handles are one
        // coordinate, not a path, and neither of them can contain this character.
        label={`${locator.endpoints[0]} & ${locator.endpoints[1]}`}
      />
      {locator.threadRootId !== null && <ThreadSegment />}
    </>
  );
}

function WorkspaceSegment({ name }: { name: string }): JSX.Element {
  return (
    <Segment
      sigil={
        <span
          aria-hidden
          className="shrink-0 self-center font-serif text-(length:--text-strong) font-semibold leading-none"
        >
          P
        </span>
      }
      label={name}
    />
  );
}

/** The thread leaf. shiki, 2026-08-21: the glyph is followed by the fixed word
 *  "thread" — a root message id is not a name, so the segment says what it is
 *  instead of showing an opaque id the reader cannot check. */
function ThreadSegment(): JSX.Element {
  return (
    <Segment
      sigil={<ThreadIcon className="size-3 shrink-0 self-center" />}
      label={t("messages.thread")}
    />
  );
}

function Segment({ sigil, label }: { sigil: JSX.Element; label: string }): JSX.Element {
  // Each segment aligns on its label's text baseline (`items-baseline`), with the
  // sigil `self-center`, so a multi-segment chip reads as one line; `items-center`
  // would align segments on synthesized baselines that differ between a letter and
  // an SVG sigil. The chip in turn sits on the prose baseline via the family rule
  // (`.markdown-body [data-locator-ref]`, markdown.css).
  return (
    <span className="inline-flex min-w-0 items-baseline gap-0.5">
      {sigil}
      <span className="truncate">{label}</span>
    </span>
  );
}

/** The coordinate in words, for the pointer tooltip and the accessible name. The
 *  container words are the ones every other conversation surface uses
 *  ({@link faceLabel}), so a locator names a channel the same way a `#conv:` chip
 *  does; a thread renames the whole coordinate because the thread IS what a
 *  thread-bearing locator points at. */
function locatorTooltip(locator: Locator): string {
  if (locator.kind === "workspace")
    return t("messages.projectNamed", { name: locator.workspace.name });
  const [kind, path] =
    locator.kind === "channel"
      ? [faceLabel("channel"), `${locator.workspace.name} / ${locator.channel.title}`]
      : [faceLabel("direct_line"), `${locator.endpoints[0]} & ${locator.endpoints[1]}`];
  return locator.threadRootId === null ? `${kind} ${path}` : t("messages.threadNamed", { path });
}
