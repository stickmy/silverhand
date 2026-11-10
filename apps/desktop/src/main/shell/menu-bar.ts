import { app, Menu, nativeImage, Tray, type MenuItemConstructorOptions } from "electron";
import trayIconPath from "../../../build/tray-template-18.png?asset";
import trayIcon2xPath from "../../../build/tray-template-36.png?asset";
import type { MenuBarConversation } from "@silverhand/client";
import { nativeCopy } from "./native-copy.js";

/**
 * The menu bar item: the monogram with the unread total beside it, as IM apps
 * show it. Either click opens a menu listing the conversations with unread
 * messages, then Open and Quit. The total mirrors the Dock badge, so both clear
 * together.
 */
let tray: Tray | null = null;
let unread = 0;
let conversations: MenuBarConversation[] = [];
let handlers: MenuBarHandlers | null = null;

interface MenuBarHandlers {
  openApp: () => void;
  openConversation: (conversationId: string) => void;
}

/** Keeps a long title from stretching the menu across the screen. */
const TITLE_LIMIT = 40;
/** Past this the list stops; the app's sidebar holds the rest. */
const LIST_LIMIT = 10;

function trayImage(): Electron.NativeImage {
  const image = nativeImage.createFromPath(trayIconPath);
  image.addRepresentation({
    scaleFactor: 2,
    buffer: nativeImage.createFromPath(trayIcon2xPath).toPNG(),
  });
  // A template image is tinted by macOS for light, dark and highlighted bars.
  image.setTemplateImage(true);
  return image;
}

function clip(text: string): string {
  return text.length > TITLE_LIMIT ? `${text.slice(0, TITLE_LIMIT - 1)}…` : text;
}

function conversationItem(item: MenuBarConversation): MenuItemConstructorOptions {
  return {
    label: clip(item.title),
    sublabel: item.project ? clip(item.project) : undefined,
    badge: { type: "none", content: String(item.count) },
    click: () => handlers?.openConversation(item.id),
  };
}

function buildMenu(): Menu {
  const labels = nativeCopy();
  const listed = conversations.slice(0, LIST_LIMIT).map(conversationItem);

  return Menu.buildFromTemplate([
    ...(listed.length > 0 ? [...listed, { type: "separator" as const }] : []),
    { label: labels.openApp, click: () => handlers?.openApp() },
    { type: "separator" },
    { label: labels.quit, click: () => app.quit() },
  ]);
}

function render(): void {
  if (!tray) {
    return;
  }
  tray.setTitle(unread > 0 ? String(unread) : "", { fontType: "monospacedDigit" });
  tray.setToolTip(app.name);
}

export function createMenuBar(next: MenuBarHandlers): void {
  if (process.platform !== "darwin" || tray) {
    return;
  }
  handlers = next;
  tray = new Tray(trayImage());
  // Built on each open so the list and the language are current.
  const popUp = (): void => tray?.popUpContextMenu(buildMenu());
  tray.on("click", popUp);
  tray.on("right-click", popUp);
  render();
}

export function setMenuBarUnread(count: number): void {
  unread = count;
  render();
}

export function setMenuBarConversations(items: unknown): void {
  conversations = Array.isArray(items) ? items.filter(isConversation) : [];
}

function isConversation(value: unknown): value is MenuBarConversation {
  if (!value || typeof value !== "object") {
    return false;
  }
  const item = value as Record<string, unknown>;
  return (
    typeof item.id === "string" &&
    typeof item.title === "string" &&
    (item.project === undefined || typeof item.project === "string") &&
    typeof item.count === "number" &&
    item.count > 0
  );
}
