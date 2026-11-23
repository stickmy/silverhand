import { AgentActivitySkeleton } from "./agent-loading";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import {
  CancelledIcon,
  ChevronUpIcon,
  EnterIcon,
  FailedIcon,
  HistoryIcon,
  InboxIcon,
  InfoIcon,
  LayersIcon,
  MessageIcon,
  PowerIcon,
  SpinnerIcon,
  SuccessIcon,
  TerminalIcon,
  ThinkingIcon,
  ToolIcon,
  WaitingIcon,
  WarningIcon,
} from "@/icons";
import dayjs from "dayjs";
import type { ConversationEventKind, ToolOutcome } from "@silverhand/shared";
import { cn } from "@/lib/cn";
import { Markdown } from "@/features/messages/components/markdown";
import {
  coalesceActivity,
  presentActivityToolCall,
  type ActivityDisplayItem,
  type ActivityEvent,
  type ToolDetail,
} from "../model/activity-presentation";
import { mergeActivityById, useAgentActivityFeed } from "../hooks/use-agent-activity-feed";
import { useAgentStreamActivity } from "../hooks/use-agent-stream-activity";
import { t } from "@/lib/i18n";

// Re-exported so the existing reconcile contract test keeps its import path.
export { mergeActivityById };

/**
 * Merge two activity-event lists — see `./useAgentActivityFeed`. (The merge
 * logic + feed cache moved there in #task:1255 P1c #2 so the central ingestor
 * can write live rows and this view is a pure reader.)
 */

interface AgentActivityTimelineProps {
  /** Parent channel / dm conversation id (never a thread — see pane). */
  conversationId: string;
  agentId: string;
}

/** The source owns cache identity; both source types share the same timeline renderer. */
export function AgentActivityTimeline({
  conversationId,
  agentId,
}: AgentActivityTimelineProps): JSX.Element {
  const feed = useAgentActivityFeed(conversationId, agentId);
  return <ActivityTimeline identity={`${conversationId}:${agentId}`} feed={feed} />;
}

export function AgentStreamActivityTimeline({ agentId }: { agentId: string }): JSX.Element {
  const feed = useAgentStreamActivity(agentId);
  return <ActivityTimeline identity={`agent:${agentId}`} feed={feed} />;
}

