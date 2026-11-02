export const channels = {
  today: "Today",
  yesterday: "Yesterday",
  cantStartTheAgent: "Can't start the agent: its computer is not connected yet.",
  cantStartTheAgent2:
    "Can't start the agent: its working directory on that computer isn't usable. Check the computer's disk space and permissions, then try again.",
  cantStartTheAgent3:
    "Can't start the agent: its computer hasn't finished registering with the server. Try again in a moment.",
  cantStartTheAgent4:
    "Can't start the agent: its computer reported an internal error. Try again, and report it if it keeps happening.",
  thisConversationsMembersAre: "This conversation's members are fixed and can't be changed.",
  thisConversationIsClosed: "This conversation is closed. Members can't be removed.",
  youDontHavePermission: "You don't have permission to remove this agent.",
  thisAgentIsNo: "This agent is no longer a member.",
  thisChannelIsClosed: "This channel is closed. Members can't be removed.",
  restoreTheChannelBefore: "Restore the channel before changing its members.",
  thisMemberHoldsA: "This member holds a higher project role than you.",
  theOwnerOfA: "The owner of a private channel can't be removed.",
  theLastMemberOf: "The last member of a private channel can't be removed.",
  youDontHavePermission2: "You don't have permission to remove this member.",
  thisChannelNoLonger: "This channel no longer exists.",
  startFailed: "Can't start the agent: {reason}",
  removeAgentFailed: "Can't remove the agent: {reason}",
  removeMemberFailed: "Can't remove the member: {reason}",
  stopFailed: "Couldn't stop the agent. Try again.",
  clearSessionFailed: "Couldn't clear the session. Try again.",
} as const;
