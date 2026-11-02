import { useEffect, useMemo, useState, type ReactNode } from "react";
import { AnimatePresence, motion, useIsPresent, useReducedMotion } from "motion/react";
import {
  ChevronDownIcon,
  ChevronUpIcon,
  LayersIcon,
  SpinnerIcon,
  ThinkingIcon,
  WaitingIcon,
} from "@/icons";
import type { AgentDto, AgentRunState } from "@silverhand/shared";
import { AgentAvatar } from "@/components/agent-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { useAgentRunStates } from "@/features/channels/hooks";
import { useAgents } from "@/features/agents/hooks/hooks";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { useOpenAgentActivity } from "../hooks/use-open-agent-activity";
import {
  activityAgentName,
  activityLineOf,
  activitySnippetKindLabel,
  activityStatusLabel,
  selectActiveRunStates,
} from "../model/composer-activity";
import { t } from "@/lib/i18n";

// ── Time helpers ─────────────────────────────────────────────────────────

function formatMs(ms: number): string {
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  const rem = s % 60;
  return rem === 0 ? `${m}m` : `${m}m ${rem}s`;
}

/** Returns elapsed milliseconds since `epochMs`, updating every second. */
function useLiveMs(epochMs: number): number {
  const [delta, setDelta] = useState(() => Date.now() - epochMs);
  useEffect(() => {
    const id = setInterval(() => setDelta(Date.now() - epochMs), 1000);
    return () => clearInterval(id);
  }, [epochMs]);
  return delta;
}

// ── Component ────────────────────────────────────────────────────────────

interface ComposerActivityHeaderProps {
  conversationId: string;
  /** When non-null, only show run-state for these agent ids. Used inside
   *  the ThreadPanel: the run-state stream is channel-scoped (a reply
   *  cluster is not a separate conversation), so without this filter the
   *  bubble would surface agents busy on the main line / other clusters.
   *  Passing the thread's follower agent ids restricts it to participants
   *  of this cluster (item 4). `null`/`undefined` = no restriction. */
  restrictToAgentIds?: readonly string[] | null;
}

/**
 * Agent run-state as the composer's top strip: it shares the shell's frame
 * (the shell squares its top corners and the strip continues its border) so
 * the two read as one box, but it is out of flow, growing the box UPWARD over
 * the end of the message list. A run starting or ending must not resize the
 * scroller (that read as the list jittering on its own); the list keeps a fixed
 * room at its end (`data-composer-activity-room`) so the resting strip never
 * covers the latest message. The resting state is exactly ONE line with
 * fixed-width slots and a single elastic, truncating middle slot, so it never
 * wraps or changes height between tool calls / thinking / compacting.
 *
 * Collapsed subject when several agents run: the highest-priority agent
 * (`statusRank`), ties broken by most-recent activity, with a `+N` chip for
 * the rest. Expanding lists every running agent in the same order.
 */
