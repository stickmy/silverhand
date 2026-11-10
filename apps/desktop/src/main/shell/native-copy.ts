export type NativeLocale = "en" | "zh";

interface NativeCopy {
  checkForUpdates: string;
  settings: string;
  file: string;
  close: string;
  view: string;
  saveImage: string;
  pngImage: string;
  saveAttachment: string;
  openApp: string;
  quit: string;
  localServer: string;
}

const COPY: Record<NativeLocale, NativeCopy> = {
  en: {
    checkForUpdates: "Check for Updates",
    settings: "Settings",
    file: "File",
    close: "Close",
    view: "View",
    saveImage: "Save image",
    pngImage: "PNG Image",
    saveAttachment: "Save attachment",
    openApp: "Open Silverhand",
    quit: "Quit Silverhand",
    localServer: "Embedded",
  },
  zh: {
    checkForUpdates: "检查更新",
    settings: "设置",
    file: "文件",
    close: "关闭",
    view: "显示",
    saveImage: "保存图片",
    pngImage: "PNG 图片",
    saveAttachment: "保存附件",
    openApp: "打开 Silverhand",
    quit: "退出 Silverhand",
    localServer: "Embedded",
  },
};

/** English until the renderer reports the language it resolved. */
let current: NativeLocale = "en";

export function nativeCopy(): NativeCopy {
  return COPY[current];
}

/** Returns whether the language changed, so callers rebuild native chrome only then. */
export function setNativeLocale(value: unknown): boolean {
  if ((value !== "en" && value !== "zh") || value === current) {
    return false;
  }
  current = value;
  return true;
}
