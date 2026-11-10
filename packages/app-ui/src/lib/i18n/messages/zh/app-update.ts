import type { appUpdate as en } from "../en/app-update";
import type { Translation } from "../../translate";

export const appUpdate: Translation<typeof en> = {
  softwareUpdate: "软件更新",
  installOnQuit: "退出时安装",
  restartAndInstall: "重启并安装",
  restartNow: "立即重启",
  retryDownload: "重新下载",
  download: "下载",
  tryAgain: "重试",
  checkForUpdates: "检查更新",
  noNewerVersionIs: "暂无新版本。",
  checkAgain: "再次检查",
  checkingForUpdates: "正在检查更新…",
  checking: "检查中…",
  readyRestart: "Silverhand {version} 已就绪，重启即可完成更新。",
  notChecked: "Silverhand {version} 尚未检查更新。",
  latest: "Silverhand {version} 已是最新版本。",
  available: "Silverhand {version} 可供更新。",
  downloading: "正在下载 Silverhand {version}…",
  readyToInstall: "Silverhand {version} 已可安装。可以立即重启，也可以在下次退出时安装。",
  preparing: "正在准备 Silverhand {version}…",
  willInstallOnQuit: "Silverhand {version} 将在退出时安装。",
};