function ActivityTimeline({
  identity,
  feed,
}: {
  identity: string;
  feed: {
    events: ActivityEvent[];
    nextBeforeId: number | null;
    phase: "loading" | "ready" | "error";
    isLoadingMore: boolean;
    loadEarlier(): Promise<void>;
  };
}): JSX.Element {
  const { events, nextBeforeId, phase, isLoadingMore, loadEarlier } = feed;

  const scrollRef = useRef<HTMLDivElement | null>(null);
  // Track whether the user is pinned near the bottom so a live append
  // auto-follows the tail (console feel) without yanking the viewport
  // when they've scrolled up to read history.
  const atBottomRef = useRef(true);
  const tailFrameRef = useRef<number[]>([]);
  const tailObserverRef = useRef<ResizeObserver | null>(null);

  const cancelTailFrames = useCallback(() => {
    for (const frame of tailFrameRef.current) cancelAnimationFrame(frame);
    tailFrameRef.current = [];
  }, []);

  const pinToTailIfNeeded = useCallback(() => {
    const el = scrollRef.current;
    if (el && atBottomRef.current) el.scrollTop = el.scrollHeight;
  }, []);

  // Pin immediately and across the next two frames. This covers the side-panel
  // open path where the scroller/list can mount before the pane width/height
  // and markdown wrapping have fully settled; ResizeObserver below handles all
  // later size changes.
  const scheduleTailPin = useCallback(() => {
    cancelTailFrames();
    pinToTailIfNeeded();
    const first = requestAnimationFrame(() => {
      pinToTailIfNeeded();
      const second = requestAnimationFrame(() => {
        pinToTailIfNeeded();
        tailFrameRef.current = [];
      });
      tailFrameRef.current = [second];
    });
    tailFrameRef.current = [first];
  }, [cancelTailFrames, pinToTailIfNeeded]);

  // Re-pin to the tail when the selected (conversation, agent) changes. The
  // ResizeObserver tail-follow (setListRef) then keeps it pinned as the backfill
  // rows paint and the list grows — no one-shot rAF needed here.
  useEffect(() => {
    atBottomRef.current = true;
    scheduleTailPin();
  }, [identity, scheduleTailPin]);

  useEffect(() => {
    return () => {
      cancelTailFrames();
      tailObserverRef.current?.disconnect();
    };
  }, [cancelTailFrames]);

  const onLoadEarlier = useCallback(() => {
    // Older rows prepend ABOVE the viewport. Rely on the browser's native
    // scroll anchoring (`overflow-anchor`, default on this plain non-virtualized
    // scroller) to hold the reading position, instead of manual scrollTop math:
    // an in-viewport anchor node stays put as content grows above it. Accepted
    // small error — Load earlier is low-frequency and merge-case precision is
    // intentionally out of scope: when older same-kind events merge into the
    // block currently under the reader its markdown re-renders and the position
    // can shift slightly, and a concurrent tail-append is not isolated; neither
    // is worth reintroducing manual compensation or a fragment anchor.
    void loadEarlier();
  }, [loadEarlier]);

  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    atBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
  }, []);

  // Tail-follow that survives late layout growth. Opening from the agent side
  // panel can mount the list before the pane's width/height and Markdown/code
  // wrapping have settled; a one-shot scroll can land mid-list. While the user
  // is pinned to bottom, observe BOTH the list (content height/wrapping) and the
  // scroller (viewport size) and re-pin on changes. Once they scroll up to read
  // history, atBottomRef flips false (onScroll) and follow stops — manual
  // up-scroll and "Load earlier" are never yanked.
  const setListRef = useCallback(
    (node: HTMLOListElement | null) => {
      tailObserverRef.current?.disconnect();
      tailObserverRef.current = null;
      if (!node) return;
      const ro = new ResizeObserver(() => {
        scheduleTailPin();
      });
      ro.observe(node);
      if (scrollRef.current) ro.observe(scrollRef.current);
      tailObserverRef.current = ro;
      scheduleTailPin();
    },
    [scheduleTailPin],
  );

  // Fold codex's per-token thinking/text fragments into display blocks. Pure
  // display layer — raw `events` (and their ids) are untouched, so dedupe /
  // backfill stay correct. See `coalesceActivity`.
  const items = useMemo(() => coalesceActivity(events), [events]);

  return (
    // `min-w-0` is load-bearing: as a flex-1 row child its default
    // `min-width:auto` refuses to shrink below content min-content, so a wide
    // Activity row (Markdown code block, long tool arg) would push the pane
    // wider than its column — which clips overflow-hidden, hiding the
    // right-pinned time with no horizontal scroll (#task:1288). With min-w-0
    // the chain is width-bounded and `.code-block` scrolls internally instead.
    <div className="flex min-h-0 min-w-0 flex-1 flex-col">
      <div ref={scrollRef} onScroll={onScroll} className="flex-1 overflow-y-auto px-5 py-3">
        {phase === "loading" ? (
          <AgentActivitySkeleton />
        ) : phase === "error" ? (
          <p className="py-8 text-center text-xs text-text-muted">
            {t("agents.couldntLoadActivity")}
          </p>
        ) : events.length === 0 ? (
          <p className="py-8 text-center text-xs text-text-muted">{t("agents.noActivityYetIn")}</p>
        ) : (
          <>
            {nextBeforeId !== null ? (
              <button
                type="button"
                onClick={() => void onLoadEarlier()}
                disabled={isLoadingMore}
                className="mx-auto mb-1 flex items-center gap-1 px-2 py-1.5 text-(length:--text-meta) text-text-muted transition-colors hover:text-text-primary disabled:opacity-50"
              >
                <ChevronUpIcon className="size-3" />
                {isLoadingMore ? t("agents.loading") : t("agents.loadEarlier")}
              </button>
            ) : null}
            <ol ref={setListRef} className="flex flex-col">
              {items.map((item) =>
                item.type === "group" ? (
                  <GroupRow key={item.key} item={item} />
                ) : (
                  <EventRow
                    key={item.key}
                    event={item.event}
                    completed={item.completed}
                    outcome={item.outcome}
                  />
                ),
              )}
            </ol>
          </>
        )}
      </div>
    </div>
  );
}

