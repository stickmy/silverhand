import type { attention as en } from "../en/attention";
import type { Translation } from "../../translate";

export const attention: Translation<typeof en> = {
  mute: "免打扰",
  unmute: "取消免打扰",
  someone: "有人",
  agent: "Agent",
  unknownError: "未知错误",
  newMention: "新的提及",
  youWereMentioned: "有人提及了你",
  newNotification: "新通知",
  mentionedYou: "提及了你",
  unknownError2: "未知错误",
  inChannel: "于 #{channel}",
  startTimeout: "@{name} 启动超时",
  exitedWithCode: "@{name} 意外退出(退出码 {code})",
  exited: "@{name} 意外退出",
  didntStartReason: "@{name} 未能启动:{reason}",
  newMentionIn: "{project} 中有新的提及",
  mentionedIn: "有人在 #{channel} 中提及了你",
  exitCode: "退出码 {code}",
  didntStart: "@{name} 未能启动",
};
