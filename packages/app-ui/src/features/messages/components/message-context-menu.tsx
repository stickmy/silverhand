import { Fragment, useEffect, useRef, useState, type ReactElement } from "react";
import { Slot } from "@radix-ui/react-slot";
import type { ContextMenuCapability } from "@silverhand/client";
import { useCoarsePointer } from "@/navigation/layout";
import "./message-context-menu.css";
import {
  BellOffIcon,
  BookmarkIcon,
  BookmarkRemoveIcon,
  CopyMarkdownIcon,
  SelectTextIcon,
  ShareLinkIcon,
  ThreadIcon,
  type IconComponent,
} from "@/icons";
import { iconMarkup } from "@/components/layout/native-page-bar";
import { getPlatform } from "@/platform";
import {
  AppContextMenu,
  AppContextMenuItem,
  AppContextMenuReactions,
  AppContextMenuSeparator,
} from "@/components/menus/app-context-menu";
import { QUICK_REACTIONS } from "./reaction-picker";
import { t } from "@/lib/i18n";

/**
 * Right-click context menu for a single MessageItem row. Wraps the row
 * element via Radix's `asChild` so the row IS the trigger — no extra
 * DOM container, no hover affordance.
 *
 * Mobile Select text delegates to the system selection on the original body.
 * Message actions are gated by callback presence:
 * - **Copy markdown** (`onCopyMarkdown`) — clipboard receives ONLY
 *   `message.content`; never include sender/time. Single-message
 *   semantic. Multi-message transcript export (with sender headers)
 *   is also clipboard-bound, but lives on the SelectionToolbar's
 *   "Copy markdown" button — see ChannelView.handleCopyMarkdownTranscript.
 * - **Save / Unsave** (`onToggleSave` + `saved`) — label switches
 *   based on current `saved` state. We never silently flip "Save"
 *   into a destructive unsave — Cindy's review constraint #2.
 * - **Share** (`onShare`) — enters multi-select mode with this
 *   message preselected. Hidden until the export pipeline lands
 *   (Cindy's "no half-baked Share entry" constraint), so callers
 *   in the unfinished commits omit `onShare` entirely.
 * - **Unfollow thread** (`onUnfollowThread`) — unsubscribe the human
 *   viewer from this message's reply cluster. Passed in only inside the
 *   ThreadPanel and only while the viewer is following, so it never
 *   shows on main-line rows or threads the viewer isn't subscribed to.
 *
 * If `trigger` ends up with no actions visible the entire wrapper
 * collapses to a transparent passthrough (no menu pops on right-click).
 */
export function MessageContextMenu({
  trigger,
  text,
  saved = false,
  onCopyMarkdown,
  onToggleSave,
  onShare,
  onUnfollowThread,
  onReact,
  onReplyInThread,
}: {
  trigger: ReactElement;
  /** The same unmodified message body used by Copy markdown. */
  text?: string;
  saved?: boolean;
  onCopyMarkdown?: () => void;
  onToggleSave?: () => void;
  onShare?: () => void;
  onUnfollowThread?: () => void;
  /** Touch only: on a pointer device the row's own hover controls react. */
  onReact?: (emoji: string) => void;
  onReplyInThread?: () => void;
}): JSX.Element {
  const touch = useCoarsePointer();
  const nativeMenu = getPlatform().contextMenu;
  const [selecting, setSelecting] = useState(false);
  const triggerRef = useRef<HTMLSpanElement>(null);
  const openingSelection = useRef(false);
  useEffect(() => {
    if (!touch) setSelecting(false);
  }, [touch]);
  useEffect(() => {
    if (!selecting) return;
    const row = triggerRef.current;
    const body = row?.querySelector("[data-message-body]");
    const ownsSelection = () => {
      const selection = window.getSelection();
      return (
        !!selection?.anchorNode &&
        !!selection.focusNode &&
        !!body?.contains(selection.anchorNode) &&
        body.contains(selection.focusNode)
      );
    };
    const onSelectionChange = () => {
      if (openingSelection.current) return;
      if (window.getSelection()?.isCollapsed || !ownsSelection()) setSelecting(false);
    };
    const onPointerDown = (event: PointerEvent) => {
      if (event.target instanceof Node && !row?.contains(event.target)) setSelecting(false);
    };
    document.addEventListener("selectionchange", onSelectionChange);
    document.addEventListener("pointerdown", onPointerDown, true);
    return () => {
      document.removeEventListener("selectionchange", onSelectionChange);
      document.removeEventListener("pointerdown", onPointerDown, true);
      if (ownsSelection()) window.getSelection()?.removeAllRanges();
    };
  }, [selecting]);
  const canSelectText = touch && !!text?.trim();
  const react = touch ? onReact : undefined;
  const sections = menuSections({
    replyInThread: onReplyInThread,
    selectText: canSelectText
      ? () => {
          openingSelection.current = true;
          setSelecting(true);
        }
      : undefined,
    saved,
    onCopyMarkdown,
    onToggleSave,
    onShare,
    onUnfollowThread,
  });
  if (!react && sections.length === 0) return trigger;

  // The system menu fits four emoji to its row.
  const reactions = react ? QUICK_REACTIONS.slice(0, nativeMenu ? 4 : 6) : [];
  const selectBody = () => {
    const body = triggerRef.current?.querySelector("[data-message-body]");
    const selection = window.getSelection();
    if (body && selection) {
      const range = document.createRange();
      range.selectNodeContents(body);
      selection.removeAllRanges();
      selection.addRange(range);
    } else {
      setSelecting(false);
    }
    openingSelection.current = false;
  };
  const triggerClassName = "message-context-trigger" + (selecting ? " message-text-selecting" : "");

  if (nativeMenu) {
    return (
      <NativeContextMenuTrigger
        trigger={trigger}
        className={triggerClassName}
        triggerRef={triggerRef}
        disabled={selecting}
        menu={nativeMenu}
        reactions={reactions}
        sections={sections}
        onReact={(emoji) => react?.(emoji)}
        onSelected={() => {
          // The body is selectable only once the class above applies.
          if (openingSelection.current) requestAnimationFrame(selectBody);
        }}
      />
    );
  }

  return (
    <AppContextMenu
      trigger={trigger}
      triggerClassName={triggerClassName}
      triggerRef={triggerRef}
      disabled={selecting}
      onCloseAutoFocus={(event) => {
        if (openingSelection.current) {
          event.preventDefault();
          selectBody();
        }
      }}
    >
      {react ? (
        <>
          <AppContextMenuReactions
            emojis={reactions}
            labelFor={(emoji) => t("messages.reactWith", { emoji })}
            onPick={react}
          />
          <AppContextMenuSeparator />
        </>
      ) : null}
      {sections.map((section, index) => (
        <Fragment key={section[0]?.id}>
          {index > 0 ? <AppContextMenuSeparator /> : null}
          {section.map(({ id, label, Icon, run }) => (
            <AppContextMenuItem key={id} icon={<Icon className="size-3.5" />} onSelect={run}>
              {label}
            </AppContextMenuItem>
          ))}
        </Fragment>
      ))}
    </AppContextMenu>
  );
}