/** Defensive payload reader — payload is `Record<string, unknown> | null`,
 *  not a static type per kind, so every access is guarded. */
function str(payload: Record<string, unknown> | null, key: string): string | null {
  const v = payload?.[key];
  return typeof v === "string" && v.length > 0 ? v : null;
}

interface KindVisual {
  Icon: typeof ToolIcon;
  tone: "neutral" | "accent" | "error" | "warning" | "muted";
}

function visualForKind(kind: ConversationEventKind): KindVisual {
  switch (kind) {
    case "thinking":
      return { Icon: ThinkingIcon, tone: "accent" };
    case "text":
      return { Icon: MessageIcon, tone: "neutral" };
    case "tool_call":
      return { Icon: ToolIcon, tone: "neutral" };
    case "tool_output":
      return { Icon: EnterIcon, tone: "muted" };
    case "shell":
      return { Icon: TerminalIcon, tone: "neutral" };
    case "error":
    case "stderr":
      return { Icon: WarningIcon, tone: "error" };
    case "parsed_event_dropped":
      return { Icon: CancelledIcon, tone: "warning" };
    case "message_stale":
      return { Icon: HistoryIcon, tone: "warning" };
    case "api_response":
      // Default = the waiting/ready queue lifecycle (hourglass). The
      // generic `notice` class is overridden at EventRow, where the
      // payload status is in scope — neutral Info, no queue connotation.
      return { Icon: WaitingIcon, tone: "warning" };
    case "session_init":
      return { Icon: PowerIcon, tone: "muted" };
    case "message_received":
      return { Icon: InboxIcon, tone: "neutral" };
    case "compaction_started":
    case "compaction_finished":
      return { Icon: LayersIcon, tone: "muted" };
    default:
      // turn_end / file_change / unknown / any future kind — render a
      // generic line rather than crash.
      return { Icon: MessageIcon, tone: "muted" };
  }
}

const TONE_TEXT: Record<KindVisual["tone"], string> = {
  neutral: "text-text-secondary",
  accent: "text-accent",
  error: "text-error",
  warning: "text-warning",
  muted: "text-text-muted",
};

/** Time + tool params share one dim tier (`--activity-meta`) so the row reads
 *  name-first; defined once here, never per event. */
const ACTIVITY_META = "text-[color:var(--activity-meta)]";

/**
 * Shared row chrome: a `time → icon → body` grid aligned to the first line.
 * `preview` (prose) stays in the body column; `block` (code / diff) starts at
 * the icon column so wide content gains the time column's width.
 */
function RowShell({
  Icon,
  tone,
  title,
  time,
  preview,
  block,
}: {
  Icon: KindVisual["Icon"];
  tone: KindVisual["tone"];
  title: React.ReactNode;
  time: string;
  preview?: React.ReactNode;
  block?: React.ReactNode;
}): JSX.Element {
  return (
    <li className="grid grid-cols-[auto_auto_minmax(0,1fr)] items-start gap-x-2.5 py-1">
      <span
        className={cn("mt-0.5 font-mono text-(length:--text-badge) tabular-nums", ACTIVITY_META)}
      >
        {time}
      </span>
      <Icon className={cn("mt-0.5 size-3.5", TONE_TEXT[tone])} />
      <div className="min-w-0 text-xs leading-snug">
        <div className="wrap-break-word">{title}</div>
        {preview ? <div className="mt-1 min-w-0">{preview}</div> : null}
      </div>
      {block ? (
        <div data-activity-block className="col-span-2 col-start-2 mt-1 min-w-0">
          {block}
        </div>
      ) : null}
    </li>
  );
}

/** Toggle on a plain click; a click that ends a text selection is a copy, not a toggle. */
function toggleUnlessSelecting(toggle: () => void): void {
  const selection = window.getSelection();
  if (selection && !selection.isCollapsed) {
    return;
  }
  toggle();
}

/**
 * A tool argument that can expand. Collapsed it is one truncated line after the
 * tool name; expanded it leaves the title and becomes a full-width block, so
 * long commands use the row's width instead of the column after the name.
 */
