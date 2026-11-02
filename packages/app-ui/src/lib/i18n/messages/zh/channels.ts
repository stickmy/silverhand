import type { channels as en } from "../en/channels";
import type { Translation } from "../../translate";

export const channels: Translation<typeof en> = {
  today: "今天",
  yesterday: "昨天",
  cantStartTheAgent: "无法启动 agent:它所在的电脑尚未连接。",
  cantStartTheAgent2: "无法启动 agent:它在那台电脑上的工作目录不可用。请检查磁盘空间和权限后重试。",
  cantStartTheAgent3: "无法启动 agent:它所在的电脑尚未完成注册，请稍后重试。",
  cantStartTheAgent4: "无法启动 agent:它所在的电脑报告了内部错误。请重试，如果问题持续请反馈。",
  thisConversationsMembersAre: "这个对话的成员是固定的，无法修改。",
  thisConversationIsClosed: "这个对话已关闭，无法移除成员。",
  youDontHavePermission: "你没有权限移除这个 agent。",
  thisAgentIsNo: "这个 agent 已不是成员。",
  thisChannelIsClosed: "这个频道已关闭，无法移除成员。",
  restoreTheChannelBefore: "请先恢复频道再修改成员。",
  thisMemberHoldsA: "这位成员的项目角色比你高。",
  theOwnerOfA: "无法移除私有频道的所有者。",
  theLastMemberOf: "无法移除私有频道的最后一位成员。",
  youDontHavePermission2: "你没有权限移除这个成员。",
  thisChannelNoLonger: "这个频道已不存在。",
  startFailed: "无法启动 agent:{reason}",
  removeAgentFailed: "无法移除 agent:{reason}",
  removeMemberFailed: "无法移除成员:{reason}",
  stopFailed: "无法停止 agent,请重试。",
  clearSessionFailed: "无法清除 session,请重试。",
};
