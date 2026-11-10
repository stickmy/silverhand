import { app, Menu, nativeImage, Tray } from "electron";
import trayIconPath from "../../../build/tray-template-18.png?asset";
import trayIcon2xPath from "../../../build/tray-template-36.png?asset";
import { nativeCopy } from "./native-copy.js";

/**
 * The menu bar item: the monogram, with the unread total beside it as IM apps
 * show it. Clicking opens the app; right-click offers Open and Quit. It mirrors
 * the same total as the Dock badge, so both clear together.
 */
let tray: Tray | null = null;
let unread = 0;
let open: () => void = () => {};

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

function render(): void {
  if (!tray) {
    return;
  }
  tray.setTitle(unread > 0 ? String(unread) : "", { fontType: "monospacedDigit" });
  tray.setToolTip(app.name);
}

export function createMenuBar(onOpen: () => void): void {
  if (process.platform !== "darwin" || tray) {
    return;
  }
  open = onOpen;
  tray = new Tray(trayImage());
  tray.on("click", () => open());
  tray.on("right-click", () => {
    const labels = nativeCopy();
    tray?.popUpContextMenu(
      Menu.buildFromTemplate([
        { label: labels.openApp, click: () => open() },
        { type: "separator" },
        { label: labels.quit, click: () => app.quit() },
      ]),
    );
  });
  render();
}

export function setMenuBarUnread(count: number): void {
  unread = count;
  render();
}