function ToolArg({
  text,
  expanded,
  onToggle,
}: {
  text: string;
  expanded: boolean;
  onToggle: () => void;
}): JSX.Element {
  return (
    <span
      role="button"
      tabIndex={0}
      aria-expanded={expanded}
      data-activity-arg
      className={cn(
        expanded
          ? "block cursor-text whitespace-pre-wrap wrap-anywhere bg-(--activity-params-bg) px-2 py-1 font-mono text-(length:--text-meta) leading-relaxed"
          : "ml-1.5 min-w-0 cursor-pointer truncate font-mono",
        ACTIVITY_META,
      )}
      onClick={() => {
        toggleUnlessSelecting(onToggle);
      }}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          onToggle();
        }
      }}
    >
      {text}
    </span>
  );
}

/**
 * Six-line window over a code or diff block. The toggle exists only when the
 * content actually overflows, measured after layout so width changes re-check.
 */
function ClampedBlock({ children }: { children: React.ReactNode }): JSX.Element {
  const ref = useRef<HTMLDivElement>(null);
  const [expanded, setExpanded] = useState(false);
  const [overflows, setOverflows] = useState(false);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el || expanded) {
      return;
    }
    const measure = () => {
      setOverflows(el.scrollHeight > el.clientHeight + 1);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => {
      observer.disconnect();
    };
  }, [expanded]);

  const clamped = !expanded && overflows;

  return (
    <div>
      <div
        ref={ref}
        data-activity-block-clamped={clamped ? "" : undefined}
        className="overflow-hidden font-mono text-(length:--text-meta) leading-relaxed"
        style={expanded ? undefined : { maxHeight: "calc(6lh + 0.5rem)" }}
      >
        {children}
      </div>
      {overflows ? (
        <button
          type="button"
          data-activity-block-toggle
          className="mt-0.5 text-(length:--text-meta) text-text-secondary hover:text-text-primary"
          onClick={() => {
            setExpanded((value) => !value);
          }}
        >
          {expanded ? t("agents.showLess") : t("agents.showAll")}
        </button>
      ) : null}
    </div>
  );
}

/** A coalesced thinking/text block. `text` renders as Markdown (assistant
 *  message); `thinking` as verbatim reasoning prose (italic, not markdown). */
function GroupRow({
  item,
}: {
  item: Extract<ActivityDisplayItem, { type: "group" }>;
}): JSX.Element {
  const { kind, text, representative } = item;
  const { Icon, tone } = visualForKind(kind);
  const time = dayjs(representative.created_at).format("HH:mm:ss");

  if (kind === "text") {
    // Whitespace-only text groups are dropped in `coalesceActivity`, so a text
    // group that reaches here always has content.
    return <RowShell Icon={Icon} tone={tone} time={time} title={<Markdown content={text} />} />;
  }

  return (
    <RowShell
      Icon={Icon}
      tone={tone}
      time={time}
      title={<span className="text-text-secondary">Thinking</span>}
      preview={
        text.trim().length > 0 ? (
          <span className="block whitespace-pre-wrap wrap-break-word text-xs italic leading-snug text-text-secondary">
            {text}
          </span>
        ) : null
      }
    />
  );
}

/** The trailing marker on a finished `tool_call`. Colour carries the verdict:
 *  only a proven `success` is green — `failed` is an error mark, `cancelled`
 *  neutral, and `completed` (the wire default for old / unclassified outputs) a
 *  neutral "done". A finished call whose success we can't prove is never green. */
function ToolOutcomeMark({
  outcome,
  className = "ml-1.5 -translate-y-px",
}: {
  outcome: ToolOutcome;
  className?: string;
}): JSX.Element {
  const base = cn("inline-block size-3 shrink-0", className);
  switch (outcome) {
    case "success":
      return <SuccessIcon className={cn(base, "text-success")} aria-label="succeeded" />;
    case "failed":
      return <FailedIcon className={cn(base, "text-error")} aria-label="failed" />;
    case "cancelled":
      return <CancelledIcon className={cn(base, "text-text-muted")} aria-label="cancelled" />;
    case "completed":
      return <SuccessIcon className={cn(base, "text-text-muted")} aria-label="finished" />;
  }
}

