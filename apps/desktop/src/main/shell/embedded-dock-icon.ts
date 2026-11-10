import { app, nativeImage } from "electron";
import embeddedIconPath from "../../../build/icon-embedded.png?asset";

/**
 * Marks the running app's Dock icon when it hosts the embedded server, so an
 * embedded instance is told apart from one connected to a remote server. Only
 * the running icon changes: Finder, Launchpad and a non-running Dock tile read
 * the bundle's `.icns`, which stays shared.
 */
export function markEmbeddedDockIcon(): void {
  if (process.platform !== "darwin" || !app.dock) {
    return;
  }

  const icon = nativeImage.createFromPath(embeddedIconPath);
  if (icon.isEmpty()) {
    return;
  }
  app.dock.setIcon(icon);
}