type MenuAction = { id: string; label: string; Icon: IconComponent; run: () => void };

/** The message's actions in two groups: what the viewer does with the message
 * itself (reply, save, follow), then taking its content out (select, copy,
 * share). One list for the web menu and the native one. */
function menuSections({
  replyInThread,
  selectText,
  saved,
  onCopyMarkdown,
  onToggleSave,
  onShare,
  onUnfollowThread,
}: {
  replyInThread?: () => void;
  selectText?: () => void;
  saved: boolean;
  onCopyMarkdown?: () => void;
  onToggleSave?: () => void;
  onShare?: () => void;
  onUnfollowThread?: () => void;
}): MenuAction[][] {
  const action = (
    id: string,
    label: string,
    Icon: IconComponent,
    run: (() => void) | undefined,
  ): MenuAction[] => (run ? [{ id, label, Icon, run }] : []);

  return [
    [
      ...action("reply", t("messages.replyInThread"), ThreadIcon, replyInThread),
      ...action(
        "save",
        saved ? t("messages.unsave") : t("messages.saveMessage"),
        saved ? BookmarkRemoveIcon : BookmarkIcon,
        onToggleSave,
      ),
      ...action("unfollow", t("messages.unfollowThread"), BellOffIcon, onUnfollowThread),
    ],
    [
      ...action("select", t("messages.selectText"), SelectTextIcon, selectText),
      ...action("copy", t("messages.copyMarkdown"), CopyMarkdownIcon, onCopyMarkdown),
      ...action("share", t("messages.share"), ShareLinkIcon, onShare),
    ],
  ].filter((section) => section.length > 0);
}

const LONG_PRESS_MS = 700;

/**
 * The row as the trigger of the platform's own context menu (iOS), which
 * draws above the native composer and bars. A touch held still opens it, as
 * Radix's web menu does; a secondary click opens it at once.
 */
function NativeContextMenuTrigger({
  trigger,
  className,
  triggerRef,
  disabled,
  menu,
  reactions,
  sections,
  onReact,
  onSelected,
}: {
  trigger: ReactElement;
  className: string;
  triggerRef: React.Ref<HTMLSpanElement>;
  disabled: boolean;
  menu: ContextMenuCapability;
  reactions: readonly string[];
  sections: MenuAction[][];
  onReact: (emoji: string) => void;
  onSelected: () => void;
}): JSX.Element {
  const timer = useRef<number | undefined>(undefined);
  const latest = useRef({ sections, onReact, onSelected });
  latest.current = { sections, onReact, onSelected };
  const clear = () => window.clearTimeout(timer.current);
  useEffect(() => clear, []);

  const open = (x: number, y: number) => {
    void menu
      .show({
        point: { x, y },
        reactions: reactions.map((emoji) => ({
          emoji,
          label: t("messages.reactWith", { emoji }),
        })),
        sections: sections.map((section) =>
          section.map(({ id, label, Icon }) => ({ id, label, icon: iconMarkup(Icon) })),
        ),
      })
      .then((choice) => {
        if (!choice) {
          return;
        }
        if (choice.kind === "react") {
          latest.current.onReact(choice.emoji);
        } else {
          latest.current.sections
            .flat()
            .find((action) => action.id === choice.id)
            ?.run();
          latest.current.onSelected();
        }
      })
      .catch(() => {});
  };

  if (disabled) {
    return (
      <Slot className={className} ref={triggerRef} style={{ WebkitTouchCallout: "default" }}>
        {trigger}
      </Slot>
    );
  }

  return (
    <Slot
      className={className}
      ref={triggerRef}
      onContextMenu={(event) => {
        clear();
        event.preventDefault();
        open(event.clientX, event.clientY);
      }}
      onPointerDown={(event) => {
        if (event.pointerType === "mouse") {
          return;
        }
        clear();
        const { clientX, clientY } = event;
        timer.current = window.setTimeout(() => open(clientX, clientY), LONG_PRESS_MS);
      }}
      onPointerMove={clear}
      onPointerUp={clear}
      onPointerCancel={clear}
    >
      {trigger}
    </Slot>
  );
}
