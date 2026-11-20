import * as Popover from "@radix-ui/react-popover";
import { useEffect, useRef, useState, type ReactNode } from "react";
import dayjs from "dayjs";
import relativeTime from "dayjs/plugin/relativeTime";
import { UserIcon } from "@/icons";
import { humanPresenceText, presenceDotStatus, usePresenceOf } from "@/features/users/presence";

dayjs.extend(relativeTime);

/**
 * Permissive shape so both `UserDto` (full row, available when the human
 * IS the viewer) and `HumanMemberDto` (chip strip / mention resolution
 * for non-viewer humans, which omits `id` / `created_at` / `avatar_url`)
 * can flow through the same popover. Only the fields actually rendered
 * appear here — adding `id` would force chip callers to invent one, and
 * `created_at` was deliberately dropped (no Joined line per shiki).
 */
export interface HumanInfoTarget {
  name: string;
  display_name: string;
}

interface HumanInfoPopoverProps {
  human: HumanInfoTarget;
  /** The element that triggers the popover (rendered via Radix
   *  `asChild`). */
  children: ReactNode;
  /**
   * The human's user id. When provided, the card shows an inline
   * online/offline presence line next to the name — the human
   * counterpart to `AgentInfoPopover`'s per-conversation presence.
   * Presence is global (event-WS connection derived), so no
   * `conversationId` is needed. Omit (e.g. when only a name is known)
   * and the line is skipped.
   */
  userId?: string;
  side?: "top" | "bottom" | "left" | "right";
  align?: "start" | "center" | "end";
  /** `'click'` opens on click for standalone controls. `'hover'`
   *  (default) opens on pointer-enter and closes on pointer-leave with
   *  a small grace window so the user can move into the popover
   *  content. Mirrors `AgentInfoPopover` so agent and human triggers
   *  feel identical. */
  trigger?: "click" | "hover";
  /**
   * Avatar object URL resolved by the parent.
   *
   * - `undefined` (omitted) — no avatar URL provided; render the
   *   placeholder. We do NOT fetch internally so high-frequency render
   *   sites (timeline message authors, inline `@mentions`, member
   *   chips) don't fire one GET per instance. Parents typically pick
   *   the source by viewer match: `useViewerAvatarObjectUrl()` for
   *   the viewer, `useUserAvatarObjectUrl({ userId })` (or the batch
   *   `useUserAvatarObjectUrls`) for any other human.
   * - `null` — parent looked it up and the human has no avatar (or it
   *   is still loading); render the placeholder.
   * - `string` — use the parent's blob URL directly.
   */
  avatarObjectUrl?: string | null;
}

const HOVER_OPEN_DELAY_MS = 80;
const HOVER_CLOSE_DELAY_MS = 120;

/**
 * Compact human (user) info card — the human counterpart to
 * `AgentInfoPopover`. Shows avatar + display name + handle, plus an
 * inline global online/offline presence line when a `userId` is given
 * (the human analogue of the agent popover's per-conversation
 * `PresenceInline`). Still lighter than the agent popover: humans are
 * not processes, so presence is global (not per-conversation), and
 * there is no crashed-retry affordance, no management actions, and no
 * Joined date (per shiki — `joined_at` on workspace_users is back-fill /
 * install noise; the right Joined surface, if any, is per-conversation,
 * not registration-row).
 *
 * Hover mode is implemented via controlled `open` rather than Radix's
 * separate HoverCard primitive to keep the dep surface narrow and
 * match the existing agent popover's interaction grammar.
 */