export function ComposerActivityHeader({
  conversationId,
  restrictToAgentIds,
}: ComposerActivityHeaderProps): JSX.Element | null {
  const { data: runStates = [] } = useAgentRunStates(conversationId);
  const { data: directoryAgents = [] } = useAgents();
  const [expanded, setExpanded] = useState(false);
  const openAgentActivity = useOpenAgentActivity();

  const agentsById = useMemo(() => {
    const m = new Map<string, AgentDto>();
    for (const agent of directoryAgents) m.set(agent.id, agent);
    return m;
  }, [directoryAgents]);

  // Active = non-terminal, optionally restricted to the thread's follower
  // agents (item 4), sorted by main-status priority then recency.
  const active = useMemo(
    () => selectActiveRunStates(runStates, restrictToAgentIds),
    [runStates, restrictToAgentIds],
  );

  // Avatars only for the agents actually shown — not the whole directory.
  const activeAgents = useMemo(() => {
    const ids = new Set(active.map((s) => s.agentId));
    return directoryAgents.filter((a) => ids.has(a.id));
  }, [directoryAgents, active]);
  const agentAvatarUrls = useAgentAvatarObjectUrls(activeAgents);

  const subject = active[0];
  // Hook order is fixed regardless of how many agents run; falls back to a
  // harmless epoch when nothing is active (the component returns null then).
  const subjectElapsedMs = useLiveMs(subject?.startedAt ?? Date.now());

  // Collapse automatically when the herd empties so a later run reopens
  // from the resting line, not a stale expanded panel.
  useEffect(() => {
    if (active.length === 0 && expanded) setExpanded(false);
  }, [active.length, expanded]);

  const line = activityLineOf(active, (id) => agentsById.get(id));
  if (!subject || !line) return null;

  const subjectAgent = agentsById.get(subject.agentId);
  const subjectName = line.name;
  const { badge, snippetKindLabel } = line;
  const showCompaction = line.compacting;
  const SnippetKindIcon =
    subject.activitySnippetKind === "thinking"
      ? ThinkingIcon
      : subject.activitySnippetKind === "api_response"
        ? WaitingIcon
        : null;
  const snippet = line.progress;
  const extra = line.others;
  const StatusIcon = showCompaction ? LayersIcon : SpinnerIcon;

  return (
    <div
      data-composer-activity=""
      className="absolute inset-x-0 bottom-full z-(--z-raised) overflow-hidden bg-bg-floating"
    >
      {expanded ? (
        <>
          <button
            type="button"
            onClick={() => setExpanded(false)}
            aria-expanded
            aria-label={t("messages.collapseRunDetails")}
            className="flex h-8 w-full min-w-0 items-center gap-2 px-3 text-xs text-text-secondary transition-colors hover:bg-bg-elevated/50"
          >
            <SpinnerIcon className="size-3 shrink-0 animate-spin text-accent" />
            <span className="font-medium text-text-secondary">
              Running
              {active.length > 1 ? ` ${active.length} agents` : ""}
            </span>
            <span className="min-w-0 flex-1" />
            <ChevronUpIcon className="size-3 shrink-0 text-text-muted/60" />
          </button>
        </>
      ) : (
        <div
          role="button"
          tabIndex={0}
          onClick={() => setExpanded(true)}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              setExpanded(true);
            }
          }}
          aria-expanded={false}
          aria-label={t("messages.runningExpand", { name: subjectName })}
          title={`${subjectName}: ${snippet}`}
          className="flex h-8 w-full min-w-0 cursor-pointer items-center gap-2 px-3 text-xs text-text-secondary transition-colors hover:bg-bg-elevated/50"
        >
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              openAgentActivity(subject.agentId);
            }}
            title={t("messages.openActivity", { name: subjectName })}
            aria-label={t("messages.openActivity", { name: subjectName })}
            className="shrink-0 cursor-pointer focus-outline"
          >
            <AgentAvatar
              runtime={subjectAgent?.runtime ?? "claude"}
              size={AVATAR_PX.xs}
              className="shrink-0"
              avatarObjectUrl={agentAvatarUrls.get(subject.agentId) ?? null}
              alt={subjectAgent?.display_name || subjectAgent?.name}
            />
          </button>
          <StatusIcon
            className={
              "size-3 shrink-0 " + (showCompaction ? "text-text-muted" : "animate-spin text-accent")
            }
          />
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              openAgentActivity(subject.agentId);
            }}
            title={t("messages.openActivity", { name: subjectName })}
            className="shrink-0 cursor-pointer font-medium text-accent outline-none focus-visible:underline"
          >
            {subjectName}
          </button>
          {badge ? (
            <span className="shrink-0 rounded border border-border-subtle/70 px-1.5 py-px font-mono text-(length:--text-badge) leading-4 text-text-muted">
              {badge}
            </span>
          ) : null}
          {snippetKindLabel ? (
            <span className="flex shrink-0 items-center gap-1 rounded border border-border-subtle/70 px-1.5 py-px text-(length:--text-badge) leading-4 text-text-muted">
              {SnippetKindIcon ? <SnippetKindIcon className="size-3" /> : null}
              <span>{snippetKindLabel}</span>
            </span>
          ) : null}
          <span className="min-w-0 flex-1 truncate text-left text-text-muted">{snippet}</span>
          {extra > 0 ? (
            <span className="shrink-0 rounded-full bg-bg-hover px-1.5 text-(length:--text-badge) leading-4 text-text-muted">
              +{extra}
            </span>
          ) : null}
          <span className="shrink-0 tabular-nums text-text-muted/60">
            {formatMs(subjectElapsedMs)}
          </span>
          <ChevronDownIcon className="size-3 shrink-0 text-text-muted/60" />
        </div>
      )}
      <AnimatePresence initial={false} key={conversationId}>
        {expanded ? (
          <ActivityDetailsTransition key="details">
            <div className="flex flex-col gap-0.5 px-1 pb-1.5">
              {active.map((state) => (
                <ActivityDetailRow
                  key={state.runId}
                  state={state}
                  agent={agentsById.get(state.agentId)}
                  avatarObjectUrl={agentAvatarUrls.get(state.agentId) ?? null}
                  onOpenActivity={openAgentActivity}
                />
              ))}
            </div>
          </ActivityDetailsTransition>
        ) : null}
      </AnimatePresence>
    </div>
  );
}

