import { useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import * as Popover from "@radix-ui/react-popover";
import dayjs from "dayjs";
import { CheckIcon, CloseIcon, SpinnerIcon } from "@/icons";
import { TASK_CONTENT_MAX_LEN, type TaskDetailDto, type TaskStatus } from "@silverhand/shared";
import { Textarea } from "@/components/ui/input";
import { ChannelIcon } from "@/components/channel-icon";
import { MiniIconButton } from "@/features/tasks/components/mini-icon-button";
import {
  PersonAvatar,
  usePersonDirectory,
  type PersonDirectory,
} from "@/features/tasks/components/person-avatar";
import { TaskStatusBadge } from "@/features/tasks/components/task-status-badge";
import { useAssignTask, useEditTask, useUpdateTaskStatus } from "@/features/tasks/hooks/hooks";
import type { TaskDetailFailure } from "@/features/tasks/model/task-detail";
import { useConversationMembers } from "@/features/conversations/use-conversation-members";
import { useDirectPairPane, projectDirectPairAssignee } from "@/features/direct-pairs/hooks";
import { cn } from "@/lib/cn";
import { t } from "@/lib/i18n";

/**
 * Substance shared by the two task-detail surfaces — the modal
 * (`TaskDetailDialog`, opened from a board card) and the
 * conversation-anchored overlay pane (`TaskDetailPanel`). Everything here is
 * chrome-agnostic: the single-field inline editor, the failure copy, the
 * single-write-slot rule, and the metadata value/write primitives. Each surface
 * supplies its OWN chrome (Dialog vs. side pane) and its OWN metadata LAYOUT
 * (fixed 240px sidebar vs. width-adaptive grid); the pieces below are the
 * identity/behaviour they both paint, so the two can never drift on how a task
 * edits, fails, or renders its facts.
 */

/** Which field is currently in inline-edit mode; `null` is the read view. Only
 *  one field edits at a time — the other field's pencil is hidden until the
 *  current edit finishes, so there is never a mid-edit switch. */
export type EditingField = "title" | "content" | null;

// ── Failure copy ────────────────────────────────────────────────────

export const failureCopy = (): Record<TaskDetailFailure, { header: string; body: string }> => ({
  not_found: {
    header: t("tasks.taskNotFound"),
    // Both causes answer 404 and the viewer can't tell them apart, so the copy
    // names both rather than asserting the one we can't verify.
    body: t("tasks.thisTaskWasDeleted"),
  },
  unreachable: {
    header: t("tasks.cantReachTheServer"),
    // No Retry here: the app-status banner already owns this signal and its
    // retry. A second, narrower retry button would be a competing control for
    // the same recovery.
    body: t("tasks.theTaskCantBe"),
  },
  transient: {
    header: t("tasks.couldntLoadThisTask"),
    body: t("tasks.theRequestFailed"),
  },
});

// ── Single write slot ───────────────────────────────────────────────

export interface TaskWriteSlot {
  /** A write occupies the single slot — the title/content inline editor being
   *  OPEN counts, not just an in-flight request. While busy, every meta trigger
   *  goes inert so one interaction can't start a second write whose response
   *  lands on top of the first. */
  busy: boolean;
  statusPending: boolean;
  statusFailed: boolean;
  onStatusChange: (status: TaskStatus) => void;
  assignPending: boolean;
  assignFailed: boolean;
  onAssign: (agentId: string | null) => void;
}

/**
 * The single write slot shared across a task's title, content, status and
 * assignee. Title/content editing is tracked by the caller (`editingField`);
 * status/assignee live here. All four share ONE slot: while any is open or in
 * flight the others go inert, because `useEditTask`'s whole-object cache write
 * can otherwise land last and clobber a newer status/assignee. Local slot only
 * — no lock, no revision; remote writes stay last-write-wins, converging over
 * `task_changed`. Both surfaces call this so the invariant holds identically.
 */
export function useTaskWriteSlot(taskId: string, editingField: EditingField): TaskWriteSlot {
  const updateStatus = useUpdateTaskStatus();
  const assign = useAssignTask();
  const metaPending = updateStatus.isPending || assign.isPending;
  return {
    busy: editingField !== null || metaPending,
    statusPending: updateStatus.isPending,
    statusFailed: updateStatus.isError,
    onStatusChange: (status) => {
      updateStatus.reset();
      updateStatus.mutate({ taskId, status });
    },
    assignPending: assign.isPending,
    assignFailed: assign.isError,
    onAssign: (agentId) => {
      assign.reset();
      assign.mutate({ taskId, agentId });
    },
  };
}

// ── Metadata rows (SSOT for field order / labels / write wiring) ─────

/** Statuses whose ASSIGNMENT is frozen. `/claim` refuses both server-side, so
 *  offering the control would promise a click the server won't honour; the row
 *  stays a plain read-only value. (The 409 remains the authority.) */
const ASSIGNMENT_FROZEN: ReadonlySet<TaskStatus> = new Set<TaskStatus>(["done", "closed"]);

const TIMESTAMP_FORMAT = "YYYY-MM-DD HH:mm";

export interface TaskMetaWriteProps extends TaskWriteSlot {
  /** The AUTHORITATIVE detail read (never a board summary merged per field). */
  view: TaskDetailDto;
}

export interface TaskMetaRowSpec {
  key: string;
  label: string;
  value: ReactNode;
  /** Inline write failure for THIS row, if any. */
  error?: string | null;
}

/**
 * The ordered metadata rows for a task, as data — the SINGLE source of which
 * fields show, in what order, with what value node and write wiring. Each
 * surface renders these into its own grid (the modal's fixed sidebar, the
 * pane's width-adaptive columns), so the field set never forks between them.
 * A missing optional fact (Closed) emits no row — never a `—`
 * placeholder, so present rows don't shift when one appears.
 */
export function useTaskMetaRows({
  view,
  busy,
  statusPending,
  statusFailed,
  onStatusChange,
  assignPending,
  assignFailed,
  onAssign,
}: TaskMetaWriteProps): TaskMetaRowSpec[] {
  const directory = usePersonDirectory();
  const canAssign = !ASSIGNMENT_FROZEN.has(view.status);

  const rows: TaskMetaRowSpec[] = [
    {
      key: "task",
      label: t("tasks.task"),
      value: (
        <span className="font-mono text-(length:--text-meta) text-text-secondary">
          #{view.task_number}
        </span>
      ),
    },
    {
      key: "status",
      label: t("tasks.status"),
      error: statusFailed ? t("tasks.couldntUpdateStatus") : null,
      value: (
        <span className="flex items-center gap-1.5">
          <TaskStatusBadge status={view.status} {...(busy ? {} : { onChange: onStatusChange })} />
          {statusPending ? <PendingSpinner /> : null}
        </span>
      ),
    },
    {
      key: "assignee",
      label: t("tasks.assignee"),
      error: assignFailed ? t("tasks.couldntUpdateAssignee") : null,
      value: (
        // Clearing is an inline ✕ on the current value (not a picker row), so
        // the dropdown holds only candidates. Revealed on row hover / focus so
        // an always-on ✕ isn't parked as chrome.
        <span className="group/assignee flex items-center gap-1">
          {canAssign && !busy ? (
            <AssigneePicker
              conversationId={view.place.conversation_id}
              placement={view.place.kind}
              currentAgentId={view.assignee?.type === "agent" ? view.assignee.id : null}
              directory={directory}
              onPick={onAssign}
            >
              <AssigneeValue view={view} directory={directory} />
            </AssigneePicker>
          ) : (
            <AssigneeValue view={view} directory={directory} />
          )}
          {canAssign && !busy && view.assignee ? (
            <MiniIconButton
              icon={CloseIcon}
              label={t("tasks.clearAssignee")}
              onClick={() => onAssign(null)}
              className="ml-1 opacity-0 transition-opacity group-hover/assignee:opacity-100 focus-visible:opacity-100"
            />
          ) : null}
          {assignPending ? <PendingSpinner /> : null}
        </span>
      ),
    },
    {
      // "From", not "Creator": answers where the task came from and reads as the
      // counterpart of the Assignee row rather than a second ownership claim.
      key: "from",
      label: t("tasks.from"),
      value: (
        <span className="flex min-w-0 items-center gap-1.5">
          {view.creator.type !== "system" && view.creator.id ? (
            <PersonAvatar
              person={{ type: view.creator.type, id: view.creator.id }}
              name={view.creator.name}
              {...directory}
            />
          ) : null}
          <span className="truncate" title={view.creator.name}>
            {view.creator.name}
          </span>
        </span>
      ),
    },
    // Only a workspace placement has a channel coordinate. A DirectPair task's
    // "channel" is the pair itself — there is nothing to name — so the row is
    // omitted rather than shown with a stand-in label (shiki dogfood, #2041).
    ...(view.place.kind === "workspace"
      ? [
          {
            key: "channel",
            label: t("tasks.channel"),
            value: (
              <span className="flex min-w-0 items-center gap-1.5">
                <ChannelIcon variant="sticker" density="sm" className="text-text-muted" />
                <span className="truncate" title={view.place.channel_name}>
                  {view.place.channel_name}
                </span>
              </span>
            ),
          } satisfies TaskMetaRowSpec,
        ]
      : []),
    {
      key: "created",
      label: t("tasks.created"),
      value: <Timestamp at={view.created_at} />,
    },
  ];

  if (view.closed_at !== null) {
    rows.push({
      key: "closed",
      label: t("tasks.closed"),
      value: <Timestamp at={view.closed_at} />,
    });
  }

  return rows;
}

function Timestamp({ at }: { at: number }): JSX.Element {
  return (
    <time dateTime={new Date(at).toISOString()} className="font-mono text-(length:--text-meta)">
      {dayjs(at).format(TIMESTAMP_FORMAT)}
    </time>
  );
}

function PendingSpinner(): JSX.Element {
  return <SpinnerIcon className="size-3 shrink-0 animate-spin text-text-muted" aria-hidden />;
}

function AssigneeValue({
  view,
  directory,
}: {
  view: TaskDetailDto;
  directory: PersonDirectory;
}): JSX.Element {
  if (!view.assignee) {
    return <span className="italic text-text-muted">{t("tasks.unassigned2")}</span>;
  }
  return (
    <span className="flex min-w-0 items-center gap-1.5">
      <PersonAvatar
        person={{ type: view.assignee.type, id: view.assignee.id }}
        name={view.assignee.name}
        {...directory}
      />
      <span className="truncate" title={view.assignee.name}>
        {view.assignee.name}
      </span>
    </span>
  );
}

// ── Assignee picker ────────────────────────────────────────────────

/**
 * Assign / re-assign to an agent of this task's PLACEMENT. The candidate source
 * is placement-specific so no row here is one the server would refuse:
 *   - workspace   → the conversation's AGENT members (the set `/claim` validates
 *                   against), from `useConversationMembers`;
 *   - direct_pair → the pair's counterpart agent, from the pair-participation
 *                   carrier (`useDirectPairsList()`). A DirectPair has no roster,
 *                   so `/members` there answers empty; the pair list is the
 *                   authority (@Cindy, #task:2041 / #task:2037).
 *
 * Humans can hold an assignee and are displayed, but the write path only takes
 * `agent_id`, so they are never offered. Clearing is the inline ✕ beside the
 * value, not a row here.
 */
function AssigneePicker({
  conversationId,
  placement,
  currentAgentId,
  directory,
  onPick,
  children,
}: {
  conversationId: string;
  placement: "workspace" | "direct_pair";
  currentAgentId: string | null;
  directory: PersonDirectory;
  /** Candidates only — `null` (clearing) belongs to the inline ✕. */
  onPick: (agentId: string) => void;
  children: ReactNode;
}): JSX.Element {
  const [open, setOpen] = useState(false);
  // Picking closes the menu always; it writes only for a DIFFERENT agent. The
  // current assignee sits in the domain as a selected / no-op row (so a DM whose
  // one counterpart already holds the task still SHOWS that agent — @Cindy,
  // #task:2041) and re-choosing it must not fire a redundant assign.
  const handleChoose = (candidate: AssigneeCandidate) => {
    setOpen(false);
    if (!candidate.isCurrent) onPick(candidate.id);
  };
  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <button
          type="button"
          aria-label={t("tasks.changeAssignee")}
          title={t("tasks.changeAssignee")}
          // Hover = a slight fade, the same affordance the status chip uses — no
          // ground, no border; two editable rows in one column answer "I'm
          // clickable" the same way.
          className="flex min-w-0 items-center rounded-(--control-radius) text-left transition-opacity hover:opacity-80 focus-outline"
        >
          {children}
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          align="start"
          sideOffset={4}
          collisionPadding={8}
          data-popover-frame="info"
          // Above the host dialog's layer (`z-[var(--z-dialog)]`) — a picker under its host is
          // unclickable. The pane host has no such layer, so this is safe there
          // too.
          className="z-(--z-popover-nested) min-w-45 max-w-60 bg-bg-floating p-1 shadow-popover outline-none"
          onCloseAutoFocus={(e) => e.preventDefault()}
        >
          {placement === "direct_pair" ? (
            <DirectPairPickerBody
              conversationId={conversationId}
              currentAgentId={currentAgentId}
              directory={directory}
              onChoose={handleChoose}
            />
          ) : (
            <WorkspacePickerBody
              conversationId={conversationId}
              currentAgentId={currentAgentId}
              directory={directory}
              onChoose={handleChoose}
            />
          )}
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

/** One assignable agent — the shape both placement sources project into, so the
 *  list markup (avatar + `@handle`) is written once. */
interface AssigneeCandidate {
  id: string;
  /** The `@`-handle shown on the row. */
  name: string;
  /** The display name on the avatar. */
  displayName: string;
  /** Already this task's assignee → shown selected, picking it is a no-op. */
  isCurrent: boolean;
}

/** The picker's list, shared by both placement bodies so the row markup and the
 *  empty/loading copy live in one place. `placeholder` is what shows when there
 *  are no candidates — the caller distinguishes loading / load-failed / genuinely
 *  empty, so a cold or failed read is never rendered as "nobody to assign".
 *  `onChoose` closes the menu for every row but writes only for a non-current
 *  one; the already-assigned row is a selected no-op (@Cindy, #task:2041). */
function CandidateList({
  candidates,
  placeholder,
  directory,
  onChoose,
}: {
  candidates: readonly AssigneeCandidate[];
  placeholder: string;
  directory: PersonDirectory;
  onChoose: (candidate: AssigneeCandidate) => void;
}): JSX.Element {
  return (
    <div className="flex flex-col gap-1">
      {candidates.length === 0 ? (
        <p className="px-2 py-1.5 text-(length:--text-meta) italic text-text-muted">
          {placeholder}
        </p>
      ) : (
        candidates.map((c) => (
          <button
            key={c.id}
            type="button"
            aria-current={c.isCurrent ? "true" : undefined}
            onClick={() => onChoose(c)}
            className="flex w-full min-w-0 items-center gap-2 rounded-(--select-item-radius) px-2 py-1 text-left text-xs transition-colors hover:bg-bg-hover"
          >
            <PersonAvatar
              person={{ type: "agent", id: c.id }}
              name={c.displayName}
              {...directory}
            />
            {/* Handle-pick surface: this is the one task place that shows a
                genuine `@`-handle (`c.name` = handle, distinct from the
                `c.displayName` on the avatar) — you're choosing who to assign,
                the same moment as typing a mention. Every DISPLAY of an
                assignee/creator (rows above, board cards, filter, TaskRef
                popover) shows the plain display name with no `@` (#task:1768,
                shiki 2026-08-01). */}
            <span className="min-w-0 flex-1 truncate" title={c.displayName}>
              @{c.name}
            </span>
            {/* The current assignee stays in the domain (so a DM always shows its
                agent) but reads as selected — a check, not a removed row. */}
            {c.isCurrent ? (
              <CheckIcon
                className="size-3.5 shrink-0 text-text-muted"
                aria-label={t("tasks.currentAssignee")}
              />
            ) : null}
          </button>
        ))
      )}
    </div>
  );
}

/** Workspace placement: candidates are the conversation's other AGENT members.
 *  The current assignee is excluded here (unlike the DM body): a workspace board
 *  has several members, the picker reads as "reassign to someone else", and the
 *  current holder already shows in the field above. Split into its own component
 *  so `useConversationMembers` (and its `/members` read) only mounts once Radix
 *  opens the popover AND only for a workspace task — a DirectPair never fires it. */
function WorkspacePickerBody({
  conversationId,
  currentAgentId,
  directory,
  onChoose,
}: {
  conversationId: string;
  currentAgentId: string | null;
  directory: PersonDirectory;
  onChoose: (candidate: AssigneeCandidate) => void;
}): JSX.Element {
  const roster = useConversationMembers(conversationId);
  const candidates = useMemo<AssigneeCandidate[]>(() => {
    if (roster.status !== "resolved") return [];
    return roster.members.flatMap((m) =>
      m.kind === "agent" && m.agent_id !== currentAgentId
        ? [
            {
              id: m.agent_id,
              name: m.name,
              displayName: m.display_name || m.name,
              isCurrent: false,
            },
          ]
        : [],
    );
  }, [roster, currentAgentId]);
  return (
    <CandidateList
      candidates={candidates}
      placeholder={roster.status === "loading" ? t("tasks.loading") : t("tasks.noOtherAgentsHere")}
      directory={directory}
      onChoose={onChoose}
    />
  );
}

/** DirectPair placement: the sole candidate is the pair's counterpart agent, read
 *  from the pair-participation carrier — a DirectPair has no roster (@Cindy,
 *  #task:2041). The candidate domain + readiness are the pure `projectDirectPairAssignee`
 *  seam: the active counterpart is ALWAYS offered, INCLUDING when it is the task's
 *  current assignee (then it reads as a selected no-op, never dropped — the empty-
 *  dropdown bug), and a cold/failed read is surfaced as loading / load-failed, never
 *  folded into an authoritative "no candidate". `useDirectPairPane`'s precedence is
 *  `ready > resolving > error > absent`: a retained MATCHING row IS an answer (the
 *  projection reads it even while a refetch runs); only a retained MISSING row is
 *  left unproven and waits as `resolving`. A human or `deleted` counterpart yields
 *  no candidate — the row's historical assignee still displays from the task DTO,
 *  independent of this set. */
function DirectPairPickerBody({
  conversationId,
  currentAgentId,
  directory,
  onChoose,
}: {
  conversationId: string;
  currentAgentId: string | null;
  directory: PersonDirectory;
  onChoose: (candidate: AssigneeCandidate) => void;
}): JSX.Element {
  const paneState = useDirectPairPane(conversationId);
  const view = projectDirectPairAssignee(paneState, currentAgentId);
  const placeholder =
    view.status === "loading"
      ? t("tasks.loading")
      : view.status === "error"
        ? t("tasks.couldntLoad")
        : t("tasks.noAgentToAssign");
  return (
    <CandidateList
      candidates={view.candidates}
      placeholder={placeholder}
      directory={directory}
      onChoose={onChoose}
    />
  );
}

// ── Inline per-field editor ────────────────────────────────────────

/**
 * In-place editor for ONE field — both title and content are auto-growing
 * textareas. Mounted only while that field is active, so a task_changed
 * refetch of the same task never remounts it and the draft survives.
 *
 * Interaction is chat-composer-style: Enter saves, Shift+Enter inserts a
 * newline, and leaving the field (blur) also saves. The commit is deliberately
 * narrow —
 *   - only a real change is sent (an untouched blur just leaves edit mode);
 *   - the write carries ONLY this field (the omitted field stays unchanged);
 *   - title is submitted VERBATIM: the single-line invariant is owned by
 *     `TaskTitleSchema` and the read view refills from the canonical response;
 *   - a validation/request failure keeps the editor open with an inline error;
 *   - an in-flight write dedups repeat blurs, a successful save latches so the
 *     focus-unmount blur can't re-submit, and an explicit Escape-cancel wins
 *     over the blur that follows it.
 * The IME guard sits at the top of the key handler so a CJK Enter — which only
 * confirms a candidate — never misfires a save.
 *
 * Chrome-agnostic (no Dialog primitives), so both the modal and the pane mount
 * it verbatim; the host owns any surrounding a11y label.
 */
export function InlineFieldEditor({
  taskId,
  field,
  initial,
  onDone,
  onCancel,
}: {
  taskId: string;
  field: "title" | "content";
  initial: string;
  onDone: () => void;
  onCancel: () => void;
}): JSX.Element {
  const edit = useEditTask();
  const [value, setValue] = useState(initial);
  // Why the editor stayed open after a blur (required title empty / body over
  // its raw limit), if at all — drives the inline error and the error border.
  const [blocked, setBlocked] = useState<"empty" | "over" | null>(null);
  const ref = useRef<HTMLTextAreaElement>(null);
  // Escape marks a cancel BEFORE the editor unmounts, so the blur that follows
  // the unmount is a no-op — an explicit cancel must beat blur-to-save.
  const cancelledRef = useRef(false);
  // A committed save latches this: Enter fires commit while the textarea is
  // still focused, so `onDone()` unmounts a focused field → a trailing blur →
  // commit() would run again. The latch makes that blur (and any repeat) a
  // no-op; a FAILED save unlatches so the kept draft can retry.
  const submittingRef = useRef(false);

  const isTitle = field === "title";

  // Both fields auto-grow so the box never scrolls internally — the host owns
  // the scroll. Reset to `auto` first (so a shrink after a delete is measured
  // too), then grow the border-box by whatever is still clipped until nothing
  // is hidden. A single `height = scrollHeight` pass leaves the last line or two
  // clipped: the box is border-box, so setting height to scrollHeight leaves the
  // content area short by the border, and the first write can change the settled
  // scrollHeight past the value just measured (#task:2145).
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    for (let i = 0; i < 4; i++) {
      const clipped = el.scrollHeight - el.clientHeight;
      if (clipped <= 0) break;
      el.style.height = `${el.offsetHeight + clipped}px`;
    }
  }, [value]);

  async function commit(): Promise<void> {
    if (cancelledRef.current || submittingRef.current) return;
    if (edit.isPending) return; // a write is already in flight — dedup blurs
    if (value === initial) {
      onDone(); // untouched → just leave edit mode, send nothing
      return;
    }
    // Mirror the schema's non-empty title rule for instant feedback; the schema
    // stays the authority (and collapses internal whitespace on the server).
    if (isTitle && value.trim().length === 0) {
      setBlocked("empty");
      return;
    }
    // Length is hard-blocked only for the body (raw 10000). Title length is NOT
    // checked here: `TaskTitleSchema` collapses internal whitespace before
    // testing its limit, so the renderer can't compute the canonical length and
    // defers title length to the server (returns as `edit.isError`, stays open).
    if (!isTitle && value.length > TASK_CONTENT_MAX_LEN) {
      setBlocked("over");
      return;
    }
    submittingRef.current = true; // latch before the write settles
    try {
      await edit.mutateAsync(isTitle ? { taskId, title: value } : { taskId, body: value });
    } catch {
      submittingRef.current = false; // failed — unlatch so a retry can resubmit
      return; // stay open; `edit.isError` renders the inline error, draft kept
    }
    onDone();
  }

  function onKeyDown(e: React.KeyboardEvent): void {
    if (e.nativeEvent.isComposing) return; // mid IME composition — leave it alone
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void commit();
      return;
    }
    if (e.key === "Escape") {
      // Cancel the edit WITHOUT dismissing any host layer: preventDefault stops
      // a Radix DismissableLayer from also closing, and the cancel flag makes
      // the ensuing blur a no-op.
      e.preventDefault();
      cancelledRef.current = true;
      onCancel();
    }
  }

  const errorMsg =
    blocked === "empty"
      ? t("tasks.titleCantBeEmpty")
      : blocked === "over"
        ? t("tasks.keepUnder", { max: TASK_CONTENT_MAX_LEN })
        : edit.isError
          ? t("tasks.couldntSaveChangesTry")
          : null;

  return (
    <div className="flex flex-col gap-1">
      <Textarea
        ref={ref}
        aria-label={isTitle ? t("tasks.taskTitle") : t("tasks.taskDescription")}
        rows={1}
        value={value}
        onChange={(e) => {
          setValue(e.target.value);
          if (blocked) setBlocked(null);
          if (edit.isError) edit.reset();
        }}
        onKeyDown={onKeyDown}
        onBlur={() => void commit()}
        autoFocus
        className={cn(
          "resize-none overflow-hidden",
          isTitle
            ? "min-h-0 py-1 text-base font-medium leading-snug"
            : "min-h-30 text-sm leading-relaxed",
          errorMsg && "border-error focus-visible:border-error",
        )}
      />
      {errorMsg ? <p className="text-xs text-error">{errorMsg}</p> : null}
    </div>
  );
}