/** A single non-coalesced event: tool_call (via `presentToolCall`, with an
 *  outcome-coloured mark when its output was folded in), an orphan tool_output
 *  marker, error/dropped reasons, lifecycle markers. */
function EventRow({
  event,
  completed,
  outcome,
}: {
  event: ActivityEvent;
  completed?: boolean;
  outcome?: ToolOutcome;
}): JSX.Element {
  const { kind, payload } = event;
  let { Icon, tone } = visualForKind(kind);
  if (kind === "api_response" && str(payload, "status") === "notice") {
    // #task:1582 display boundary: waiting/ready are the queue
    // lifecycle (hourglass); `notice` is the generic normalized
    // response — neutral Info semantics, not a wait.
    Icon = InfoIcon;
    tone = "neutral";
  }
  const time = dayjs(event.created_at).format("HH:mm:ss");

  if (kind === "tool_call") {
    return <ToolCallRow event={event} completed={completed} outcome={outcome} />;
  }

  if (kind === "compaction_started" || kind === "compaction_finished") {
    // Paired compaction (#task:1187): the compaction glyph (Layers) stays put
    // on the left; the in-flight/done state rides a TRAILING marker after the
    // title — a spinning loader while running, then the same green success
    // check tool_call uses, once its `compaction_finished` folds in
    // (`completed`). A `compaction_finished` only reaches here as an ORPHAN
    // (paired ones drop in coalesceActivity) → standalone done. An orphan
    // started stays loading — never fabricate a completion.
    const done = kind === "compaction_finished" || completed === true;
    return (
      <RowShell
        Icon={Icon}
        tone={tone}
        time={time}
        title={
          <span className="text-text-muted">
            {done ? "Context compacted" : "Compacting context…"}
            {done ? (
              <SuccessIcon
                className="ml-1.5 inline-block size-3 -translate-y-px text-success"
                aria-label="completed"
              />
            ) : (
              <SpinnerIcon
                className="ml-1.5 inline-block size-3 -translate-y-px animate-spin text-text-muted"
                aria-label="in progress"
              />
            )}
          </span>
        }
      />
    );
  }

  let title: React.ReactNode;
  if (kind === "tool_output") {
    // Orphan output (its tool_call paged out by retention): a one-line marker.
    // This row is the only surviving record of that call's verdict, so the
    // outcome rides the same mark a folded call uses — a failed/cancelled orphan
    // reads honestly, never as a neutral "returned". Missing/unknown → completed.
    const name = str(payload, "name");
    const oc = outcome ?? "completed";
    const verb = oc === "failed" ? "failed" : oc === "cancelled" ? "cancelled" : "returned";
    title = (
      <span className={oc === "failed" ? "text-error" : "text-text-muted"}>
        {name ? `${name} ${verb}` : verb}
        <ToolOutcomeMark outcome={oc} />
      </span>
    );
  } else if (kind === "error" || kind === "stderr") {
    title = (
      <span className="wrap-break-word text-error">{str(payload, "message") ?? "Error"}</span>
    );
  } else if (kind === "api_response") {
    const isNotice = str(payload, "status") === "notice";
    title = (
      <span
        className={
          isNotice ? "wrap-break-word text-text-secondary" : "wrap-break-word text-warning"
        }
      >
        {str(payload, "message") ?? "Provider response"}
      </span>
    );
  } else if (kind === "parsed_event_dropped") {
    const pk = str(payload, "parsed_kind");
    title = (
      <span className="text-warning">
        {pk ? `${pk} dropped (too large)` : "Event dropped (too large)"}
      </span>
    );
  } else if (kind === "message_stale") {
    // The ONLY place this label is produced now (#task:2099). It used to be
    // guessed from the agent's command line in `activityPresentation.ts`; the
    // row is the backend's own verdict, so it needs no payload to say so.
    title = <span className="text-warning">Message send stale</span>;
  } else if (kind === "session_init") {
    title = <span className="text-text-muted">Session started</span>;
  } else if (kind === "message_received") {
    // #task:2146: the runtime accepted an inbound delivery. Payload is just the
    // accepted count (no body/ids — the timeline records that input arrived, not
    // what it said). One canonical label; the count is a dim suffix only when a
    // batch of more than one settled at once, never a second phrasing.
    // Same text color as a command label (no independent hue): a row's outcome is
    // carried only by its marker, so a distinct color here would read as a state.
    const count = typeof payload?.["count"] === "number" ? (payload["count"] as number) : 1;
    title = (
      <span className="font-semibold text-text-primary">
        Message received
        {count > 1 ? (
          <span className={cn("ml-1.5 font-normal tabular-nums", ACTIVITY_META)}>×{count}</span>
        ) : null}
      </span>
    );
  } else {
    title = <span className="text-text-muted">{kind}</span>;
  }

  return <RowShell Icon={Icon} tone={tone} time={time} title={title} />;
}