export function HumanInfoPopover({
  human,
  children,
  userId,
  side = "bottom",
  align = "start",
  trigger = "hover",
  avatarObjectUrl,
}: HumanInfoPopoverProps): JSX.Element {
  const [hoverOpen, setHoverOpen] = useState(false);
  // Single timer reused for both open and close so consecutive
  // pointer transitions cancel each other out cleanly. Mirrors the
  // agent popover.
  const timerRef = useRef<number | null>(null);
  const clearTimer = (): void => {
    if (timerRef.current !== null) {
      window.clearTimeout(timerRef.current);
      timerRef.current = null;
    }
  };
  useEffect(() => () => clearTimer(), []);

  const scheduleOpen = (): void => {
    if (trigger !== "hover") return;
    clearTimer();
    timerRef.current = window.setTimeout(() => setHoverOpen(true), HOVER_OPEN_DELAY_MS);
  };
  const scheduleClose = (): void => {
    if (trigger !== "hover") return;
    clearTimer();
    timerRef.current = window.setTimeout(() => setHoverOpen(false), HOVER_CLOSE_DELAY_MS);
  };

  const rootProps = trigger === "hover" ? { open: hoverOpen, onOpenChange: setHoverOpen } : {};

  const label = human.display_name?.trim() || human.name;

  return (
    <Popover.Root {...rootProps}>
      <Popover.Trigger asChild onPointerEnter={scheduleOpen} onPointerLeave={scheduleClose}>
        {children}
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          side={side}
          align={align}
          sideOffset={6}
          collisionPadding={8}
          onPointerEnter={scheduleOpen}
          onPointerLeave={scheduleClose}
          data-popover-frame="info"
          className="z-(--z-dialog) w-65 max-w-[calc(100vw-32px)] bg-bg-floating p-3 shadow-popover outline-none data-[state=open]:animate-in data-[state=open]:fade-in-0 data-[state=open]:zoom-in-95"
        >
          <div className="flex items-start gap-3">
            {avatarObjectUrl ? (
              <span
                data-avatar-ground
                className="inline-block size-9 shrink-0 overflow-hidden rounded-(--avatar-radius)"
              >
                <img src={avatarObjectUrl} alt="" className="size-full object-cover" />
              </span>
            ) : (
              // Same neutral square as the chip that opens this card — no
              // display-name initial (#task:1700): a letter identifies nobody
              // and reads as a different avatar system from the uploaded
              // images next to it.
              <span
                data-avatar-ground
                className="inline-flex size-9 shrink-0 items-center justify-center rounded-(--avatar-radius) text-text-muted"
                aria-hidden
              >
                <UserIcon className="size-5" />
              </span>
            )}
            <div className="min-w-0 flex-1">
              <div className="flex min-w-0 items-center gap-2">
                <span className="min-w-0 truncate text-sm font-semibold text-text-primary">
                  {label}
                </span>
                {userId ? <HumanPresenceInline userId={userId} /> : null}
              </div>
              <div className="truncate font-mono text-xs text-text-muted">@{human.name}</div>
            </div>
          </div>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

const PRESENCE_DOT: Record<"active" | "idle", string> = {
  active: "bg-success",
  idle: "bg-text-muted/40",
};

/**
 * Inline online/offline line next to the name — the human counterpart
 * to `AgentInfoPopover`'s `PresenceInline`. Reads the shared global
 * `['presence']` cache (kept live by `usePresenceSync` in the app
 * shell), so it costs no extra fetch per popover. A cache miss reads as
 * offline (no live connection observed). Only mounts while the popover
 * content is open, matching the agent popover's per-instance read.
 */
function HumanPresenceInline({ userId }: { userId: string }): JSX.Element {
  const presence = usePresenceOf(userId);
  const dot = presenceDotStatus(presence?.presence);
  const lastSeenAt = presence?.last_seen_at ?? null;
  const label = humanPresenceText(
    presence?.presence,
    lastSeenAt,
    lastSeenAt != null ? dayjs(lastSeenAt).fromNow() : "",
  );
  return (
    <span
      className="inline-flex shrink-0 items-center gap-1 text-(length:--text-badge) text-text-muted"
      title={label}
    >
      <span
        data-status-pip
        className={`size-1.5 rounded-(--pill-radius) ${PRESENCE_DOT[dot]}`}
        aria-hidden
      />
      {label}
    </span>
  );
}
