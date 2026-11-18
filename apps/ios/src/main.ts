import { configurePlatform, mountApp, resumeApp } from "@silverhand/app-ui";
import { nativeBridge } from "./bridge";
import { installDeviceChrome } from "./device-chrome";
import { createIOSPlatform } from "./platform";
import "./mobile.css";

// This WebView has one native profile. Never retain server-scoped browser state
// across logout or origin changes, including modules hydrated at import time.
// Appearance and language are device preferences, not server state, so they survive.
const APPEARANCE_KEY = "silverhand:appearance";
const LOCALE_KEY = "silverhand:locale";
const DEVICE_PREFERENCE_KEYS = [APPEARANCE_KEY, LOCALE_KEY];

function resetApplication(): void {
  const kept = DEVICE_PREFERENCE_KEYS.map((key) => [key, localStorage.getItem(key)] as const);
  localStorage.clear();
  for (const [key, value] of kept) {
    if (value !== null) {
      localStorage.setItem(key, value);
    }
  }
  sessionStorage.clear();
  window.location.replace(window.location.origin + "/");
}

async function start(): Promise<void> {
  const root = document.getElementById("root");
  if (!root) throw new Error("Missing root element");
  // The phone's default theme. A stored choice, once made, is kept.
  if (localStorage.getItem(APPEARANCE_KEY) === null) {
    localStorage.setItem(APPEARANCE_KEY, JSON.stringify({ theme: "glass" }));
  }
  await nativeBridge.addListener("sessionInvalidated", resetApplication);
  // Keep the React tree and pending send identities alive across suspension.
  // Native closes sockets in the background; resume revalidates and refetches.
  await nativeBridge.addListener("foreground", () => {
    void resumeApp();
  });
  // A docked keyboard already covers the home-indicator area. Do not add
  // that screen-edge inset a second time above the keyboard/accessory bar.
  await nativeBridge.addListener("keyboardChanged", ({ docked }) => {
    const style = document.documentElement.style;
    if (docked) style.setProperty("--input-safe-area-bottom", "0px");
    else style.removeProperty("--input-safe-area-bottom");
  });
  await installDeviceChrome(nativeBridge);
  // A reloaded page (logout, a new server) starts with no bars; native still
  // draws the ones the last page declared until told otherwise.
  await Promise.all([
    nativeBridge.titlebar({ state: null }),
    nativeBridge.pageBar({ state: null }),
    nativeBridge.composerRemove({}),
  ]);
  const { available: contextMenu } = await nativeBridge.contextMenuAvailable();
  configurePlatform(createIOSPlatform(nativeBridge, resetApplication, { contextMenu }));
  await nativeBridge.getProfile();
  mountApp(root, { loginFirst: true, showServerAddress: true });
}

void start().catch((error) => {
  const root = document.getElementById("root");
  // app-ui never mounted, so its i18n is unavailable here.
  const zh = navigator.language.toLowerCase().startsWith("zh");
  const headline = zh
    ? "Silverhand 无法启动，请重新打开应用。"
    : "Silverhand could not start. Please reopen the app.";
  if (root) root.textContent = `${headline} (${String(error)})`;
});