function ToolCallRow({
  event,
  completed,
  outcome,
}: {
  event: ActivityEvent;
  completed?: boolean;
  outcome?: ToolOutcome;
}): JSX.Element {
  const [argExpanded, setArgExpanded] = useState(false);

  const { Icon, tone } = visualForKind(event.kind);
  const time = dayjs(event.created_at).format("HH:mm:ss");
  const p = presentActivityToolCall(event.payload);
  const toggleArg = () => {
    setArgExpanded((value) => !value);
  };

  const expandedArg =
    p.arg && argExpanded ? <ToolArg text={p.arg} expanded onToggle={toggleArg} /> : null;
  const detail = p.detail ? (
    <ClampedBlock>
      <ToolDetailView detail={p.detail} />
    </ClampedBlock>
  ) : null;
  const block =
    expandedArg || detail ? (
      <div className="flex flex-col gap-1">
        {expandedArg}
        {detail}
      </div>
    ) : null;

  return (
    <RowShell
      Icon={Icon}
      tone={tone}
      time={time}
      title={
        <span className="flex min-w-0 items-start text-text-primary">
          <span className="shrink-0 font-semibold">{p.verb}</span>
          {completed ? (
            <span className="ml-1.5 flex h-[1lh] shrink-0 items-center">
              <ToolOutcomeMark outcome={outcome ?? "completed"} className="" />
            </span>
          ) : null}
          {p.arg && !argExpanded ? (
            <ToolArg text={p.arg} expanded={false} onToggle={toggleArg} />
          ) : null}
        </span>
      }
      block={block}
    />
  );
}

/** Renders a tool_call's optional richer body: a diff (Edit/Write), or a
 *  monospace code/JSON block (shell / fallback). */
function ToolDetailView({ detail }: { detail: ToolDetail }): JSX.Element {
  if (detail.kind === "diff") {
    return <DiffView removed={detail.removed} added={detail.added} />;
  }
  return (
    <pre
      className={cn(
        "m-0 whitespace-pre-wrap wrap-anywhere bg-(--activity-params-bg) px-2 py-1 font-[inherit] leading-[inherit]",
        ACTIVITY_META,
      )}
    >
      {detail.text}
    </pre>
  );
}

/** Edit/Write diff preview built from `tool_call.input` (old/new). Removed
 *  lines under a red rail, added under a green rail; tool RESULT output is
 *  never shown (marker-only). */
function DiffView({ removed, added }: { removed: string; added: string }): JSX.Element {
  return (
    <div className="flex flex-col gap-px border border-border-subtle">
      <DiffSide
        text={removed}
        sign="-"
        railClass="border-error bg-error/10"
        signClass="text-error"
      />
      <DiffSide
        text={added}
        sign="+"
        railClass="border-success bg-success/10"
        signClass="text-success"
      />
    </div>
  );
}

function DiffSide({
  text,
  sign,
  railClass,
  signClass,
}: {
  text: string;
  sign: string;
  railClass: string;
  signClass: string;
}): React.ReactNode {
  if (text.length === 0) return null;
  return (
    <div className={cn("border-l-2", railClass)}>
      {text.split("\n").map((line, i) => (
        <div key={i} className="flex gap-2 px-2">
          <span className={cn("shrink-0 select-none", signClass)}>{sign}</span>
          <span className="min-w-0 whitespace-pre-wrap wrap-anywhere text-text-secondary">
            {line.length > 0 ? line : " "}
          </span>
        </div>
      ))}
    </div>
  );
}