/** Only disclosure changes animate; streaming text and timers update in place. */
function ActivityDetailsTransition({ children }: { children: ReactNode }): JSX.Element {
  const present = useIsPresent();
  const reducedMotion = useReducedMotion();

  return (
    <motion.div
      ref={(node) => node?.toggleAttribute("inert", !present)}
      aria-hidden={!present || undefined}
      style={{ pointerEvents: present ? undefined : "none" }}
      initial={{ height: 0, opacity: 0, overflow: "hidden" }}
      animate={{
        height: "auto",
        opacity: 1,
        overflow: "hidden",
        transitionEnd: { overflow: "visible" },
      }}
      exit={{ height: 0, opacity: 0, overflow: "hidden" }}
      transition={{ duration: reducedMotion ? 0 : 0.14, ease: "easeOut" }}
    >
      {children}
    </motion.div>
  );
}

function ActivityDetailRow({
  state,
  agent,
  avatarObjectUrl,
  onOpenActivity,
}: {
  state: AgentRunState;
  agent: AgentDto | undefined;
  avatarObjectUrl: string | null;
  onOpenActivity: (agentId: string) => void;
}): JSX.Element {
  const elapsedMs = useLiveMs(state.startedAt);
  const compactionElapsedMs = useLiveMs(state.compactingSince ?? state.startedAt);
  const runtime = agent?.runtime ?? "claude";
  const name = activityAgentName(agent);
  const progressText = state.activitySnippet || activityStatusLabel(state);
  const snippetKindLabel = state.activitySnippet
    ? activitySnippetKindLabel(state.activitySnippetKind)
    : null;
  const SnippetKindIcon =
    state.activitySnippetKind === "thinking"
      ? ThinkingIcon
      : state.activitySnippetKind === "api_response"
        ? WaitingIcon
        : null;
  const toolEntries = Object.entries(state.toolCallCounts ?? {}).filter(([, count]) => count > 0);
  const compactionText = "Compacting…";
  const CompactionIcon = LayersIcon;

  return (
    <div className="flex w-full min-w-0 items-start gap-2 rounded-(--control-radius) px-2 py-1 text-xs">
      <span className="pt-0.5">
        <button
          type="button"
          onClick={() => onOpenActivity(state.agentId)}
          title={t("messages.openActivity", { name })}
          aria-label={t("messages.openActivity", { name })}
          className="block shrink-0 cursor-pointer focus-outline"
        >
          <AgentAvatar
            runtime={runtime}
            size={AVATAR_PX.xs}
            avatarObjectUrl={avatarObjectUrl}
            alt={agent?.display_name || agent?.name}
          />
        </button>
      </span>
      <span className="min-w-0 flex-1 space-y-1">
        <span className="flex min-w-0 items-center gap-1.5 leading-5">
          <button
            type="button"
            onClick={() => onOpenActivity(state.agentId)}
            title={t("messages.openActivity", { name })}
            className="shrink-0 cursor-pointer font-medium text-accent outline-none focus-visible:underline"
          >
            {name}
          </button>
          <SpinnerIcon className="size-3 shrink-0 animate-spin text-accent" />
          {snippetKindLabel ? (
            <span className="flex shrink-0 items-center gap-1 rounded border border-border-subtle/70 px-1.5 py-px text-(length:--text-badge) leading-4 text-text-muted">
              {SnippetKindIcon ? <SnippetKindIcon className="size-3" /> : null}
              <span>{snippetKindLabel}</span>
            </span>
          ) : null}
          <span className="min-w-0 flex-1 truncate text-text-secondary">{progressText}</span>
          <span className="shrink-0 tabular-nums text-text-muted/50">{formatMs(elapsedMs)}</span>
        </span>
        {state.compacting ? (
          <span className="flex min-w-0 items-center gap-1.5 leading-5 text-text-muted">
            <CompactionIcon className="size-3 shrink-0" />
            <span className="truncate">{compactionText}</span>
            <span className="shrink-0 tabular-nums text-current/70">
              {formatMs(compactionElapsedMs)}
            </span>
          </span>
        ) : null}
        {toolEntries.length > 0 ? (
          <span className="flex min-w-0 flex-wrap items-center gap-1">
            {toolEntries.map(([toolName, count]) => (
              <span
                key={`${toolName}:${count}`}
                className="rounded border border-border-subtle/70 px-1.5 py-px font-mono text-(length:--text-badge) leading-4 text-text-muted"
              >
                <span className="mr-1 text-text-muted/70">{count}</span>
                {toolName}
              </span>
            ))}
          </span>
        ) : null}
      </span>
    </div>
  );
}
