import type { common as en } from "../en/common";
import type { Translation } from "../../translate";

export const common: Translation<typeof en> = {
  cancel: "取消",
  close: "关闭",
  save: "保存",
  back: "返回",
  retry: "重试",
  checking: "正在检查…",
  continue: "继续",
  loading: "正在加载",
  unknownError: "未知错误。",
  somethingWrong: "出了点问题，请重试。",
  unreachable: "无法连接服务器，请重试。",
  update: "更新",
  change: "更改",
  technicalDetails: "技术详情",
  archived: "已归档",
  search: "搜索",
  you: "你",
  unknownUser: "未知用户",
  online: "在线",
  offline: "离线",
  attachmentCount: {
    other: "{count} 个附件",
  },
  lastSeen: "{time}在线",
  copyFailed: "无法复制，请重试。",
  updateFailed: "无法更新，请重试。",
};
