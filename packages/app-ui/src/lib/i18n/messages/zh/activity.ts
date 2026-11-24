import type { activity as en } from "../en/activity";
import type { Translation } from "../../translate";

export const activity: Translation<typeof en> = {
  loadingActivity: "正在加载动态",
  all: "全部",
  mentions: "提及",
  threads: "话题",
  activity: "动态",
  couldntLoadActivity: "无法加载动态",
  checkYourConnectionAnd: "请检查网络后重试",
  allCaughtUp: "全部已读",
  requiresAttention: "需要处理",
  subscribed: "已订阅",
  markAsRead: "标为已读",
  markAllRead: "全部标为已读",
  markAllReadFailed: "未能全部标为已读，请重试",
  newCount: "{n} 条未读",
  emptyFilter: {
    all: "暂无内容",
    mention: "没有提及",
    thread: "没有话题",
  },
};
